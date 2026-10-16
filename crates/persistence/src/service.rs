//! A dedicated database thread.
//!
//! SQLite calls are fast but they are still disk I/O, and the GPUI render loop
//! must never block on them. The connection lives on its own thread; callers
//! send closures and get the result back through a oneshot channel they can
//! await from any executor.

use std::{sync::mpsc, thread};

use anyhow::{anyhow, Result};
use futures::channel::oneshot;

use crate::Database;

type Job = Box<dyn FnOnce(&mut Database) + Send>;

/// Cheap to clone; every clone talks to the same database thread.
#[derive(Clone)]
pub struct DbHandle {
    tx: mpsc::Sender<Job>,
}

impl DbHandle {
    /// Moves `db` onto a new thread. The thread exits once every handle is gone.
    pub fn spawn(mut db: Database) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("cockpit-db".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job(&mut db);
                }
            })?;
        Ok(Self { tx })
    }

    /// Runs `f` on the database thread and resolves to its result.
    pub fn call<R, F>(&self, f: F) -> impl std::future::Future<Output = Result<R>> + Send + 'static
    where
        R: Send + 'static,
        F: FnOnce(&mut Database) -> Result<R> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let sent = self.tx.send(Box::new(move |db| {
            let _ = reply_tx.send(f(db));
        }));
        async move {
            sent.map_err(|_| anyhow!("database thread has stopped"))?;
            reply_rx
                .await
                .map_err(|_| anyhow!("database thread dropped the request"))?
        }
    }

    /// Runs `f` and blocks the calling thread for the result. Only for startup
    /// and shutdown paths that have nothing else to do.
    pub fn call_blocking<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Database) -> Result<R> + Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Box::new(move |db| {
                let _ = reply_tx.send(f(db));
            }))
            .map_err(|_| anyhow!("database thread has stopped"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("database thread dropped the request"))?
    }

    /// Runs `f` without waiting; failures are logged. For writes like event
    /// appends where the caller has no use for the outcome.
    pub fn fire<F>(&self, what: &'static str, f: F)
    where
        F: FnOnce(&mut Database) -> Result<()> + Send + 'static,
    {
        let _ = self.tx.send(Box::new(move |db| {
            if let Err(err) = f(db) {
                tracing::warn!("database write failed ({what}): {err:#}");
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_run_in_order_on_the_db_thread() {
        let handle = DbHandle::spawn(Database::open_in_memory().unwrap()).unwrap();
        handle.fire("set", |db| db.set_setting("k", "v1"));
        let value = futures::executor::block_on(
            handle.call(|db| db.setting("k")),
        )
        .unwrap();
        assert_eq!(value.as_deref(), Some("v1"));

        let blocking = handle
            .call_blocking(|db| {
                db.set_setting("k", "v2")?;
                db.setting("k")
            })
            .unwrap();
        assert_eq!(blocking.as_deref(), Some("v2"));
    }

    #[test]
    fn errors_are_propagated() {
        let handle = DbHandle::spawn(Database::open_in_memory().unwrap()).unwrap();
        let result: Result<()> =
            futures::executor::block_on(handle.call(|_| Err(anyhow!("boom"))));
        assert_eq!(result.unwrap_err().to_string(), "boom");
    }
}

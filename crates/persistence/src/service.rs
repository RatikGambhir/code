//! A dedicated database thread.
//!
//! SQLite calls are fast but they are still disk I/O, and the GPUI render loop
//! must never block on them. The connection lives on its own thread; callers
//! send closures and get the result back through a oneshot channel they can
//! await from any executor. Every closure runs inside [`Database::read`] or
//! [`Database::write`], so each call is one transaction.

use std::{sync::mpsc, thread};

use anyhow::{anyhow, Result};
use futures::channel::oneshot;

use crate::{Database, Reader, Writer};

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

    fn submit<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Database) -> Result<R> + Send + 'static,
    ) -> impl std::future::Future<Output = Result<R>> + Send + 'static {
        let (reply_tx, reply_rx) = oneshot::channel();
        let sent = self.tx.send(Box::new(move |db| {
            let _ = reply_tx.send(job(db));
        }));
        async move {
            sent.map_err(|_| anyhow!("database thread has stopped"))?;
            reply_rx
                .await
                .map_err(|_| anyhow!("database thread dropped the request"))?
        }
    }

    /// Runs `f` with read access on the database thread.
    pub fn read<R, F>(&self, f: F) -> impl std::future::Future<Output = Result<R>> + Send + 'static
    where
        R: Send + 'static,
        F: FnOnce(&Reader<'_>) -> Result<R> + Send + 'static,
    {
        self.submit(move |db| db.read(f))
    }

    /// Runs `f` as one atomic transaction on the database thread.
    pub fn write<R, F>(&self, f: F) -> impl std::future::Future<Output = Result<R>> + Send + 'static
    where
        R: Send + 'static,
        F: FnOnce(&Writer<'_>) -> Result<R> + Send + 'static,
    {
        self.submit(move |db| db.write(f))
    }

    /// Like [`DbHandle::read`], blocking the calling thread. Only for startup
    /// paths that have nothing else to do.
    pub fn read_blocking<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Reader<'_>) -> Result<R> + Send + 'static,
    {
        futures::executor::block_on(self.read(f))
    }

    /// Like [`DbHandle::write`], blocking the calling thread.
    pub fn write_blocking<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Writer<'_>) -> Result<R> + Send + 'static,
    {
        futures::executor::block_on(self.write(f))
    }

    /// Runs a write transaction without waiting; failures are logged. For
    /// writes like event appends where the caller has no use for the outcome.
    pub fn write_detached<F>(&self, what: &'static str, f: F)
    where
        F: FnOnce(&Writer<'_>) -> Result<()> + Send + 'static,
    {
        let _ = self.tx.send(Box::new(move |db| {
            if let Err(err) = db.write(f) {
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
        handle.write_detached("set", |w| w.set_setting("k", "v1"));
        let value = futures::executor::block_on(handle.read(|r| r.setting("k"))).unwrap();
        assert_eq!(value.as_deref(), Some("v1"));

        let blocking = handle
            .write_blocking(|w| {
                w.set_setting("k", "v2")?;
                w.setting("k")
            })
            .unwrap();
        assert_eq!(blocking.as_deref(), Some("v2"));
    }

    #[test]
    fn errors_are_propagated_and_roll_back() {
        let handle = DbHandle::spawn(Database::open_in_memory().unwrap()).unwrap();
        let result: Result<()> = handle.write_blocking(|w| {
            w.set_setting("k", "v")?;
            Err(anyhow!("boom"))
        });
        assert_eq!(result.unwrap_err().to_string(), "boom");
        assert_eq!(handle.read_blocking(|r| r.setting("k")).unwrap(), None);
    }
}

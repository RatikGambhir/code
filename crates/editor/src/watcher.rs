//! Recursive file watching for a workspace.
//!
//! Raw events are noisy (an editor save is often create+modify+rename), so
//! they are batched: the first event opens a short window, and everything
//! that arrives within it is delivered as one [`FsChange`] with the distinct
//! paths involved.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use notify::{RecursiveMode, Watcher as _};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsChange {
    pub paths: BTreeSet<PathBuf>,
    /// Something inside `.git` changed (HEAD, index, refs) — refresh status.
    pub git_changed: bool,
}

pub struct WorkspaceWatcher {
    _watcher: notify::RecommendedWatcher,
}

const BATCH_WINDOW: Duration = Duration::from_millis(150);

impl WorkspaceWatcher {
    pub fn watch(root: &Path) -> Result<(Self, async_channel::Receiver<FsChange>)> {
        let (raw_tx, raw_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = raw_tx.send(event);
        })?;
        watcher.watch(root, RecursiveMode::Recursive)?;

        let (tx, rx) = async_channel::unbounded();
        let root = root.to_path_buf();
        std::thread::Builder::new()
            .name("cockpit-fs-watch".into())
            .spawn(move || {
                while let Ok(first) = raw_rx.recv() {
                    let mut change = FsChange::default();
                    absorb(&root, first, &mut change);
                    let deadline = std::time::Instant::now() + BATCH_WINDOW;
                    loop {
                        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                        match raw_rx.recv_timeout(remaining) {
                            Ok(event) => absorb(&root, event, &mut change),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    if (!change.paths.is_empty() || change.git_changed) && tx.send_blocking(change).is_err() {
                        return;
                    }
                }
            })?;

        Ok((Self { _watcher: watcher }, rx))
    }
}

fn absorb(root: &Path, event: notify::Result<notify::Event>, change: &mut FsChange) {
    let Ok(event) = event else { return };
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return;
    }
    for path in event.paths {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if relative.components().next().is_some_and(|c| c.as_os_str() == ".git") {
            // Object writes are irrelevant; HEAD, index and refs are what
            // change the status the user sees.
            let name = relative.to_string_lossy();
            if !name.contains("/objects/") && !name.ends_with(".lock") {
                change.git_changed = true;
            }
            continue;
        }
        change.paths.insert(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_file_changes_and_git_changes_separately() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        let (_watcher, rx) = WorkspaceWatcher::watch(&root).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        std::fs::write(root.join("a.txt"), "x").unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();

        let mut saw_file = false;
        let mut saw_git = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !(saw_file && saw_git) {
            if let Ok(change) = rx.try_recv() {
                saw_file |= change.paths.iter().any(|p| p.ends_with("a.txt"));
                saw_git |= change.git_changed;
                assert!(change.paths.iter().all(|p| !p.to_string_lossy().contains("/.git/")));
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(saw_file && saw_git);
    }
}

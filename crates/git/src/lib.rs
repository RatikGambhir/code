//! Git integration through the `git` executable.
//!
//! Shelling out keeps behaviour identical to what the user sees in their
//! terminal (hooks, config, credential helpers, sparse checkouts) and avoids
//! linking libgit2. Every call is synchronous; run them off the UI thread.
//!
//! Status uses `--porcelain=v2 -z`, the format git guarantees to keep stable
//! for scripts, and passes `GIT_OPTIONAL_LOCKS=0` so a status refresh never
//! competes for `index.lock` with an agent that is committing.

mod status;

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

pub use status::{BranchInfo, FileState, Status, StatusEntry, StatusKind};

#[derive(Debug, thiserror::Error)]
#[error("git {args} failed: {stderr}")]
pub struct GitError {
    pub args: String,
    pub stderr: String,
    pub code: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct Repository {
    root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub timestamp: i64,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
}

impl Repository {
    /// Finds the repository containing `path`, if there is one.
    pub fn discover(path: &Path) -> Result<Option<Self>> {
        let output = git_command(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("running git (is it installed?)")?;
        if !output.status.success() {
            return Ok(None);
        }
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Some(Self {
            root: PathBuf::from(root),
        }))
    }

    /// Creates a new repository at `path` (used by tests and "Initialize
    /// repository" in the UI).
    pub fn init(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let repo = Self {
            root: path.to_path_buf(),
        };
        repo.run(["init", "-q"])?;
        Ok(repo)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn run<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let output = git_command(&self.root)
            .args(&args)
            .output()
            .context("running git")?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(GitError {
                args: args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" "),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                code: output.status.code(),
            }
            .into())
        }
    }

    fn run_text<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(String::from_utf8_lossy(&self.run(args)?.stdout).into_owned())
    }

    /// Working tree status, including untracked files.
    pub fn status(&self) -> Result<Status> {
        let output = self.run([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ])?;
        status::parse(&output.stdout)
    }

    /// The checked-out branch, or `None` when HEAD is detached.
    pub fn current_branch(&self) -> Result<Option<String>> {
        Ok(self.status()?.branch.head)
    }

    /// Diff of one file. `staged` selects index-vs-HEAD instead of
    /// worktree-vs-index. Untracked files are shown as all-added.
    pub fn diff_file(&self, path: &str, staged: bool) -> Result<String> {
        if !staged {
            let tracked = self
                .run(["ls-files", "--error-unmatch", "--", path])
                .is_ok();
            if !tracked {
                // `git diff --no-index` exits 1 when there are differences.
                let output = git_command(&self.root)
                    .args(["diff", "--no-color", "--no-index", "--", "/dev/null", path])
                    .output()?;
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
        }
        let mut args = vec!["diff", "--no-color", "--no-ext-diff"];
        if staged {
            args.push("--cached");
        }
        args.extend(["--", path]);
        self.run_text(args)
    }

    /// Everything that differs from HEAD (staged and unstaged together).
    pub fn diff_head(&self) -> Result<String> {
        if !self.has_head() {
            return self.run_text(["diff", "--no-color", "--cached"]);
        }
        self.run_text(["diff", "--no-color", "--no-ext-diff", "HEAD"])
    }

    /// The file as committed in HEAD, or `None` if it is new.
    pub fn head_contents(&self, path: &str) -> Result<Option<String>> {
        match self.run(["show", &format!("HEAD:{path}")]) {
            Ok(output) => Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned())),
            Err(_) => Ok(None),
        }
    }

    pub fn has_head(&self) -> bool {
        self.run(["rev-parse", "--verify", "-q", "HEAD"]).is_ok()
    }

    pub fn stage(&self, paths: &[&str]) -> Result<()> {
        let mut args = vec!["add", "--"];
        args.extend(paths);
        self.run(args)?;
        Ok(())
    }

    pub fn unstage(&self, paths: &[&str]) -> Result<()> {
        let mut args = if self.has_head() {
            vec!["restore", "--staged", "--"]
        } else {
            // Nothing to restore from before the first commit.
            vec!["rm", "--cached", "-q", "--"]
        };
        args.extend(paths);
        self.run(args)?;
        Ok(())
    }

    /// Throws away worktree changes to `path` (restores it from the index).
    pub fn discard(&self, path: &str) -> Result<()> {
        self.run(["restore", "--", path])?;
        Ok(())
    }

    /// Commits what is staged and returns the new commit's sha.
    pub fn commit(&self, message: &str) -> Result<String> {
        anyhow::ensure!(!message.trim().is_empty(), "commit message is empty");
        self.run(["commit", "-q", "-m", message])?;
        Ok(self.run_text(["rev-parse", "HEAD"])?.trim().to_string())
    }

    pub fn log(&self, limit: usize) -> Result<Vec<Commit>> {
        if !self.has_head() {
            return Ok(Vec::new());
        }
        let text = self.run_text([
            "log".to_string(),
            format!("-n{limit}"),
            "--format=%H%x1f%an%x1f%at%x1f%s%x1e".to_string(),
        ])?;
        Ok(text
            .split('\x1e')
            .filter_map(|record| {
                let mut fields = record.trim_matches('\n').split('\x1f');
                Some(Commit {
                    sha: fields.next()?.to_string(),
                    author: fields.next()?.to_string(),
                    timestamp: fields.next()?.parse().ok()?,
                    summary: fields.next().unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    /// Creates a worktree at `path` on a new branch `branch` starting from
    /// HEAD. Agents working in parallel each get one, so their edits cannot
    /// collide in the user's checkout.
    pub fn add_worktree(&self, path: &Path, branch: &str) -> Result<Repository> {
        self.run([
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-q"),
            OsStr::new("-b"),
            OsStr::new(branch),
            path.as_os_str(),
        ])?;
        Ok(Repository {
            root: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        })
    }

    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
        if force {
            args.push(OsStr::new("--force"));
        }
        args.push(path.as_os_str());
        self.run(args)?;
        Ok(())
    }

    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let text = self.run_text(["worktree", "list", "--porcelain"])?;
        let mut worktrees = Vec::new();
        let mut current: Option<Worktree> = None;
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                worktrees.extend(current.take());
                current = Some(Worktree {
                    path: PathBuf::from(path),
                    head: None,
                    branch: None,
                });
            } else if let Some(head) = line.strip_prefix("HEAD ") {
                if let Some(wt) = current.as_mut() {
                    wt.head = Some(head.to_string());
                }
            } else if let Some(branch) = line.strip_prefix("branch ") {
                if let Some(wt) = current.as_mut() {
                    wt.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
                }
            }
        }
        worktrees.extend(current);
        Ok(worktrees)
    }
}

fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(&dir.path().join("repo")).unwrap();
        repo.run(["config", "user.email", "test@example.com"]).unwrap();
        repo.run(["config", "user.name", "Test"]).unwrap();
        repo.run(["config", "commit.gpgsign", "false"]).unwrap();
        (dir, repo)
    }

    fn write(repo: &Repository, path: &str, contents: &str) {
        let full = repo.root().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    #[test]
    fn discover_finds_the_root_from_a_subdirectory() {
        let (_dir, repo) = repo();
        write(&repo, "src/main.rs", "fn main() {}\n");
        let found = Repository::discover(&repo.root().join("src")).unwrap().unwrap();
        assert_eq!(
            found.root().canonicalize().unwrap(),
            repo.root().canonicalize().unwrap()
        );
        let outside = tempfile::tempdir().unwrap();
        assert!(Repository::discover(outside.path()).unwrap().is_none());
    }

    #[test]
    fn status_stage_commit_cycle() {
        let (_dir, repo) = repo();
        write(&repo, "a.txt", "one\n");
        write(&repo, "dir/b.txt", "two\n");

        let status = repo.status().unwrap();
        assert_eq!(status.entries.len(), 2);
        assert!(status.entries.iter().all(|e| e.kind == StatusKind::Untracked));
        assert!(status.branch.oid.is_none(), "no commit yet");

        repo.stage(&["a.txt", "dir/b.txt"]).unwrap();
        let status = repo.status().unwrap();
        assert!(status
            .entries
            .iter()
            .all(|e| e.index == FileState::Added && e.worktree == FileState::Unmodified));

        let sha = repo.commit("initial").unwrap();
        assert_eq!(sha.len(), 40);
        assert!(repo.status().unwrap().entries.is_empty());
        assert_eq!(repo.log(5).unwrap()[0].summary, "initial");

        write(&repo, "a.txt", "one\nmore\n");
        let status = repo.status().unwrap();
        assert_eq!(status.entries[0].path, "a.txt");
        assert_eq!(status.entries[0].worktree, FileState::Modified);
        let diff = repo.diff_file("a.txt", false).unwrap();
        assert!(diff.contains("+more"), "{diff}");
        assert_eq!(repo.head_contents("a.txt").unwrap().as_deref(), Some("one\n"));

        repo.stage(&["a.txt"]).unwrap();
        assert!(repo.diff_file("a.txt", true).unwrap().contains("+more"));
        repo.unstage(&["a.txt"]).unwrap();
        repo.discard("a.txt").unwrap();
        assert!(repo.status().unwrap().entries.is_empty());
    }

    #[test]
    fn untracked_diff_shows_the_whole_file() {
        let (_dir, repo) = repo();
        write(&repo, "new.txt", "hello\n");
        let diff = repo.diff_file("new.txt", false).unwrap();
        assert!(diff.contains("+hello"), "{diff}");
    }

    #[test]
    fn renames_are_reported_with_their_origin() {
        let (_dir, repo) = repo();
        write(&repo, "old name.txt", "content that is long enough to be detected\n");
        repo.stage(&["old name.txt"]).unwrap();
        repo.commit("add").unwrap();
        repo.run(["mv", "old name.txt", "new name.txt"]).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(status.entries.len(), 1);
        let entry = &status.entries[0];
        assert_eq!(entry.kind, StatusKind::Renamed);
        assert_eq!(entry.path, "new name.txt");
        assert_eq!(entry.orig_path.as_deref(), Some("old name.txt"));
    }

    #[test]
    fn worktrees_isolate_parallel_work() {
        let (dir, repo) = repo();
        write(&repo, "a.txt", "base\n");
        repo.stage(&["a.txt"]).unwrap();
        repo.commit("base").unwrap();

        let wt_path = dir.path().join("agent-1");
        let wt = repo.add_worktree(&wt_path, "agent/task-1").unwrap();
        std::fs::write(wt.root().join("a.txt"), "changed by agent\n").unwrap();
        assert!(repo.status().unwrap().entries.is_empty(), "main checkout untouched");
        assert_eq!(wt.status().unwrap().entries.len(), 1);

        let worktrees = repo.worktrees().unwrap();
        assert_eq!(worktrees.len(), 2);
        assert!(worktrees
            .iter()
            .any(|w| w.branch.as_deref() == Some("agent/task-1")));

        repo.remove_worktree(&wt_path, true).unwrap();
        assert_eq!(repo.worktrees().unwrap().len(), 1);
    }

    #[test]
    fn errors_carry_gits_message() {
        let (_dir, repo) = repo();
        let err = repo.commit("nothing staged").unwrap_err();
        assert!(err.downcast_ref::<GitError>().is_some(), "{err:#}");
    }
}

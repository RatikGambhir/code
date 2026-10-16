//! Isolating parallel agents in git worktrees.
//!
//! Two agents editing one checkout will trample each other. A thread that
//! should run alongside others gets its own worktree on its own branch, next
//! to (not inside) the repository; the user reviews and merges the branch.

use std::path::{Path, PathBuf};

use anyhow::Result;
use cockpit_git::Repository;

/// `fix the login page!` → `fix-the-login-page`.
pub fn slugify(label: &str) -> String {
    let mut slug = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "task".into()
    } else {
        slug
    }
}

/// Where a worktree for `label` goes and which branch it uses:
/// `<parent>/.cockpit-worktrees/<repo>/<slug>` on `cockpit/<slug>`.
pub fn worktree_location(repo_root: &Path, label: &str) -> (PathBuf, String) {
    let slug = slugify(label);
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let parent = repo_root.parent().unwrap_or(repo_root);
    (
        parent.join(".cockpit-worktrees").join(repo_name).join(&slug),
        format!("cockpit/{slug}"),
    )
}

/// Creates a worktree for `label`, picking a free name if one is taken.
pub fn create_isolated_worktree(repo: &Repository, label: &str) -> Result<Repository> {
    let existing: Vec<String> = repo
        .worktrees()?
        .into_iter()
        .filter_map(|w| w.branch)
        .collect();
    let (base_path, base_branch) = worktree_location(repo.root(), label);
    for attempt in 1..100 {
        let (path, branch) = if attempt == 1 {
            (base_path.clone(), base_branch.clone())
        } else {
            (
                PathBuf::from(format!("{}-{attempt}", base_path.display())),
                format!("{base_branch}-{attempt}"),
            )
        };
        if path.exists() || existing.contains(&branch) {
            continue;
        }
        std::fs::create_dir_all(path.parent().expect("worktree path has a parent"))?;
        return repo.add_worktree(&path, &branch);
    }
    anyhow::bail!("no free worktree name for {label:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_short_and_safe() {
        assert_eq!(slugify("Fix the login page!"), "fix-the-login-page");
        assert_eq!(slugify("  --  "), "task");
        assert!(slugify(&"word ".repeat(50)).len() <= 40);
    }

    #[test]
    fn locations_sit_beside_the_repo() {
        let (path, branch) = worktree_location(Path::new("/src/app"), "Add auth");
        assert_eq!(path, PathBuf::from("/src/.cockpit-worktrees/app/add-auth"));
        assert_eq!(branch, "cockpit/add-auth");
    }

    #[test]
    fn creates_unique_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(&dir.path().join("app")).unwrap();
        std::fs::write(repo.root().join("a.txt"), "x").unwrap();
        repo.stage(&["a.txt"]).unwrap();
        // CI machines may lack a git identity.
        let status = std::process::Command::new("git")
            .current_dir(repo.root())
            .args(["-c", "user.email=t@e.st", "-c", "user.name=T", "-c", "commit.gpgsign=false", "commit", "-qm", "init"])
            .status()
            .unwrap();
        assert!(status.success());

        let first = create_isolated_worktree(&repo, "Add auth").unwrap();
        let second = create_isolated_worktree(&repo, "Add auth").unwrap();
        assert_ne!(first.root(), second.root());
        assert!(first.root().join("a.txt").exists());
        assert_eq!(repo.worktrees().unwrap().len(), 3);
    }
}

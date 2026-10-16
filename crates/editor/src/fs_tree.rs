//! Directory listing for the explorer.

use std::{
    cmp::Ordering,
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::Result;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// Matched by a `.gitignore` (shown dimmed, like `target/`).
    pub ignored: bool,
}

/// Entries that are never worth showing.
const ALWAYS_HIDDEN: &[&str] = &[".git", ".DS_Store", ".hg", ".svn"];

/// Lists `dir`: directories first, then files, each case-insensitively
/// sorted. Gitignored entries are included but flagged.
pub fn list_dir(dir: &Path) -> Result<Vec<DirEntry>> {
    let not_ignored: HashSet<PathBuf> = ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .build()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .collect();

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if ALWAYS_HIDDEN.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;
        let is_symlink = file_type.is_symlink();
        let is_dir = if is_symlink {
            path.is_dir()
        } else {
            file_type.is_dir()
        };
        entries.push(DirEntry {
            ignored: !not_ignored.contains(&path),
            name,
            path,
            is_dir,
            is_symlink,
        });
    }
    entries.sort_by(compare_entries);
    Ok(entries)
}

fn compare_entries(a: &DirEntry, b: &DirEntry) -> Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

/// Every non-ignored file under `root` (relative paths), up to `limit`. Used
/// by quick-open.
pub fn list_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| !ALWAYS_HIDDEN.contains(&entry.file_name().to_string_lossy().as_ref()))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|entry| entry.path().strip_prefix(root).ok().map(Path::to_path_buf))
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn directories_first_and_gitignored_entries_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("b.rs"), "").unwrap();
        fs::write(root.join("A.rs"), "").unwrap();
        fs::write(root.join("debug.log"), "").unwrap();

        let entries = list_dir(root).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "target", ".gitignore", "A.rs", "b.rs", "debug.log"]);
        let ignored: Vec<_> = entries.iter().filter(|e| e.ignored).map(|e| e.name.as_str()).collect();
        assert_eq!(ignored, vec!["target", "debug.log"]);
    }

    #[test]
    fn list_files_skips_ignored_and_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join(".git/objects/x"), "").unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join("src/nested/lib.rs"), "").unwrap();
        fs::write(root.join(".gitignore"), "out/\n").unwrap();
        fs::create_dir(root.join("out")).unwrap();
        fs::write(root.join("out/gen.rs"), "").unwrap();

        let mut files = list_files(root, 100);
        files.sort();
        assert_eq!(files, vec![PathBuf::from(".gitignore"), PathBuf::from("src/nested/lib.rs")]);
    }
}

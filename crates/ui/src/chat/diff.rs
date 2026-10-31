//! Diffs for the chat's diff panel: the files a conversation changed, and
//! the working tree's changes split per file. Pure functions, no UI.

use cockpit_agents::{protocol::ItemStatus, Entry, EntryKind};

/// One file's changes, ready to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// As the diff names it: relative to the repository or the agent's cwd.
    pub path: String,
    pub change: ChangeKind,
    /// Unified diff text (one or more hunks, with or without file headers).
    pub diff: String,
    pub added: usize,
    pub removed: usize,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl ChangeKind {
    /// The one-letter badge, as in source control.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
        }
    }

    /// Reads the transcript's wording ("added", "renamed → x", ...).
    fn from_transcript(change: &str) -> Self {
        match change {
            "added" | "add" | "create" | "created" => Self::Added,
            "deleted" | "delete" | "removed" => Self::Deleted,
            c if c.starts_with("renamed") || c.starts_with("rename") => Self::Renamed,
            _ => Self::Modified,
        }
    }
}

/// Totals over a set of files.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffSummary {
    pub files: usize,
    pub added: usize,
    pub removed: usize,
}

impl DiffSummary {
    pub fn of(files: &[FileDiff]) -> Self {
        files.iter().fold(Self::default(), |acc, f| Self {
            files: acc.files + 1,
            added: acc.added + f.added,
            removed: acc.removed + f.removed,
        })
    }
}

/// Lines added and removed, not counting the `---`/`+++` file headers.
pub fn count_changes(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(added, removed), line| {
        if line.starts_with("+++") || line.starts_with("---") {
            (added, removed)
        } else if line.starts_with('+') {
            (added + 1, removed)
        } else if line.starts_with('-') {
            (added, removed + 1)
        } else {
            (added, removed)
        }
    })
}

/// Every file the conversation edited, once each, in the order first
/// touched. Several edits to one file are shown one after the other.
/// Declined and failed edits are left out: they never reached the disk.
pub fn chat_changes(entries: &[Entry]) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    for entry in entries {
        let EntryKind::FileChange { files: changed, status } = &entry.kind else { continue };
        if matches!(status, ItemStatus::Declined | ItemStatus::Failed) {
            continue;
        }
        for file in changed {
            let (added, removed) = count_changes(&file.diff);
            let kind = ChangeKind::from_transcript(&file.change);
            match files.iter_mut().find(|f| f.path == file.path) {
                Some(existing) => {
                    if !file.diff.is_empty() {
                        if !existing.diff.is_empty() && !existing.diff.ends_with('\n') {
                            existing.diff.push('\n');
                        }
                        existing.diff.push_str(&file.diff);
                    }
                    existing.added += added;
                    existing.removed += removed;
                    // A file created and then edited is still new; a later
                    // delete wins over everything.
                    if kind == ChangeKind::Deleted || existing.change == ChangeKind::Modified {
                        existing.change = kind;
                    }
                }
                None => files.push(FileDiff {
                    path: file.path.clone(),
                    change: kind,
                    diff: file.diff.clone(),
                    added,
                    removed,
                }),
            }
        }
    }
    files
}

/// Splits `git diff` output into one entry per file.
pub fn split_git_diff(text: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut block: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            if let Some(done) = block.replace(Vec::new()) {
                files.extend(parse_git_file(&done));
            }
        }
        if let Some(block) = block.as_mut() {
            block.push(line);
        }
    }
    if let Some(done) = block {
        files.extend(parse_git_file(&done));
    }
    files
}

fn parse_git_file(block: &[&str]) -> Option<FileDiff> {
    let header = block.first()?;
    let mut change = ChangeKind::Modified;
    let (mut old_path, mut new_path) = (None, None);
    for line in block.iter().take_while(|l| !l.starts_with("@@")) {
        if line.starts_with("new file mode") {
            change = ChangeKind::Added;
        } else if line.starts_with("deleted file mode") {
            change = ChangeKind::Deleted;
        } else if let Some(to) = line.strip_prefix("rename to ") {
            change = ChangeKind::Renamed;
            new_path = Some(to.to_string());
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if path != "/dev/null" {
                new_path = Some(strip_side(path).to_string());
            }
        } else if let Some(path) = line.strip_prefix("--- ") {
            if path != "/dev/null" {
                old_path = Some(strip_side(path).to_string());
            }
        }
    }
    // Binary files and mode-only changes have no ---/+++ lines.
    let path = new_path.or(old_path).or_else(|| {
        let rest = header.strip_prefix("diff --git ")?;
        rest.rsplit_once(" b/").map(|(_, b)| b.to_string())
    })?;
    let diff = block.join("\n");
    let (added, removed) = count_changes(&diff);
    Some(FileDiff { path, change, diff, added, removed })
}

/// `a/src/x.rs` → `src/x.rs`; also handles the absolute paths that
/// `git diff --no-index` prints for untracked files.
fn strip_side(path: &str) -> &str {
    let path = path.trim_end_matches('\t');
    path.strip_prefix("a/").or_else(|| path.strip_prefix("b/")).unwrap_or(path)
}

/// One line of a diff, as the panel draws it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine<'a> {
    pub kind: LineKind,
    /// Line number in the old file (context and removed lines).
    pub old: Option<u32>,
    /// Line number in the new file (context and added lines).
    pub new: Option<u32>,
    pub text: &'a str,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// `@@ -a,b +c,d @@ context`
    Hunk,
    Context,
    Added,
    Removed,
}

/// The lines worth drawing, with line numbers from the hunk headers.
/// File headers (`diff --git`, `index`, `---`, `+++`, modes) are dropped.
pub fn parse_lines(diff: &str) -> Vec<DiffLine<'_>> {
    let mut out = Vec::new();
    let (mut old, mut new) = (0u32, 0u32);
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some((o, n)) = hunk_starts(line) {
                (old, new) = (o, n);
            }
            in_hunk = true;
            out.push(DiffLine { kind: LineKind::Hunk, old: None, new: None, text: line });
            continue;
        }
        if line.starts_with("diff --git ") {
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            // Agents sometimes send bare `+`/`-` lines with no hunk header.
            let bare = !(line.starts_with("---") || line.starts_with("+++") || line.starts_with("index "))
                && (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '));
            if !bare {
                continue;
            }
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            // "\ No newline at end of file"
            Some(b'\\') => continue,
            _ => (LineKind::Context, line),
        };
        let numbered = in_hunk;
        let (o, n) = match kind {
            LineKind::Added => {
                new += 1;
                (None, numbered.then_some(new - 1))
            }
            LineKind::Removed => {
                old += 1;
                (numbered.then_some(old - 1), None)
            }
            _ => {
                old += 1;
                new += 1;
                (numbered.then_some(old - 1), numbered.then_some(new - 1))
            }
        };
        out.push(DiffLine { kind, old: o, new: n, text });
    }
    out
}

/// `@@ -12,7 +12,9 @@` → (12, 12).
fn hunk_starts(line: &str) -> Option<(u32, u32)> {
    let mut parts = line.split_whitespace().skip(1);
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let start = |s: &str| s.split(',').next()?.parse::<u32>().ok();
    Some((start(old)?, start(new)?))
}

#[cfg(test)]
mod tests {
    use cockpit_agents::transcript::ChangedFile;

    use super::*;

    fn file_change(id: &str, status: ItemStatus, files: &[(&str, &str, &str)]) -> Entry {
        Entry {
            id: id.into(),
            turn_id: None,
            kind: EntryKind::FileChange {
                files: files
                    .iter()
                    .map(|(path, change, diff)| ChangedFile {
                        path: (*path).into(),
                        change: (*change).into(),
                        diff: (*diff).into(),
                    })
                    .collect(),
                status,
            },
            streaming: false,
        }
    }

    #[test]
    fn count_changes_skips_file_headers() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n+more\n same\n";
        assert_eq!(count_changes(diff), (2, 1));
    }

    #[test]
    fn chat_changes_merge_edits_per_file() {
        let entries = [
            file_change("1", ItemStatus::Completed, &[("a.rs", "added", "@@ -0,0 +1,2 @@\n+one\n+two")]),
            file_change("2", ItemStatus::Completed, &[("b.rs", "modified", "@@ -1 +1 @@\n-x\n+y")]),
            file_change("3", ItemStatus::Completed, &[("a.rs", "modified", "@@ -2 +2 @@\n-two\n+three")]),
        ];
        let files = chat_changes(&entries);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a.rs");
        assert_eq!(files[0].change, ChangeKind::Added, "created then edited is still new");
        assert_eq!((files[0].added, files[0].removed), (3, 1));
        assert!(files[0].diff.contains("+three"));
        assert_eq!(DiffSummary::of(&files), DiffSummary { files: 2, added: 4, removed: 2 });
    }

    #[test]
    fn chat_changes_skip_declined_edits() {
        let entries = [
            file_change("1", ItemStatus::Declined, &[("a.rs", "modified", "+x")]),
            file_change("2", ItemStatus::Failed, &[("b.rs", "modified", "+x")]),
        ];
        assert!(chat_changes(&entries).is_empty());
    }

    #[test]
    fn split_git_diff_per_file() {
        let text = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
 fn a() {}
+fn b() {}
 fn c() {}
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let files = split_git_diff(text);
        let summary: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.change, f.added, f.removed)).collect();
        assert_eq!(
            summary,
            [
                ("src/lib.rs", ChangeKind::Modified, 1, 0),
                ("new.txt", ChangeKind::Added, 1, 0),
                ("gone.txt", ChangeKind::Deleted, 0, 1),
            ]
        );
    }

    #[test]
    fn split_git_diff_handles_binary_files() {
        let files = split_git_diff("diff --git a/logo.png b/logo.png\nBinary files differ\n");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "logo.png");
    }

    #[test]
    fn parse_lines_numbers_from_hunk_headers() {
        let diff = "--- a/x\n+++ b/x\n@@ -10,3 +10,3 @@ fn main\n keep\n-old\n+new\n\\ No newline at end of file\n";
        let lines = parse_lines(diff);
        let shown: Vec<_> = lines.iter().map(|l| (l.kind, l.old, l.new, l.text)).collect();
        assert_eq!(
            shown,
            [
                (LineKind::Hunk, None, None, "@@ -10,3 +10,3 @@ fn main"),
                (LineKind::Context, Some(10), Some(10), "keep"),
                (LineKind::Removed, Some(11), None, "old"),
                (LineKind::Added, None, Some(11), "new"),
            ]
        );
    }

    #[test]
    fn parse_lines_accepts_bare_hunks() {
        let lines = parse_lines("-a\n+b");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].kind, LineKind::Removed);
        assert_eq!(lines[1].new, None, "no header, no numbers");
    }
}

//! Parser for `git status --porcelain=v2 --branch -z`.
//!
//! <https://git-scm.com/docs/git-status#_porcelain_format_version_2>

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    /// Commit HEAD points at; `None` before the first commit.
    pub oid: Option<String>,
    /// Branch name; `None` when detached.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub branch: BranchInfo,
    pub entries: Vec<StatusEntry>,
}

impl Status {
    pub fn staged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_staged())
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.has_worktree_changes())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusKind {
    Ordinary,
    Renamed,
    Unmerged,
    Untracked,
    Ignored,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileState {
    Unmodified,
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    Unmerged,
    Untracked,
}

impl FileState {
    fn from_code(code: u8) -> Result<Self> {
        Ok(match code {
            b'.' => FileState::Unmodified,
            b'M' => FileState::Modified,
            b'T' => FileState::TypeChanged,
            b'A' => FileState::Added,
            b'D' => FileState::Deleted,
            b'R' => FileState::Renamed,
            b'C' => FileState::Copied,
            b'U' => FileState::Unmerged,
            other => bail!("unknown status code {:?}", other as char),
        })
    }

    /// One-letter badge for the source control list.
    pub fn badge(self) -> &'static str {
        match self {
            FileState::Unmodified => "",
            FileState::Modified => "M",
            FileState::TypeChanged => "T",
            FileState::Added => "A",
            FileState::Deleted => "D",
            FileState::Renamed => "R",
            FileState::Copied => "C",
            FileState::Unmerged => "U",
            FileState::Untracked => "U",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    pub kind: StatusKind,
    /// Path relative to the repository root.
    pub path: String,
    /// For renames and copies, the path it came from.
    pub orig_path: Option<String>,
    pub index: FileState,
    pub worktree: FileState,
}

impl StatusEntry {
    pub fn is_staged(&self) -> bool {
        !matches!(self.kind, StatusKind::Untracked | StatusKind::Ignored)
            && self.index != FileState::Unmodified
    }

    pub fn has_worktree_changes(&self) -> bool {
        self.kind == StatusKind::Untracked || self.worktree != FileState::Unmodified
    }

    /// The state to show when only one badge fits.
    pub fn display_state(&self) -> FileState {
        match self.kind {
            StatusKind::Untracked => FileState::Untracked,
            StatusKind::Unmerged => FileState::Unmerged,
            _ if self.worktree != FileState::Unmodified => self.worktree,
            _ => self.index,
        }
    }
}

pub(crate) fn parse(output: &[u8]) -> Result<Status> {
    let text = String::from_utf8_lossy(output);
    let mut status = Status::default();
    let mut records = text.split('\0').filter(|r| !r.is_empty());

    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix("# ") {
            parse_header(header, &mut status.branch);
            continue;
        }
        let (kind, rest) = record.split_at(1);
        match kind {
            "1" => {
                // 1 XY sub mH mI mW hH hI path
                let fields: Vec<&str> = rest.trim_start().splitn(8, ' ').collect();
                let [xy, _, _, _, _, _, _, path] = fields[..] else {
                    bail!("malformed ordinary entry: {record:?}");
                };
                let (index, worktree) = parse_xy(xy)?;
                status.entries.push(StatusEntry {
                    kind: StatusKind::Ordinary,
                    path: path.to_string(),
                    orig_path: None,
                    index,
                    worktree,
                });
            }
            "2" => {
                // 2 XY sub mH mI mW hH hI Xscore path NUL origPath
                let fields: Vec<&str> = rest.trim_start().splitn(9, ' ').collect();
                let [xy, _, _, _, _, _, _, _, path] = fields[..] else {
                    bail!("malformed rename entry: {record:?}");
                };
                let orig = records.next().context("rename entry without origin")?;
                let (index, worktree) = parse_xy(xy)?;
                status.entries.push(StatusEntry {
                    kind: StatusKind::Renamed,
                    path: path.to_string(),
                    orig_path: Some(orig.to_string()),
                    index,
                    worktree,
                });
            }
            "u" => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                let fields: Vec<&str> = rest.trim_start().splitn(10, ' ').collect();
                let [xy, _, _, _, _, _, _, _, _, path] = fields[..] else {
                    bail!("malformed unmerged entry: {record:?}");
                };
                let (index, worktree) = parse_xy(xy)?;
                status.entries.push(StatusEntry {
                    kind: StatusKind::Unmerged,
                    path: path.to_string(),
                    orig_path: None,
                    index,
                    worktree,
                });
            }
            "?" | "!" => status.entries.push(StatusEntry {
                kind: if kind == "?" {
                    StatusKind::Untracked
                } else {
                    StatusKind::Ignored
                },
                path: rest.trim_start().to_string(),
                orig_path: None,
                index: FileState::Unmodified,
                worktree: FileState::Untracked,
            }),
            _ => bail!("unknown status record: {record:?}"),
        }
    }

    status.entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(status)
}

fn parse_xy(xy: &str) -> Result<(FileState, FileState)> {
    let bytes = xy.as_bytes();
    anyhow::ensure!(bytes.len() == 2, "bad XY field {xy:?}");
    Ok((FileState::from_code(bytes[0])?, FileState::from_code(bytes[1])?))
}

fn parse_header(header: &str, branch: &mut BranchInfo) {
    let Some((key, value)) = header.split_once(' ') else {
        return;
    };
    match key {
        "branch.oid" => branch.oid = (value != "(initial)").then(|| value.to_string()),
        "branch.head" => branch.head = (value != "(detached)").then(|| value.to_string()),
        "branch.upstream" => branch.upstream = Some(value.to_string()),
        "branch.ab" => {
            for part in value.split(' ') {
                if let Some(n) = part.strip_prefix('+') {
                    branch.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    branch.behind = n.parse().unwrap_or(0);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headers_and_every_entry_kind() {
        let output = [
            "# branch.oid 1234567890abcdef1234567890abcdef12345678",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 .M N... 100644 100644 100644 aaaa bbbb src/file with spaces.rs",
            "2 R. N... 100644 100644 100644 aaaa bbbb R100 new.rs",
            "old.rs",
            "u UU N... 100644 100644 100644 100644 a b c conflict.rs",
            "? untracked.txt",
            "",
        ]
        .join("\0");
        let status = parse(output.as_bytes()).unwrap();
        assert_eq!(status.branch.head.as_deref(), Some("main"));
        assert_eq!(status.branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.branch.ahead, status.branch.behind), (2, 1));
        assert_eq!(status.entries.len(), 4);

        let by_path = |p: &str| status.entries.iter().find(|e| e.path == p).unwrap();
        let modified = by_path("src/file with spaces.rs");
        assert_eq!(modified.worktree, FileState::Modified);
        assert!(!modified.is_staged());
        let renamed = by_path("new.rs");
        assert_eq!(renamed.orig_path.as_deref(), Some("old.rs"));
        assert!(renamed.is_staged());
        assert_eq!(by_path("conflict.rs").display_state(), FileState::Unmerged);
        assert_eq!(by_path("untracked.txt").display_state(), FileState::Untracked);
        assert_eq!(status.staged().count(), 2);
    }

    #[test]
    fn detached_and_initial_heads_are_none() {
        let output = "# branch.oid (initial)\0# branch.head (detached)\0";
        let status = parse(output.as_bytes()).unwrap();
        assert_eq!(status.branch.oid, None);
        assert_eq!(status.branch.head, None);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse(b"1 XYZ\0").is_err());
        assert!(parse(b"zz\0").is_err());
    }
}

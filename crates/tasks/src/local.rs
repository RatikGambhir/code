//! The workspace's own projects and issues, kept in Hangar's data directory
//! (one JSON file per workspace folder, so nothing is written into the repo).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::model::{Board, Issue, Priority, Project, Source, Status, StatusKind};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalBoard {
    /// `CIT` in `CIT-12`.
    pub prefix: String,
    /// The number the next issue gets.
    #[serde(default = "first_number")]
    pub next_number: u32,
    #[serde(flatten)]
    pub board: Board,
}

fn first_number() -> u32 {
    1
}

impl LocalBoard {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            next_number: 1,
            board: Board::default(),
        }
    }

    pub fn add_issue(
        &mut self,
        title: &str,
        status: StatusKind,
        project_id: Option<String>,
        now: i64,
    ) -> &Issue {
        let number = self.next_number;
        self.next_number += 1;
        self.board.issues.push(Issue {
            id: format!("local-{number}"),
            identifier: format!("{}-{number}", self.prefix),
            title: title.trim().to_string(),
            status: Status::local(status),
            priority: Priority::None,
            project_id,
            created_at: now,
            updated_at: now,
            source: Source::Local,
            ..Issue::default()
        });
        self.board.issues.last().expect("just pushed")
    }

    pub fn add_project(&mut self, name: &str, color: Option<String>) -> &Project {
        let last = self
            .board
            .projects
            .iter()
            .filter_map(|p| p.id.strip_prefix("local-project-")?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        let id = format!("local-project-{}", last + 1);
        self.board.projects.push(Project {
            id,
            name: name.trim().to_string(),
            color,
            source: Source::Local,
            ..Project::default()
        });
        self.board.projects.last().expect("just pushed")
    }

    /// Applies `edit` to one issue and stamps it. Returns whether it existed.
    pub fn edit_issue(&mut self, id: &str, now: i64, edit: impl FnOnce(&mut Issue)) -> bool {
        let Some(issue) = self.board.issues.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        edit(issue);
        issue.updated_at = now;
        true
    }

    pub fn remove_issue(&mut self, id: &str) -> bool {
        let before = self.board.issues.len();
        self.board.issues.retain(|i| i.id != id);
        self.board.issues.len() != before
    }

    /// Fills in each local project's progress from its issues.
    pub fn refresh_progress(&mut self) {
        let progress: Vec<f32> = self
            .board
            .projects
            .iter()
            .map(|p| self.board.derived_progress(&p.id))
            .collect();
        for (project, progress) in self.board.projects.iter_mut().zip(progress) {
            project.progress = progress;
        }
    }
}

/// `city-builder` → `CIT`; falls back to `TSK`.
pub fn prefix_for(root: &Path) -> String {
    let letters: String = root
        .file_name()
        .map(|n| {
            n.to_string_lossy()
                .chars()
                .filter(|c| c.is_ascii_alphabetic())
                .take(3)
                .collect()
        })
        .unwrap_or_default();
    if letters.is_empty() {
        "TSK".to_string()
    } else {
        letters.to_ascii_uppercase()
    }
}

/// Where a workspace's board is saved: named by a hash of its path.
pub fn path_for(data_dir: &Path, root: &Path) -> PathBuf {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let name: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    data_dir.join("tasks").join(format!("{name}.json"))
}

pub fn load(path: &Path, root: &Path) -> Result<LocalBoard> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut board: LocalBoard = serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?;
            board.refresh_progress();
            Ok(board)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(LocalBoard::new(prefix_for(root)))
        }
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes atomically: a crash mid-save keeps the previous file.
pub fn save(path: &Path, board: &LocalBoard) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(board)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_come_from_the_folder_name() {
        assert_eq!(prefix_for(Path::new("/code/city-builder")), "CIT");
        assert_eq!(prefix_for(Path::new("/code/42")), "TSK");
    }

    #[test]
    fn issues_number_up_and_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = Path::new("/code/city-builder");
        let path = path_for(dir.path(), root);

        let mut board = load(&path, root).unwrap();
        let project = board.add_project("Launch", None).id.clone();
        board.add_issue(
            "  Draw roads ",
            StatusKind::Unstarted,
            Some(project.clone()),
            10,
        );
        let second = board
            .add_issue("Zoning", StatusKind::Backlog, None, 11)
            .id
            .clone();
        assert!(board.edit_issue(&second, 20, |i| i.status =
            Status::local(StatusKind::Completed)));
        save(&path, &board).unwrap();

        let loaded = load(&path, root).unwrap();
        assert_eq!(loaded.board.issues[0].identifier, "CIT-1");
        assert_eq!(loaded.board.issues[0].title, "Draw roads");
        assert_eq!(loaded.board.issues[1].updated_at, 20);
        assert_eq!(loaded.next_number, 3);
        assert_eq!(loaded.board.projects[0].id, project);

        let mut loaded = loaded;
        assert!(loaded.remove_issue(&second));
        assert!(!loaded.remove_issue(&second));
    }
}

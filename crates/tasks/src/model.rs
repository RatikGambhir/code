//! Projects and issues as the task views show them, whichever source they
//! came from.

use serde::{Deserialize, Serialize};

/// Where an item lives.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Kept by Hangar for this workspace; editable here.
    #[default]
    Local,
    /// Stored in Linear.
    Linear,
}

/// An issue's workflow category, as Linear defines them.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    Triage,
    Started,
    #[default]
    Unstarted,
    Backlog,
    Completed,
    Canceled,
}

impl StatusKind {
    /// Groups in list order: work in flight first, finished work last.
    pub const ORDER: [StatusKind; 6] = [
        StatusKind::Triage,
        StatusKind::Started,
        StatusKind::Unstarted,
        StatusKind::Backlog,
        StatusKind::Completed,
        StatusKind::Canceled,
    ];

    /// The statuses a local issue can be in, in the order a picker shows.
    pub const LOCAL: [StatusKind; 5] = [
        StatusKind::Backlog,
        StatusKind::Unstarted,
        StatusKind::Started,
        StatusKind::Completed,
        StatusKind::Canceled,
    ];

    pub fn from_linear(kind: &str) -> Self {
        match kind {
            "triage" => Self::Triage,
            "backlog" => Self::Backlog,
            "started" => Self::Started,
            "completed" => Self::Completed,
            "canceled" | "cancelled" | "duplicate" => Self::Canceled,
            _ => Self::Unstarted,
        }
    }

    /// The default name of a local status.
    pub fn label(self) -> &'static str {
        match self {
            Self::Triage => "Triage",
            Self::Backlog => "Backlog",
            Self::Unstarted => "Todo",
            Self::Started => "In Progress",
            Self::Completed => "Done",
            Self::Canceled => "Canceled",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Triage | Self::Started | Self::Unstarted)
    }

    pub fn is_closed(self) -> bool {
        matches!(self, Self::Completed | Self::Canceled)
    }
}

/// An issue's status: a team-specific name in a fixed category.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub kind: StatusKind,
    /// `#rrggbb` as Linear reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl Status {
    pub fn local(kind: StatusKind) -> Self {
        Self {
            name: kind.label().to_string(),
            kind,
            color: None,
        }
    }
}

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Urgent,
    High,
    Medium,
    Low,
    #[default]
    None,
}

impl Priority {
    /// Picker order, as in Linear.
    pub const ALL: [Priority; 5] = [
        Priority::None,
        Priority::Urgent,
        Priority::High,
        Priority::Medium,
        Priority::Low,
    ];

    /// Linear numbers priorities 0 (none) and 1 (urgent) to 4 (low).
    pub fn from_linear(value: u8) -> Self {
        match value {
            1 => Self::Urgent,
            2 => Self::High,
            3 => Self::Medium,
            4 => Self::Low,
            _ => Self::None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Urgent => "Urgent",
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
            Self::None => "No priority",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ProjectState {
    Started,
    #[default]
    Planned,
    Backlog,
    Paused,
    Completed,
    Canceled,
}

impl ProjectState {
    pub fn from_linear(kind: &str) -> Self {
        match kind {
            "backlog" => Self::Backlog,
            "started" => Self::Started,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "canceled" | "cancelled" => Self::Canceled,
            _ => Self::Planned,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Backlog => "Backlog",
            Self::Planned => "Planned",
            Self::Started => "In Progress",
            Self::Paused => "Paused",
            Self::Completed => "Completed",
            Self::Canceled => "Canceled",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub state: ProjectState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    /// `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_date: Option<String>,
    /// 0.0 to 1.0, as Linear computes it. Local projects derive it.
    #[serde(default)]
    pub progress: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub source: Source,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    /// `ENG-123`.
    pub identifier: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: Status,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Assigned to the signed-in Linear user.
    #[serde(default)]
    pub mine: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<Label>,
    /// Seconds since the Unix epoch.
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub source: Source,
}

/// Everything one source holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Board {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub issues: Vec<Issue>,
}

impl Board {
    /// Share of a project's issues that are done, ignoring canceled ones.
    pub fn derived_progress(&self, project_id: &str) -> f32 {
        let (done, total) = self
            .issues
            .iter()
            .filter(|i| {
                i.project_id.as_deref() == Some(project_id) && i.status.kind != StatusKind::Canceled
            })
            .fold((0usize, 0usize), |(done, total), i| {
                (
                    done + usize::from(i.status.kind == StatusKind::Completed),
                    total + 1,
                )
            });
        if total == 0 {
            0.0
        } else {
            done as f32 / total as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_kinds_and_priorities_map() {
        assert_eq!(StatusKind::from_linear("started"), StatusKind::Started);
        assert_eq!(StatusKind::from_linear("unstarted"), StatusKind::Unstarted);
        assert_eq!(StatusKind::from_linear("duplicate"), StatusKind::Canceled);
        assert_eq!(Priority::from_linear(1), Priority::Urgent);
        assert_eq!(Priority::from_linear(0), Priority::None);
        assert_eq!(ProjectState::from_linear("paused"), ProjectState::Paused);
    }

    #[test]
    fn progress_ignores_canceled_issues() {
        let issue = |kind| Issue {
            project_id: Some("p".into()),
            status: Status::local(kind),
            ..Issue::default()
        };
        let board = Board {
            projects: vec![],
            issues: vec![
                issue(StatusKind::Completed),
                issue(StatusKind::Started),
                issue(StatusKind::Canceled),
            ],
        };
        assert!((board.derived_progress("p") - 0.5).abs() < f32::EPSILON);
        assert_eq!(board.derived_progress("missing"), 0.0);
    }
}

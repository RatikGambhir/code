//! What the GitHub view shows, read straight from GitHub's REST answers.
//! Users are kept as their login and times as seconds since the epoch.

use serde::{Deserialize, Deserializer};

/// The signed-in user.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct User {
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub html_url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Repo {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    #[serde(deserialize_with = "login")]
    pub owner: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default_branch: String,
    pub html_url: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub stargazers_count: u64,
    #[serde(default)]
    pub forks_count: u64,
    /// Open issues and pull requests together, as GitHub counts them.
    #[serde(default)]
    pub open_issues_count: u64,
    #[serde(default, deserialize_with = "timestamp")]
    pub pushed_at: i64,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    #[default]
    File,
    Dir,
    Symlink,
    Submodule,
}

/// One item of a directory listing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: EntryKind,
    #[serde(default)]
    pub size: u64,
}

/// A file's text, or why there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileContent {
    Text(String),
    Binary { size: u64 },
    TooLarge { size: u64 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Label {
    pub name: String,
    /// Hex without the `#`.
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct BranchRef {
    #[serde(rename = "ref")]
    pub name: String,
    #[serde(default)]
    pub sha: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct PullRequest {
    #[serde(default)]
    pub node_id: String,
    pub number: u64,
    pub title: String,
    /// `open` or `closed`.
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(deserialize_with = "login")]
    pub user: String,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default, deserialize_with = "logins")]
    pub assignees: Vec<String>,
    #[serde(default, deserialize_with = "logins")]
    pub requested_reviewers: Vec<String>,
    #[serde(default)]
    pub requested_teams: Vec<ReviewTeam>,
    pub milestone: Option<Milestone>,
    pub head: BranchRef,
    pub base: BranchRef,
    #[serde(default, deserialize_with = "timestamp")]
    pub created_at: i64,
    #[serde(default, deserialize_with = "timestamp")]
    pub updated_at: i64,
    #[serde(default, deserialize_with = "optional_timestamp")]
    pub merged_at: Option<i64>,
    // Only in the single pull request answer.
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    #[serde(default)]
    pub changed_files: Option<u64>,
    #[serde(default)]
    pub commits: Option<u64>,
    #[serde(default)]
    pub mergeable_state: Option<String>,
}

impl PullRequest {
    pub fn status(&self) -> PullStatus {
        if self.merged_at.is_some() {
            PullStatus::Merged
        } else if self.state == "closed" {
            PullStatus::Closed
        } else if self.draft {
            PullStatus::Draft
        } else {
            PullStatus::Open
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PullStatus {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PullStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Draft => "Draft",
            Self::Merged => "Merged",
            Self::Closed => "Closed",
        }
    }
}

/// A file a pull request changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct PullFile {
    pub filename: String,
    /// `added`, `removed`, `modified`, `renamed`, ...
    pub status: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    /// The unified diff hunks; missing for binary or very large files.
    #[serde(default)]
    pub patch: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub state: String,
    #[serde(deserialize_with = "login")]
    pub user: String,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default, deserialize_with = "logins")]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub comments: u64,
    pub milestone: Option<Milestone>,
    #[serde(default, deserialize_with = "timestamp")]
    pub created_at: i64,
    #[serde(default, deserialize_with = "timestamp")]
    pub updated_at: i64,
    /// Set when the issue is really a pull request (the issues API lists both).
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Milestone {
    pub number: u64,
    pub title: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ReviewTeam {
    pub slug: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Review {
    pub id: u64,
    #[serde(deserialize_with = "login")]
    pub user: String,
    pub state: String,
    pub body: Option<String>,
    #[serde(default, deserialize_with = "timestamp")]
    pub submitted_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Comment {
    pub id: u64,
    #[serde(deserialize_with = "login")]
    pub user: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default, deserialize_with = "timestamp")]
    pub created_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Workflow {
    pub id: u64,
    pub name: String,
    pub path: String,
    /// `active`, `disabled_manually`, ...
    pub state: String,
    pub html_url: String,
}

/// Where a run or job stands: GitHub's `status` and, once done, `conclusion`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Queued,
    InProgress,
    Success,
    Failure,
    Cancelled,
    Skipped,
    Neutral,
}

impl RunState {
    pub fn from_github(status: &str, conclusion: Option<&str>) -> Self {
        match (status, conclusion) {
            ("completed", Some("success")) => Self::Success,
            ("completed", Some("failure" | "timed_out" | "startup_failure")) => Self::Failure,
            ("completed", Some("cancelled")) => Self::Cancelled,
            ("completed", Some("skipped")) => Self::Skipped,
            ("completed", _) => Self::Neutral,
            ("in_progress", _) => Self::InProgress,
            _ => Self::Queued,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::InProgress => "In progress",
            Self::Success => "Success",
            Self::Failure => "Failed",
            Self::Cancelled => "Canceled",
            Self::Skipped => "Skipped",
            Self::Neutral => "Completed",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Run {
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub display_title: Option<String>,
    #[serde(default)]
    pub run_number: u64,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub head_branch: Option<String>,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub workflow_id: u64,
    pub html_url: String,
    #[serde(default, deserialize_with = "optional_login")]
    pub actor: Option<String>,
    #[serde(default, deserialize_with = "timestamp")]
    pub created_at: i64,
    #[serde(default, deserialize_with = "timestamp")]
    pub updated_at: i64,
}

impl Run {
    pub fn state(&self) -> RunState {
        RunState::from_github(&self.status, self.conclusion.as_deref())
    }

    pub fn title(&self) -> &str {
        self.display_title
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or("Workflow run")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Step {
    pub number: u64,
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
}

impl Step {
    pub fn state(&self) -> RunState {
        RunState::from_github(&self.status, self.conclusion.as_deref())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Job {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default, deserialize_with = "timestamp")]
    pub started_at: i64,
    #[serde(default, deserialize_with = "timestamp")]
    pub completed_at: i64,
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Job {
    pub fn state(&self) -> RunState {
        RunState::from_github(&self.status, self.conclusion.as_deref())
    }
}

// --- field readers ---------------------------------------------------------

#[derive(Deserialize)]
struct Login {
    login: String,
}

fn login<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<Login>::deserialize(d)?.map_or_else(|| "ghost".into(), |u| u.login))
}

fn optional_login<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<Login>::deserialize(d)?.map(|u| u.login))
}

fn logins<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<Login>>::deserialize(d)?
        .unwrap_or_default()
        .into_iter()
        .map(|u| u.login)
        .collect())
}

fn parse_time(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp())
}

fn timestamp<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(Option::<String>::deserialize(d)?
        .as_deref()
        .and_then(parse_time)
        .unwrap_or(0))
}

fn optional_timestamp<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.as_deref().and_then(parse_time))
}

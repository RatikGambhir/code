//! A read-only client for GitHub's REST API.

use anyhow::{bail, Context as _, Result};
use serde::{de::DeserializeOwned, Deserialize};

use crate::model::{
    Comment, Entry, FileContent, Issue, Job, PullFile, PullRequest, Repo, Run, User, Workflow,
};

const API: &str = "https://api.github.com";
/// Repositories listed at most (10 pages).
pub const MAX_REPOS: usize = 1000;
/// Files larger than this are not shown inline.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Which pull requests or issues to list.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum StateFilter {
    #[default]
    Open,
    Closed,
    All,
}

impl StateFilter {
    pub const ALL: [StateFilter; 3] = [Self::Open, Self::Closed, Self::All];

    pub fn as_query(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Closed => "Closed",
            Self::All => "All",
        }
    }
}

#[derive(Clone)]
pub struct Client {
    token: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token stays out of logs.
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            agent: crate::auth::agent(),
        }
    }

    /// Who the token belongs to; also checks that it works.
    pub fn viewer(&self) -> Result<User> {
        self.get_json("/user")
    }

    /// Repositories the user owns, collaborates on or can see through an
    /// organization, most recently pushed first.
    pub fn repos(&self) -> Result<Vec<Repo>> {
        let mut repos = Vec::new();
        for page in 1.. {
            let batch: Vec<Repo> = self.get_json(&format!(
                "/user/repos?per_page=100&page={page}&sort=pushed&affiliation=owner,collaborator,organization_member"
            ))?;
            let done = batch.len() < 100;
            repos.extend(batch);
            if done || repos.len() >= MAX_REPOS {
                break;
            }
        }
        repos.truncate(MAX_REPOS);
        Ok(repos)
    }

    pub fn branches(&self, repo: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Branch {
            name: String,
        }
        let branches: Vec<Branch> =
            self.get_json(&format!("/repos/{repo}/branches?per_page=100"))?;
        Ok(branches.into_iter().map(|b| b.name).collect())
    }

    /// A directory's entries, folders first, then by name.
    pub fn list_dir(&self, repo: &str, path: &str, git_ref: &str) -> Result<Vec<Entry>> {
        let mut entries: Vec<Entry> = self.get_json(&contents_url(repo, path, git_ref))?;
        entries.sort_by(|a, b| {
            let dir = |e: &Entry| e.kind != crate::model::EntryKind::Dir;
            dir(a)
                .cmp(&dir(b))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    /// A file's text. `size` comes from the listing, so large files are not
    /// downloaded at all.
    pub fn file(&self, repo: &str, path: &str, git_ref: &str, size: u64) -> Result<FileContent> {
        if size > MAX_FILE_BYTES {
            return Ok(FileContent::TooLarge { size });
        }
        let mut response = self
            .request(&contents_url(repo, path, git_ref))
            .header("Accept", "application/vnd.github.raw+json")
            .call()
            .context("contacting GitHub")?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let text = response.body_mut().read_to_string().unwrap_or_default();
            return Err(status_error(status, &text, remaining(&response)));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_FILE_BYTES + 1)
            .read_to_vec()
            .context("downloading the file")?;
        Ok(classify(bytes))
    }

    pub fn pulls(&self, repo: &str, state: StateFilter) -> Result<Vec<PullRequest>> {
        self.get_json(&format!(
            "/repos/{repo}/pulls?state={}&per_page=50&sort=updated&direction=desc",
            state.as_query()
        ))
    }

    pub fn pull(&self, repo: &str, number: u64) -> Result<PullRequest> {
        self.get_json(&format!("/repos/{repo}/pulls/{number}"))
    }

    pub fn pull_files(&self, repo: &str, number: u64) -> Result<Vec<PullFile>> {
        self.get_json(&format!("/repos/{repo}/pulls/{number}/files?per_page=100"))
    }

    /// Issues only: the issues API also lists pull requests, which are dropped.
    pub fn issues(&self, repo: &str, state: StateFilter) -> Result<Vec<Issue>> {
        let issues: Vec<Issue> = self.get_json(&format!(
            "/repos/{repo}/issues?state={}&per_page=50&sort=updated&direction=desc",
            state.as_query()
        ))?;
        Ok(issues
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    pub fn issue(&self, repo: &str, number: u64) -> Result<Issue> {
        self.get_json(&format!("/repos/{repo}/issues/{number}"))
    }

    /// Conversation comments on an issue or pull request.
    pub fn comments(&self, repo: &str, number: u64) -> Result<Vec<Comment>> {
        self.get_json(&format!(
            "/repos/{repo}/issues/{number}/comments?per_page=100"
        ))
    }

    pub fn workflows(&self, repo: &str) -> Result<Vec<Workflow>> {
        #[derive(Deserialize)]
        struct Page {
            workflows: Vec<Workflow>,
        }
        let page: Page = self.get_json(&format!("/repos/{repo}/actions/workflows?per_page=100"))?;
        Ok(page.workflows)
    }

    /// Recent runs of every workflow, or of one.
    pub fn runs(&self, repo: &str, workflow: Option<u64>) -> Result<Vec<Run>> {
        #[derive(Deserialize)]
        struct Page {
            workflow_runs: Vec<Run>,
        }
        let path = match workflow {
            Some(id) => format!("/repos/{repo}/actions/workflows/{id}/runs?per_page=30"),
            None => format!("/repos/{repo}/actions/runs?per_page=30"),
        };
        let page: Page = self.get_json(&path)?;
        Ok(page.workflow_runs)
    }

    pub fn jobs(&self, repo: &str, run: u64) -> Result<Vec<Job>> {
        #[derive(Deserialize)]
        struct Page {
            jobs: Vec<Job>,
        }
        let page: Page = self.get_json(&format!(
            "/repos/{repo}/actions/runs/{run}/jobs?per_page=100"
        ))?;
        Ok(page.jobs)
    }

    // --- transport ---------------------------------------------------------

    fn request(&self, path: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        self.agent
            .get(format!("{API}{path}"))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let mut response = self
            .request(path)
            .header("Accept", "application/vnd.github+json")
            .call()
            .context("contacting GitHub")?;
        let status = response.status().as_u16();
        let left = remaining(&response);
        let text = response
            .body_mut()
            .with_config()
            .limit(32 * 1024 * 1024)
            .read_to_string()
            .context("reading GitHub's answer")?;
        if !(200..300).contains(&status) {
            return Err(status_error(status, &text, left));
        }
        serde_json::from_str(&text).context("reading GitHub's answer")
    }
}

/// Requests left in this rate-limit window, when GitHub says.
fn remaining(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// A readable error for a failed request.
fn status_error(status: u16, text: &str, remaining: Option<u64>) -> anyhow::Error {
    let message = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string));
    match status {
        401 => anyhow::anyhow!("GitHub rejected the token. Sign in again."),
        403 | 429 if remaining == Some(0) => {
            anyhow::anyhow!("GitHub's rate limit is used up. Try again in a few minutes.")
        }
        404 => anyhow::anyhow!("Not found on GitHub, or this account cannot see it."),
        _ => match message {
            Some(message) => anyhow::anyhow!("GitHub: {message}"),
            None => anyhow::anyhow!("GitHub answered HTTP {status}"),
        },
    }
}

fn classify(bytes: Vec<u8>) -> FileContent {
    let size = bytes.len() as u64;
    if size > MAX_FILE_BYTES {
        return FileContent::TooLarge { size };
    }
    // Git's own test: a NUL byte near the start means binary.
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return FileContent::Binary { size };
    }
    match String::from_utf8(bytes) {
        Ok(text) => FileContent::Text(text),
        Err(_) => FileContent::Binary { size },
    }
}

fn contents_url(repo: &str, path: &str, git_ref: &str) -> String {
    let path = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(encode)
        .collect::<Vec<_>>()
        .join("/");
    let mut url = format!("/repos/{repo}/contents/{path}");
    if !git_ref.is_empty() {
        url.push_str("?ref=");
        url.push_str(&encode(git_ref));
    }
    url
}

/// Percent-encodes everything but RFC 3986's unreserved characters.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Fails when `repo` is not `owner/name`, so it cannot steer a request elsewhere.
pub fn check_repo_name(repo: &str) -> Result<()> {
    let valid = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if valid(owner) && valid(name) => Ok(()),
        _ => bail!("not a repository name: {repo}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntryKind, PullStatus, RunState};

    #[test]
    fn contents_urls_encode_paths_and_refs() {
        assert_eq!(
            contents_url("a/b", "src/my file.rs", "feature/x"),
            "/repos/a/b/contents/src/my%20file.rs?ref=feature%2Fx"
        );
        assert_eq!(contents_url("a/b", "", ""), "/repos/a/b/contents/");
    }

    #[test]
    fn repo_names_are_checked() {
        assert!(check_repo_name("rust-lang/rust").is_ok());
        assert!(check_repo_name("a/b.c_d").is_ok());
        assert!(check_repo_name("a/../b").is_err());
        assert!(check_repo_name("a").is_err());
        assert!(check_repo_name("a/b?x=1").is_err());
    }

    #[test]
    fn binary_and_text_files_are_told_apart() {
        assert_eq!(classify(b"fn main() {}".to_vec()), FileContent::Text("fn main() {}".into()));
        assert_eq!(classify(vec![0x89, b'P', 0, 1]), FileContent::Binary { size: 4 });
    }

    #[test]
    fn errors_read_well() {
        assert!(status_error(401, "", None).to_string().contains("Sign in again"));
        assert!(status_error(403, "", Some(0)).to_string().contains("rate limit"));
        assert_eq!(
            status_error(422, r#"{"message":"Validation Failed"}"#, Some(10)).to_string(),
            "GitHub: Validation Failed"
        );
    }

    #[test]
    fn answers_parse() {
        let repos: Vec<Repo> = serde_json::from_str(
            r#"[{"id":1,"name":"hangar","full_name":"ada/hangar","owner":{"login":"ada"},"private":true,
                "description":null,"default_branch":"main","html_url":"https://github.com/ada/hangar",
                "language":"Rust","stargazers_count":3,"forks_count":1,"open_issues_count":2,
                "pushed_at":"2026-11-01T10:00:00Z"}]"#,
        )
        .unwrap();
        assert_eq!((repos[0].owner.as_str(), repos[0].private), ("ada", true));
        assert!(repos[0].pushed_at > 0);

        let entries: Vec<Entry> = serde_json::from_str(
            r#"[{"name":"src","path":"src","type":"dir","size":0},{"name":"x","path":"x","type":"submodule"}]"#,
        )
        .unwrap();
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[1].kind, EntryKind::Submodule);

        let pull: PullRequest = serde_json::from_str(
            r#"{"number":7,"title":"Fix","state":"closed","draft":false,"user":{"login":"bo"},
                "body":"b","html_url":"u","labels":[{"name":"bug","color":"d73a4a"}],
                "head":{"ref":"fix"},"base":{"ref":"main"},"created_at":"2026-11-01T10:00:00Z",
                "updated_at":"2026-11-01T11:00:00Z","merged_at":"2026-11-01T12:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(pull.status(), PullStatus::Merged);
        assert_eq!(pull.head.name, "fix");

        let issue: Issue = serde_json::from_str(
            r#"{"number":3,"title":"t","state":"open","user":null,"html_url":"u","assignees":[{"login":"cy"}],
                "pull_request":{"url":"x"}}"#,
        )
        .unwrap();
        assert_eq!(issue.user, "ghost");
        assert_eq!(issue.assignees, vec!["cy".to_string()]);
        assert!(issue.pull_request.is_some());

        assert_eq!(RunState::from_github("completed", Some("timed_out")), RunState::Failure);
        assert_eq!(RunState::from_github("waiting", None), RunState::Queued);
    }
}

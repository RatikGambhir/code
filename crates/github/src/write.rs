//! Repository writes made with the same sign-in as the GitHub view.

use anyhow::{bail, ensure, Context as _, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};
use ureq::http::Method;

use crate::{
    api::{check_repo_name, classify, contents_url, encode, MAX_FILE_BYTES},
    Client, FileContent, Issue, PullRequest, Review,
};

#[derive(Clone, Copy)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub fn name(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }
}

#[derive(Clone, Copy)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Clone, Copy)]
pub enum RunAction {
    Cancel,
    Rerun,
    RerunFailedJobs,
}

pub struct EditableFile {
    pub text: String,
    pub sha: String,
}

impl Client {
    pub fn create_issue(&self, repo: &str, title: &str, body: &str) -> Result<Issue> {
        self.repo_write(
            repo,
            Method::POST,
            "/issues",
            Some(json!({ "title": title, "body": body })),
        )
    }

    /// GitHub uses the issues endpoint for fields shared by issues and PRs.
    pub fn update_issue(&self, repo: &str, number: u64, fields: Value) -> Result<Issue> {
        let issue: Issue = self.repo_write(
            repo,
            Method::PATCH,
            &format!("/issues/{number}"),
            Some(fields.clone()),
        )?;
        // GitHub may silently ignore some metadata when the viewer lacks access.
        for (field, actual) in [
            ("assignees", issue.assignees.clone()),
            (
                "labels",
                issue
                    .labels
                    .iter()
                    .map(|label| label.name.clone())
                    .collect(),
            ),
        ] {
            if let Some(expected) = fields[field].as_array() {
                ensure!(
                    expected.len() == actual.len()
                        && expected.iter().all(|name| name.as_str().is_some_and(|name|
                            actual.iter().any(|value| value.eq_ignore_ascii_case(name)))),
                    "GitHub did not apply the {field}. Check the names and your repository permissions."
                );
            }
        }
        if let Some(expected) = fields.get("milestone") {
            ensure!(
                expected.as_u64() == issue.milestone.as_ref().map(|milestone| milestone.number),
                "GitHub did not apply the milestone. Check the number and your repository permissions."
            );
        }
        Ok(issue)
    }

    pub fn create_pull(&self, repo: &str, fields: Value) -> Result<PullRequest> {
        self.repo_write(repo, Method::POST, "/pulls", Some(fields))
    }

    pub fn update_pull(&self, repo: &str, number: u64, fields: Value) -> Result<PullRequest> {
        self.repo_write(
            repo,
            Method::PATCH,
            &format!("/pulls/{number}"),
            Some(fields),
        )
    }

    pub fn set_pull_draft(&self, node_id: &str, draft: bool) -> Result<()> {
        let mutation = if draft {
            "convertPullRequestToDraft"
        } else {
            "markPullRequestReadyForReview"
        };
        let query = format!(
            "mutation($id: ID!) {{ {mutation}(input: {{pullRequestId: $id}}) {{ pullRequest {{ isDraft }} }} }}"
        );
        let response = self.write(
            Method::POST,
            "/graphql",
            Some(json!({ "query": query, "variables": { "id": node_id } })),
        )?;
        if let Some(message) = response["errors"][0]["message"].as_str() {
            bail!("GitHub: {message}");
        }
        ensure!(
            response["data"][mutation]["pullRequest"]["isDraft"].as_bool() == Some(draft),
            "GitHub did not update the draft status"
        );
        Ok(())
    }

    pub fn merge_pull(
        &self,
        repo: &str,
        number: u64,
        sha: &str,
        method: MergeMethod,
    ) -> Result<()> {
        ensure!(!sha.is_empty(), "Refresh the pull request before merging");
        let response = self.repo_request(
            repo,
            Method::PUT,
            &format!("/pulls/{number}/merge"),
            Some(json!({ "sha": sha, "merge_method": method.name() })),
        )?;
        ensure!(
            response["merged"].as_bool() == Some(true),
            "GitHub: {}",
            response["message"]
                .as_str()
                .unwrap_or("Pull request was not merged")
        );
        Ok(())
    }

    pub fn add_comment(&self, repo: &str, number: u64, body: &str) -> Result<()> {
        self.repo_request(
            repo,
            Method::POST,
            &format!("/issues/{number}/comments"),
            Some(json!({ "body": body })),
        )?;
        Ok(())
    }

    pub fn edit_comment(&self, repo: &str, id: u64, body: &str) -> Result<()> {
        self.repo_request(
            repo,
            Method::PATCH,
            &format!("/issues/comments/{id}"),
            Some(json!({ "body": body })),
        )?;
        Ok(())
    }

    pub fn delete_comment(&self, repo: &str, id: u64) -> Result<()> {
        self.repo_request(
            repo,
            Method::DELETE,
            &format!("/issues/comments/{id}"),
            None,
        )?;
        Ok(())
    }

    pub fn reviews(&self, repo: &str, number: u64) -> Result<Vec<Review>> {
        self.get_pages(&format!("/repos/{repo}/pulls/{number}/reviews"))
    }

    pub fn submit_review(
        &self,
        repo: &str,
        number: u64,
        sha: &str,
        event: ReviewEvent,
        body: &str,
    ) -> Result<()> {
        ensure!(!sha.is_empty(), "Refresh the pull request before reviewing");
        let event = match event {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        };
        self.repo_request(
            repo,
            Method::POST,
            &format!("/pulls/{number}/reviews"),
            Some(json!({ "event": event, "body": body, "commit_id": sha })),
        )?;
        Ok(())
    }

    pub fn change_reviewers(
        &self,
        repo: &str,
        number: u64,
        reviewers: &[String],
        teams: &[String],
        remove: bool,
    ) -> Result<()> {
        let method = if remove { Method::DELETE } else { Method::POST };
        self.repo_request(
            repo,
            method,
            &format!("/pulls/{number}/requested_reviewers"),
            Some(json!({ "reviewers": reviewers, "team_reviewers": teams })),
        )?;
        Ok(())
    }

    pub fn dispatch_workflow(
        &self,
        repo: &str,
        workflow: u64,
        git_ref: &str,
        inputs: Value,
    ) -> Result<()> {
        self.repo_request(
            repo,
            Method::POST,
            &format!("/actions/workflows/{workflow}/dispatches"),
            Some(json!({ "ref": git_ref, "inputs": inputs })),
        )?;
        Ok(())
    }

    pub fn change_run(&self, repo: &str, run: u64, action: RunAction) -> Result<()> {
        let command = match action {
            RunAction::Cancel => "cancel",
            RunAction::Rerun => "rerun",
            RunAction::RerunFailedJobs => "rerun-failed-jobs",
        };
        self.repo_request(
            repo,
            Method::POST,
            &format!("/actions/runs/{run}/{command}"),
            None,
        )?;
        Ok(())
    }

    pub fn editable_file(&self, repo: &str, path: &str, branch: &str) -> Result<EditableFile> {
        #[derive(Deserialize)]
        struct Blob {
            content: String,
        }
        let file = self.file_metadata(repo, path, branch)?;
        ensure!(file.size <= MAX_FILE_BYTES, "Choose a text file up to 2 MB");
        let blob: Blob =
            self.get_json(&format!("/repos/{repo}/git/blobs/{}", encode(&file.sha)))?;
        let content = blob.content.replace(['\n', '\r'], "");
        let bytes = STANDARD
            .decode(content)
            .context("decoding GitHub file contents")?;
        let FileContent::Text(text) = classify(bytes) else {
            bail!("Choose a text file up to 2 MB");
        };
        Ok(EditableFile {
            text,
            sha: file.sha,
        })
    }

    pub fn commit_file(
        &self,
        repo: &str,
        branch: &str,
        path: &str,
        message: &str,
        text: &str,
        sha: Option<&str>,
    ) -> Result<()> {
        check_repo_name(repo)?;
        check_path(path)?;
        ensure!(
            text.len() as u64 <= MAX_FILE_BYTES,
            "Choose a text file up to 2 MB"
        );
        let mut body =
            json!({ "message": message, "branch": branch, "content": STANDARD.encode(text) });
        if let Some(sha) = sha {
            ensure!(!sha.is_empty(), "Reload the file before committing");
            body["sha"] = json!(sha);
        }
        self.write(Method::PUT, &contents_url(repo, path, ""), Some(body))?;
        Ok(())
    }

    pub fn file_sha(&self, repo: &str, path: &str, branch: &str) -> Result<String> {
        Ok(self.file_metadata(repo, path, branch)?.sha)
    }

    pub fn delete_file(
        &self,
        repo: &str,
        branch: &str,
        path: &str,
        message: &str,
        sha: &str,
    ) -> Result<()> {
        check_repo_name(repo)?;
        check_path(path)?;
        ensure!(!sha.is_empty(), "Reload the file before deleting it");
        self.write(
            Method::DELETE,
            &contents_url(repo, path, ""),
            Some(json!({ "message": message, "branch": branch, "sha": sha })),
        )?;
        Ok(())
    }

    fn file_metadata(&self, repo: &str, path: &str, branch: &str) -> Result<FileMetadata> {
        check_repo_name(repo)?;
        check_path(path)?;
        let file: FileMetadata = self.get_json(&contents_url(repo, path, branch))?;
        ensure!(
            file.kind == "file" && file.submodule_git_url.is_none() && file.target.is_none(),
            "Choose a regular repository file"
        );
        Ok(file)
    }

    pub fn create_branch(&self, repo: &str, name: &str, from: &str) -> Result<()> {
        check_repo_name(repo)?;
        let branch: Value = self.get_json(&format!("/repos/{repo}/branches/{}", encode(from)))?;
        let sha = branch["commit"]["sha"]
            .as_str()
            .context("reading the source branch commit")?;
        self.repo_request(
            repo,
            Method::POST,
            "/git/refs",
            Some(json!({ "ref": format!("refs/heads/{name}"), "sha": sha })),
        )?;
        Ok(())
    }

    fn repo_request(
        &self,
        repo: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        check_repo_name(repo)?;
        self.write(method, &format!("/repos/{repo}{path}"), body)
    }

    fn repo_write<T: serde::de::DeserializeOwned>(
        &self,
        repo: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T> {
        serde_json::from_value(self.repo_request(repo, method, path, body)?)
            .context("reading GitHub's answer")
    }
}

#[derive(Deserialize)]
struct FileMetadata {
    sha: String,
    size: u64,
    #[serde(rename = "type")]
    kind: String,
    submodule_git_url: Option<String>,
    target: Option<String>,
}

fn check_path(path: &str) -> Result<()> {
    ensure!(
        path.split('/')
            .all(|segment| !matches!(segment, "" | "." | "..")),
        "Enter a repository-relative file path"
    );
    Ok(())
}

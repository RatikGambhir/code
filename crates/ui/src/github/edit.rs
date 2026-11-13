//! GitHub write actions opened from the repository views.

use anyhow::{ensure, Context as _, Result};
use cockpit_github::{
    write::{MergeMethod, ReviewEvent, RunAction},
    Client, Comment, Issue, Milestone, PullRequest, Run,
};
use gpui::{prelude::*, App, Context, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    menu::{DropdownMenu as _, PopupMenuItem},
    Sizable as _,
};
use serde_json::{json, Value};

use crate::remote_form::{self, Choice, Field, Fields, Values};

use super::view::GitHubView;

#[derive(Clone, Copy)]
pub(super) enum Target {
    Issue(Option<u64>),
    Pull(Option<u64>),
    Code,
    FileDeleted,
    Branch,
    Actions,
}

#[derive(Clone)]
pub(super) enum Edit {
    NewIssue,
    Issue(Issue),
    IssueState {
        number: u64,
        close: bool,
    },
    NewPull {
        head: String,
        base: String,
    },
    Pull(PullRequest),
    PullState {
        number: u64,
        close: bool,
    },
    PullDraft {
        number: u64,
        node: String,
        draft: bool,
    },
    Merge {
        number: u64,
        sha: String,
        base: String,
    },
    Review {
        number: u64,
        sha: String,
        author: bool,
    },
    Reviewers {
        number: u64,
        remove: bool,
    },
    Metadata {
        target: Target,
        number: u64,
        assignees: Vec<String>,
        labels: Vec<String>,
        milestone: Option<Milestone>,
    },
    Comment {
        target: Target,
        number: u64,
        comment: Option<Comment>,
    },
    DeleteComment {
        target: Target,
        number: u64,
        id: u64,
    },
    Branch {
        from: String,
    },
    File {
        branch: String,
        path: String,
        new: bool,
    },
    DeleteFile {
        branch: String,
        path: String,
    },
    Workflow {
        id: u64,
        name: String,
        git_ref: String,
    },
    Run {
        run: Run,
        action: RunAction,
    },
}

impl GitHubView {
    pub(super) fn edit_button(
        &self,
        id: &'static str,
        label: &'static str,
        edit: Edit,
        primary: bool,
        cx: &Context<Self>,
    ) -> Button {
        let button = Button::new(id).small().label(label).on_click(
            cx.listener(move |this, _, window, cx| this.open_edit(edit.clone(), window, cx)),
        );
        if primary {
            button.primary()
        } else {
            button
        }
    }

    pub(super) fn edit_menu(
        &self,
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        actions: Vec<(&'static str, Edit)>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let view = cx.entity().downgrade();
        Button::new(id)
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                for (label, edit) in &actions {
                    let (view, edit) = (view.clone(), edit.clone());
                    menu = menu.item(PopupMenuItem::new(*label).on_click(move |_, window, cx| {
                        let (view, edit) = (view.clone(), edit.clone());
                        window.defer(cx, move |window, cx| {
                            view.update(cx, |view, cx| view.open_edit(edit, window, cx))
                                .ok();
                        });
                    }));
                }
                menu
            })
            .into_any_element()
    }

    fn open_edit(&self, edit: Edit, window: &mut Window, cx: &mut App) {
        let model = self.model.clone();
        let state = model.read(cx);
        let (Some(client), Some(repo)) = (state.client(), state.repo.as_ref()) else {
            return;
        };
        let name = repo.full_name().to_string();
        let target = edit.target();
        let title = format!("{} · {name}", edit.title());
        let submit = edit.submit_label();
        let fields = edit.fields(&client, &name);
        let repo_name = name.clone();
        remote_form::open(
            title,
            submit,
            fields,
            move |values| edit.save(&client, &name, values),
            move |created, cx| {
                model.update(cx, |model, cx| {
                    model.refresh_after_write(&repo_name, target, created, cx)
                })
            },
            window,
            cx,
        );
    }
}

impl Edit {
    fn target(&self) -> Target {
        match self {
            Self::NewIssue => Target::Issue(None),
            Self::Issue(issue) => Target::Issue(Some(issue.number)),
            Self::IssueState { number, .. } => Target::Issue(Some(*number)),
            Self::NewPull { .. } => Target::Pull(None),
            Self::Pull(pull) => Target::Pull(Some(pull.number)),
            Self::PullState { number, .. }
            | Self::PullDraft { number, .. }
            | Self::Merge { number, .. }
            | Self::Review { number, .. }
            | Self::Reviewers { number, .. } => Target::Pull(Some(*number)),
            Self::Metadata { target, .. }
            | Self::Comment { target, .. }
            | Self::DeleteComment { target, .. } => *target,
            Self::Branch { .. } => Target::Branch,
            Self::File { .. } => Target::Code,
            Self::DeleteFile { .. } => Target::FileDeleted,
            Self::Workflow { .. } | Self::Run { .. } => Target::Actions,
        }
    }

    fn title(&self) -> String {
        match self {
            Self::NewIssue => "New issue".into(),
            Self::Issue(issue) => format!("Edit issue #{}", issue.number),
            Self::IssueState { number, close } => format!(
                "{} issue #{number}",
                if *close { "Close" } else { "Reopen" }
            ),
            Self::NewPull { .. } => "New pull request".into(),
            Self::Pull(pull) => format!("Edit pull request #{}", pull.number),
            Self::PullState { number, close } => format!(
                "{} pull request #{number}",
                if *close { "Close" } else { "Reopen" }
            ),
            Self::PullDraft { number, draft, .. } => format!(
                "{} #{number}",
                if *draft {
                    "Convert to draft"
                } else {
                    "Mark ready for review"
                }
            ),
            Self::Merge { number, .. } => format!("Merge pull request #{number}"),
            Self::Review { number, .. } => format!("Review pull request #{number}"),
            Self::Reviewers { number, remove } => format!(
                "{} for #{number}",
                if *remove {
                    "Remove review requests"
                } else {
                    "Request review"
                }
            ),
            Self::Metadata { number, .. } => {
                format!("Manage assignees, labels and milestone for #{number}")
            }
            Self::Comment {
                number, comment, ..
            } => format!(
                "{} comment on #{number}",
                if comment.is_some() { "Edit" } else { "Add" }
            ),
            Self::DeleteComment { number, .. } => format!("Delete comment on #{number}"),
            Self::Branch { .. } => "New branch".into(),
            Self::File { new: true, .. } => "New file".into(),
            Self::File { path, .. } => format!("Edit {path}"),
            Self::DeleteFile { path, .. } => format!("Delete {path}"),
            Self::Workflow { name, .. } => format!("Run {name}"),
            Self::Run { run, .. } => format!("{} #{}", self.submit_label(), run.run_number),
        }
    }

    fn submit_label(&self) -> &'static str {
        match self {
            Self::NewIssue | Self::NewPull { .. } | Self::Branch { .. } => "Create",
            Self::IssueState { close, .. } | Self::PullState { close, .. } => {
                if *close {
                    "Close"
                } else {
                    "Reopen"
                }
            }
            Self::PullDraft { draft, .. } => {
                if *draft {
                    "Convert to draft"
                } else {
                    "Mark ready"
                }
            }
            Self::Merge { .. } => "Merge",
            Self::Review { .. } => "Submit review",
            Self::Reviewers { remove, .. } => {
                if *remove {
                    "Remove requests"
                } else {
                    "Request review"
                }
            }
            Self::Comment { comment: None, .. } => "Post comment",
            Self::DeleteComment { .. } => "Delete comment",
            Self::File { .. } | Self::DeleteFile { .. } => "Commit to branch",
            Self::Workflow { .. } => "Run workflow",
            Self::Run { action, .. } => match action {
                RunAction::Cancel => "Cancel run",
                RunAction::Rerun => "Rerun workflow",
                RunAction::RerunFailedJobs => "Rerun failed jobs",
            },
            _ => "Save changes",
        }
    }

    fn fields(&self, client: &Client, repo: &str) -> Fields {
        let fields = match self {
            Self::NewIssue => title_and_body("", None),
            Self::Issue(issue) => title_and_body(&issue.title, issue.body.as_deref()),
            Self::IssueState { close: true, .. } => vec![Field::Select(
                "reason",
                "Reason",
                Some("completed".into()),
                choices(&[("completed", "Completed"), ("not_planned", "Not planned")]),
            )],
            Self::IssueState { .. } => vec![Field::Note("Reopen this issue?".into())],
            Self::NewPull { head, base } => {
                let mut fields = title_and_body("", None);
                fields.extend([
                    Field::Text("head", "Compare branch or owner:branch", head.clone()),
                    Field::Text("base", "Base branch", base.clone()),
                    Field::Select(
                        "draft",
                        "Pull request type",
                        Some("ready".into()),
                        choices(&[("ready", "Ready for review"), ("draft", "Draft")]),
                    ),
                ]);
                fields
            }
            Self::Pull(pull) => {
                let mut fields = title_and_body(&pull.title, pull.body.as_deref());
                if pull.state == "open" {
                    fields.push(Field::Text("base", "Base branch", pull.base.name.clone()));
                }
                fields
            }
            Self::PullState { close, .. } => vec![Field::Note(format!(
                "{} this pull request?",
                if *close { "Close" } else { "Reopen" }
            ))],
            Self::PullDraft { draft, .. } => vec![Field::Note(
                if *draft {
                    "Convert this pull request to a draft?"
                } else {
                    "Mark this pull request ready for review?"
                }
                .into(),
            )],
            Self::Merge { base, .. } => vec![
                Field::Note(format!("Merge the reviewed commit into {base}.")),
                Field::Select(
                    "method",
                    "Merge method",
                    Some("merge".into()),
                    choices(&[
                        ("merge", "Create a merge commit"),
                        ("squash", "Squash and merge"),
                        ("rebase", "Rebase and merge"),
                    ]),
                ),
            ],
            Self::Review { author, .. } => vec![
                Field::Select(
                    "event",
                    "Review decision",
                    Some("comment".into()),
                    if *author {
                        choices(&[("comment", "Comment")])
                    } else {
                        choices(&[
                            ("comment", "Comment"),
                            ("approve", "Approve"),
                            ("changes", "Request changes"),
                        ])
                    },
                ),
                Field::Area("body", "Review comment", String::new()),
            ],
            Self::Reviewers { .. } => vec![
                Field::Text(
                    "users",
                    "GitHub usernames, separated by commas",
                    String::new(),
                ),
                Field::Text("teams", "Team slugs, separated by commas", String::new()),
            ],
            Self::Metadata {
                assignees,
                labels,
                milestone,
                ..
            } => vec![
                Field::Text(
                    "assignees",
                    "Assignees, separated by commas",
                    assignees.join(", "),
                ),
                Field::Area("labels", "Existing labels, one per line", labels.join("\n")),
                Field::Text(
                    "milestone",
                    "Milestone number (blank to clear)",
                    milestone
                        .as_ref()
                        .map_or_else(String::new, |m| m.number.to_string()),
                ),
            ],
            Self::Comment { comment, .. } => vec![Field::Area(
                "body",
                "Comment",
                comment
                    .as_ref()
                    .and_then(|c| c.body.clone())
                    .unwrap_or_default(),
            )],
            Self::DeleteComment { .. } => {
                vec![Field::Note("Permanently delete this comment?".into())]
            }
            Self::Branch { from } => vec![
                Field::Text("name", "New branch name", String::new()),
                Field::Text("from", "Source branch", from.clone()),
            ],
            Self::File {
                branch,
                path,
                new: true,
            } => vec![
                Field::Note(format!("Commit to {branch}.")),
                Field::Text("path", "Repository path", path.clone()),
                Field::Text("message", "Commit message", String::new()),
                Field::Area("content", "File contents", String::new()),
            ],
            Self::File {
                branch,
                path,
                new: false,
            } => {
                let (client, repo, branch, path) = (
                    client.clone(),
                    repo.to_string(),
                    branch.clone(),
                    path.clone(),
                );
                return Fields::Load(Box::new(move || {
                    let file = client.editable_file(&repo, &path, &branch)?;
                    Ok(vec![
                        Field::Note(format!("Commit to {branch}.")),
                        Field::Text("message", "Commit message", String::new()),
                        Field::Area("content", "File contents", file.text),
                        Field::Hidden("sha", file.sha),
                    ])
                }));
            }
            Self::DeleteFile { branch, path } => {
                let (client, repo, branch, path) = (
                    client.clone(),
                    repo.to_string(),
                    branch.clone(),
                    path.clone(),
                );
                return Fields::Load(Box::new(move || {
                    let sha = client.file_sha(&repo, &path, &branch)?;
                    Ok(vec![
                        Field::Note(format!("Delete {path} from {branch}.")),
                        Field::Text("message", "Commit message", format!("Delete {path}")),
                        Field::Hidden("sha", sha),
                    ])
                }));
            }
            Self::Workflow { git_ref, .. } => vec![
                Field::Text("ref", "Branch or tag", git_ref.clone()),
                Field::Area("inputs", "Workflow inputs as a JSON object", "{}".into()),
            ],
            Self::Run { run, .. } => vec![Field::Note(format!(
                "{} for {}?",
                self.submit_label(),
                run.title()
            ))],
        };
        Fields::Ready(fields)
    }

    fn save(&self, client: &Client, repo: &str, values: Values) -> Result<Option<u64>> {
        match self {
            Self::NewIssue => {
                return Ok(Some(
                    client
                        .create_issue(
                            repo,
                            values.required("title", "a title")?,
                            values.get("body"),
                        )?
                        .number,
                ))
            }
            Self::Issue(issue) => {
                let fields = changed_title_and_body(&values)?;
                if fields != json!({}) {
                    client.update_issue(repo, issue.number, fields)?;
                }
            }
            Self::IssueState { number, close } => {
                let fields = if *close {
                    json!({ "state": "closed", "state_reason": values.get("reason") })
                } else {
                    json!({ "state": "open", "state_reason": "reopened" })
                };
                client.update_issue(repo, *number, fields)?;
            }
            Self::NewPull { .. } => {
                return Ok(Some(
                    client
                        .create_pull(
                            repo,
                            json!({
                                "title": values.required("title", "a title")?,
                                "body": values.get("body"),
                                "head": values.required("head", "a compare branch")?,
                                "base": values.required("base", "a base branch")?,
                                "draft": values.get("draft") == "draft",
                            }),
                        )?
                        .number,
                ))
            }
            Self::Pull(pull) => {
                let mut fields = changed_title_and_body(&values)?;
                if values.changed("base") {
                    fields["base"] = json!(values.required("base", "a base branch")?);
                }
                if fields != json!({}) {
                    client.update_pull(repo, pull.number, fields)?;
                }
            }
            Self::PullState { number, close } => {
                client.update_pull(
                    repo,
                    *number,
                    json!({ "state": if *close { "closed" } else { "open" } }),
                )?;
            }
            Self::PullDraft { node, draft, .. } => client.set_pull_draft(node, *draft)?,
            Self::Merge { number, sha, .. } => {
                let method = match values.get("method") {
                    "squash" => MergeMethod::Squash,
                    "rebase" => MergeMethod::Rebase,
                    _ => MergeMethod::Merge,
                };
                client.merge_pull(repo, *number, sha, method)?;
            }
            Self::Review { number, sha, .. } => {
                let event = match values.get("event") {
                    "approve" => ReviewEvent::Approve,
                    "changes" => ReviewEvent::RequestChanges,
                    _ => ReviewEvent::Comment,
                };
                if !matches!(event, ReviewEvent::Approve) {
                    values.required("body", "a review comment")?;
                }
                let body = values.get("body");
                client.submit_review(repo, *number, sha, event, body)?;
            }
            Self::Reviewers { number, remove } => {
                let users = list(values.get("users"), ',', true);
                let teams = list(values.get("teams"), ',', true);
                ensure!(
                    !users.is_empty() || !teams.is_empty(),
                    "Enter a username or team slug."
                );
                client.change_reviewers(repo, *number, &users, &teams, *remove)?;
            }
            Self::Metadata { number, .. } => {
                let mut fields = json!({});
                if values.changed("assignees") {
                    fields["assignees"] = json!(list(values.get("assignees"), ',', true));
                }
                if values.changed("labels") {
                    fields["labels"] = json!(list(values.get("labels"), '\n', false));
                }
                if values.changed("milestone") {
                    let text = values.get("milestone").trim();
                    fields["milestone"] = if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text.parse::<u64>().context("Enter a milestone number")?)
                    };
                }
                if fields != json!({}) {
                    client.update_issue(repo, *number, fields)?;
                }
            }
            Self::Comment {
                number, comment, ..
            } => {
                values.required("body", "a comment")?;
                let body = values.get("body");
                if let Some(comment) = comment {
                    client.edit_comment(repo, comment.id, body)?;
                } else {
                    client.add_comment(repo, *number, body)?;
                }
            }
            Self::DeleteComment { id, .. } => client.delete_comment(repo, *id)?,
            Self::Branch { .. } => client.create_branch(
                repo,
                values.required("name", "a branch name")?,
                values.required("from", "a source branch")?,
            )?,
            Self::File { branch, path, new } => {
                let path = if *new {
                    values.required("path", "a file path")?
                } else {
                    path
                };
                client.commit_file(
                    repo,
                    branch,
                    path,
                    values.required("message", "a commit message")?,
                    values.get("content"),
                    (!new).then(|| values.get("sha")),
                )?;
            }
            Self::DeleteFile { branch, path } => client.delete_file(
                repo,
                branch,
                path,
                values.required("message", "a commit message")?,
                values.get("sha"),
            )?,
            Self::Workflow { id, .. } => {
                let inputs = values.get("inputs").trim();
                let inputs: Value = if inputs.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(inputs)
                        .context("Enter workflow inputs as a JSON object")?
                };
                ensure!(
                    inputs.is_object(),
                    "Enter workflow inputs as a JSON object."
                );
                client.dispatch_workflow(
                    repo,
                    *id,
                    values.required("ref", "a branch or tag")?,
                    inputs,
                )?;
            }
            Self::Run { run, action } => client.change_run(repo, run.id, *action)?,
        }
        Ok(None)
    }
}

fn choices(items: &[(&str, &str)]) -> Vec<Choice> {
    items
        .iter()
        .map(|&(id, name)| Choice::new(id, name.to_string()))
        .collect()
}

fn title_and_body(title: &str, body: Option<&str>) -> Vec<Field> {
    vec![
        Field::Text("title", "Title", title.to_string()),
        Field::Area("body", "Description", body.unwrap_or_default().to_string()),
    ]
}

fn changed_title_and_body(values: &Values) -> Result<Value> {
    let mut fields = json!({});
    if values.changed("title") {
        fields["title"] = json!(values.required("title", "a title")?);
    }
    if values.changed("body") {
        fields["body"] = json!(values.get("body"));
    }
    Ok(fields)
}

fn list(text: &str, delimiter: char, username: bool) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for value in text.split(delimiter).map(str::trim) {
        let value = if username {
            value.trim_start_matches('@')
        } else {
            value
        };
        if !value.is_empty() && !result.iter().any(|name| name.eq_ignore_ascii_case(value)) {
            result.push(value.to_string());
        }
    }
    result
}

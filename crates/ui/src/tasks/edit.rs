//! Linear issue forms and comment actions in the Tasks view.

use anyhow::{ensure, Context as _, Result};
use cockpit_tasks::{
    key,
    linear::{
        self,
        write::{self, Comment, NamedId},
    },
    Issue, Priority, Source,
};
use gpui::{div, prelude::*, px, AnyElement, App, Context, FontWeight, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, Sizable as _,
};
use serde_json::{json, Value};

use super::{model::View, view::TasksView, widgets};
use crate::{
    remote_form::{self, Choice, Field, Fields, Values},
    theme::{tokens, TEXT_BODY, TEXT_SMALL},
};

#[derive(Clone, Copy)]
pub(super) enum IssueFields {
    All,
    Status,
    Assignee,
}

#[derive(Clone)]
pub(super) enum Edit {
    Create {
        project: Option<String>,
    },
    Issue {
        id: String,
        fields: IssueFields,
    },
    Labels {
        id: String,
    },
    Archive {
        id: String,
    },
    Comment {
        issue: String,
        comment: Option<Comment>,
    },
    DeleteComment {
        issue: String,
        id: String,
    },
}

enum Saved {
    Issue(Box<Issue>, bool),
    Archived(String),
    Comments(String),
    NoChange,
}

impl TasksView {
    pub(super) fn start_linear_create(&self, window: &mut Window, cx: &mut App) {
        let project = match &self.model.read(cx).selection.view {
            View::Project(id) => Some(id.clone()),
            _ => None,
        };
        self.open_linear_edit(
            "New Linear issue".into(),
            Edit::Create { project },
            window,
            cx,
        );
    }

    pub(super) fn linear_edit_button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        issue: &Issue,
        edit: Edit,
        cx: &Context<Self>,
    ) -> Button {
        let title = format!("{label} · {}", issue.identifier);
        Button::new(id)
            .label(label)
            .ghost()
            .xsmall()
            .on_click(cx.listener(move |view, _, window, cx| {
                view.open_linear_edit(title.clone(), edit.clone(), window, cx)
            }))
    }

    pub(super) fn open_linear_edit(
        &self,
        title: String,
        edit: Edit,
        window: &mut Window,
        cx: &mut App,
    ) {
        let model = self.model.clone();
        let linear = &model.read(cx).linear;
        let Some(viewer) = linear
            .viewer
            .as_ref()
            .filter(|_| linear.write_authorized && !linear.connecting)
        else {
            return;
        };
        let viewer_id = viewer.id.clone();
        let expected_viewer = viewer_id.clone();
        let fields = edit.fields(&viewer_id);
        remote_form::open(
            title,
            edit.submit_label(),
            fields,
            move |values| {
                let authorization = authorization(&viewer_id, true)?;
                edit.save(&authorization, &viewer_id, values)
            },
            move |saved, cx| {
                model.update(cx, |model, cx| {
                if !model.linear.viewer.as_ref().is_some_and(|viewer| viewer.id == expected_viewer) {
                    return;
                }
                match saved {
                    Saved::Issue(issue, created) => model.apply_linear_issue(*issue, created, cx),
                    Saved::Archived(id) => model.remove_linear_issue(&id, cx),
                    Saved::Comments(id) => {
                        if matches!(&model.open_issue, Some((Source::Linear, open)) if *open == id) {
                            model.load_linear_comments(cx);
                        }
                    }
                    Saved::NoChange => {}
                }
            })
            },
            window,
            cx,
        );
    }

    pub(super) fn render_linear_comments(
        &self,
        issue: &Issue,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let linear = &self.model.read(cx).linear;
        let comments = linear.comments.clone();
        let viewer = linear.viewer.as_ref().map(|viewer| viewer.id.clone());
        let editable = linear.write_authorized && !linear.connecting;
        let t = tokens(cx).clone();
        let body = match comments {
            None => div()
                .text_size(TEXT_SMALL)
                .text_color(t.fg_subtle)
                .child("Loading comments…")
                .into_any_element(),
            Some(Err(error)) => v_flex()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(TEXT_SMALL)
                        .text_color(t.danger)
                        .child(error),
                )
                .child(
                    Button::new("retry-linear-comments")
                        .label("Retry")
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model
                                .update(cx, |model, cx| model.load_linear_comments(cx))
                        })),
                )
                .into_any_element(),
            Some(Ok(comments)) => v_flex()
                .gap(px(12.))
                .children(comments.into_iter().map(|comment| {
                    let own = editable
                        && comment
                            .user
                            .as_ref()
                            .is_some_and(|user| viewer.as_ref() == Some(&user.id));
                    let author = comment
                        .user
                        .as_ref()
                        .map_or("Unknown user", |user| user.name.as_str());
                    v_flex()
                        .gap(px(4.))
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .text_size(TEXT_SMALL)
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(author.to_string()),
                                )
                                .child(
                                    div()
                                        .text_color(t.fg_subtle)
                                        .child(widgets::short_date(comment.timestamp())),
                                ),
                        )
                        .child(
                            div()
                                .text_size(TEXT_BODY)
                                .text_color(t.fg_muted)
                                .child(comment.body.clone()),
                        )
                        .when(own, |el| {
                            el.child(
                                h_flex()
                                    .gap(px(6.))
                                    .child(self.linear_edit_button(
                                        gpui::SharedString::from(format!(
                                            "linear-comment-edit-{}",
                                            comment.id
                                        )),
                                        "Edit comment",
                                        issue,
                                        Edit::Comment {
                                            issue: issue.id.clone(),
                                            comment: Some(comment.clone()),
                                        },
                                        cx,
                                    ))
                                    .child(self.linear_edit_button(
                                        gpui::SharedString::from(format!(
                                            "linear-comment-delete-{}",
                                            comment.id
                                        )),
                                        "Delete comment",
                                        issue,
                                        Edit::DeleteComment {
                                            issue: issue.id.clone(),
                                            id: comment.id,
                                        },
                                        cx,
                                    )),
                            )
                        })
                        .into_any_element()
                }))
                .into_any_element(),
        };
        v_flex()
            .gap(px(8.))
            .child(div().h(px(1.)).bg(t.border))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::MEDIUM)
                            .text_size(TEXT_BODY)
                            .child("Comments"),
                    )
                    .when(editable, |el| {
                        el.child(self.linear_edit_button(
                            "linear-comment-add",
                            "Add comment",
                            issue,
                            Edit::Comment {
                                issue: issue.id.clone(),
                                comment: None,
                            },
                            cx,
                        ))
                    }),
            )
            .child(body)
            .into_any_element()
    }
}

impl Edit {
    fn submit_label(&self) -> &'static str {
        match self {
            Self::Create { .. } => "Create issue",
            Self::Archive { .. } => "Archive issue",
            Self::Comment { comment: None, .. } => "Post comment",
            Self::DeleteComment { .. } => "Delete comment",
            _ => "Save changes",
        }
    }

    fn fields(&self, viewer: &str) -> Fields {
        let (edit, viewer) = (self.clone(), viewer.to_string());
        let fields = match self {
            Self::Archive { .. } => vec![Field::Note(
                "Archive this issue and remove it from the active board?".into(),
            )],
            Self::Comment { comment, .. } => vec![Field::Area(
                "body",
                "Comment",
                comment
                    .as_ref()
                    .map_or_else(String::new, |comment| comment.body.clone()),
            )],
            Self::DeleteComment { .. } => {
                vec![Field::Note("Permanently delete this comment?".into())]
            }
            _ => {
                return Fields::Load(Box::new(move || {
                    let authorization = authorization(&viewer, false)?;
                    edit.load_fields(&authorization)
                }))
            }
        };
        Fields::Ready(fields)
    }

    fn load_fields(&self, authorization: &str) -> Result<Vec<Field>> {
        match self {
            Self::Create { project } => {
                let mut teams = write::teams(authorization, project.as_deref())?;
                teams.sort_by_key(|team| team.name.to_lowercase());
                ensure!(!teams.is_empty(), "No Linear teams are available here.");
                let team = (teams.len() == 1).then(|| teams[0].id.clone());
                Ok(vec![
                    picker("teamId", "Team", team, teams, None),
                    Field::Text("title", "Title", String::new()),
                    Field::Area("description", "Description", String::new()),
                    priority_picker(0),
                ])
            }
            Self::Issue { id, fields } => {
                let issue = write::editable_issue(authorization, id)?;
                let mut result = Vec::new();
                if matches!(fields, IssueFields::All) {
                    result.extend([
                        Field::Text("title", "Title", issue.title),
                        Field::Area(
                            "description",
                            "Description",
                            issue.description.unwrap_or_default(),
                        ),
                    ]);
                }
                if matches!(fields, IssueFields::All | IssueFields::Status) {
                    let mut statuses = write::statuses(authorization, &issue.team)?;
                    let current = include_current(&mut statuses, Some(issue.state));
                    result.push(picker("stateId", "Status", current, statuses, None));
                }
                if matches!(fields, IssueFields::All | IssueFields::Assignee) {
                    let mut people = write::assignees(authorization, &issue.team)?;
                    let current = include_current(&mut people, issue.assignee);
                    result.push(picker(
                        "assigneeId",
                        "Assignee",
                        current,
                        people,
                        Some("Unassigned"),
                    ));
                }
                if matches!(fields, IssueFields::All) {
                    let mut projects = write::projects(authorization, &issue.team)?;
                    let current = include_current(&mut projects, issue.project);
                    result.extend([
                        priority_picker(issue.priority.clamp(0.0, 4.0) as u8),
                        picker(
                            "projectId",
                            "Project",
                            current,
                            projects,
                            Some("No project"),
                        ),
                        Field::Text(
                            "dueDate",
                            "Due date (YYYY-MM-DD)",
                            issue.due_date.unwrap_or_default(),
                        ),
                    ]);
                }
                Ok(result)
            }
            Self::Labels { id } => {
                let (mut available, applied) = write::label_choices(authorization, id)?;
                available.retain(|label| !applied.iter().any(|existing| existing.id == label.id));
                Ok(vec![
                    picker("add", "Add label", None, available, Some("No addition")),
                    picker("remove", "Remove label", None, applied, Some("No removal")),
                ])
            }
            _ => Ok(Vec::new()),
        }
    }

    fn save(&self, authorization: &str, viewer: &str, values: Values) -> Result<Saved> {
        match self {
            Self::Create { project } => {
                let mut fields = json!({
                    "teamId": values.required("teamId", "a team")?,
                    "title": values.required("title", "a title")?,
                    "description": values.get("description"),
                    "priority": values.get("priority").parse::<u8>()?,
                });
                if let Some(project) = project {
                    fields["projectId"] = json!(project);
                }
                Ok(Saved::Issue(
                    Box::new(write::create_issue(authorization, viewer, fields)?),
                    true,
                ))
            }
            Self::Issue { id, .. } => {
                let fields = changed_fields(&values)?;
                if fields == json!({}) {
                    return Ok(Saved::NoChange);
                }
                Ok(Saved::Issue(
                    Box::new(write::update_issue(authorization, viewer, id, fields)?),
                    false,
                ))
            }
            Self::Labels { id } => {
                let mut fields = json!({});
                if !values.get("add").is_empty() {
                    fields["addedLabelIds"] = json!([values.get("add")]);
                }
                if !values.get("remove").is_empty() {
                    fields["removedLabelIds"] = json!([values.get("remove")]);
                }
                if fields == json!({}) {
                    return Ok(Saved::NoChange);
                }
                Ok(Saved::Issue(
                    Box::new(write::update_issue(authorization, viewer, id, fields)?),
                    false,
                ))
            }
            Self::Archive { id } => {
                write::archive_issue(authorization, id)?;
                Ok(Saved::Archived(id.clone()))
            }
            Self::Comment { issue, comment } => {
                let body = values.get("body");
                values.required("body", "a comment")?;
                if let Some(comment) = comment {
                    write::edit_comment(authorization, &comment.id, body)?;
                } else {
                    write::add_comment(authorization, issue, body)?;
                }
                Ok(Saved::Comments(issue.clone()))
            }
            Self::DeleteComment { issue, id } => {
                write::delete_comment(authorization, id)?;
                Ok(Saved::Comments(issue.clone()))
            }
        }
    }
}

fn authorization(viewer_id: &str, write: bool) -> Result<String> {
    let key = key::usable_credential()?.context("Connect Linear to continue.")?;
    ensure!(
        !write || key.allows_writes(),
        "Reconnect Linear to grant write access."
    );
    let header = key.header();
    let viewer = linear::verify(&header)?;
    ensure!(
        viewer.id == viewer_id,
        "The Linear account changed. Reopen the issue after refreshing."
    );
    Ok(header)
}

fn include_current(options: &mut Vec<NamedId>, current: Option<NamedId>) -> Option<String> {
    let current = current?;
    let id = current.id.clone();
    if !options.iter().any(|option| option.id == id) {
        options.push(current);
    }
    Some(id)
}

fn picker(
    key: &'static str,
    label: &'static str,
    selected: Option<String>,
    options: Vec<NamedId>,
    empty: Option<&str>,
) -> Field {
    let options = empty
        .map(|name| Choice::new("", name.to_string()))
        .into_iter()
        .chain(
            options
                .into_iter()
                .map(|option| Choice::new(option.id, option.name)),
        )
        .collect();
    Field::Select(
        key,
        label,
        selected.or_else(|| empty.map(|_| String::new())),
        options,
    )
}

fn priority_picker(priority: u8) -> Field {
    Field::Select(
        "priority",
        "Priority",
        Some(priority.to_string()),
        Priority::ALL
            .iter()
            .enumerate()
            .map(|(i, priority)| Choice::new(i.to_string(), priority.label()))
            .collect(),
    )
}

fn changed_fields(values: &Values) -> Result<Value> {
    let mut fields = json!({});
    if values.changed("title") {
        fields["title"] = json!(values.required("title", "a title")?);
    }
    if values.changed("description") {
        fields["description"] = json!(values.get("description"));
    }
    if values.changed("stateId") {
        fields["stateId"] = json!(values.required("stateId", "a status")?);
    }
    if values.changed("priority") {
        fields["priority"] = json!(values.get("priority").parse::<u8>()?);
    }
    for key in ["assigneeId", "projectId", "dueDate"] {
        if values.changed(key) {
            let value = values.get(key).trim();
            if key == "dueDate" && !value.is_empty() {
                chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                    .context("Enter a date in YYYY-MM-DD format")?;
            }
            fields[key] = if value.is_empty() {
                Value::Null
            } else {
                json!(value)
            };
        }
    }
    Ok(fields)
}

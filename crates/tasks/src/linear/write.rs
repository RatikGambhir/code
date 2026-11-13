//! Issue and comment operations, plus the choices needed by the issue editor.

use anyhow::{ensure, Context as _, Result};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};

use super::{request, timestamp, PageInfo, RawIssue, ISSUE_FIELDS};
use crate::Issue;

#[derive(Clone, Debug, Deserialize)]
pub struct NamedId {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
pub struct EditableIssue {
    pub title: String,
    pub description: Option<String>,
    pub priority: f64,
    pub team: Team,
    pub state: NamedId,
    pub assignee: Option<NamedId>,
    pub project: Option<NamedId>,
    #[serde(rename = "dueDate")]
    pub due_date: Option<String>,
}

#[derive(Deserialize)]
pub struct Team {
    pub id: String,
    pub private: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: String,
    pub body: String,
    pub user: Option<NamedId>,
    pub created_at: String,
}

impl Comment {
    pub fn timestamp(&self) -> i64 {
        timestamp(Some(&self.created_at))
    }
}

pub fn teams(authorization: &str, project: Option<&str>) -> Result<Vec<NamedId>> {
    let (query, path, variables) = match project {
        Some(id) => (
            "query($id: String!, $after: String) { project(id: $id) {
                teams(first: 100, after: $after) { nodes { id name } pageInfo { hasNextPage endCursor } }
            }}",
            "/project/teams",
            json!({ "id": id }),
        ),
        None => (
            "query($after: String) {
                teams(first: 100, after: $after) { nodes { id name } pageInfo { hasNextPage endCursor } }
            }",
            "/teams",
            json!({}),
        ),
    };
    paginate(authorization, query, path, variables)
}

pub fn editable_issue(authorization: &str, id: &str) -> Result<EditableIssue> {
    #[derive(Deserialize)]
    struct Data {
        issue: EditableIssue,
    }
    let data: Data = request(
        authorization,
        "
        query($id: String!) { issue(id: $id) {
            title description priority dueDate team { id private }
            state { id name } assignee { id name } project { id name }
        } }",
        json!({ "id": id }),
    )?;
    Ok(data.issue)
}

pub fn statuses(authorization: &str, team: &Team) -> Result<Vec<NamedId>> {
    #[derive(Deserialize)]
    struct WorkflowState {
        #[serde(flatten)]
        choice: NamedId,
        position: f64,
    }
    let mut states: Vec<WorkflowState> = paginate(
        authorization,
        "
        query($id: String!, $after: String) { team(id: $id) {
            states(first: 100, after: $after) {
                nodes { id name position } pageInfo { hasNextPage endCursor }
            }
        } }",
        "/team/states",
        json!({ "id": team.id }),
    )?;
    states.sort_by(|a, b| a.position.total_cmp(&b.position));
    Ok(states.into_iter().map(|state| state.choice).collect())
}

pub fn assignees(authorization: &str, team: &Team) -> Result<Vec<NamedId>> {
    let mut assignees: Vec<NamedId> = if team.private {
        paginate(
            authorization,
            "
            query($id: String!, $after: String) { team(id: $id) {
                members(first: 100, after: $after, filter: { active: { eq: true } }) {
                    nodes { id name } pageInfo { hasNextPage endCursor }
                }
            } }",
            "/team/members",
            json!({ "id": team.id }),
        )?
    } else {
        paginate(
            authorization,
            "
            query($after: String) {
                users(first: 100, after: $after, filter: { active: { eq: true } }) {
                    nodes { id name } pageInfo { hasNextPage endCursor }
                }
            }",
            "/users",
            json!({}),
        )?
    };
    assignees.sort_by_key(|user| user.name.to_lowercase());
    Ok(assignees)
}

pub fn projects(authorization: &str, team: &Team) -> Result<Vec<NamedId>> {
    paginate(
        authorization,
        "
        query($id: String!, $after: String) { team(id: $id) {
            projects(first: 100, after: $after) {
                nodes { id name } pageInfo { hasNextPage endCursor }
            }
        } }",
        "/team/projects",
        json!({ "id": team.id }),
    )
}

pub fn label_choices(authorization: &str, issue: &str) -> Result<(Vec<NamedId>, Vec<NamedId>)> {
    let issue_data = editable_issue(authorization, issue)?;
    let available = paginate(authorization, "
        query($id: ID!, $after: String) {
            issueLabels(first: 100, after: $after, filter: {
                isGroup: { eq: false }, or: [{ team: { id: { eq: $id } } }, { team: { null: true } }]
            }) { nodes { id name } pageInfo { hasNextPage endCursor } }
        }", "/issueLabels", json!({ "id": issue_data.team.id }))?;
    let applied = paginate(
        authorization,
        "
        query($id: String!, $after: String) { issue(id: $id) {
            labels(first: 100, after: $after) {
                nodes { id name } pageInfo { hasNextPage endCursor }
            }
        } }",
        "/issue/labels",
        json!({ "id": issue }),
    )?;
    Ok((available, applied))
}

pub fn create_issue(authorization: &str, viewer_id: &str, fields: Value) -> Result<Issue> {
    let query = format!(
        "mutation($input: IssueCreateInput!) {{
        issueCreate(input: $input) {{ success issue {{ {ISSUE_FIELDS} }} }}
    }}"
    );
    let payload = mutate(
        authorization,
        &query,
        "issueCreate",
        json!({ "input": fields }),
    )?;
    issue_from_payload(payload, viewer_id)
}

pub fn update_issue(
    authorization: &str,
    viewer_id: &str,
    id: &str,
    fields: Value,
) -> Result<Issue> {
    let query = format!(
        "mutation($id: String!, $input: IssueUpdateInput!) {{
        issueUpdate(id: $id, input: $input) {{ success issue {{ {ISSUE_FIELDS} }} }}
    }}"
    );
    let payload = mutate(
        authorization,
        &query,
        "issueUpdate",
        json!({ "id": id, "input": fields }),
    )?;
    issue_from_payload(payload, viewer_id)
}

pub fn archive_issue(authorization: &str, id: &str) -> Result<()> {
    mutate(
        authorization,
        "mutation($id: String!) { issueArchive(id: $id) { success } }",
        "issueArchive",
        json!({ "id": id }),
    )?;
    Ok(())
}

pub fn comments(authorization: &str, id: &str) -> Result<Vec<Comment>> {
    paginate(
        authorization,
        "
        query($id: String!, $after: String) { issue(id: $id) {
            comments(first: 50, after: $after) {
                nodes { id body createdAt user { id name } } pageInfo { hasNextPage endCursor }
            }
        } }",
        "/issue/comments",
        json!({ "id": id }),
    )
}

pub fn add_comment(authorization: &str, issue_id: &str, body: &str) -> Result<()> {
    mutate(
        authorization,
        "
        mutation($input: CommentCreateInput!) { commentCreate(input: $input) { success } }",
        "commentCreate",
        json!({ "input": { "issueId": issue_id, "body": body } }),
    )?;
    Ok(())
}

pub fn edit_comment(authorization: &str, id: &str, body: &str) -> Result<()> {
    mutate(
        authorization,
        "
        mutation($id: String!, $input: CommentUpdateInput!) {
            commentUpdate(id: $id, input: $input) { success }
        }",
        "commentUpdate",
        json!({ "id": id, "input": { "body": body } }),
    )?;
    Ok(())
}

pub fn delete_comment(authorization: &str, id: &str) -> Result<()> {
    mutate(
        authorization,
        "mutation($id: String!) { commentDelete(id: $id) { success } }",
        "commentDelete",
        json!({ "id": id }),
    )?;
    Ok(())
}

fn mutate(authorization: &str, query: &str, key: &str, variables: Value) -> Result<Value> {
    let mut data: Value = request(authorization, query, variables)?;
    let payload = data[key].take();
    ensure!(
        payload["success"].as_bool() == Some(true),
        "Linear did not save the change"
    );
    Ok(payload)
}

fn issue_from_payload(mut payload: Value, viewer_id: &str) -> Result<Issue> {
    let raw: RawIssue = serde_json::from_value(payload["issue"].take())
        .context("reading the saved Linear issue")?;
    Ok(raw.into_issue(viewer_id))
}

fn paginate<T: DeserializeOwned>(
    authorization: &str,
    query: &str,
    path: &str,
    mut variables: Value,
) -> Result<Vec<T>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Connection<T> {
        nodes: Vec<T>,
        page_info: PageInfo,
    }
    let mut results = Vec::new();
    loop {
        let data: Value = request(authorization, query, variables.clone())?;
        let page: Connection<T> = serde_json::from_value(
            data.pointer(path)
                .context("Linear returned no results for this item")?
                .clone(),
        )
        .context("reading Linear's results")?;
        results.extend(page.nodes);
        if !page.page_info.has_next_page {
            return Ok(results);
        }
        let next = page
            .page_info
            .end_cursor
            .context("Linear returned no cursor for the next page")?;
        ensure!(
            variables["after"].as_str() != Some(&next),
            "Linear returned a duplicate page"
        );
        variables["after"] = json!(next);
    }
}

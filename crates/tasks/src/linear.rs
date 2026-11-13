//! Linear's GraphQL API. Calls take the
//! `Authorization` header value: a personal API key, or `Bearer <token>`
//! from the browser sign-in.
//!
//! One round trip fetches the account, its projects and the most recently
//! updated issues; further pages follow the cursor up to [`MAX_ISSUES`].

use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::model::{
    Board, Issue, Label, Priority, Project, ProjectState, Source, Status, StatusKind,
};

const ENDPOINT: &str = "https://api.linear.app/graphql";
/// Issues per request; small enough to stay under Linear's query complexity limit.
const PAGE: usize = 100;
/// The most recently updated issues kept.
pub const MAX_ISSUES: usize = 500;

pub mod write;

const ISSUE_FIELDS: &str = "
    id identifier title description url priority createdAt updatedAt
    state { name type color }
    assignee { id name }
    project { id }
    labels(first: 5) { nodes { name color } }";

fn issue_page_fragment() -> String {
    format!("fragment IssuePage on IssueConnection {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {ISSUE_FIELDS} }} }}")
}

/// The signed-in Linear user.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Viewer {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "organization_name")]
    pub organization: Option<String>,
}

fn organization_name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    struct Org {
        name: String,
    }
    Ok(Option::<Org>::deserialize(d)?.map(|o| o.name))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinearBoard {
    pub viewer: Viewer,
    pub board: Board,
}

/// Checks a sign-in by asking who it belongs to.
pub fn verify(authorization: &str) -> Result<Viewer> {
    #[derive(Deserialize)]
    struct Data {
        viewer: Viewer,
    }
    let data: Data = request(
        authorization,
        "query { viewer { id name organization { name } } }",
        json!({}),
    )?;
    Ok(data.viewer)
}

/// Projects and recent issues the sign-in can see.
pub fn fetch(authorization: &str) -> Result<LinearBoard> {
    let fragment = issue_page_fragment();
    let first = format!(
        "query Board($first: Int!) {{
  viewer {{ id name organization {{ name }} }}
  projects(first: 50, orderBy: updatedAt) {{
    nodes {{ id name description color url progress targetDate lead {{ name }} status {{ name type }} }}
  }}
  issues(first: $first, orderBy: updatedAt) {{ ...IssuePage }}
}}{fragment}"
    );
    let data: FirstPage = request(authorization, &first, json!({ "first": PAGE }))?;
    let viewer = data.viewer;
    let mut board = Board {
        projects: data
            .projects
            .nodes
            .into_iter()
            .map(RawProject::into_project)
            .collect(),
        issues: Vec::new(),
    };
    let mut page = data.issues;
    loop {
        let next = page
            .page_info
            .has_next_page
            .then(|| page.page_info.end_cursor.clone())
            .flatten();
        board
            .issues
            .extend(page.nodes.into_iter().map(|raw| raw.into_issue(&viewer.id)));
        let Some(after) = next.filter(|_| board.issues.len() < MAX_ISSUES) else {
            break;
        };
        let more = format!(
            "query More($first: Int!, $after: String) {{ issues(first: $first, after: $after, orderBy: updatedAt) {{ ...IssuePage }} }}{fragment}"
        );
        let data: MorePage = request(authorization, &more, json!({ "first": PAGE, "after": after }))?;
        page = data.issues;
    }
    board.issues.truncate(MAX_ISSUES);
    Ok(LinearBoard { viewer, board })
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(format!(
            "{}/{}",
            cockpit_common::APP_NAME,
            cockpit_common::APP_VERSION
        ))
        .build()
        .into()
}

fn request<T: for<'de> Deserialize<'de>>(
    authorization: &str,
    query: &str,
    variables: Value,
) -> Result<T> {
    let body = json!({ "query": query, "variables": variables });
    let mut response = agent()
        .post(ENDPOINT)
        .header("Authorization", authorization.trim())
        .header("Content-Type", "application/json")
        .send(serde_json::to_string(&body)?)
        .context("contacting Linear")?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    parse_response(status, &text)
}

/// Turns an HTTP status and GraphQL body into data or a readable error.
fn parse_response<T: for<'de> Deserialize<'de>>(status: u16, text: &str) -> Result<T> {
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let message = value["errors"][0]["message"].as_str().map(str::to_string);
    if status == 401
        || message
            .as_deref()
            .is_some_and(|m| m.contains("Authentication"))
    {
        bail!("Linear rejected the sign-in. Connect Linear again.");
    }
    if let Some(message) = message {
        bail!("Linear: {message}");
    }
    if !(200..300).contains(&status) {
        bail!("Linear answered HTTP {status}");
    }
    serde_json::from_value(value["data"].clone()).context("reading Linear's answer")
}

// --- wire types ------------------------------------------------------------

#[derive(Deserialize)]
struct FirstPage {
    viewer: Viewer,
    projects: Nodes<RawProject>,
    issues: IssuePage,
}

#[derive(Deserialize)]
struct MorePage {
    issues: IssuePage,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssuePage {
    page_info: PageInfo,
    nodes: Vec<RawIssue>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

#[derive(Deserialize)]
struct RawStatus {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Deserialize)]
struct RawAssignee {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct RawLabel {
    name: String,
    color: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProject {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    color: Option<String>,
    url: Option<String>,
    #[serde(default)]
    progress: f32,
    target_date: Option<String>,
    lead: Option<Named>,
    status: Option<RawStatus>,
}

impl RawProject {
    fn into_project(self) -> Project {
        Project {
            id: self.id,
            name: self.name,
            description: self.description.unwrap_or_default(),
            state: self.status.map_or(ProjectState::Planned, |s| {
                ProjectState::from_linear(&s.kind)
            }),
            color: self.color,
            lead: self.lead.map(|l| l.name),
            target_date: self.target_date,
            progress: self.progress.clamp(0.0, 1.0),
            url: self.url,
            source: Source::Linear,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIssue {
    id: String,
    identifier: String,
    title: String,
    description: Option<String>,
    url: Option<String>,
    #[serde(default)]
    priority: f64,
    created_at: Option<String>,
    updated_at: Option<String>,
    state: Option<RawStatus>,
    assignee: Option<RawAssignee>,
    project: Option<Id>,
    labels: Option<Nodes<RawLabel>>,
}

impl RawIssue {
    fn into_issue(self, viewer_id: &str) -> Issue {
        let status = self.state.map_or_else(
            || Status::local(StatusKind::Unstarted),
            |s| Status {
                kind: StatusKind::from_linear(&s.kind),
                name: s.name,
                color: s.color,
            },
        );
        Issue {
            id: self.id,
            identifier: self.identifier,
            title: self.title,
            description: self.description.unwrap_or_default(),
            status,
            priority: Priority::from_linear(self.priority.clamp(0.0, 4.0) as u8),
            mine: self.assignee.as_ref().is_some_and(|a| a.id == viewer_id),
            assignee: self.assignee.map(|a| a.name),
            project_id: self.project.map(|p| p.id),
            labels: self
                .labels
                .map(|l| {
                    l.nodes
                        .into_iter()
                        .map(|l| Label {
                            name: l.name,
                            color: l.color,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            created_at: timestamp(self.created_at.as_deref()),
            updated_at: timestamp(self.updated_at.as_deref()),
            url: self.url,
            source: Source::Linear,
        }
    }
}

fn timestamp(text: Option<&str>) -> i64 {
    text.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map_or(0, |t| t.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = r##"{"data":{
      "viewer":{"id":"u1","name":"Ada","organization":{"name":"Acme"}},
      "projects":{"nodes":[{"id":"p1","name":"Launch","description":null,"color":"#5e6ad2","url":"https://linear.app/acme/project/launch",
        "progress":0.42,"targetDate":"2026-12-01","lead":{"name":"Ada"},"status":{"name":"In Progress","type":"started"}}]},
      "issues":{"pageInfo":{"hasNextPage":false,"endCursor":"c1"},"nodes":[
        {"id":"i1","identifier":"ENG-7","title":"Fix login","description":"Steps","url":"https://linear.app/acme/issue/ENG-7",
         "priority":1,"createdAt":"2026-11-01T10:00:00.000Z","updatedAt":"2026-11-02T10:00:00.000Z",
         "state":{"name":"In Review","type":"started","color":"#f2c94c"},"assignee":{"id":"u1","name":"Ada"},
         "project":{"id":"p1"},"labels":{"nodes":[{"name":"Bug","color":"#eb5757"}]}},
        {"id":"i2","identifier":"ENG-8","title":"Docs","description":null,"url":null,"priority":0,
         "createdAt":null,"updatedAt":null,"state":null,"assignee":null,"project":null,"labels":null}]}}}"##;

    #[test]
    fn first_page_parses_into_the_board() {
        let page: FirstPage = parse_response(200, FIRST).unwrap();
        assert_eq!(page.viewer.organization.as_deref(), Some("Acme"));
        let project = page
            .projects
            .nodes
            .into_iter()
            .next()
            .unwrap()
            .into_project();
        assert_eq!(
            (project.state, project.lead.as_deref()),
            (ProjectState::Started, Some("Ada"))
        );
        assert!((project.progress - 0.42).abs() < 1e-6);

        let issues: Vec<Issue> = page
            .issues
            .nodes
            .into_iter()
            .map(|i| i.into_issue("u1"))
            .collect();
        let fix = &issues[0];
        assert_eq!(
            (fix.status.kind, fix.status.name.as_str()),
            (StatusKind::Started, "In Review")
        );
        assert_eq!(fix.priority, Priority::Urgent);
        assert!(fix.mine);
        assert_eq!(fix.labels[0].name, "Bug");
        assert_eq!(fix.updated_at - fix.created_at, 86_400);

        let docs = &issues[1];
        assert!(!docs.mine);
        assert_eq!(docs.status.kind, StatusKind::Unstarted);
        assert_eq!(docs.updated_at, 0);
    }

    #[test]
    fn errors_read_well() {
        let err =
            parse_response::<Value>(401, r#"{"errors":[{"message":"Authentication required"}]}"#)
                .unwrap_err();
        assert!(err.to_string().contains("rejected the sign-in"));
        let err =
            parse_response::<Value>(200, r#"{"errors":[{"message":"Field 'x' doesn't exist"}]}"#)
                .unwrap_err();
        assert_eq!(err.to_string(), "Linear: Field 'x' doesn't exist");
        let err = parse_response::<Value>(502, "<html>").unwrap_err();
        assert_eq!(err.to_string(), "Linear answered HTTP 502");
    }
}

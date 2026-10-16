//! The conversation as the UI shows it, built by folding [`AgentEvent`]s.
//!
//! This is a pure reducer: no I/O, no UI types. It is fed live events while a
//! turn runs, and rebuilt from `Turn`s when a thread is resumed after a
//! restart, so both paths produce the same transcript.

use std::collections::HashMap;

use crate::{
    events::{AgentEvent, ApprovalKind, PendingApproval},
    protocol::*,
    rpc::RequestId,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ChangedFile {
    pub path: String,
    /// "added", "deleted", "modified" or "renamed → new/path".
    pub change: String,
    pub diff: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    Accepted,
    AcceptedForSession,
    Declined,
    /// The server stopped waiting (turn ended or was interrupted).
    Expired,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EntryKind {
    User {
        text: String,
    },
    Agent {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Plan {
        text: String,
    },
    Command {
        command: String,
        cwd: String,
        output: String,
        status: ItemStatus,
        exit_code: Option<i32>,
        duration_ms: Option<i64>,
    },
    FileChange {
        files: Vec<ChangedFile>,
        status: ItemStatus,
    },
    Tool {
        label: String,
        detail: Option<String>,
        success: Option<bool>,
    },
    Approval {
        request_id: RequestId,
        kind: ApprovalKind,
        state: ApprovalState,
    },
    Error {
        message: String,
    },
    Notice {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub turn_id: Option<String>,
    pub kind: EntryKind,
    /// Still receiving deltas.
    pub streaming: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    active_turn: Option<String>,
    last_status: Option<TurnStatus>,
    usage: Option<ThreadTokenUsage>,
    plan: Vec<PlanStep>,
    plan_explanation: Option<String>,
    diff: Option<String>,
    /// Increments on every change, so views can cache derived layout.
    revision: u64,
    next_local: u64,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds the transcript of a resumed thread.
    pub fn from_turns(turns: &[Turn]) -> Self {
        let mut transcript = Self::new();
        for turn in turns {
            for item in &turn.items {
                transcript.upsert_item(&turn.id, item, false);
            }
            if let Some(error) = &turn.error {
                transcript.push_entry(Entry {
                    id: format!("error-{}", turn.id),
                    turn_id: Some(turn.id.clone()),
                    kind: EntryKind::Error {
                        message: error.message.clone(),
                    },
                    streaming: false,
                });
            }
            transcript.last_status = Some(turn.status);
        }
        transcript
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The running turn, if any.
    pub fn active_turn(&self) -> Option<&str> {
        self.active_turn.as_deref()
    }

    pub fn is_running(&self) -> bool {
        self.active_turn.is_some()
    }

    pub fn last_status(&self) -> Option<TurnStatus> {
        self.last_status
    }

    pub fn usage(&self) -> Option<&ThreadTokenUsage> {
        self.usage.as_ref()
    }

    pub fn plan(&self) -> (&[PlanStep], Option<&str>) {
        (&self.plan, self.plan_explanation.as_deref())
    }

    /// The latest aggregated diff of the running (or last) turn.
    pub fn diff(&self) -> Option<&str> {
        self.diff.as_deref()
    }

    pub fn pending_approvals(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| {
            matches!(
                e.kind,
                EntryKind::Approval {
                    state: ApprovalState::Pending,
                    ..
                }
            )
        })
    }

    /// Shows the user's prompt immediately, before the server echoes it.
    pub fn push_local_user(&mut self, text: &str) {
        self.next_local += 1;
        self.push_entry(Entry {
            id: format!("local-{}", self.next_local),
            turn_id: None,
            kind: EntryKind::User { text: text.to_string() },
            streaming: false,
        });
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.next_local += 1;
        self.push_entry(Entry {
            id: format!("notice-{}", self.next_local),
            turn_id: self.active_turn.clone(),
            kind: EntryKind::Notice { text: text.into() },
            streaming: false,
        });
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.next_local += 1;
        self.push_entry(Entry {
            id: format!("error-local-{}", self.next_local),
            turn_id: self.active_turn.clone(),
            kind: EntryKind::Error { message: text.into() },
            streaming: false,
        });
    }

    /// Records the user's decision on an approval.
    pub fn set_approval_state(&mut self, request_id: &RequestId, new_state: ApprovalState) {
        for entry in &mut self.entries {
            if let EntryKind::Approval { request_id: id, state, .. } = &mut entry.kind {
                if id == request_id && *state == ApprovalState::Pending {
                    *state = new_state;
                    self.revision += 1;
                }
            }
        }
    }

    /// Folds one event in. Returns whether anything visible changed.
    pub fn apply(&mut self, event: &AgentEvent) -> bool {
        let before = self.revision;
        match event {
            AgentEvent::TurnStarted { turn, .. } => {
                self.active_turn = Some(turn.id.clone());
                self.diff = None;
                self.plan.clear();
                self.plan_explanation = None;
                self.revision += 1;
            }
            AgentEvent::TurnCompleted { turn, .. } => {
                if self.active_turn.as_deref() == Some(turn.id.as_str()) || self.active_turn.is_none() {
                    self.active_turn = None;
                }
                self.last_status = Some(turn.status);
                for entry in &mut self.entries {
                    if entry.turn_id.as_deref() == Some(turn.id.as_str()) {
                        entry.streaming = false;
                    }
                    if let EntryKind::Approval { state, .. } = &mut entry.kind {
                        if *state == ApprovalState::Pending && entry.turn_id.as_deref() == Some(turn.id.as_str()) {
                            *state = ApprovalState::Expired;
                        }
                    }
                }
                if let Some(error) = &turn.error {
                    self.push_entry(Entry {
                        id: format!("error-{}", turn.id),
                        turn_id: Some(turn.id.clone()),
                        kind: EntryKind::Error { message: error.message.clone() },
                        streaming: false,
                    });
                } else if turn.status == TurnStatus::Interrupted {
                    self.push_notice("Turn interrupted");
                }
                self.revision += 1;
            }
            AgentEvent::ItemStarted { turn_id, item, .. } => self.upsert_item(turn_id, item, true),
            AgentEvent::ItemCompleted { turn_id, item, .. } => self.upsert_item(turn_id, item, false),
            AgentEvent::AgentMessageDelta { item_id, delta, .. } => {
                self.append_text(item_id, delta, |text| EntryKind::Agent { text })
            }
            AgentEvent::ReasoningDelta { item_id, delta, .. } => {
                self.append_text(item_id, delta, |text| EntryKind::Reasoning { text })
            }
            AgentEvent::CommandOutputDelta { item_id, delta, .. } => {
                if let Some(&ix) = self.index.get(item_id) {
                    if let EntryKind::Command { output, .. } = &mut self.entries[ix].kind {
                        output.push_str(delta);
                        self.revision += 1;
                    }
                }
            }
            AgentEvent::PlanUpdated { explanation, steps, .. } => {
                self.plan = steps.clone();
                self.plan_explanation = explanation.clone();
                self.revision += 1;
            }
            AgentEvent::DiffUpdated { diff, .. } => {
                self.diff = Some(diff.clone());
                self.revision += 1;
            }
            AgentEvent::TokenUsage { usage, .. } => {
                self.usage = Some(*usage);
                self.revision += 1;
            }
            AgentEvent::ApprovalRequested(PendingApproval {
                request_id,
                turn_id,
                item_id,
                kind,
                ..
            }) => {
                let entry = Entry {
                    id: format!("approval-{request_id}"),
                    turn_id: Some(turn_id.clone()),
                    kind: EntryKind::Approval {
                        request_id: request_id.clone(),
                        kind: kind.clone(),
                        state: ApprovalState::Pending,
                    },
                    streaming: false,
                };
                // Place the card right after the item it is about.
                match self.index.get(item_id).copied() {
                    Some(ix) => self.insert_entry(ix + 1, entry),
                    None => self.push_entry(entry),
                }
            }
            AgentEvent::ApprovalResolved { request_id, .. } => {
                self.set_approval_state(request_id, ApprovalState::Expired)
            }
            AgentEvent::ToolCallRequested(call) => {
                let id = format!("toolcall-{}", call.params.call_id);
                if !self.index.contains_key(&id) {
                    self.push_entry(Entry {
                        id,
                        turn_id: Some(call.params.turn_id.clone()),
                        kind: EntryKind::Tool {
                            label: tool_label(call.params.namespace.as_deref(), &call.params.tool),
                            detail: Some(call.params.arguments.to_string()),
                            success: None,
                        },
                        streaming: true,
                    });
                }
            }
            AgentEvent::Error { message, will_retry, turn_id, .. } => {
                let message = if *will_retry {
                    format!("{message} (retrying)")
                } else {
                    message.clone()
                };
                self.next_local += 1;
                self.push_entry(Entry {
                    id: format!("error-event-{}", self.next_local),
                    turn_id: Some(turn_id.clone()),
                    kind: EntryKind::Error { message },
                    streaming: false,
                });
            }
            AgentEvent::Warning { message, .. } => self.push_notice(message.clone()),
            AgentEvent::Disconnected { reason } => {
                self.active_turn = None;
                for entry in &mut self.entries {
                    entry.streaming = false;
                }
                self.push_error(reason.clone());
            }
            AgentEvent::ThreadStarted(_)
            | AgentEvent::ThreadNameUpdated { .. }
            | AgentEvent::LoginCompleted { .. }
            | AgentEvent::AccountChanged
            | AgentEvent::Unhandled { .. } => {}
        }
        self.revision != before
    }

    fn push_entry(&mut self, entry: Entry) {
        self.index.insert(entry.id.clone(), self.entries.len());
        self.entries.push(entry);
        self.revision += 1;
    }

    fn insert_entry(&mut self, at: usize, entry: Entry) {
        self.entries.insert(at, entry);
        self.reindex();
        self.revision += 1;
    }

    fn reindex(&mut self) {
        self.index = self
            .entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| (entry.id.clone(), ix))
            .collect();
    }

    fn append_text(&mut self, item_id: &str, delta: &str, make: impl FnOnce(String) -> EntryKind) {
        match self.index.get(item_id).copied() {
            Some(ix) => match &mut self.entries[ix].kind {
                EntryKind::Agent { text } | EntryKind::Reasoning { text } | EntryKind::Plan { text } => {
                    text.push_str(delta);
                    self.revision += 1;
                }
                _ => {}
            },
            None => self.push_entry(Entry {
                id: item_id.to_string(),
                turn_id: self.active_turn.clone(),
                kind: make(delta.to_string()),
                streaming: true,
            }),
        }
    }

    fn upsert_item(&mut self, turn_id: &str, item: &ThreadItem, streaming: bool) {
        let Some(id) = item.id() else { return };
        let Some(kind) = entry_kind(item) else { return };

        // The server echoes the user's message; adopt the optimistic local
        // entry instead of showing the prompt twice.
        if let (ThreadItem::UserMessage { .. }, EntryKind::User { text }) = (item, &kind) {
            if let Some(ix) = self.entries.iter().rposition(|e| {
                e.id.starts_with("local-") && matches!(&e.kind, EntryKind::User { text: t } if t == text)
            }) {
                self.entries[ix].id = id.to_string();
                self.entries[ix].turn_id = Some(turn_id.to_string());
                self.reindex();
                self.revision += 1;
                return;
            }
        }

        match self.index.get(id).copied() {
            Some(ix) => {
                let entry = &mut self.entries[ix];
                // Keep streamed text if the started item arrives empty.
                let keep_streamed = matches!(
                    (&entry.kind, &kind),
                    (EntryKind::Agent { text: old }, EntryKind::Agent { text: new }) if new.is_empty() && !old.is_empty()
                );
                // A completed command item may omit output already streamed.
                let kind = match (&entry.kind, kind) {
                    (
                        EntryKind::Command { output: streamed, .. },
                        EntryKind::Command { command, cwd, output, status, exit_code, duration_ms },
                    ) if output.is_empty() => EntryKind::Command {
                        command,
                        cwd,
                        output: streamed.clone(),
                        status,
                        exit_code,
                        duration_ms,
                    },
                    (_, kind) => kind,
                };
                if !keep_streamed {
                    entry.kind = kind;
                }
                entry.streaming = streaming;
                self.revision += 1;
            }
            None => self.push_entry(Entry {
                id: id.to_string(),
                turn_id: Some(turn_id.to_string()),
                kind,
                streaming,
            }),
        }

        // A tool call that came in as a request is the same thing as this item.
        if let ThreadItem::DynamicToolCall { success, .. } = item {
            for entry in &mut self.entries {
                if let EntryKind::Tool { success: s, .. } = &mut entry.kind {
                    if entry.streaming && entry.turn_id.as_deref() == Some(turn_id) && !streaming {
                        *s = *success;
                        entry.streaming = false;
                    }
                }
            }
        }
    }
}

fn tool_label(namespace: Option<&str>, tool: &str) -> String {
    match namespace {
        Some(ns) => format!("{ns}.{tool}"),
        None => tool.to_string(),
    }
}

fn entry_kind(item: &ThreadItem) -> Option<EntryKind> {
    Some(match item {
        ThreadItem::UserMessage { content, .. } => EntryKind::User {
            text: content
                .iter()
                .filter_map(|input| match input {
                    UserInput::Text { text, .. } => Some(text.clone()),
                    UserInput::LocalImage { path } => Some(format!("[image: {path}]")),
                    UserInput::Image { url } => Some(format!("[image: {url}]")),
                    UserInput::Mention { name, .. } => Some(format!("@{name}")),
                    UserInput::Unknown => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        ThreadItem::AgentMessage { text, .. } => EntryKind::Agent { text: text.clone() },
        ThreadItem::Plan { text, .. } => EntryKind::Plan { text: text.clone() },
        ThreadItem::Reasoning { summary, content, .. } => EntryKind::Reasoning {
            text: if summary.is_empty() {
                content.join("\n\n")
            } else {
                summary.join("\n\n")
            },
        },
        ThreadItem::CommandExecution {
            command,
            cwd,
            status,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } => EntryKind::Command {
            command: command.clone(),
            cwd: cwd.clone(),
            output: aggregated_output.clone().unwrap_or_default(),
            status: *status,
            exit_code: *exit_code,
            duration_ms: *duration_ms,
        },
        ThreadItem::FileChange { changes, status, .. } => EntryKind::FileChange {
            files: changes
                .iter()
                .map(|change| ChangedFile {
                    path: change.path.clone(),
                    change: match &change.kind {
                        PatchChangeKind::Add => "added".into(),
                        PatchChangeKind::Delete => "deleted".into(),
                        PatchChangeKind::Update { move_path: Some(to) } => format!("renamed → {to}"),
                        PatchChangeKind::Update { move_path: None } | PatchChangeKind::Unknown => {
                            "modified".into()
                        }
                    },
                    diff: change.diff.clone(),
                })
                .collect(),
            status: *status,
        },
        ThreadItem::McpToolCall { server, tool, error, .. } => EntryKind::Tool {
            label: format!("{server}.{tool}"),
            detail: error.as_ref().map(|e| e.to_string()),
            success: Some(error.is_none()),
        },
        ThreadItem::DynamicToolCall {
            namespace,
            tool,
            arguments,
            success,
            ..
        } => EntryKind::Tool {
            label: tool_label(namespace.as_deref(), tool),
            detail: Some(arguments.to_string()),
            success: *success,
        },
        ThreadItem::WebSearch { query, .. } => EntryKind::Tool {
            label: "web search".into(),
            detail: query.clone(),
            success: None,
        },
        ThreadItem::ContextCompaction { .. } => EntryKind::Notice {
            text: "Context compacted".into(),
        },
        ThreadItem::Other => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str, status: TurnStatus) -> Turn {
        Turn {
            id: id.into(),
            status,
            ..Default::default()
        }
    }

    fn started(item: ThreadItem) -> AgentEvent {
        AgentEvent::ItemStarted { thread_id: "t".into(), turn_id: "u".into(), item }
    }

    fn completed(item: ThreadItem) -> AgentEvent {
        AgentEvent::ItemCompleted { thread_id: "t".into(), turn_id: "u".into(), item }
    }

    #[test]
    fn a_turn_streams_and_completes() {
        let mut t = Transcript::new();
        t.push_local_user("fix the login page");
        assert!(t.apply(&AgentEvent::TurnStarted { thread_id: "t".into(), turn: turn("u", TurnStatus::InProgress) }));
        assert_eq!(t.active_turn(), Some("u"));

        // The server echo replaces the optimistic entry.
        t.apply(&started(ThreadItem::UserMessage {
            id: "item-user".into(),
            content: vec![UserInput::text("fix the login page")],
        }));
        assert_eq!(t.entries().len(), 1);
        assert_eq!(t.entries()[0].id, "item-user");

        t.apply(&started(ThreadItem::AgentMessage { id: "m".into(), text: String::new() }));
        for delta in ["Look", "ing at ", "auth.rs"] {
            t.apply(&AgentEvent::AgentMessageDelta { thread_id: "t".into(), item_id: "m".into(), delta: delta.into() });
        }
        assert_eq!(t.entries()[1].kind, EntryKind::Agent { text: "Looking at auth.rs".into() });
        assert!(t.entries()[1].streaming);

        t.apply(&completed(ThreadItem::AgentMessage { id: "m".into(), text: "Looking at auth.rs".into() }));
        assert!(!t.entries()[1].streaming);

        t.apply(&AgentEvent::TurnCompleted { thread_id: "t".into(), turn: turn("u", TurnStatus::Completed) });
        assert!(!t.is_running());
        assert_eq!(t.last_status(), Some(TurnStatus::Completed));
    }

    #[test]
    fn command_output_streams_and_survives_an_empty_completion() {
        let mut t = Transcript::new();
        let command = |status, output: Option<&str>| ThreadItem::CommandExecution {
            id: "c".into(),
            command: "cargo test".into(),
            cwd: "/w".into(),
            status,
            aggregated_output: output.map(str::to_string),
            exit_code: None,
            duration_ms: None,
        };
        t.apply(&started(command(ItemStatus::InProgress, None)));
        t.apply(&AgentEvent::CommandOutputDelta { thread_id: "t".into(), item_id: "c".into(), delta: "running 3 tests\n".into() });
        t.apply(&completed(command(ItemStatus::Completed, None)));
        let EntryKind::Command { output, status, .. } = &t.entries()[0].kind else { panic!() };
        assert_eq!(output, "running 3 tests\n");
        assert_eq!(*status, ItemStatus::Completed);
    }

    #[test]
    fn approvals_sit_under_their_item_and_expire_with_the_turn() {
        let mut t = Transcript::new();
        t.apply(&AgentEvent::TurnStarted { thread_id: "t".into(), turn: turn("u", TurnStatus::InProgress) });
        t.apply(&started(ThreadItem::CommandExecution {
            id: "c".into(), command: "rm -rf dist".into(), cwd: "/w".into(),
            status: ItemStatus::InProgress, aggregated_output: None, exit_code: None, duration_ms: None,
        }));
        t.apply(&started(ThreadItem::AgentMessage { id: "m".into(), text: "hi".into() }));
        t.apply(&AgentEvent::ApprovalRequested(PendingApproval {
            request_id: RequestId::Int(1),
            thread_id: "t".into(),
            turn_id: "u".into(),
            item_id: "c".into(),
            kind: ApprovalKind::Command { command: Some("rm -rf dist".into()), cwd: None, reason: None },
        }));
        assert!(matches!(t.entries()[1].kind, EntryKind::Approval { .. }), "{:?}", t.entries());
        assert_eq!(t.pending_approvals().count(), 1);

        t.apply(&AgentEvent::TurnCompleted { thread_id: "t".into(), turn: turn("u", TurnStatus::Interrupted) });
        assert_eq!(t.pending_approvals().count(), 0);
        assert!(matches!(
            t.entries()[1].kind,
            EntryKind::Approval { state: ApprovalState::Expired, .. }
        ));
        assert!(matches!(t.entries().last().unwrap().kind, EntryKind::Notice { .. }));
    }

    #[test]
    fn user_decisions_are_recorded() {
        let mut t = Transcript::new();
        t.apply(&AgentEvent::ApprovalRequested(PendingApproval {
            request_id: RequestId::Str("r".into()),
            thread_id: "t".into(),
            turn_id: "u".into(),
            item_id: "missing".into(),
            kind: ApprovalKind::FileChange { reason: None, grant_root: None },
        }));
        t.set_approval_state(&RequestId::Str("r".into()), ApprovalState::AcceptedForSession);
        assert!(matches!(
            t.entries()[0].kind,
            EntryKind::Approval { state: ApprovalState::AcceptedForSession, .. }
        ));
        // A later "resolved" from the server does not overwrite the decision.
        t.apply(&AgentEvent::ApprovalResolved { thread_id: "t".into(), request_id: RequestId::Str("r".into()) });
        assert!(matches!(
            t.entries()[0].kind,
            EntryKind::Approval { state: ApprovalState::AcceptedForSession, .. }
        ));
    }

    #[test]
    fn resumed_threads_rebuild_the_same_entries() {
        let turns = vec![Turn {
            id: "u1".into(),
            status: TurnStatus::Failed,
            error: Some(TurnError { message: "rate limited".into(), ..Default::default() }),
            items: vec![
                ThreadItem::UserMessage { id: "a".into(), content: vec![UserInput::text("hello")] },
                ThreadItem::FileChange {
                    id: "f".into(),
                    status: ItemStatus::Completed,
                    changes: vec![FileUpdateChange {
                        path: "src/a.rs".into(),
                        kind: PatchChangeKind::Update { move_path: Some("src/b.rs".into()) },
                        diff: "@@".into(),
                    }],
                },
                ThreadItem::Other,
            ],
            ..Default::default()
        }];
        let t = Transcript::from_turns(&turns);
        assert_eq!(t.entries().len(), 3);
        let EntryKind::FileChange { files, .. } = &t.entries()[1].kind else { panic!() };
        assert_eq!(files[0].change, "renamed → src/b.rs");
        assert!(matches!(t.entries()[2].kind, EntryKind::Error { .. }));
        assert_eq!(t.last_status(), Some(TurnStatus::Failed));
    }

    #[test]
    fn disconnect_stops_streaming() {
        let mut t = Transcript::new();
        t.apply(&AgentEvent::TurnStarted { thread_id: "t".into(), turn: turn("u", TurnStatus::InProgress) });
        t.apply(&AgentEvent::AgentMessageDelta { thread_id: "t".into(), item_id: "m".into(), delta: "x".into() });
        t.apply(&AgentEvent::Disconnected { reason: "codex exited".into() });
        assert!(!t.is_running());
        assert!(t.entries().iter().all(|e| !e.streaming));
    }
}

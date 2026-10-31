//! Claude Code's stream-json protocol, mapped onto Hangar's events.
//!
//! `claude -p --input-format stream-json --output-format stream-json` writes
//! one JSON object per line: `system`, `assistant`, `user` (tool results),
//! `stream_event` (token deltas), `result` (end of turn), and
//! `control_request`s that need an answer (`can_use_tool` permission prompts,
//! `mcp_message` calls to in-process MCP servers). [`Mapper`] is a pure
//! reducer over those lines: it returns the [`AgentEvent`]s to publish and
//! the lines to write back, and does no I/O, so the whole mapping is unit
//! tested without a process.
//!
//! Message shapes were checked against Claude Code 2.1.131. Like the Codex
//! protocol module, everything is read leniently: unknown message types and
//! fields are ignored rather than failing the line.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{json, Value};

use crate::{
    events::{AgentEvent, ApprovalKind, PendingApproval, ToolCall},
    protocol::*,
    rpc::RequestId,
};

/// Something the mapper wants done.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Event(AgentEvent),
    /// A line to write to Claude Code's stdin.
    Write(Value),
    /// Claude Code answered one of our control requests.
    ControlResponse {
        request_id: String,
        result: Result<Value, String>,
    },
}

/// An in-process MCP server Claude Code can call (`type: "sdk"` in
/// `--mcp-config`). Hangar's dynamic tools (the IDE browser) are offered to
/// Claude this way, so they go through the same tool policy as for Codex.
#[derive(Clone, Debug, PartialEq)]
pub struct McpServerSpec {
    pub name: String,
    pub tools: Vec<McpToolSpec>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl McpServerSpec {
    /// One server per dynamic tool namespace; loose functions share a
    /// `hangar` server.
    pub fn from_dynamic(specs: &[DynamicToolSpec]) -> Vec<McpServerSpec> {
        let mut servers: Vec<McpServerSpec> = Vec::new();
        let mut loose = Vec::new();
        for spec in specs {
            match spec {
                DynamicToolSpec::Namespace { name, tools, .. } => servers.push(McpServerSpec {
                    name: name.clone(),
                    tools: tools.iter().filter_map(McpToolSpec::from_dynamic).collect(),
                }),
                function => loose.extend(McpToolSpec::from_dynamic(function)),
            }
        }
        if !loose.is_empty() {
            servers.push(McpServerSpec { name: "hangar".into(), tools: loose });
        }
        servers
    }
}

impl McpToolSpec {
    fn from_dynamic(spec: &DynamicToolSpec) -> Option<McpToolSpec> {
        match spec {
            DynamicToolSpec::Function { name, description, input_schema } => Some(McpToolSpec {
                name: name.clone(),
                description: description.clone(),
                input_schema: input_schema.clone(),
            }),
            DynamicToolSpec::Namespace { .. } => None,
        }
    }
}

/// Splits `mcp__server__tool`.
pub fn split_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let split = rest.find("__")?;
    Some((&rest[..split], &rest[split + 2..]))
}

struct ActiveTurn {
    id: String,
    interrupted: bool,
    auth_error: bool,
}

/// A tool Claude started and has not reported a result for.
enum PendingTool {
    Command { command: String },
    Files { changes: Vec<FileUpdateChange> },
    Generic { tool: String, detail: Option<String> },
    Dynamic { namespace: String, tool: String, arguments: Value },
    /// Tracked only so its result is not mistaken for something else.
    Silent,
}

struct ApprovalContext {
    tool_name: String,
    input: Value,
    suggestions: Vec<Value>,
    tool_use_id: Option<String>,
}

struct McpCall {
    id: Value,
}

pub struct Mapper {
    thread_id: String,
    cwd: String,
    /// Makes turn ids unique across backends and restarts.
    nonce: String,
    next_turn: u64,
    turn: Option<ActiveTurn>,
    tools: HashMap<String, PendingTool>,
    /// Tool uses the user declined, so their results show as declined.
    denied: HashSet<String>,
    /// Content blocks seen per assistant message id, so a block's item id
    /// matches the index its stream deltas used.
    blocks_seen: HashMap<String, usize>,
    stream_message: Option<String>,
    approvals: HashMap<String, ApprovalContext>,
    mcp_servers: Vec<McpServerSpec>,
    mcp_calls: HashMap<String, McpCall>,
    /// Dynamic tool uses waiting for their `tools/call`, oldest first.
    dynamic_waiting: VecDeque<(String, String)>,
    /// tool_use id → the call id the transcript knows the call by.
    dynamic_calls: HashMap<String, String>,
    model: Option<String>,
    context_tokens: Option<i64>,
    auth_failed: bool,
}

impl Mapper {
    pub fn new(thread_id: impl Into<String>, cwd: impl Into<String>, nonce: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            cwd: cwd.into(),
            nonce: nonce.into(),
            next_turn: 0,
            turn: None,
            tools: HashMap::new(),
            denied: HashSet::new(),
            blocks_seen: HashMap::new(),
            stream_message: None,
            approvals: HashMap::new(),
            mcp_servers: Vec::new(),
            mcp_calls: HashMap::new(),
            dynamic_waiting: VecDeque::new(),
            dynamic_calls: HashMap::new(),
            model: None,
            context_tokens: None,
            auth_failed: false,
        }
    }

    pub fn set_mcp_servers(&mut self, servers: Vec<McpServerSpec>) {
        self.mcp_servers = servers;
    }

    pub fn mcp_servers(&self) -> &[McpServerSpec] {
        &self.mcp_servers
    }

    /// The model Claude Code last reported using.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn is_running(&self) -> bool {
        self.turn.is_some()
    }

    pub fn active_turn(&self) -> Option<&str> {
        self.turn.as_ref().map(|t| t.id.as_str())
    }

    /// Whether a turn failed because Claude Code is not (or no longer)
    /// signed in. Cleared by [`Self::take_auth_failure`].
    pub fn take_auth_failure(&mut self) -> bool {
        std::mem::take(&mut self.auth_failed)
    }

    pub fn has_approval(&self, request_id: &str) -> bool {
        self.approvals.contains_key(request_id)
    }

    pub fn has_tool_call(&self, request_id: &str) -> bool {
        self.mcp_calls.contains_key(request_id)
    }

    /// Starts a turn for a prompt the user sent. Returns the turn and the
    /// events that announce it (the prompt is echoed so the optimistic entry
    /// in the transcript is adopted, as with Codex).
    pub fn begin_turn(&mut self, text: &str) -> (Turn, Vec<AgentEvent>) {
        self.next_turn += 1;
        let id = format!("{}-{}", self.nonce, self.next_turn);
        self.turn = Some(ActiveTurn { id: id.clone(), interrupted: false, auth_error: false });
        let turn = Turn { id: id.clone(), status: TurnStatus::InProgress, ..Default::default() };
        let events = vec![
            AgentEvent::TurnStarted { thread_id: self.thread_id.clone(), turn: turn.clone() },
            AgentEvent::ItemCompleted {
                thread_id: self.thread_id.clone(),
                turn_id: id.clone(),
                item: ThreadItem::UserMessage { id: format!("user-{id}"), content: vec![UserInput::text(text)] },
            },
        ];
        (turn, events)
    }

    /// The user asked to stop; the result that follows ends the turn as
    /// interrupted rather than failed.
    pub fn interrupt(&mut self) {
        if let Some(turn) = &mut self.turn {
            turn.interrupted = true;
        }
    }

    pub fn handle_line(&mut self, line: &str) -> Vec<Output> {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => self.handle_value(&value),
            Err(err) => {
                tracing::warn!(target: "claude", "unparseable line from Claude Code: {err}");
                Vec::new()
            }
        }
    }

    pub fn handle_value(&mut self, message: &Value) -> Vec<Output> {
        let mut out = Vec::new();
        match str_at(message, "type") {
            Some("stream_event") => self.on_stream_event(message, &mut out),
            Some("assistant") => self.on_assistant(message, &mut out),
            Some("user") => self.on_user(message, &mut out),
            Some("result") => self.on_result(message, &mut out),
            Some("system") => self.on_system(message, &mut out),
            Some("control_request") => self.on_control_request(message, &mut out),
            Some("control_response") => {
                let response = &message["response"];
                let request_id = str_at(response, "request_id").unwrap_or_default().to_string();
                let result = if str_at(response, "subtype") == Some("success") {
                    Ok(response.get("response").cloned().unwrap_or(Value::Null))
                } else {
                    Err(str_at(response, "error").unwrap_or("Claude Code refused the request").to_string())
                };
                out.push(Output::ControlResponse { request_id, result });
            }
            Some("control_cancel_request") => {
                let request_id = str_at(message, "request_id").unwrap_or_default().to_string();
                if self.approvals.remove(&request_id).is_some() {
                    out.push(Output::Event(AgentEvent::ApprovalResolved {
                        thread_id: self.thread_id.clone(),
                        request_id: RequestId::Str(request_id.clone()),
                    }));
                }
                self.mcp_calls.remove(&request_id);
            }
            _ => {}
        }
        out
    }

    // --- streaming -------------------------------------------------------

    fn on_stream_event(&mut self, message: &Value, out: &mut Vec<Output>) {
        if is_subagent(message) {
            return;
        }
        let event = &message["event"];
        match str_at(event, "type") {
            Some("message_start") => {
                self.stream_message = str_at(&event["message"], "id").map(str::to_string);
            }
            Some("content_block_delta") => {
                let (Some(message_id), Some(index)) = (self.stream_message.clone(), event["index"].as_u64()) else {
                    return;
                };
                let item_id = format!("{message_id}:{index}");
                let delta = &event["delta"];
                let thread_id = self.thread_id.clone();
                match str_at(delta, "type") {
                    Some("text_delta") => {
                        if let Some(text) = str_at(delta, "text").filter(|t| !t.is_empty()) {
                            out.push(Output::Event(AgentEvent::AgentMessageDelta {
                                thread_id,
                                item_id,
                                delta: text.to_string(),
                            }));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = str_at(delta, "thinking").filter(|t| !t.is_empty()) {
                            out.push(Output::Event(AgentEvent::ReasoningDelta {
                                thread_id,
                                item_id,
                                delta: text.to_string(),
                            }));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // --- assistant messages ------------------------------------------------

    fn on_assistant(&mut self, message: &Value, out: &mut Vec<Output>) {
        if is_subagent(message) {
            return;
        }
        if str_at(message, "error") == Some("authentication_failed") {
            self.mark_auth_error();
        }
        let body = &message["message"];
        let message_id = str_at(body, "id")
            .or_else(|| str_at(message, "uuid"))
            .unwrap_or("message")
            .to_string();
        let synthetic = str_at(body, "model") == Some("<synthetic>");
        if let Some(model) = str_at(body, "model").filter(|_| !synthetic) {
            self.model = Some(model.to_string());
        }
        if !synthetic {
            if let Some(usage) = body.get("usage").filter(|u| u.is_object()) {
                let context = int_at(usage, "input_tokens")
                    + int_at(usage, "cache_creation_input_tokens")
                    + int_at(usage, "cache_read_input_tokens")
                    + int_at(usage, "output_tokens");
                if context > 0 {
                    self.context_tokens = Some(context);
                }
            }
        }
        let Some(blocks) = body["content"].as_array() else { return };
        for block in blocks {
            let seen = self.blocks_seen.entry(message_id.clone()).or_insert(0);
            let item_id = format!("{message_id}:{seen}");
            *seen += 1;
            match str_at(block, "type") {
                Some("text") => {
                    let text = str_at(block, "text").unwrap_or_default();
                    if !text.trim().is_empty() {
                        self.item(out, false, ThreadItem::AgentMessage { id: item_id, text: text.to_string() });
                    }
                }
                Some("thinking") => {
                    let text = str_at(block, "thinking").unwrap_or_default();
                    if !text.trim().is_empty() {
                        self.item(
                            out,
                            false,
                            ThreadItem::Reasoning { id: item_id, summary: Vec::new(), content: vec![text.to_string()] },
                        );
                    }
                }
                Some("tool_use") => {
                    let id = str_at(block, "id").unwrap_or(&item_id).to_string();
                    let name = str_at(block, "name").unwrap_or("tool").to_string();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    self.start_tool(id, &name, input, out);
                }
                _ => {}
            }
        }
    }

    fn start_tool(&mut self, id: String, name: &str, input: Value, out: &mut Vec<Output>) {
        match name {
            "Bash" => {
                let command = str_at(&input, "command").unwrap_or_default().to_string();
                self.item(
                    out,
                    true,
                    ThreadItem::CommandExecution {
                        id: id.clone(),
                        command: command.clone(),
                        cwd: self.cwd.clone(),
                        status: ItemStatus::InProgress,
                        aggregated_output: None,
                        exit_code: None,
                        duration_ms: None,
                    },
                );
                self.tools.insert(id, PendingTool::Command { command });
            }
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
                let changes = file_changes(name, &input, &self.cwd);
                self.item(
                    out,
                    true,
                    ThreadItem::FileChange { id: id.clone(), changes: changes.clone(), status: ItemStatus::InProgress },
                );
                self.tools.insert(id, PendingTool::Files { changes });
            }
            "TodoWrite" => {
                let steps = input["todos"]
                    .as_array()
                    .map(|todos| {
                        todos
                            .iter()
                            .map(|todo| PlanStep {
                                step: str_at(todo, "content").unwrap_or_default().to_string(),
                                status: match str_at(todo, "status") {
                                    Some("completed") => PlanStepStatus::Completed,
                                    Some("in_progress") => PlanStepStatus::InProgress,
                                    Some("pending") => PlanStepStatus::Pending,
                                    _ => PlanStepStatus::Unknown,
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Output::Event(AgentEvent::PlanUpdated {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id(),
                    explanation: None,
                    steps,
                }));
                self.tools.insert(id, PendingTool::Silent);
            }
            _ => {
                let dynamic = split_mcp_tool_name(name)
                    .filter(|(server, _)| self.mcp_servers.iter().any(|s| s.name == *server))
                    .map(|(server, tool)| (server.to_string(), tool.to_string()));
                if let Some((namespace, tool)) = dynamic {
                    // The call itself arrives as an MCP `tools/call`, which
                    // the transcript shows; nothing to add yet.
                    self.dynamic_waiting.push_back((name.to_string(), id.clone()));
                    self.tools.insert(id, PendingTool::Dynamic { namespace, tool, arguments: input });
                } else {
                    let detail = summarize_tool(name, &input, &self.cwd);
                    self.item(
                        out,
                        true,
                        ThreadItem::ToolCall { id: id.clone(), tool: name.to_string(), detail: detail.clone(), success: None },
                    );
                    self.tools.insert(id, PendingTool::Generic { tool: name.to_string(), detail });
                }
            }
        }
    }

    // --- tool results --------------------------------------------------------

    fn on_user(&mut self, message: &Value, out: &mut Vec<Output>) {
        if is_subagent(message) {
            return;
        }
        let Some(blocks) = message["message"]["content"].as_array() else { return };
        for block in blocks {
            if str_at(block, "type") != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = str_at(block, "tool_use_id") else { continue };
            let is_error = block["is_error"].as_bool().unwrap_or(false);
            let text = tool_result_text(&block["content"]);
            self.complete_tool(tool_use_id, Some(text), is_error, out);
        }
    }

    fn complete_tool(&mut self, tool_use_id: &str, output: Option<String>, is_error: bool, out: &mut Vec<Output>) {
        let Some(pending) = self.tools.remove(tool_use_id) else { return };
        let declined = self.denied.remove(tool_use_id);
        let status = if declined {
            ItemStatus::Declined
        } else if is_error {
            ItemStatus::Failed
        } else {
            ItemStatus::Completed
        };
        let id = tool_use_id.to_string();
        let item = match pending {
            PendingTool::Command { command } => ThreadItem::CommandExecution {
                exit_code: match (is_error, output.as_deref()) {
                    (false, Some(_)) => Some(0),
                    (true, Some(text)) => parse_exit_code(text),
                    _ => None,
                },
                id,
                command,
                cwd: self.cwd.clone(),
                status,
                aggregated_output: output,
                duration_ms: None,
            },
            PendingTool::Files { changes } => ThreadItem::FileChange { id, changes, status },
            PendingTool::Generic { tool, detail } => ThreadItem::ToolCall {
                id,
                tool,
                detail,
                success: Some(status == ItemStatus::Completed),
            },
            PendingTool::Dynamic { namespace, tool, arguments } => {
                self.dynamic_waiting.retain(|(_, waiting)| waiting != tool_use_id);
                ThreadItem::DynamicToolCall {
                    // Complete the entry the `tools/call` created.
                    id: match self.dynamic_calls.remove(tool_use_id) {
                        Some(call_id) => format!("toolcall-{call_id}"),
                        None => id,
                    },
                    namespace: Some(namespace),
                    tool,
                    arguments,
                    success: Some(status == ItemStatus::Completed),
                }
            }
            PendingTool::Silent => return,
        };
        self.item(out, false, item);
    }

    // --- end of turn ---------------------------------------------------------

    fn on_result(&mut self, message: &Value, out: &mut Vec<Output>) {
        let Some(turn) = self.turn.take() else { return };
        // Whatever is still running did not finish (interrupt, error).
        let unfinished: Vec<String> = self.tools.keys().cloned().collect();
        for id in unfinished {
            self.complete_tool(&id, None, true, out);
        }
        self.dynamic_waiting.clear();
        self.approvals.clear();

        let usage = &message["usage"];
        let cache_write = int_at(usage, "cache_creation_input_tokens");
        let cache_read = int_at(usage, "cache_read_input_tokens");
        let input = int_at(usage, "input_tokens") + cache_write + cache_read;
        let output = int_at(usage, "output_tokens");
        let breakdown = TokenUsageBreakdown {
            total_tokens: input + output,
            input_tokens: input,
            cached_input_tokens: cache_read,
            cache_write_input_tokens: cache_write,
            output_tokens: output,
            reasoning_output_tokens: 0,
        };
        if breakdown.total_tokens > 0 {
            out.push(Output::Event(AgentEvent::TurnUsage {
                thread_id: self.thread_id.clone(),
                turn_id: turn.id.clone(),
                usage: breakdown,
                context_tokens: self.context_tokens,
                model_context_window: context_window(&message["modelUsage"], self.model.as_deref()),
            }));
        }

        let auth_error = turn.auth_error || message["api_error_status"].as_i64() == Some(401);
        let is_error = message["is_error"].as_bool().unwrap_or(false)
            || str_at(message, "subtype").is_some_and(|s| s.starts_with("error"));
        let (status, error) = if turn.interrupted {
            (TurnStatus::Interrupted, None)
        } else if auth_error {
            self.auth_failed = true;
            out.push(Output::Event(AgentEvent::AccountChanged));
            (
                TurnStatus::Failed,
                Some("Claude Code is not signed in, or its sign-in expired. Sign in again from this panel.".to_string()),
            )
        } else if is_error {
            (TurnStatus::Failed, Some(result_error(message)))
        } else {
            (TurnStatus::Completed, None)
        };
        out.push(Output::Event(AgentEvent::TurnCompleted {
            thread_id: self.thread_id.clone(),
            turn: Turn {
                id: turn.id,
                status,
                error: error.map(|message| TurnError { message, ..Default::default() }),
                duration_ms: message["duration_ms"].as_i64(),
                ..Default::default()
            },
        }));
    }

    /// Claude Code went away. Ends the running turn, if any.
    pub fn process_exited(&mut self, reason: &str) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for request_id in self.approvals.drain().map(|(id, _)| id).collect::<Vec<_>>() {
            out.push(Output::Event(AgentEvent::ApprovalResolved {
                thread_id: self.thread_id.clone(),
                request_id: RequestId::Str(request_id),
            }));
        }
        self.mcp_calls.clear();
        if self.turn.is_some() {
            self.on_result(
                &json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "result": reason}),
                &mut out,
            );
        }
        out.into_iter()
            .filter_map(|o| match o {
                Output::Event(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    // --- system ------------------------------------------------------------

    fn on_system(&mut self, message: &Value, out: &mut Vec<Output>) {
        match str_at(message, "subtype") {
            Some("init") => {
                if let Some(model) = str_at(message, "model") {
                    self.model = Some(model.to_string());
                }
            }
            Some("api_retry") => {
                let error = str_at(message, "error").unwrap_or("API error");
                if error == "authentication_failed" {
                    self.mark_auth_error();
                }
                let Some(turn_id) = self.turn.as_ref().map(|t| t.id.clone()) else { return };
                let status = message["error_status"].as_i64().map(|s| format!(" (HTTP {s})")).unwrap_or_default();
                let attempt = message["attempt"].as_i64().unwrap_or(1);
                let max = message["max_retries"].as_i64().unwrap_or(attempt);
                out.push(Output::Event(AgentEvent::Error {
                    thread_id: self.thread_id.clone(),
                    turn_id,
                    message: format!("{}{status}; retrying ({attempt} of {max})", humanize(error)),
                    will_retry: true,
                }));
            }
            Some("compact_boundary") => {
                let id = str_at(message, "uuid").unwrap_or("compaction").to_string();
                self.item(out, false, ThreadItem::ContextCompaction { id });
            }
            _ => {}
        }
    }

    // --- control requests ----------------------------------------------------

    fn on_control_request(&mut self, message: &Value, out: &mut Vec<Output>) {
        let request_id = str_at(message, "request_id").unwrap_or_default().to_string();
        let request = &message["request"];
        match str_at(request, "subtype") {
            Some("can_use_tool") => self.on_permission_request(request_id, request, out),
            Some("mcp_message") => self.on_mcp_message(request_id, request, out),
            other => out.push(Output::Write(control_error(
                &request_id,
                &format!("Hangar does not support the {} control request", other.unwrap_or("unnamed")),
            ))),
        }
    }

    fn on_permission_request(&mut self, request_id: String, request: &Value, out: &mut Vec<Output>) {
        let tool_name = str_at(request, "tool_name").unwrap_or("tool").to_string();
        let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
        let tool_use_id = str_at(request, "tool_use_id").map(str::to_string);
        let reason = str_at(request, "description")
            .or_else(|| str_at(request, "decision_reason"))
            .map(str::to_string);
        let kind = match tool_name.as_str() {
            "Bash" => ApprovalKind::Command {
                command: str_at(&input, "command").map(str::to_string),
                cwd: Some(self.cwd.clone()),
                reason,
            },
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
                let path = summarize_tool(&tool_name, &input, &self.cwd).unwrap_or_default();
                ApprovalKind::FileChange {
                    reason: Some(match reason {
                        Some(reason) => format!("{path}: {reason}"),
                        None => path,
                    }),
                    grant_root: None,
                }
            }
            _ => ApprovalKind::Tool {
                tool: str_at(request, "display_name").unwrap_or(&tool_name).to_string(),
                detail: summarize_tool(&tool_name, &input, &self.cwd),
                reason,
            },
        };
        let suggestions = request["permission_suggestions"].as_array().cloned().unwrap_or_default();
        out.push(Output::Event(AgentEvent::ApprovalRequested(PendingApproval {
            request_id: RequestId::Str(request_id.clone()),
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id(),
            item_id: tool_use_id.clone().unwrap_or_default(),
            kind,
        })));
        self.approvals.insert(request_id, ApprovalContext { tool_name, input, suggestions, tool_use_id });
    }

    /// The user's answer to a permission prompt, as the line to send back.
    /// `None` when the request is unknown (already answered or cancelled).
    pub fn approval_reply(&mut self, request_id: &str, decision: ApprovalDecision) -> Option<Value> {
        let context = self.approvals.remove(request_id)?;
        let response = match decision {
            ApprovalDecision::Accept => json!({"behavior": "allow", "updatedInput": context.input}),
            ApprovalDecision::AcceptForSession => {
                // Claude's own suggestions (a rule for this command, say),
                // kept to this session rather than written to settings files.
                let mut rules: Vec<Value> = context
                    .suggestions
                    .into_iter()
                    .map(|mut suggestion| {
                        if suggestion.get("destination").is_some() {
                            suggestion["destination"] = json!("session");
                        }
                        suggestion
                    })
                    .collect();
                if rules.is_empty() {
                    rules.push(json!({
                        "type": "addRules",
                        "rules": [{"toolName": context.tool_name}],
                        "behavior": "allow",
                        "destination": "session",
                    }));
                }
                json!({"behavior": "allow", "updatedInput": context.input, "updatedPermissions": rules})
            }
            ApprovalDecision::Decline | ApprovalDecision::Cancel => {
                if let Some(id) = &context.tool_use_id {
                    self.denied.insert(id.clone());
                }
                let mut deny = json!({"behavior": "deny", "message": "The user declined this action in Hangar."});
                if decision == ApprovalDecision::Cancel {
                    deny["interrupt"] = json!(true);
                    self.interrupt();
                }
                deny
            }
        };
        Some(control_success(request_id, response))
    }

    fn on_mcp_message(&mut self, request_id: String, request: &Value, out: &mut Vec<Output>) {
        let server = str_at(request, "server_name").unwrap_or_default().to_string();
        let message = &request["message"];
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        let method = str_at(message, "method").unwrap_or_default();
        let Some(spec) = self.mcp_servers.iter().find(|s| s.name == server).cloned() else {
            out.push(Output::Write(mcp_reply(&request_id, id, Err((-32601, format!("unknown MCP server {server}"))))));
            return;
        };
        let Some(id) = id else {
            // Notifications (`notifications/initialized`) need only an ack.
            out.push(Output::Write(control_success(&request_id, json!({"mcp_response": {"jsonrpc": "2.0", "result": {}}}))));
            return;
        };
        let reply = match method {
            "initialize" => Ok(json!({
                "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": spec.name, "version": cockpit_common::APP_VERSION},
            })),
            "tools/list" => Ok(json!({
                "tools": spec.tools.iter().map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                })).collect::<Vec<_>>(),
            })),
            "ping" => Ok(json!({})),
            "tools/call" => {
                let tool = str_at(&message["params"], "name").unwrap_or_default().to_string();
                let arguments = message["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
                let full_name = format!("mcp__{server}__{tool}");
                if let Some(ix) = self.dynamic_waiting.iter().position(|(name, _)| *name == full_name) {
                    if let Some((_, tool_use_id)) = self.dynamic_waiting.remove(ix) {
                        self.dynamic_calls.insert(tool_use_id, request_id.clone());
                    }
                }
                self.mcp_calls.insert(request_id.clone(), McpCall { id });
                out.push(Output::Event(AgentEvent::ToolCallRequested(ToolCall {
                    request_id: RequestId::Str(request_id.clone()),
                    params: DynamicToolCallParams {
                        thread_id: self.thread_id.clone(),
                        turn_id: self.turn_id(),
                        call_id: request_id,
                        namespace: Some(server),
                        tool,
                        arguments,
                    },
                })));
                return;
            }
            other => Err((-32601, format!("{other} is not supported"))),
        };
        out.push(Output::Write(mcp_reply(&request_id, Some(id), reply)));
    }

    /// Hangar's answer to a dynamic tool call, as the line to send back.
    pub fn tool_reply(&mut self, request_id: &str, response: &DynamicToolCallResponse) -> Option<Value> {
        let call = self.mcp_calls.remove(request_id)?;
        let content: Vec<Value> = response
            .content_items
            .iter()
            .map(|item| match item {
                ToolOutputItem::InputText { text } => json!({"type": "text", "text": text}),
                ToolOutputItem::InputImage { image_url } => match parse_data_url(image_url) {
                    Some((mime, data)) => json!({"type": "image", "data": data, "mimeType": mime}),
                    None => json!({"type": "text", "text": format!("Image: {image_url}")}),
                },
            })
            .collect();
        Some(mcp_reply(
            request_id,
            Some(call.id),
            Ok(json!({"content": content, "isError": !response.success})),
        ))
    }

    // --- helpers -------------------------------------------------------------

    fn turn_id(&self) -> String {
        self.turn.as_ref().map(|t| t.id.clone()).unwrap_or_default()
    }

    fn mark_auth_error(&mut self) {
        match &mut self.turn {
            Some(turn) => turn.auth_error = true,
            None => self.auth_failed = true,
        }
    }

    fn item(&self, out: &mut Vec<Output>, started: bool, item: ThreadItem) {
        let (thread_id, turn_id) = (self.thread_id.clone(), self.turn_id());
        out.push(Output::Event(if started {
            AgentEvent::ItemStarted { thread_id, turn_id, item }
        } else {
            AgentEvent::ItemCompleted { thread_id, turn_id, item }
        }));
    }
}

// ---------------------------------------------------------------------------
// Control messages
// ---------------------------------------------------------------------------

pub fn control_request(request_id: &str, request: Value) -> Value {
    json!({"type": "control_request", "request_id": request_id, "request": request})
}

pub fn control_success(request_id: &str, response: Value) -> Value {
    json!({"type": "control_response", "response": {"subtype": "success", "request_id": request_id, "response": response}})
}

pub fn control_error(request_id: &str, error: &str) -> Value {
    json!({"type": "control_response", "response": {"subtype": "error", "request_id": request_id, "error": error}})
}

/// A user prompt in stream-json input form.
pub fn user_message(text: &str) -> Value {
    json!({
        "type": "user",
        "message": {"role": "user", "content": text},
        "parent_tool_use_id": null,
        "session_id": "default",
    })
}

fn mcp_reply(request_id: &str, id: Option<Value>, result: Result<Value, (i64, String)>) -> Value {
    let mut message = json!({"jsonrpc": "2.0", "id": id.unwrap_or(Value::Null)});
    match result {
        Ok(result) => message["result"] = result,
        Err((code, text)) => message["error"] = json!({"code": code, "message": text}),
    }
    control_success(request_id, json!({"mcp_response": message}))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn int_at(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Messages from a subagent (`Task`) carry the tool use that started it.
fn is_subagent(message: &Value) -> bool {
    message.get("parent_tool_use_id").is_some_and(|v| !v.is_null())
        || message.get("isSidechain").and_then(Value::as_bool).unwrap_or(false)
}

fn humanize(error: &str) -> String {
    let mut text = error.replace('_', " ");
    if let Some(first) = text.get(..1) {
        text = first.to_uppercase() + &text[1..];
    }
    text
}

fn result_error(message: &Value) -> String {
    if let Some(text) = str_at(message, "result").filter(|t| !t.trim().is_empty()) {
        return text.to_string();
    }
    if let Some(errors) = message["errors"].as_array().filter(|e| !e.is_empty()) {
        return errors.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
    }
    match str_at(message, "subtype") {
        Some("error_max_turns") => "Claude stopped: it reached the maximum number of turns.".into(),
        Some(other) => format!("Claude Code ended the turn with {}.", humanize(other).to_lowercase()),
        None => "Claude Code ended the turn with an error.".into(),
    }
}

fn context_window(model_usage: &Value, model: Option<&str>) -> Option<i64> {
    let models = model_usage.as_object()?;
    let entry = model
        .and_then(|m| models.get(m))
        .or_else(|| models.values().max_by_key(|u| int_at(u, "contextWindow")))?;
    entry["contextWindow"].as_i64().filter(|w| *w > 0)
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match str_at(part, "type") {
                Some("text") => str_at(part, "text").map(str::to_string),
                Some("image") => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Claude Code reports a failing command as "Exit code N" before its output.
fn parse_exit_code(text: &str) -> Option<i32> {
    let rest = text.trim_start().strip_prefix("Exit code ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse().ok()
}

fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    Some((mime, data))
}

fn relative<'a>(path: &'a str, cwd: &str) -> &'a str {
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        return path;
    }
    path.strip_prefix(cwd).and_then(|rest| rest.strip_prefix('/')).unwrap_or(path)
}

/// A one-line description of a tool call for the transcript.
pub fn summarize_tool(name: &str, input: &Value, cwd: &str) -> Option<String> {
    let field = |key: &str| str_at(input, key).filter(|s| !s.is_empty());
    let summary = match name {
        "Read" | "Write" | "Edit" | "MultiEdit" => field("file_path").map(|p| relative(p, cwd).to_string()),
        "NotebookEdit" => field("notebook_path").map(|p| relative(p, cwd).to_string()),
        "Glob" | "Grep" => field("pattern").map(|pattern| match field("path") {
            Some(path) => format!("{pattern} in {}", relative(path, cwd)),
            None => pattern.to_string(),
        }),
        "WebFetch" => field("url").map(str::to_string),
        "WebSearch" => field("query").map(str::to_string),
        "Task" | "Agent" => field("description").map(str::to_string),
        "Skill" => field("skill").or_else(|| field("command")).map(str::to_string),
        "LS" => field("path").map(|p| relative(p, cwd).to_string()),
        "Bash" => field("command").map(str::to_string),
        _ => match input {
            Value::Object(map) if map.is_empty() => None,
            Value::Null => None,
            other => Some(other.to_string()),
        },
    };
    summary.map(|s| cockpit_common::truncate_chars(&s, 160))
}

fn file_changes(name: &str, input: &Value, cwd: &str) -> Vec<FileUpdateChange> {
    let path_key = if name == "NotebookEdit" { "notebook_path" } else { "file_path" };
    let Some(full) = str_at(input, path_key) else { return Vec::new() };
    let path = relative(full, cwd).to_string();
    let (kind, diff) = match name {
        "Write" => {
            let exists = std::path::Path::new(full).exists();
            let old = if exists { std::fs::read_to_string(full).unwrap_or_default() } else { String::new() };
            (
                if exists { PatchChangeKind::Update { move_path: None } } else { PatchChangeKind::Add },
                unified_diff(&path, &old, str_at(input, "content").unwrap_or_default()),
            )
        }
        "MultiEdit" => {
            let diff = input["edits"]
                .as_array()
                .map(|edits| {
                    edits
                        .iter()
                        .map(|edit| {
                            unified_diff(
                                &path,
                                str_at(edit, "old_string").unwrap_or_default(),
                                str_at(edit, "new_string").unwrap_or_default(),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            (PatchChangeKind::Update { move_path: None }, diff)
        }
        "NotebookEdit" => (
            PatchChangeKind::Update { move_path: None },
            unified_diff(&path, "", str_at(input, "new_source").unwrap_or_default()),
        ),
        _ => (
            PatchChangeKind::Update { move_path: None },
            unified_diff(
                &path,
                str_at(input, "old_string").unwrap_or_default(),
                str_at(input, "new_string").unwrap_or_default(),
            ),
        ),
    };
    vec![FileUpdateChange { path, kind, diff }]
}

/// A small unified diff of `old` → `new`: shared leading and trailing lines
/// become context, the middle is removed and added.
fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let prefix = old_lines.iter().zip(&new_lines).take_while(|(a, b)| a == b).count();
    let max_suffix = old_lines.len().min(new_lines.len()) - prefix;
    let suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let context = 3;
    let mut diff = format!("--- a/{path}\n+++ b/{path}\n@@ @@\n");
    for line in &old_lines[prefix.saturating_sub(context)..prefix] {
        diff.push_str(&format!(" {line}\n"));
    }
    for line in &old_lines[prefix..old_lines.len() - suffix] {
        diff.push_str(&format!("-{line}\n"));
    }
    for line in &new_lines[prefix..new_lines.len() - suffix] {
        diff.push_str(&format!("+{line}\n"));
    }
    let tail = &old_lines[old_lines.len() - suffix..];
    for line in &tail[..tail.len().min(context)] {
        diff.push_str(&format!(" {line}\n"));
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper() -> Mapper {
        Mapper::new("thr", "/work", "n")
    }

    fn events(outputs: Vec<Output>) -> Vec<AgentEvent> {
        outputs
            .into_iter()
            .filter_map(|o| match o {
                Output::Event(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    fn writes(outputs: &[Output]) -> Vec<Value> {
        outputs
            .iter()
            .filter_map(|o| match o {
                Output::Write(v) => Some(v.clone()),
                _ => None,
            })
            .collect()
    }

    fn feed(m: &mut Mapper, value: Value) -> Vec<Output> {
        m.handle_line(&value.to_string())
    }

    #[test]
    fn a_text_turn_streams_and_completes_with_usage() {
        let mut m = mapper();
        let (turn, started) = m.begin_turn("hello");
        assert_eq!(turn.id, "n-1");
        assert!(matches!(&started[0], AgentEvent::TurnStarted { turn, .. } if turn.id == "n-1"));
        assert!(matches!(&started[1], AgentEvent::ItemCompleted { item: ThreadItem::UserMessage { content, .. }, .. }
            if content == &vec![UserInput::text("hello")]));

        feed(&mut m, json!({"type": "system", "subtype": "init", "model": "claude-opus-5-5", "session_id": "s"}));
        feed(&mut m, json!({"type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "message_start", "message": {"id": "msg_1"}}}));
        let deltas = events(feed(&mut m, json!({"type": "stream_event", "parent_tool_use_id": null,
            "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}})));
        assert_eq!(deltas, vec![AgentEvent::AgentMessageDelta { thread_id: "thr".into(), item_id: "msg_1:0".into(), delta: "Hi".into() }]);

        let done = events(feed(&mut m, json!({"type": "assistant", "parent_tool_use_id": null, "message": {
            "id": "msg_1", "model": "claude-opus-5-5", "content": [{"type": "text", "text": "Hi there"}],
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 0, "output_tokens": 5}}})));
        assert!(matches!(&done[0], AgentEvent::ItemCompleted { item: ThreadItem::AgentMessage { id, text }, .. }
            if id == "msg_1:0" && text == "Hi there"));

        let end = events(feed(&mut m, json!({"type": "result", "subtype": "success", "is_error": false,
            "duration_ms": 1200, "usage": {"input_tokens": 10, "cache_read_input_tokens": 1000,
            "cache_creation_input_tokens": 20, "output_tokens": 5},
            "modelUsage": {"claude-opus-5-5": {"contextWindow": 1000000}}})));
        let AgentEvent::TurnUsage { usage, context_tokens, model_context_window, .. } = &end[0] else { panic!("{end:?}") };
        assert_eq!(usage.input_tokens, 1030);
        assert_eq!(usage.cached_input_tokens, 1000);
        assert_eq!(usage.total_tokens, 1035);
        assert_eq!(*context_tokens, Some(1015));
        assert_eq!(*model_context_window, Some(1_000_000));
        let AgentEvent::TurnCompleted { turn, .. } = &end[1] else { panic!() };
        assert_eq!(turn.status, TurnStatus::Completed);
        assert_eq!(turn.duration_ms, Some(1200));
        assert!(!m.is_running());
        assert_eq!(m.model(), Some("claude-opus-5-5"));
    }

    #[test]
    fn split_assistant_messages_keep_stream_indices() {
        // Claude Code emits one assistant line per content block, all with
        // the same message id; item ids must match the stream indices.
        let mut m = mapper();
        m.begin_turn("x");
        let first = events(feed(&mut m, json!({"type": "assistant", "message": {"id": "msg_2",
            "content": [{"type": "thinking", "thinking": ""}]}})));
        assert!(first.is_empty(), "empty (omitted) thinking is not shown");
        let second = events(feed(&mut m, json!({"type": "assistant", "message": {"id": "msg_2",
            "content": [{"type": "text", "text": "answer"}]}})));
        assert!(matches!(&second[0], AgentEvent::ItemCompleted { item: ThreadItem::AgentMessage { id, .. }, .. } if id == "msg_2:1"));
    }

    #[test]
    fn bash_needs_approval_then_runs() {
        let mut m = mapper();
        m.begin_turn("run tests");
        let started = events(feed(&mut m, json!({"type": "assistant", "message": {"id": "msg_3", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "cargo test", "description": "Run tests"}}]}})));
        assert!(matches!(&started[0], AgentEvent::ItemStarted { item: ThreadItem::CommandExecution { command, status: ItemStatus::InProgress, .. }, .. } if command == "cargo test"));

        let asked = events(feed(&mut m, json!({"type": "control_request", "request_id": "perm-1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "display_name": "Bash",
            "input": {"command": "cargo test"}, "tool_use_id": "toolu_1", "description": "Run tests",
            "permission_suggestions": [{"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "cargo test:*"}],
                                         "behavior": "allow", "destination": "localSettings"}]}})));
        let AgentEvent::ApprovalRequested(approval) = &asked[0] else { panic!() };
        assert_eq!(approval.request_id, RequestId::Str("perm-1".into()));
        assert_eq!(approval.item_id, "toolu_1");
        assert_eq!(approval.turn_id, "n-1");
        assert!(matches!(&approval.kind, ApprovalKind::Command { command: Some(c), reason: Some(r), .. } if c == "cargo test" && r == "Run tests"));

        let reply = m.approval_reply("perm-1", ApprovalDecision::AcceptForSession).unwrap();
        assert_eq!(reply["type"], "control_response");
        assert_eq!(reply["response"]["request_id"], "perm-1");
        let response = &reply["response"]["response"];
        assert_eq!(response["behavior"], "allow");
        assert_eq!(response["updatedInput"]["command"], "cargo test");
        // "For this session" never writes the user's settings files.
        assert_eq!(response["updatedPermissions"][0]["destination"], "session");
        assert!(m.approval_reply("perm-1", ApprovalDecision::Accept).is_none(), "answered once");

        let result = events(feed(&mut m, json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "test result: ok", "is_error": false}]}})));
        assert!(matches!(&result[0], AgentEvent::ItemCompleted { item: ThreadItem::CommandExecution {
            status: ItemStatus::Completed, exit_code: Some(0), aggregated_output: Some(out), .. }, .. } if out == "test result: ok"));
    }

    #[test]
    fn declined_and_failed_commands() {
        let mut m = mapper();
        m.begin_turn("x");
        feed(&mut m, json!({"type": "assistant", "message": {"id": "m", "content": [
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "rm -rf build"}},
            {"type": "tool_use", "id": "t2", "name": "Bash", "input": {"command": "false"}}]}}));
        feed(&mut m, json!({"type": "control_request", "request_id": "p", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "rm -rf build"}, "tool_use_id": "t1"}}));
        let deny = m.approval_reply("p", ApprovalDecision::Decline).unwrap();
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert!(deny["response"]["response"].get("interrupt").is_none());

        let results = events(feed(&mut m, json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "The user declined", "is_error": true},
            {"type": "tool_result", "tool_use_id": "t2", "content": [{"type": "text", "text": "Exit code 1\n"}], "is_error": true}]}})));
        assert!(matches!(&results[0], AgentEvent::ItemCompleted { item: ThreadItem::CommandExecution { status: ItemStatus::Declined, .. }, .. }));
        assert!(matches!(&results[1], AgentEvent::ItemCompleted { item: ThreadItem::CommandExecution {
            status: ItemStatus::Failed, exit_code: Some(1), .. }, .. }));
    }

    #[test]
    fn edits_become_file_changes_with_diffs() {
        let mut m = mapper();
        m.begin_turn("x");
        let started = events(feed(&mut m, json!({"type": "assistant", "message": {"id": "m", "content": [
            {"type": "tool_use", "id": "e1", "name": "Edit", "input": {"file_path": "/work/src/lib.rs",
             "old_string": "fn a() {}\nfn b() {}\nfn c() {}", "new_string": "fn a() {}\nfn b2() {}\nfn c() {}"}}]}})));
        let AgentEvent::ItemStarted { item: ThreadItem::FileChange { changes, .. }, .. } = &started[0] else { panic!() };
        assert_eq!(changes[0].path, "src/lib.rs");
        assert_eq!(changes[0].diff, "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ @@\n fn a() {}\n-fn b() {}\n+fn b2() {}\n fn c() {}\n");

        let asked = events(feed(&mut m, json!({"type": "control_request", "request_id": "p", "request": {
            "subtype": "can_use_tool", "tool_name": "Edit", "input": {"file_path": "/work/src/lib.rs"}, "tool_use_id": "e1"}})));
        assert!(matches!(&asked[0], AgentEvent::ApprovalRequested(PendingApproval { kind: ApprovalKind::FileChange { reason: Some(r), .. }, .. }) if r == "src/lib.rs"));
        m.approval_reply("p", ApprovalDecision::Accept).unwrap();

        let done = events(feed(&mut m, json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "e1", "content": "ok"}]}})));
        assert!(matches!(&done[0], AgentEvent::ItemCompleted { item: ThreadItem::FileChange { status: ItemStatus::Completed, .. }, .. }));

        let write = events(feed(&mut m, json!({"type": "assistant", "message": {"id": "m2", "content": [
            {"type": "tool_use", "id": "w1", "name": "Write", "input": {"file_path": "/work/does/not/exist.txt", "content": "a\nb"}}]}})));
        let AgentEvent::ItemStarted { item: ThreadItem::FileChange { changes, .. }, .. } = &write[0] else { panic!() };
        assert_eq!(changes[0].kind, PatchChangeKind::Add);
        assert!(changes[0].diff.ends_with("+a\n+b\n"));
    }

    #[test]
    fn other_tools_and_todos() {
        let mut m = mapper();
        m.begin_turn("x");
        let out = events(feed(&mut m, json!({"type": "assistant", "message": {"id": "m", "content": [
            {"type": "tool_use", "id": "r1", "name": "Read", "input": {"file_path": "/work/README.md"}},
            {"type": "tool_use", "id": "td", "name": "TodoWrite", "input": {"todos": [
                {"content": "Write tests", "status": "completed", "activeForm": "Writing tests"},
                {"content": "Fix bug", "status": "in_progress", "activeForm": "Fixing"}]}}]}})));
        assert!(matches!(&out[0], AgentEvent::ItemStarted { item: ThreadItem::ToolCall { tool, detail: Some(d), success: None, .. }, .. }
            if tool == "Read" && d == "README.md"));
        let AgentEvent::PlanUpdated { steps, .. } = &out[1] else { panic!() };
        assert_eq!(steps[1].status, PlanStepStatus::InProgress);

        let done = events(feed(&mut m, json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "r1", "content": "# Readme"},
            {"type": "tool_result", "tool_use_id": "td", "content": "ok"}]}})));
        assert_eq!(done.len(), 1, "todo results are not shown");
        assert!(matches!(&done[0], AgentEvent::ItemCompleted { item: ThreadItem::ToolCall { success: Some(true), .. }, .. }));

        let asked = events(feed(&mut m, json!({"type": "control_request", "request_id": "w", "request": {
            "subtype": "can_use_tool", "tool_name": "WebFetch", "display_name": "Fetch",
            "input": {"url": "https://example.com"}, "tool_use_id": "x"}})));
        assert!(matches!(&asked[0], AgentEvent::ApprovalRequested(PendingApproval { kind: ApprovalKind::Tool { tool, detail: Some(d), .. }, .. })
            if tool == "Fetch" && d == "https://example.com"));
    }

    #[test]
    fn subagent_messages_are_skipped() {
        let mut m = mapper();
        m.begin_turn("x");
        let out = feed(&mut m, json!({"type": "assistant", "parent_tool_use_id": "toolu_task", "message": {"id": "m",
            "content": [{"type": "text", "text": "inner"}]}}));
        assert!(out.is_empty());
    }

    #[test]
    fn auth_failures_fail_the_turn_and_flag_the_account() {
        // Captured from Claude Code 2.1.131 with an expired sign-in.
        let mut m = mapper();
        m.begin_turn("hi");
        let retry = events(feed(&mut m, json!({"type": "system", "subtype": "api_retry", "attempt": 1, "max_retries": 10,
            "retry_delay_ms": 538.9, "error_status": 401, "error": "authentication_failed", "session_id": "s", "uuid": "u"})));
        assert!(matches!(&retry[0], AgentEvent::Error { will_retry: true, message, .. } if message.contains("HTTP 401")));
        feed(&mut m, json!({"type": "assistant", "message": {"id": "438a", "model": "<synthetic>", "role": "assistant",
            "content": [{"type": "text", "text": "Failed to authenticate. API Error: 401 OAuth access token is invalid."}],
            "usage": {"input_tokens": 0, "output_tokens": 0}}, "parent_tool_use_id": null, "session_id": "s",
            "error": "authentication_failed"}));
        let end = events(feed(&mut m, json!({"type": "result", "subtype": "success", "is_error": true,
            "api_error_status": 401, "duration_ms": 2008, "num_turns": 1,
            "result": "Failed to authenticate. API Error: 401 OAuth access token is invalid.",
            "usage": {"input_tokens": 0, "output_tokens": 0}, "modelUsage": {}})));
        assert!(end.contains(&AgentEvent::AccountChanged));
        let Some(AgentEvent::TurnCompleted { turn, .. }) = end.last() else { panic!() };
        assert_eq!(turn.status, TurnStatus::Failed);
        assert!(turn.error.as_ref().unwrap().message.contains("Sign in again"));
        assert!(m.take_auth_failure());
        assert!(!m.take_auth_failure());
        assert_ne!(m.model(), Some("<synthetic>"));
    }

    #[test]
    fn interrupts_and_crashes_end_the_turn() {
        let mut m = mapper();
        m.begin_turn("x");
        feed(&mut m, json!({"type": "assistant", "message": {"id": "m", "content": [
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "sleep 100"}}]}}));
        m.interrupt();
        let end = events(feed(&mut m, json!({"type": "result", "subtype": "error_during_execution", "is_error": true})));
        assert!(matches!(&end[0], AgentEvent::ItemCompleted { item: ThreadItem::CommandExecution { status: ItemStatus::Failed, .. }, .. }),
            "the running command does not spin forever");
        assert!(matches!(end.last(), Some(AgentEvent::TurnCompleted { turn, .. }) if turn.status == TurnStatus::Interrupted && turn.error.is_none()));

        m.begin_turn("y");
        feed(&mut m, json!({"type": "control_request", "request_id": "p", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {}}}));
        let exited = m.process_exited("Claude Code exited (signal 9)");
        assert!(matches!(&exited[0], AgentEvent::ApprovalResolved { request_id, .. } if *request_id == RequestId::Str("p".into())));
        let Some(AgentEvent::TurnCompleted { turn, .. }) = exited.last() else { panic!() };
        assert_eq!(turn.status, TurnStatus::Failed);
        assert!(turn.error.as_ref().unwrap().message.contains("signal 9"));
        assert!(m.process_exited("again").is_empty(), "nothing left to end");
    }

    #[test]
    fn cancelled_permission_prompts_resolve() {
        let mut m = mapper();
        m.begin_turn("x");
        feed(&mut m, json!({"type": "control_request", "request_id": "p", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {}}}));
        let out = events(feed(&mut m, json!({"type": "control_cancel_request", "request_id": "p"})));
        assert!(matches!(&out[0], AgentEvent::ApprovalResolved { .. }));
        assert!(!m.has_approval("p"));
    }

    #[test]
    fn dynamic_tools_are_served_over_mcp() {
        let mut m = mapper();
        m.set_mcp_servers(McpServerSpec::from_dynamic(&[DynamicToolSpec::Namespace {
            name: "ide_browser".into(),
            description: "browser".into(),
            tools: vec![DynamicToolSpec::Function {
                name: "navigate".into(),
                description: "Open a URL".into(),
                input_schema: json!({"type": "object", "properties": {"url": {"type": "string"}}}),
            }],
        }]));
        // The handshake Claude Code performs (as captured from 2.1.131).
        let init = feed(&mut m, json!({"type": "control_request", "request_id": "c1", "request": {"subtype": "mcp_message",
            "server_name": "ide_browser", "message": {"method": "initialize", "params": {"protocolVersion": "2025-11-25",
            "capabilities": {}, "clientInfo": {"name": "claude-code"}}, "jsonrpc": "2.0", "id": 0}}}));
        let reply = &writes(&init)[0]["response"];
        assert_eq!(reply["request_id"], "c1");
        assert_eq!(reply["response"]["mcp_response"]["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(reply["response"]["mcp_response"]["id"], 0);
        let ack = feed(&mut m, json!({"type": "control_request", "request_id": "c2", "request": {"subtype": "mcp_message",
            "server_name": "ide_browser", "message": {"method": "notifications/initialized", "jsonrpc": "2.0"}}}));
        assert_eq!(writes(&ack)[0]["response"]["subtype"], "success");
        let list = feed(&mut m, json!({"type": "control_request", "request_id": "c3", "request": {"subtype": "mcp_message",
            "server_name": "ide_browser", "message": {"method": "tools/list", "jsonrpc": "2.0", "id": 1}}}));
        assert_eq!(writes(&list)[0]["response"]["response"]["mcp_response"]["result"]["tools"][0]["name"], "navigate");

        m.begin_turn("open the app");
        let used = feed(&mut m, json!({"type": "assistant", "message": {"id": "m", "content": [
            {"type": "tool_use", "id": "toolu_b", "name": "mcp__ide_browser__navigate", "input": {"url": "http://localhost:3000"}}]}}));
        assert!(used.is_empty(), "the tools/call shows the call");
        let call = events(feed(&mut m, json!({"type": "control_request", "request_id": "c4", "request": {"subtype": "mcp_message",
            "server_name": "ide_browser", "message": {"method": "tools/call", "jsonrpc": "2.0", "id": 2,
            "params": {"name": "navigate", "arguments": {"url": "http://localhost:3000"}}}}})));
        let AgentEvent::ToolCallRequested(call) = &call[0] else { panic!() };
        assert_eq!(call.params.namespace.as_deref(), Some("ide_browser"));
        assert_eq!(call.params.tool, "navigate");
        assert_eq!(call.params.turn_id, "n-1");

        let reply = m
            .tool_reply("c4", &DynamicToolCallResponse {
                content_items: vec![
                    ToolOutputItem::InputText { text: "Loaded".into() },
                    ToolOutputItem::InputImage { image_url: "data:image/png;base64,AAA=".into() },
                ],
                success: true,
            })
            .unwrap();
        let result = &reply["response"]["response"]["mcp_response"];
        assert_eq!(result["id"], 2);
        assert_eq!(result["result"]["content"][0]["text"], "Loaded");
        assert_eq!(result["result"]["content"][1], json!({"type": "image", "data": "AAA=", "mimeType": "image/png"}));
        assert_eq!(result["result"]["isError"], false);

        let done = events(feed(&mut m, json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "toolu_b", "content": [{"type": "text", "text": "Loaded"}]}]}})));
        assert!(matches!(&done[0], AgentEvent::ItemCompleted { item: ThreadItem::DynamicToolCall { id, success: Some(true), .. }, .. }
            if id == "toolcall-c4"));

        let unknown = feed(&mut m, json!({"type": "control_request", "request_id": "c5", "request": {"subtype": "mcp_message",
            "server_name": "nope", "message": {"method": "tools/list", "id": 3}}}));
        assert!(writes(&unknown)[0]["response"]["response"]["mcp_response"]["error"].is_object());
    }

    #[test]
    fn unsupported_control_requests_get_an_error_and_responses_are_routed() {
        let mut m = mapper();
        let out = feed(&mut m, json!({"type": "control_request", "request_id": "h", "request": {"subtype": "hook_callback"}}));
        assert_eq!(writes(&out)[0]["response"]["subtype"], "error");
        let out = feed(&mut m, json!({"type": "control_response", "response": {"subtype": "error", "request_id": "r4",
            "error": "Unsupported control request subtype: bogus_thing"}}));
        assert_eq!(out, vec![Output::ControlResponse { request_id: "r4".into(),
            result: Err("Unsupported control request subtype: bogus_thing".into()) }]);
        assert!(m.handle_line("not json").is_empty());
        assert!(feed(&mut m, json!({"type": "keep_alive"})).is_empty());
    }

    #[test]
    fn helpers() {
        assert_eq!(split_mcp_tool_name("mcp__ide_browser__take_screenshot"), Some(("ide_browser", "take_screenshot")));
        assert_eq!(split_mcp_tool_name("Read"), None);
        assert_eq!(parse_exit_code("Exit code 127\ncommand not found"), Some(127));
        assert_eq!(parse_exit_code("fine"), None);
        assert_eq!(summarize_tool("Grep", &json!({"pattern": "TODO", "path": "/work/src"}), "/work").as_deref(), Some("TODO in src"));
        assert_eq!(summarize_tool("Mystery", &json!({}), "/work"), None);
        assert_eq!(unified_diff("f", "a\nb", "a\nb\nc"), "--- a/f\n+++ b/f\n@@ @@\n a\n b\n+c\n");
        assert_eq!(control_request("i", json!({"subtype": "interrupt"}))["request"]["subtype"], "interrupt");
        assert_eq!(user_message("hi")["message"]["content"], "hi");
    }
}

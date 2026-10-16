//! Normalized agent events.
//!
//! The UI, the orchestrator and the event log all consume [`AgentEvent`]
//! rather than raw protocol messages, so a protocol change touches one
//! function ([`AgentEvent::from_notification`]) instead of every consumer.

use serde_json::Value;

use crate::{
    protocol::*,
    rpc::{RequestId, RpcError},
};

/// An approval Codex is waiting on.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingApproval {
    pub request_id: RequestId,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub kind: ApprovalKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ApprovalKind {
    Command {
        command: Option<String>,
        cwd: Option<String>,
        reason: Option<String>,
    },
    FileChange {
        reason: Option<String>,
        grant_root: Option<String>,
    },
}

/// A call to one of the client's dynamic tools.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub request_id: RequestId,
    pub params: DynamicToolCallParams,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    ThreadStarted(Thread),
    ThreadNameUpdated {
        thread_id: String,
        name: Option<String>,
    },
    TurnStarted {
        thread_id: String,
        turn: Turn,
    },
    TurnCompleted {
        thread_id: String,
        turn: Turn,
    },
    ItemStarted {
        thread_id: String,
        turn_id: String,
        item: ThreadItem,
    },
    ItemCompleted {
        thread_id: String,
        turn_id: String,
        item: ThreadItem,
    },
    AgentMessageDelta {
        thread_id: String,
        item_id: String,
        delta: String,
    },
    ReasoningDelta {
        thread_id: String,
        item_id: String,
        delta: String,
    },
    CommandOutputDelta {
        thread_id: String,
        item_id: String,
        delta: String,
    },
    PlanUpdated {
        thread_id: String,
        turn_id: String,
        explanation: Option<String>,
        steps: Vec<PlanStep>,
    },
    DiffUpdated {
        thread_id: String,
        turn_id: String,
        diff: String,
    },
    TokenUsage {
        thread_id: String,
        turn_id: String,
        usage: ThreadTokenUsage,
    },
    ApprovalRequested(PendingApproval),
    /// The server no longer needs an answer (the turn ended, say).
    ApprovalResolved {
        thread_id: String,
        request_id: RequestId,
    },
    ToolCallRequested(ToolCall),
    Error {
        thread_id: String,
        turn_id: String,
        message: String,
        will_retry: bool,
    },
    Warning {
        thread_id: Option<String>,
        message: String,
    },
    LoginCompleted {
        success: bool,
        error: Option<String>,
    },
    AccountChanged,
    /// The app server process ended or the connection broke.
    Disconnected {
        reason: String,
    },
    /// A notification this build does not interpret.
    Unhandled {
        method: String,
    },
}

impl AgentEvent {
    /// The thread an event belongs to, when it belongs to one.
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            AgentEvent::ThreadStarted(thread) => Some(&thread.id),
            AgentEvent::ThreadNameUpdated { thread_id, .. }
            | AgentEvent::TurnStarted { thread_id, .. }
            | AgentEvent::TurnCompleted { thread_id, .. }
            | AgentEvent::ItemStarted { thread_id, .. }
            | AgentEvent::ItemCompleted { thread_id, .. }
            | AgentEvent::AgentMessageDelta { thread_id, .. }
            | AgentEvent::ReasoningDelta { thread_id, .. }
            | AgentEvent::CommandOutputDelta { thread_id, .. }
            | AgentEvent::PlanUpdated { thread_id, .. }
            | AgentEvent::DiffUpdated { thread_id, .. }
            | AgentEvent::TokenUsage { thread_id, .. }
            | AgentEvent::ApprovalResolved { thread_id, .. }
            | AgentEvent::Error { thread_id, .. } => Some(thread_id),
            AgentEvent::ApprovalRequested(approval) => Some(&approval.thread_id),
            AgentEvent::ToolCallRequested(call) => Some(&call.params.thread_id),
            AgentEvent::Warning { thread_id, .. } => thread_id.as_deref(),
            AgentEvent::LoginCompleted { .. }
            | AgentEvent::AccountChanged
            | AgentEvent::Disconnected { .. }
            | AgentEvent::Unhandled { .. } => None,
        }
    }

    /// Whether the event is worth a row in the append-only event log.
    /// Streaming deltas are not: the completed item carries the same text.
    pub fn is_loggable(&self) -> bool {
        !matches!(
            self,
            AgentEvent::AgentMessageDelta { .. }
                | AgentEvent::ReasoningDelta { .. }
                | AgentEvent::CommandOutputDelta { .. }
                | AgentEvent::Unhandled { .. }
        )
    }

    /// Short, stable name for the event log.
    pub fn kind(&self) -> &'static str {
        match self {
            AgentEvent::ThreadStarted(_) => "thread/started",
            AgentEvent::ThreadNameUpdated { .. } => "thread/name/updated",
            AgentEvent::TurnStarted { .. } => "turn/started",
            AgentEvent::TurnCompleted { .. } => "turn/completed",
            AgentEvent::ItemStarted { .. } => "item/started",
            AgentEvent::ItemCompleted { .. } => "item/completed",
            AgentEvent::AgentMessageDelta { .. } => "item/agentMessage/delta",
            AgentEvent::ReasoningDelta { .. } => "item/reasoning/delta",
            AgentEvent::CommandOutputDelta { .. } => "item/commandExecution/outputDelta",
            AgentEvent::PlanUpdated { .. } => "turn/plan/updated",
            AgentEvent::DiffUpdated { .. } => "turn/diff/updated",
            AgentEvent::TokenUsage { .. } => "thread/tokenUsage/updated",
            AgentEvent::ApprovalRequested(_) => "approval/requested",
            AgentEvent::ApprovalResolved { .. } => "approval/resolved",
            AgentEvent::ToolCallRequested(_) => "tool/call",
            AgentEvent::Error { .. } => "error",
            AgentEvent::Warning { .. } => "warning",
            AgentEvent::LoginCompleted { .. } => "account/login/completed",
            AgentEvent::AccountChanged => "account/updated",
            AgentEvent::Disconnected { .. } => "disconnected",
            AgentEvent::Unhandled { .. } => "unhandled",
        }
    }

    /// Interprets a server notification. Malformed payloads for known methods
    /// are logged and reported as `Unhandled` rather than dropped silently.
    pub fn from_notification(method: &str, params: Value) -> Self {
        fn decode<T: serde::de::DeserializeOwned>(method: &str, params: Value) -> Option<T> {
            match serde_json::from_value(params) {
                Ok(value) => Some(value),
                Err(err) => {
                    tracing::warn!("could not decode {method}: {err}");
                    None
                }
            }
        }

        let event = match method {
            "thread/started" => {
                decode::<ThreadStartedNotification>(method, params).map(|n| AgentEvent::ThreadStarted(n.thread))
            }
            "thread/name/updated" => decode::<ThreadNameUpdatedNotification>(method, params).map(|n| {
                AgentEvent::ThreadNameUpdated {
                    thread_id: n.thread_id,
                    name: n.thread_name,
                }
            }),
            "turn/started" => decode::<TurnNotification>(method, params).map(|n| AgentEvent::TurnStarted {
                thread_id: n.thread_id,
                turn: n.turn,
            }),
            "turn/completed" => decode::<TurnNotification>(method, params).map(|n| AgentEvent::TurnCompleted {
                thread_id: n.thread_id,
                turn: n.turn,
            }),
            "item/started" => decode::<ItemNotification>(method, params).map(|n| AgentEvent::ItemStarted {
                thread_id: n.thread_id,
                turn_id: n.turn_id,
                item: n.item,
            }),
            "item/completed" => decode::<ItemNotification>(method, params).map(|n| AgentEvent::ItemCompleted {
                thread_id: n.thread_id,
                turn_id: n.turn_id,
                item: n.item,
            }),
            "item/agentMessage/delta" => decode::<DeltaNotification>(method, params).map(|n| {
                AgentEvent::AgentMessageDelta {
                    thread_id: n.thread_id,
                    item_id: n.item_id,
                    delta: n.delta,
                }
            }),
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                decode::<DeltaNotification>(method, params).map(|n| AgentEvent::ReasoningDelta {
                    thread_id: n.thread_id,
                    item_id: n.item_id,
                    delta: n.delta,
                })
            }
            "item/commandExecution/outputDelta" => decode::<DeltaNotification>(method, params).map(|n| {
                AgentEvent::CommandOutputDelta {
                    thread_id: n.thread_id,
                    item_id: n.item_id,
                    delta: n.delta,
                }
            }),
            "turn/plan/updated" => decode::<TurnPlanUpdatedNotification>(method, params).map(|n| {
                AgentEvent::PlanUpdated {
                    thread_id: n.thread_id,
                    turn_id: n.turn_id,
                    explanation: n.explanation,
                    steps: n.plan,
                }
            }),
            "turn/diff/updated" => decode::<TurnDiffUpdatedNotification>(method, params).map(|n| {
                AgentEvent::DiffUpdated {
                    thread_id: n.thread_id,
                    turn_id: n.turn_id,
                    diff: n.diff,
                }
            }),
            "thread/tokenUsage/updated" => decode::<ThreadTokenUsageUpdatedNotification>(method, params)
                .map(|n| AgentEvent::TokenUsage {
                    thread_id: n.thread_id,
                    turn_id: n.turn_id,
                    usage: n.token_usage,
                }),
            "serverRequest/resolved" => decode::<ServerRequestResolvedNotification>(method, params).map(|n| {
                AgentEvent::ApprovalResolved {
                    thread_id: n.thread_id,
                    request_id: n.request_id,
                }
            }),
            "error" => decode::<ErrorNotification>(method, params).map(|n| AgentEvent::Error {
                thread_id: n.thread_id,
                turn_id: n.turn_id,
                message: n.error.message,
                will_retry: n.will_retry,
            }),
            "warning" | "guardianWarning" => decode::<WarningNotification>(method, params).map(|n| {
                AgentEvent::Warning {
                    thread_id: n.thread_id,
                    message: n.message,
                }
            }),
            "configWarning" => decode::<ConfigWarningNotification>(method, params).map(|n| {
                AgentEvent::Warning {
                    thread_id: None,
                    message: match n.details {
                        Some(details) => format!("{} {}", n.summary, details),
                        None => n.summary,
                    },
                }
            }),
            "account/login/completed" => decode::<AccountLoginCompletedNotification>(method, params)
                .map(|n| AgentEvent::LoginCompleted {
                    success: n.success,
                    error: n.error,
                }),
            "account/updated" => Some(AgentEvent::AccountChanged),
            _ => None,
        };
        event.unwrap_or_else(|| AgentEvent::Unhandled {
            method: method.to_string(),
        })
    }

    /// Interprets a request the server makes of the client. Requests Cockpit
    /// cannot service come back as `Err`, carrying the error to reply with.
    pub fn from_request(id: RequestId, method: &str, params: Value) -> Result<Self, RpcError> {
        let invalid = |err: serde_json::Error| RpcError {
            code: RpcError::INVALID_REQUEST,
            message: format!("could not decode {method}: {err}"),
            data: None,
        };
        match method {
            "item/commandExecution/requestApproval" => {
                let p: CommandApprovalParams = serde_json::from_value(params).map_err(invalid)?;
                Ok(AgentEvent::ApprovalRequested(PendingApproval {
                    request_id: id,
                    thread_id: p.thread_id,
                    turn_id: p.turn_id,
                    item_id: p.item_id,
                    kind: ApprovalKind::Command {
                        command: p.command,
                        cwd: p.cwd,
                        reason: p.reason,
                    },
                }))
            }
            "item/fileChange/requestApproval" => {
                let p: FileChangeApprovalParams = serde_json::from_value(params).map_err(invalid)?;
                Ok(AgentEvent::ApprovalRequested(PendingApproval {
                    request_id: id,
                    thread_id: p.thread_id,
                    turn_id: p.turn_id,
                    item_id: p.item_id,
                    kind: ApprovalKind::FileChange {
                        reason: p.reason,
                        grant_root: p.grant_root,
                    },
                }))
            }
            "item/tool/call" => {
                let params: DynamicToolCallParams = serde_json::from_value(params).map_err(invalid)?;
                Ok(AgentEvent::ToolCallRequested(ToolCall {
                    request_id: id,
                    params,
                }))
            }
            other => Err(RpcError::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notifications_are_normalized() {
        let event = AgentEvent::from_notification(
            "item/agentMessage/delta",
            json!({"threadId": "t", "turnId": "u", "itemId": "i", "delta": "Hel"}),
        );
        assert_eq!(
            event,
            AgentEvent::AgentMessageDelta { thread_id: "t".into(), item_id: "i".into(), delta: "Hel".into() }
        );
        assert!(!event.is_loggable());

        let usage = AgentEvent::from_notification(
            "thread/tokenUsage/updated",
            json!({"threadId": "t", "turnId": "u", "tokenUsage": {
                "total": {"totalTokens": 10, "inputTokens": 7, "cachedInputTokens": 0,
                          "cacheWriteInputTokens": 0, "outputTokens": 3, "reasoningOutputTokens": 1},
                "last": {"totalTokens": 10, "inputTokens": 7, "cachedInputTokens": 0,
                         "cacheWriteInputTokens": 0, "outputTokens": 3, "reasoningOutputTokens": 1},
                "modelContextWindow": 200000}}),
        );
        let AgentEvent::TokenUsage { usage, .. } = usage else { panic!() };
        assert_eq!(usage.total.total_tokens, 10);
        assert_eq!(usage.model_context_window, Some(200000));
    }

    #[test]
    fn unknown_and_malformed_notifications_are_unhandled() {
        assert_eq!(
            AgentEvent::from_notification("remoteControl/status/changed", json!({})),
            AgentEvent::Unhandled { method: "remoteControl/status/changed".into() }
        );
        assert!(matches!(
            AgentEvent::from_notification("turn/started", json!({"oops": true})),
            AgentEvent::Unhandled { .. }
        ));
    }

    #[test]
    fn approval_and_tool_requests() {
        let approval = AgentEvent::from_request(
            RequestId::Int(4),
            "item/commandExecution/requestApproval",
            json!({"kind": "exec", "threadId": "t", "turnId": "u", "itemId": "i",
                   "startedAtMs": 1, "environmentId": null, "command": "rm -rf build", "cwd": "/w",
                   "reason": "clean"}),
        )
        .unwrap();
        let AgentEvent::ApprovalRequested(approval) = approval else { panic!() };
        assert_eq!(approval.request_id, RequestId::Int(4));
        assert!(matches!(approval.kind, ApprovalKind::Command { command: Some(ref c), .. } if c == "rm -rf build"));

        let tool = AgentEvent::from_request(
            RequestId::Int(5),
            "item/tool/call",
            json!({"threadId": "t", "turnId": "u", "callId": "c", "namespace": "ide_browser",
                   "tool": "navigate", "arguments": {"url": "http://localhost:3000"}}),
        )
        .unwrap();
        assert_eq!(tool.thread_id(), Some("t"));

        let unsupported = AgentEvent::from_request(RequestId::Int(6), "attestation/generate", json!({}));
        assert_eq!(unsupported.unwrap_err().code, RpcError::METHOD_NOT_FOUND);
    }
}

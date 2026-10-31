//! The subset of the Codex App Server protocol (v2) that Cockpit speaks.
//!
//! Field names and shapes follow the schema `codex app-server generate-ts`
//! emits (checked against codex-cli 0.163). The Claude backend
//! ([`crate::claude`]) maps Claude Code's messages onto these same types, so
//! the transcript and the UI have one vocabulary for both providers. The server evolves quickly, so
//! everything here is written to *tolerate* drift: unknown fields are
//! ignored, optional fields default, and unknown enum values or item types
//! decode to an `Unknown`/`Other` variant instead of failing the message.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeCapabilities {
    /// Opt into experimental methods and fields. Required for dynamic tools.
    pub experimental_api: bool,
    pub request_attestation: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opt_out_notification_methods: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
    pub capabilities: Option<InitializeCapabilities>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InitializeResponse {
    pub user_agent: String,
    pub codex_home: Option<String>,
    pub platform_os: Option<String>,
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// When Codex asks before running commands or applying edits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AskForApproval {
    /// Ask for anything not known to be safe.
    #[serde(rename = "untrusted")]
    Untrusted,
    /// The model decides when to ask.
    #[serde(rename = "on-request")]
    OnRequest,
    /// Never ask; failures go back to the model.
    #[serde(rename = "never")]
    Never,
    /// Granular policies and anything newer.
    #[serde(untagged)]
    Other(Value),
}

/// What the agent's commands may touch.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SandboxMode {
    #[serde(rename = "read-only")]
    ReadOnly,
    #[serde(rename = "workspace-write")]
    WorkspaceWrite,
    #[serde(rename = "danger-full-access")]
    DangerFullAccess,
}

/// The sandbox as `turn/start` takes it; it applies to that turn and the
/// ones after it, so a permission change reaches running threads.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SandboxPolicy {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

// ---------------------------------------------------------------------------
// Threads and turns
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<AskForApproval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    /// Client-implemented tools (experimental API).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_tools: Option<Vec<DynamicToolSpec>>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<AskForApproval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThreadStartResponse {
    pub thread: Thread,
    pub model: String,
    pub cwd: Option<String>,
    pub reasoning_effort: Option<String>,
}

pub type ThreadResumeResponse = ThreadStartResponse;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Thread {
    pub id: String,
    pub preview: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub ephemeral: bool,
    /// Only populated on resume/fork/read responses.
    pub turns: Vec<Turn>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<AskForApproval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<SandboxPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    pub turn: Turn,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Turn {
    pub id: String,
    pub items: Vec<ThreadItem>,
    pub status: TurnStatus,
    pub error: Option<TurnError>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    #[default]
    InProgress,
    Completed,
    Interrupted,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct TurnError {
    pub message: String,
    pub additional_details: Option<String>,
    pub codex_error_info: Option<Value>,
}

/// One piece of user input.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    Text {
        text: String,
        #[serde(default)]
        text_elements: Vec<Value>,
    },
    LocalImage {
        path: String,
    },
    Image {
        url: String,
    },
    Mention {
        name: String,
        path: String,
    },
    #[serde(other)]
    Unknown,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
    Declined,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum PatchChangeKind {
    #[serde(rename = "add")]
    Add,
    #[serde(rename = "delete")]
    Delete,
    #[serde(rename = "update")]
    Update {
        #[serde(default)]
        move_path: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
    #[serde(default)]
    pub diff: String,
}

/// Everything a turn produces, from the user's message to commands and edits.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadItem {
    #[serde(rename_all = "camelCase")]
    UserMessage {
        id: String,
        #[serde(default)]
        content: Vec<UserInput>,
    },
    #[serde(rename_all = "camelCase")]
    AgentMessage {
        id: String,
        #[serde(default)]
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Plan {
        id: String,
        #[serde(default)]
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<String>,
        #[serde(default)]
        content: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    CommandExecution {
        id: String,
        #[serde(default)]
        command: String,
        #[serde(default)]
        cwd: String,
        #[serde(default)]
        status: ItemStatus,
        #[serde(default)]
        aggregated_output: Option<String>,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        duration_ms: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    FileChange {
        id: String,
        #[serde(default)]
        changes: Vec<FileUpdateChange>,
        #[serde(default)]
        status: ItemStatus,
    },
    #[serde(rename_all = "camelCase")]
    McpToolCall {
        id: String,
        #[serde(default)]
        server: String,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        status: Value,
        #[serde(default)]
        error: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    DynamicToolCall {
        id: String,
        #[serde(default)]
        namespace: Option<String>,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        success: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    WebSearch {
        id: String,
        #[serde(default)]
        query: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ContextCompaction {
        #[serde(default)]
        id: String,
    },
    /// A built-in tool call with a one-line summary (Claude's Read, Grep,
    /// WebFetch…). Codex never sends it; the Claude backend builds it.
    #[serde(rename = "cockpitToolCall", rename_all = "camelCase")]
    ToolCall {
        id: String,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        success: Option<bool>,
    },
    /// Item types this build does not render.
    #[serde(other)]
    Other,
}

impl ThreadItem {
    pub fn id(&self) -> Option<&str> {
        match self {
            ThreadItem::UserMessage { id, .. }
            | ThreadItem::AgentMessage { id, .. }
            | ThreadItem::Plan { id, .. }
            | ThreadItem::Reasoning { id, .. }
            | ThreadItem::CommandExecution { id, .. }
            | ThreadItem::FileChange { id, .. }
            | ThreadItem::McpToolCall { id, .. }
            | ThreadItem::DynamicToolCall { id, .. }
            | ThreadItem::WebSearch { id, .. }
            | ThreadItem::ToolCall { id, .. }
            | ThreadItem::ContextCompaction { id } => Some(id),
            ThreadItem::Other => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartedNotification {
    pub thread: Thread,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnNotification {
    pub thread_id: String,
    pub turn: Turn,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    pub item: ThreadItem,
    pub thread_id: String,
    pub turn_id: String,
}

/// Streaming text for an item (agent message, command output, reasoning, plan).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeltaNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnDiffUpdatedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub diff: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStepStatus,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnPlanUpdatedNotification {
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub explanation: Option<String>,
    #[serde(default)]
    pub plan: Vec<PlanStep>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenUsageBreakdown {
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
}

impl TokenUsageBreakdown {
    /// Component-wise sum, used to keep running totals.
    pub fn saturating_add(&self, other: &TokenUsageBreakdown) -> TokenUsageBreakdown {
        TokenUsageBreakdown {
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            cached_input_tokens: self.cached_input_tokens.saturating_add(other.cached_input_tokens),
            cache_write_input_tokens: self.cache_write_input_tokens.saturating_add(other.cache_write_input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            reasoning_output_tokens: self.reasoning_output_tokens.saturating_add(other.reasoning_output_tokens),
        }
    }

    /// Component-wise `self - earlier`, clamped at zero. Used to turn the
    /// server's running totals into per-update deltas.
    pub fn saturating_sub(&self, earlier: &TokenUsageBreakdown) -> TokenUsageBreakdown {
        TokenUsageBreakdown {
            total_tokens: (self.total_tokens - earlier.total_tokens).max(0),
            input_tokens: (self.input_tokens - earlier.input_tokens).max(0),
            cached_input_tokens: (self.cached_input_tokens - earlier.cached_input_tokens).max(0),
            cache_write_input_tokens: (self.cache_write_input_tokens - earlier.cache_write_input_tokens)
                .max(0),
            output_tokens: (self.output_tokens - earlier.output_tokens).max(0),
            reasoning_output_tokens: (self.reasoning_output_tokens - earlier.reasoning_output_tokens)
                .max(0),
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThreadTokenUsage {
    /// Running total for the thread.
    pub total: TokenUsageBreakdown,
    /// The most recent model call.
    pub last: TokenUsageBreakdown,
    pub model_context_window: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsageUpdatedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub token_usage: ThreadTokenUsage,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    pub error: TurnError,
    #[serde(default)]
    pub will_retry: bool,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarningNotification {
    #[serde(default)]
    pub thread_id: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWarningNotification {
    pub summary: String,
    #[serde(default)]
    pub details: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadNameUpdatedNotification {
    pub thread_id: String,
    #[serde(default)]
    pub thread_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolvedNotification {
    pub thread_id: String,
    pub request_id: crate::rpc::RequestId,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLoginCompletedNotification {
    #[serde(default)]
    pub login_id: Option<String>,
    pub success: bool,
    #[serde(default)]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Server requests: approvals and dynamic tools
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommandApprovalParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeApprovalParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub grant_root: Option<String>,
}

/// The user's answer to an approval request.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Accept,
    /// Accept, and stop asking about this kind of action for the session.
    AcceptForSession,
    Decline,
    /// Decline and interrupt the turn.
    Cancel,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApprovalResponse {
    pub decision: ApprovalDecision,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallParams {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    #[serde(default)]
    pub namespace: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolOutputItem {
    InputText {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    InputImage {
        /// A `data:` URL or remote URL.
        image_url: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallResponse {
    pub content_items: Vec<ToolOutputItem>,
    pub success: bool,
}

impl DynamicToolCallResponse {
    pub fn text(text: impl Into<String>, success: bool) -> Self {
        Self {
            content_items: vec![ToolOutputItem::InputText { text: text.into() }],
            success,
        }
    }
}

/// A client-implemented tool advertised at `thread/start`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DynamicToolSpec {
    #[serde(rename_all = "camelCase")]
    Function {
        name: String,
        description: String,
        input_schema: Value,
    },
    Namespace {
        name: String,
        description: String,
        /// Must all be `Function`s.
        tools: Vec<DynamicToolSpec>,
    },
}

// ---------------------------------------------------------------------------
// Models and accounts
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelListResponse {
    pub data: Vec<Model>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ReasoningEffortOption {
    pub reasoning_effort: String,
    pub description: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Model {
    pub id: String,
    pub model: String,
    pub display_name: String,
    pub description: String,
    pub hidden: bool,
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
    pub default_reasoning_effort: String,
    pub input_modalities: Vec<String>,
    pub is_default: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetAccountParams {
    pub refresh_token: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Account {
    ApiKey,
    #[serde(rename_all = "camelCase")]
    Chatgpt {
        #[serde(default)]
        email: Option<String>,
        #[serde(default)]
        plan_type: Value,
    },
    /// Signed in to Claude Code (built from `claude auth status`).
    #[serde(rename_all = "camelCase")]
    Claude {
        #[serde(default)]
        email: Option<String>,
        /// "claude.ai", "console", an API key or a cloud provider.
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        plan: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GetAccountResponse {
    pub account: Option<Account>,
    /// The backend needs a sign-in before it can run turns. (Codex's name
    /// for it; the Claude backend sets it too.)
    pub requires_openai_auth: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum LoginParams {
    #[serde(rename_all = "camelCase")]
    ApiKey { api_key: String },
    Chatgpt,
    ChatgptDeviceCode,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum LoginResponse {
    ApiKey,
    #[serde(rename_all = "camelCase")]
    Chatgpt { login_id: String, auth_url: String },
    #[serde(rename_all = "camelCase")]
    ChatgptDeviceCode {
        login_id: String,
        verification_url: String,
        user_code: String,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_overrides_serialize_like_the_schema() {
        let params = TurnStartParams {
            thread_id: "t".into(),
            input: vec![UserInput::text("hi")],
            cwd: None,
            approval_policy: Some(AskForApproval::OnRequest),
            sandbox_policy: Some(SandboxPolicy::WorkspaceWrite),
            model: None,
            effort: None,
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["sandboxPolicy"], json!({"type": "workspaceWrite"}));
        assert_eq!(json["approvalPolicy"], json!("on-request"));
        assert!(json.get("model").is_none());
        assert_eq!(
            serde_json::to_value(SandboxPolicy::DangerFullAccess).unwrap(),
            json!({"type": "dangerFullAccess"})
        );
    }
    use serde_json::json;

    #[test]
    fn approval_policy_round_trips_and_tolerates_granular() {
        assert_eq!(serde_json::to_value(AskForApproval::OnRequest).unwrap(), json!("on-request"));
        let parsed: AskForApproval = serde_json::from_value(json!("never")).unwrap();
        assert_eq!(parsed, AskForApproval::Never);
        let granular: AskForApproval =
            serde_json::from_value(json!({"granular": {"rules": true}})).unwrap();
        assert!(matches!(granular, AskForApproval::Other(_)));
        assert_eq!(serde_json::to_value(SandboxMode::WorkspaceWrite).unwrap(), json!("workspace-write"));
    }

    #[test]
    fn user_text_input_matches_the_schema() {
        assert_eq!(
            serde_json::to_value(UserInput::text("hi")).unwrap(),
            json!({"type": "text", "text": "hi", "text_elements": []})
        );
    }

    #[test]
    fn thread_start_response_from_a_real_server() {
        // Captured from `codex app-server` 0.163.
        let response: ThreadStartResponse = serde_json::from_value(json!({
            "thread": {"id": "01a1", "sessionId": "01a1", "preview": "", "ephemeral": true,
                "modelProvider": "openai", "model": "gpt-6.1-sol", "createdAt": 1792118371,
                "updatedAt": 1792118371, "status": {"type": "idle"}, "path": null, "cwd": "/tmp",
                "cliVersion": "0.163.0", "source": "vscode", "turns": []},
            "model": "gpt-6.1-sol", "modelProvider": "openai", "cwd": "/tmp",
            "approvalPolicy": "on-request", "approvalsReviewer": "user",
            "sandbox": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": false},
            "reasoningEffort": null
        }))
        .unwrap();
        assert_eq!(response.thread.id, "01a1");
        assert_eq!(response.model, "gpt-6.1-sol");
    }

    #[test]
    fn items_decode_and_unknown_types_do_not_fail() {
        let items: Vec<ThreadItem> = serde_json::from_value(json!([
            {"type": "agentMessage", "id": "a", "text": "hello", "phase": null},
            {"type": "commandExecution", "id": "c", "command": "ls", "cwd": "/tmp",
             "status": "completed", "aggregatedOutput": "x", "exitCode": 0, "durationMs": 5,
             "source": "agent", "commandActions": []},
            {"type": "fileChange", "id": "f", "status": "inProgress", "changes": [
                {"path": "a.rs", "kind": {"type": "update", "move_path": null}, "diff": "@@"},
                {"path": "b.rs", "kind": {"type": "add"}, "diff": ""}
            ]},
            {"type": "somethingFromTheFuture", "id": "z", "whatever": 1},
            {"type": "commandExecution", "id": "d", "status": "brandNewStatus"}
        ]))
        .unwrap();
        assert_eq!(items.len(), 5);
        assert!(matches!(&items[0], ThreadItem::AgentMessage { text, .. } if text == "hello"));
        assert!(matches!(&items[1], ThreadItem::CommandExecution { exit_code: Some(0), .. }));
        let ThreadItem::FileChange { changes, .. } = &items[2] else { panic!() };
        assert_eq!(changes[0].kind, PatchChangeKind::Update { move_path: None });
        assert_eq!(items[3], ThreadItem::Other);
        assert!(matches!(&items[4], ThreadItem::CommandExecution { status: ItemStatus::Unknown, .. }));
    }

    #[test]
    fn dynamic_tool_specs_use_the_canonical_namespace_format() {
        let spec = DynamicToolSpec::Namespace {
            name: "ide_browser".into(),
            description: "browser".into(),
            tools: vec![DynamicToolSpec::Function {
                name: "navigate".into(),
                description: "go".into(),
                input_schema: json!({"type": "object"}),
            }],
        };
        assert_eq!(
            serde_json::to_value(spec).unwrap(),
            json!({"type": "namespace", "name": "ide_browser", "description": "browser",
                   "tools": [{"type": "function", "name": "navigate", "description": "go",
                              "inputSchema": {"type": "object"}}]})
        );
    }

    #[test]
    fn tool_responses_serialize_like_the_schema() {
        let response = DynamicToolCallResponse {
            content_items: vec![
                ToolOutputItem::InputText { text: "ok".into() },
                ToolOutputItem::InputImage { image_url: "data:image/png;base64,AA".into() },
            ],
            success: true,
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({"contentItems": [
                {"type": "inputText", "text": "ok"},
                {"type": "inputImage", "imageUrl": "data:image/png;base64,AA"}
            ], "success": true})
        );
        assert_eq!(serde_json::to_value(ApprovalDecision::AcceptForSession).unwrap(), json!("acceptForSession"));
    }

    #[test]
    fn token_usage_deltas() {
        let earlier = TokenUsageBreakdown { total_tokens: 100, input_tokens: 80, output_tokens: 20, ..Default::default() };
        let later = TokenUsageBreakdown { total_tokens: 150, input_tokens: 110, output_tokens: 40, ..Default::default() };
        let delta = later.saturating_sub(&earlier);
        assert_eq!(delta.total_tokens, 50);
        assert_eq!(delta.input_tokens, 30);
        assert_eq!(earlier.saturating_sub(&later).total_tokens, 0);
    }

    #[test]
    fn accounts_and_logins() {
        let response: GetAccountResponse =
            serde_json::from_value(json!({"account": null, "requiresOpenaiAuth": true, "workspaceRouting": null}))
                .unwrap();
        assert!(response.account.is_none() && response.requires_openai_auth);
        let chatgpt: Account =
            serde_json::from_value(json!({"type": "chatgpt", "email": "a@b.c", "planType": "plus"})).unwrap();
        assert!(matches!(chatgpt, Account::Chatgpt { email: Some(_), .. }));
        assert_eq!(serde_json::to_value(LoginParams::Chatgpt).unwrap(), json!({"type": "chatgpt"}));
        let login: LoginResponse = serde_json::from_value(
            json!({"type": "chatgpt", "loginId": "l", "authUrl": "https://auth"}),
        )
        .unwrap();
        assert!(matches!(login, LoginResponse::Chatgpt { .. }));
    }
}

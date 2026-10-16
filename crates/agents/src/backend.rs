//! The agent backend abstraction.
//!
//! The UI and orchestrator talk to an [`AgentBackend`], not to Codex
//! directly, so another backend that can host threads, stream events and ask
//! for approvals could be dropped in without touching the GPUI views.
//! Futures are boxed and `'static` so the trait is object safe and any
//! executor can drive them.

use anyhow::Result;
use futures::future::BoxFuture;

use crate::{
    client::CodexClient,
    protocol::*,
    rpc::RequestId,
};

/// Settings for a new thread.
#[derive(Clone, Debug, Default)]
pub struct ThreadConfig {
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub sandbox: Option<SandboxMode>,
    pub developer_instructions: Option<String>,
    pub dynamic_tools: Vec<DynamicToolSpec>,
}

/// One user turn.
#[derive(Clone, Debug)]
pub struct Prompt {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub approval_policy: Option<AskForApproval>,
}

pub trait AgentBackend: Send + Sync + 'static {
    /// Human-readable backend name ("Codex").
    fn name(&self) -> &str;
    fn start_thread(&self, config: ThreadConfig) -> BoxFuture<'static, Result<ThreadStartResponse>>;
    fn resume_thread(&self, thread_id: String, config: ThreadConfig) -> BoxFuture<'static, Result<ThreadResumeResponse>>;
    /// Starts a turn and resolves to its id once the server accepted it.
    fn submit_prompt(&self, prompt: Prompt) -> BoxFuture<'static, Result<Turn>>;
    fn interrupt(&self, thread_id: String, turn_id: String) -> BoxFuture<'static, Result<()>>;
    fn resolve_approval(&self, request_id: &RequestId, decision: ApprovalDecision) -> Result<()>;
    fn respond_tool_call(&self, request_id: &RequestId, response: DynamicToolCallResponse) -> Result<()>;
    fn models(&self) -> BoxFuture<'static, Result<Vec<Model>>>;
    fn account(&self) -> BoxFuture<'static, Result<GetAccountResponse>>;
    fn login(&self, params: LoginParams) -> BoxFuture<'static, Result<LoginResponse>>;
    fn shutdown(&self);
}

impl AgentBackend for CodexClient {
    fn name(&self) -> &str {
        "Codex"
    }

    fn start_thread(&self, config: ThreadConfig) -> BoxFuture<'static, Result<ThreadStartResponse>> {
        Box::pin(self.thread_start(ThreadStartParams {
            model: config.model,
            cwd: config.cwd,
            approval_policy: config.approval_policy,
            sandbox: config.sandbox,
            developer_instructions: config.developer_instructions,
            ephemeral: None,
            dynamic_tools: (!config.dynamic_tools.is_empty()).then_some(config.dynamic_tools),
        }))
    }

    fn resume_thread(&self, thread_id: String, config: ThreadConfig) -> BoxFuture<'static, Result<ThreadResumeResponse>> {
        Box::pin(self.thread_resume(ThreadResumeParams {
            thread_id,
            model: config.model,
            cwd: config.cwd,
            approval_policy: config.approval_policy,
            sandbox: config.sandbox,
        }))
    }

    fn submit_prompt(&self, prompt: Prompt) -> BoxFuture<'static, Result<Turn>> {
        let request = self.turn_start(TurnStartParams {
            thread_id: prompt.thread_id,
            input: prompt.input,
            cwd: None,
            approval_policy: prompt.approval_policy,
            model: prompt.model,
            effort: prompt.effort,
        });
        Box::pin(async move { Ok(request.await?.turn) })
    }

    fn interrupt(&self, thread_id: String, turn_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(self.turn_interrupt(&thread_id, &turn_id))
    }

    fn resolve_approval(&self, request_id: &RequestId, decision: ApprovalDecision) -> Result<()> {
        CodexClient::resolve_approval(self, request_id, decision)
    }

    fn respond_tool_call(&self, request_id: &RequestId, response: DynamicToolCallResponse) -> Result<()> {
        CodexClient::respond_tool_call(self, request_id, response)
    }

    fn models(&self) -> BoxFuture<'static, Result<Vec<Model>>> {
        Box::pin(CodexClient::models(self))
    }

    fn account(&self) -> BoxFuture<'static, Result<GetAccountResponse>> {
        Box::pin(CodexClient::account(self))
    }

    fn login(&self, params: LoginParams) -> BoxFuture<'static, Result<LoginResponse>> {
        Box::pin(CodexClient::login(self, params))
    }

    fn shutdown(&self) {
        CodexClient::shutdown(self)
    }
}

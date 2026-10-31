//! Agent integrations: OpenAI Codex and Anthropic Claude.
//!
//! Cockpit drives Codex through its App Server (`codex app-server`), the
//! same JSON-RPC interface the official IDE extensions use, and Claude
//! through Claude Code's headless stream-json mode ([`claude`]), the
//! interface the Claude Agent SDK uses. This crate is the Codex transport
//! ([`client`]), the typed protocol both backends speak ([`protocol`]), a
//! normalized event stream ([`events`]), the conversation model the UI
//! renders ([`transcript`]) and the backend abstraction ([`backend`]).
//!
//! Nothing here depends on GPUI. Futures returned by the client can be
//! awaited from any executor; process I/O runs on a private Tokio runtime
//! owned by [`AgentRuntime`].

pub mod backend;
pub mod claude;
pub mod client;
pub mod events;
pub mod protocol;
pub mod rpc;
pub mod transcript;

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};

pub use backend::{AgentBackend, Prompt, ThreadConfig};
pub use claude::{ClaudeClient, ClaudeLaunch};
pub use client::{CodexClient, CodexLaunch};
pub use events::{AgentEvent, ApprovalKind, PendingApproval, ToolCall};
pub use transcript::{ApprovalState, Entry, EntryKind, Transcript};

/// Owns the Tokio runtime agent processes run on.
pub struct AgentRuntime {
    runtime: tokio::runtime::Runtime,
}

/// Which company's agent a thread runs on.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Codex, through the Codex App Server.
    #[default]
    OpenAi,
    /// Claude, through Claude Code.
    Anthropic,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::OpenAi, Provider::Anthropic];

    /// The agent's name ("Codex", "Claude").
    pub fn agent_name(self) -> &'static str {
        match self {
            Provider::OpenAi => "Codex",
            Provider::Anthropic => "Claude",
        }
    }

    pub fn company(self) -> &'static str {
        match self {
            Provider::OpenAi => "OpenAI",
            Provider::Anthropic => "Anthropic",
        }
    }

    /// The value stored with each thread.
    pub fn key(self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
        }
    }

    /// Reads a stored key; threads saved before providers existed are Codex's.
    pub fn from_key(key: Option<&str>) -> Provider {
        match key {
            Some("anthropic") => Provider::Anthropic,
            _ => Provider::OpenAi,
        }
    }
}

/// A live, initialized connection to an agent backend.
pub struct Connection {
    pub backend: Arc<dyn AgentBackend>,
    /// The Codex client, when this is a Codex connection.
    pub client: Option<CodexClient>,
    pub events: async_channel::Receiver<AgentEvent>,
    pub server: protocol::InitializeResponse,
}

impl AgentRuntime {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("cockpit-agents")
            .enable_all()
            .build()
            .context("starting the agent runtime")?;
        Ok(Self { runtime })
    }

    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// Launches `codex app-server` in `cwd` and performs the handshake. The
    /// returned future can be awaited from any executor.
    pub fn connect_codex(
        &self,
        launch: CodexLaunch,
        cwd: PathBuf,
        experimental_api: bool,
    ) -> impl std::future::Future<Output = Result<Connection>> + Send + 'static {
        let handle = self.handle();
        let task = self.runtime.spawn(async move {
            let (client, events) = CodexClient::spawn(&launch, &cwd, &handle)?;
            let server = client
                .initialize(experimental_api)
                .await
                .context("the Codex app server did not complete its handshake")?;
            Ok::<_, anyhow::Error>(Connection {
                backend: Arc::new(client.clone()),
                client: Some(client),
                events,
                server,
            })
        });
        async move { task.await.context("agent runtime stopped")? }
    }

    /// Checks that Claude Code runs and prepares a backend for `cwd`. Each
    /// thread starts its own Claude Code process when it first sends a
    /// prompt.
    pub fn connect_claude(
        &self,
        launch: ClaudeLaunch,
        cwd: PathBuf,
    ) -> impl std::future::Future<Output = Result<Connection>> + Send + 'static {
        let handle = self.handle();
        let task = self.runtime.spawn(async move {
            let (client, events) = ClaudeClient::connect(launch, cwd, handle)
                .await
                .context("Claude Code did not start")?;
            let server = protocol::InitializeResponse {
                user_agent: format!("Claude Code {}", client.version()),
                codex_home: None,
                platform_os: None,
            };
            Ok::<_, anyhow::Error>(Connection {
                backend: Arc::new(client),
                client: None,
                events,
                server,
            })
        });
        async move { task.await.context("agent runtime stopped")? }
    }
}

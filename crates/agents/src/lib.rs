//! Codex integration.
//!
//! Cockpit drives Codex through its App Server (`codex app-server`), the
//! same JSON-RPC interface the official IDE extensions use. This crate is the
//! transport ([`client`]), the typed protocol ([`protocol`]), a normalized
//! event stream ([`events`]), the conversation model the UI renders
//! ([`transcript`]) and the backend abstraction ([`backend`]).
//!
//! Nothing here depends on GPUI. Futures returned by the client can be
//! awaited from any executor; process I/O runs on a private Tokio runtime
//! owned by [`AgentRuntime`].

pub mod backend;
pub mod client;
pub mod events;
pub mod protocol;
pub mod rpc;
pub mod transcript;

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};

pub use backend::{AgentBackend, Prompt, ThreadConfig};
pub use client::{CodexClient, CodexLaunch};
pub use events::{AgentEvent, ApprovalKind, PendingApproval, ToolCall};
pub use transcript::{ApprovalState, Entry, EntryKind, Transcript};

/// Owns the Tokio runtime agent processes run on.
pub struct AgentRuntime {
    runtime: tokio::runtime::Runtime,
}

/// A live, initialized connection to an agent backend.
pub struct Connection {
    pub backend: Arc<dyn AgentBackend>,
    pub client: CodexClient,
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
                client,
                events,
                server,
            })
        });
        async move { task.await.context("agent runtime stopped")? }
    }
}

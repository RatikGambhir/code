//! The transport: a connection to one `codex app-server` process.
//!
//! The client is runtime-agnostic at its edges: requests return futures that
//! any executor can await (GPUI's included), and events arrive on an
//! `async_channel`. Internally the reader and writer run as tasks on a Tokio
//! runtime, which owns the child process's pipes.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, Context as _, Result};
use parking_lot::Mutex;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};

use crate::{
    events::AgentEvent,
    protocol::*,
    rpc::{self, Message, RequestId, RpcError},
};

/// Requests awaiting a response. `None` once the connection has closed, so a
/// request made after that fails at once instead of waiting forever.
type PendingMap = Arc<Mutex<Option<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>>;

/// Fails everything still waiting and refuses new requests.
fn close_pending(pending: &PendingMap, reason: &str) {
    if let Some(waiters) = pending.lock().take() {
        for (_, waiter) in waiters {
            let _ = waiter.send(Err(RpcError {
                code: RpcError::INTERNAL_ERROR,
                message: reason.to_string(),
                data: None,
            }));
        }
    }
}

/// How to launch the app server.
#[derive(Clone, Debug)]
pub struct CodexLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Extra `-c key=value` overrides.
    pub config_overrides: Vec<(String, String)>,
    pub env: Vec<(String, String)>,
}

impl CodexLaunch {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: vec!["app-server".into()],
            config_overrides: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Locates `codex`: `$COCKPIT_CODEX_BIN`, then `PATH`, then the usual
    /// install locations.
    pub fn discover() -> Option<Self> {
        if let Some(path) = std::env::var_os("COCKPIT_CODEX_BIN") {
            return Some(Self::new(path));
        }
        let mut dirs = cockpit_common::shell_env::common_tool_dirs();
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(Path::new(&home).join(".codex/bin"));
        }
        cockpit_common::shell_env::find_executable("codex", &dirs).map(Self::new)
    }
}

#[derive(Clone)]
pub struct CodexClient {
    inner: Arc<Inner>,
}

struct Inner {
    outgoing: mpsc::UnboundedSender<Value>,
    pending: PendingMap,
    next_id: AtomicI64,
    /// The child process, when we launched one. Killed on drop.
    child: Mutex<Option<tokio::process::Child>>,
    runtime: tokio::runtime::Handle,
}

impl CodexClient {
    /// Launches the app server on `runtime` and returns the client plus its
    /// event stream. The `initialize` handshake is not performed yet.
    pub fn spawn(
        launch: &CodexLaunch,
        cwd: &Path,
        runtime: &tokio::runtime::Handle,
    ) -> Result<(Self, async_channel::Receiver<AgentEvent>)> {
        let _guard = runtime.enter();
        let mut command = tokio::process::Command::new(&launch.program);
        command.args(&launch.args);
        for (key, value) in &launch.config_overrides {
            command.arg("-c").arg(format!("{key}={value}"));
        }
        command
            .current_dir(cwd)
            .envs(launch.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", launch.program.display()))?;
        let stdin = child.stdin.take().context("app server has no stdin")?;
        let stdout = child.stdout.take().context("app server has no stdout")?;
        if let Some(stderr) = child.stderr.take() {
            runtime.spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "codex", "{line}");
                }
            });
        }
        let (client, events) = Self::connect(stdout, stdin, runtime);
        *client.inner.child.lock() = Some(child);
        Ok((client, events))
    }

    /// Wires a client to an existing transport (used by tests and for
    /// connecting to a daemon over a socket).
    pub fn connect(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        runtime: &tokio::runtime::Handle,
    ) -> (Self, async_channel::Receiver<AgentEvent>) {
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<Value>();
        let (events_tx, events_rx) = async_channel::unbounded();
        let pending: PendingMap = Arc::new(Mutex::new(Some(HashMap::new())));

        // Writer: one JSON object per line.
        let writer_pending = pending.clone();
        runtime.spawn(async move {
            let mut writer = writer;
            while let Some(message) = outgoing_rx.recv().await {
                let mut line = match serde_json::to_vec(&message) {
                    Ok(line) => line,
                    Err(err) => {
                        tracing::error!("could not encode message: {err}");
                        continue;
                    }
                };
                line.push(b'\n');
                if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                    close_pending(&writer_pending, "writing to the Codex app server failed");
                    break;
                }
            }
        });

        let client = Self {
            inner: Arc::new(Inner {
                outgoing: outgoing_tx,
                pending: pending.clone(),
                next_id: AtomicI64::new(1),
                child: Mutex::new(None),
                runtime: runtime.clone(),
            }),
        };

        // Reader: route responses to their waiters, everything else to events.
        let responder = client.inner.outgoing.clone();
        runtime.spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            let reason = loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => match Message::parse(&line) {
                        Ok(Message::Response { id, result }) => {
                            let RequestId::Int(id) = id else { continue };
                            let waiter = pending.lock().as_mut().and_then(|map| map.remove(&id));
                            if let Some(waiter) = waiter {
                                let _ = waiter.send(result);
                            }
                        }
                        Ok(Message::Notification { method, params }) => {
                            let _ = events_tx.send(AgentEvent::from_notification(&method, params)).await;
                        }
                        Ok(Message::Request { id, method, params }) => {
                            match AgentEvent::from_request(id.clone(), &method, params) {
                                Ok(event) => {
                                    let _ = events_tx.send(event).await;
                                }
                                Err(error) => {
                                    tracing::info!("declining server request {method}: {error}");
                                    let _ = responder.send(rpc::response(&id, Err(error)));
                                }
                            }
                        }
                        Err(err) => tracing::warn!("unparseable line from app server: {err}"),
                    },
                    Ok(None) => break "the Codex app server exited".to_string(),
                    Err(err) => break format!("reading from the Codex app server failed: {err}"),
                }
            };
            close_pending(&pending, &reason);
            let _ = events_tx.send(AgentEvent::Disconnected { reason }).await;
        });

        (client, events_rx)
    }

    /// Sends a request and awaits its decoded result. The returned future is
    /// `Send + 'static` and does not need a Tokio context to be polled.
    pub fn request<P: Serialize, R: DeserializeOwned + Send + 'static>(
        &self,
        method: &str,
        params: P,
    ) -> impl std::future::Future<Output = Result<R>> + Send + 'static {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let method = method.to_string();
        let sent = serde_json::to_value(params)
            .map_err(anyhow::Error::from)
            .and_then(|params| {
                match self.inner.pending.lock().as_mut() {
                    Some(map) => map.insert(id, tx),
                    None => anyhow::bail!("the Codex app server is not running"),
                };
                self.inner
                    .outgoing
                    .send(rpc::request(id, &method, params))
                    .map_err(|_| anyhow!("the Codex app server is not running"))
            });
        if sent.is_err() {
            if let Some(map) = self.inner.pending.lock().as_mut() {
                map.remove(&id);
            }
        }
        async move {
            sent?;
            let value = rx
                .await
                .map_err(|_| anyhow!("{method}: connection closed"))?
                .map_err(|err| anyhow!("{method}: {err}"))?;
            serde_json::from_value(value).with_context(|| format!("decoding the {method} response"))
        }
    }

    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        self.inner
            .outgoing
            .send(rpc::notification(method, params))
            .map_err(|_| anyhow!("the Codex app server is not running"))
    }

    /// Answers a request the server made (an approval, a tool call).
    pub fn respond(&self, id: &RequestId, result: Result<Value, RpcError>) -> Result<()> {
        self.inner
            .outgoing
            .send(rpc::response(id, result))
            .map_err(|_| anyhow!("the Codex app server is not running"))
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.inner.runtime
    }

    // --- typed API -------------------------------------------------------

    /// `initialize` followed by the `initialized` notification.
    pub async fn initialize(&self, experimental_api: bool) -> Result<InitializeResponse> {
        let response: InitializeResponse = self
            .request(
                "initialize",
                InitializeParams {
                    client_info: ClientInfo {
                        name: "cockpit".into(),
                        title: Some(cockpit_common::APP_NAME.into()),
                        version: cockpit_common::APP_VERSION.into(),
                    },
                    capabilities: Some(InitializeCapabilities {
                        experimental_api,
                        request_attestation: false,
                        opt_out_notification_methods: None,
                    }),
                },
            )
            .await?;
        self.notify("initialized", None)?;
        Ok(response)
    }

    pub fn thread_start(
        &self,
        params: ThreadStartParams,
    ) -> impl std::future::Future<Output = Result<ThreadStartResponse>> + Send + 'static {
        self.request("thread/start", params)
    }

    pub fn thread_resume(
        &self,
        params: ThreadResumeParams,
    ) -> impl std::future::Future<Output = Result<ThreadResumeResponse>> + Send + 'static {
        self.request("thread/resume", params)
    }

    pub fn turn_start(
        &self,
        params: TurnStartParams,
    ) -> impl std::future::Future<Output = Result<TurnStartResponse>> + Send + 'static {
        self.request("turn/start", params)
    }

    pub fn turn_interrupt(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
        let request = self.request::<_, Value>(
            "turn/interrupt",
            TurnInterruptParams {
                thread_id: thread_id.into(),
                turn_id: turn_id.into(),
            },
        );
        async move { request.await.map(|_| ()) }
    }

    /// Every model the server offers (following pagination).
    pub fn models(&self) -> impl std::future::Future<Output = Result<Vec<Model>>> + Send + 'static {
        let client = self.clone();
        async move {
            let mut models = Vec::new();
            let mut cursor = None;
            loop {
                let page: ModelListResponse = client
                    .request(
                        "model/list",
                        ModelListParams {
                            cursor: cursor.take(),
                            limit: Some(100),
                        },
                    )
                    .await?;
                models.extend(page.data.into_iter().filter(|m| !m.hidden));
                match page.next_cursor {
                    Some(next) if !next.is_empty() => cursor = Some(next),
                    _ => break,
                }
            }
            Ok(models)
        }
    }

    pub fn account(&self) -> impl std::future::Future<Output = Result<GetAccountResponse>> + Send + 'static {
        self.request("account/read", GetAccountParams { refresh_token: false })
    }

    pub fn login(&self, params: LoginParams) -> impl std::future::Future<Output = Result<LoginResponse>> + Send + 'static {
        self.request("account/login/start", params)
    }

    pub fn resolve_approval(&self, request_id: &RequestId, decision: ApprovalDecision) -> Result<()> {
        self.respond(
            request_id,
            Ok(serde_json::to_value(ApprovalResponse { decision })?),
        )
    }

    pub fn respond_tool_call(&self, request_id: &RequestId, response: DynamicToolCallResponse) -> Result<()> {
        self.respond(request_id, Ok(serde_json::to_value(response)?))
    }

    /// Stops the app server, if this client launched it.
    pub fn shutdown(&self) {
        if let Some(mut child) = self.inner.child.lock().take() {
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A scripted fake app server on the far side of an in-memory pipe.
    async fn fake_server(
        stream: tokio::io::DuplexStream,
    ) -> Vec<Value> {
        let (read, mut write) = tokio::io::split(stream);
        let mut lines = BufReader::new(read).lines();
        let mut seen = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).unwrap();
            seen.push(message.clone());
            let reply = match message["method"].as_str() {
                Some("initialize") => Some(json!({"id": message["id"], "result": {
                    "userAgent": "fake/1.0", "codexHome": "/tmp/codex", "platformOs": "macos"}})),
                Some("thread/start") => Some(json!({"id": message["id"], "result": {
                    "thread": {"id": "thr_1", "preview": "", "turns": []}, "model": "m1"}})),
                Some("turn/start") => {
                    // Ask for approval before answering, like the real server.
                    let approval = json!({"id": 900, "method": "item/commandExecution/requestApproval",
                        "params": {"threadId": "thr_1", "turnId": "turn_1", "itemId": "item_1",
                                   "command": "cargo test", "cwd": "/w", "startedAtMs": 0}});
                    write.write_all(format!("{approval}\n").as_bytes()).await.unwrap();
                    let delta = json!({"method": "item/agentMessage/delta", "params": {
                        "threadId": "thr_1", "turnId": "turn_1", "itemId": "msg_1", "delta": "Hi"},
                        "emittedAtMs": 1});
                    write.write_all(format!("{delta}\n").as_bytes()).await.unwrap();
                    Some(json!({"id": message["id"], "result": {"turn": {
                        "id": "turn_1", "items": [], "status": "inProgress", "error": null}}}))
                }
                Some("model/list") => {
                    let cursor = message["params"]["cursor"].as_str();
                    Some(match cursor {
                        None => json!({"id": message["id"], "result": {"data": [
                            {"id": "a", "model": "a", "displayName": "A", "isDefault": true},
                            {"id": "hidden", "model": "hidden", "hidden": true}], "nextCursor": "p2"}}),
                        Some(_) => json!({"id": message["id"], "result": {"data": [
                            {"id": "b", "model": "b", "displayName": "B"}], "nextCursor": null}}),
                    })
                }
                Some("unknown/method") => Some(json!({"id": message["id"],
                    "error": {"code": -32600, "message": "Invalid request: unknown variant"}})),
                Some("shutdown/now") => break,
                _ => None,
            };
            if let Some(reply) = reply {
                write.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
            }
        }
        seen
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_turn_approval_and_disconnect() {
        let (client_end, server_end) = duplex(64 * 1024);
        let server = tokio::spawn(fake_server(server_end));
        let (read, write) = tokio::io::split(client_end);
        let (client, events) = CodexClient::connect(read, write, &tokio::runtime::Handle::current());

        let init = client.initialize(true).await.unwrap();
        assert_eq!(init.user_agent, "fake/1.0");

        let thread = client.thread_start(ThreadStartParams::default()).await.unwrap();
        assert_eq!(thread.thread.id, "thr_1");

        let turn = client
            .turn_start(TurnStartParams {
                thread_id: "thr_1".into(),
                input: vec![UserInput::text("run the tests")],
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                model: None,
                effort: None,
            })
            .await
            .unwrap();
        assert_eq!(turn.turn.status, TurnStatus::InProgress);

        let AgentEvent::ApprovalRequested(approval) = events.recv().await.unwrap() else {
            panic!("expected an approval request");
        };
        assert_eq!(approval.request_id, RequestId::Int(900));
        client.resolve_approval(&approval.request_id, ApprovalDecision::Accept).unwrap();
        assert!(matches!(events.recv().await.unwrap(), AgentEvent::AgentMessageDelta { .. }));

        let models = client.models().await.unwrap();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);

        let err = client.request::<_, Value>("unknown/method", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "{err}");

        client.notify("shutdown/now", None).unwrap();
        let seen = server.await.unwrap();
        assert!(matches!(events.recv().await.unwrap(), AgentEvent::Disconnected { .. }));

        // The approval answer went back with the server's id.
        let answer = seen.iter().find(|m| m["id"] == 900).expect("approval response");
        assert_eq!(answer["result"]["decision"], "accept");
        // `initialized` follows `initialize`.
        assert_eq!(seen[1]["method"], "initialized");
        // Requests after the disconnect fail instead of hanging.
        assert!(client.account().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsupported_server_requests_are_answered_with_an_error() {
        let (client_end, server_end) = duplex(4096);
        let (read, write) = tokio::io::split(client_end);
        let (_client, _events) = CodexClient::connect(read, write, &tokio::runtime::Handle::current());
        let (server_read, mut server_write) = tokio::io::split(server_end);
        server_write
            .write_all(b"{\"id\":\"x1\",\"method\":\"attestation/generate\",\"params\":{}}\n")
            .await
            .unwrap();
        let mut lines = BufReader::new(server_read).lines();
        let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(reply["id"], "x1");
        assert_eq!(reply["error"]["code"], RpcError::METHOD_NOT_FOUND);
    }
}

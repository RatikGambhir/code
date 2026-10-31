//! Claude integration: Claude Code driven as a headless agent.
//!
//! Cockpit runs the Claude Code CLI the way the Claude Agent SDK does:
//! `claude -p --input-format stream-json --output-format stream-json`, one
//! process per thread, prompts written to stdin and messages read from
//! stdout. Permission prompts arrive as `can_use_tool` control requests
//! (`--permission-prompt-tool stdio`) and become ordinary Cockpit
//! approvals; Cockpit's dynamic tools are served to Claude as an in-process
//! MCP server. Claude Code owns sign-in (claude.ai, Console, API key or a
//! cloud provider) and its credentials; Cockpit never sees them.
//!
//! [`mapper`] turns Claude Code's messages into [`AgentEvent`]s, so the
//! transcript, orchestrator and UI work for Claude exactly as for Codex.

pub mod history;
pub mod mapper;
pub mod models;

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context as _, Result};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};

use self::mapper::{control_request, user_message, Mapper, McpServerSpec, Output};
pub use self::models::DEFAULT_MODEL;
use crate::{
    backend::{AgentBackend, Prompt, ThreadConfig},
    events::AgentEvent,
    protocol::*,
    rpc::RequestId,
};

/// How to launch Claude Code.
#[derive(Clone, Debug)]
pub struct ClaudeLaunch {
    pub program: PathBuf,
    pub env: Vec<(String, String)>,
}

impl ClaudeLaunch {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), env: Vec::new() }
    }

    /// Locates `claude`: `$COCKPIT_CLAUDE_BIN`, then `PATH`, then the usual
    /// install locations (the native installer's `~/.local/bin`, the older
    /// `~/.claude/local`, Homebrew and npm).
    pub fn discover() -> Option<Self> {
        if let Some(path) = std::env::var_os("COCKPIT_CLAUDE_BIN") {
            return Some(Self::new(path));
        }
        let mut dirs = cockpit_common::shell_env::common_tool_dirs();
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(Path::new(&home).join(".claude/local"));
        }
        cockpit_common::shell_env::find_executable("claude", &dirs).map(Self::new)
    }

    fn command(&self, cwd: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .current_dir(cwd)
            .envs(self.env.iter().cloned())
            // Set when Cockpit itself was started from a Claude Code shell.
            .env_remove("CLAUDECODE")
            .kill_on_drop(true);
        command
    }
}

/// Claude Code's permission modes, from Cockpit's permission presets.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PermissionMode {
    /// Asks before edits and commands.
    Default,
    /// Edits files without asking; asks before commands.
    AcceptEdits,
    /// Never asks.
    BypassPermissions,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn from_sandbox_policy(policy: SandboxPolicy) -> Self {
        match policy {
            SandboxPolicy::ReadOnly => PermissionMode::Default,
            SandboxPolicy::WorkspaceWrite => PermissionMode::AcceptEdits,
            SandboxPolicy::DangerFullAccess => PermissionMode::BypassPermissions,
        }
    }

    pub fn from_sandbox_mode(mode: SandboxMode) -> Self {
        match mode {
            SandboxMode::ReadOnly => PermissionMode::Default,
            SandboxMode::WorkspaceWrite => PermissionMode::AcceptEdits,
            SandboxMode::DangerFullAccess => PermissionMode::BypassPermissions,
        }
    }
}

/// What a thread's process is started with. A change takes effect by
/// restarting the process (between turns) with `--resume`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSettings {
    pub model: String,
    pub effort: Option<String>,
    pub mode: PermissionMode,
    pub append_system_prompt: Option<String>,
    pub mcp_servers: Vec<McpServerSpec>,
}

/// The command line for one thread's process.
pub fn process_args(settings: &SessionSettings, session_id: &str, resume: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
    ]
    .map(String::from)
    .to_vec();
    args.extend(["--model".into(), settings.model.clone()]);
    if let Some(effort) = settings.effort.as_ref().filter(|e| !e.is_empty()) {
        args.extend(["--effort".into(), effort.clone()]);
    }
    args.extend(["--permission-mode".into(), settings.mode.as_str().into()]);
    if settings.mode == PermissionMode::BypassPermissions {
        args.push("--allow-dangerously-skip-permissions".into());
    }
    if resume {
        args.extend(["--resume".into(), session_id.into()]);
    } else {
        args.extend(["--session-id".into(), session_id.into()]);
    }
    if let Some(prompt) = settings.append_system_prompt.as_ref().filter(|p| !p.is_empty()) {
        args.extend(["--append-system-prompt".into(), prompt.clone()]);
    }
    if !settings.mcp_servers.is_empty() {
        let servers: serde_json::Map<String, Value> = settings
            .mcp_servers
            .iter()
            .map(|s| (s.name.clone(), json!({"type": "sdk", "name": s.name})))
            .collect();
        args.extend(["--mcp-config".into(), json!({"mcpServers": servers}).to_string()]);
        // Cockpit's tool policy vets these calls; Claude need not ask too.
        args.push("--allowedTools".into());
        args.extend(settings.mcp_servers.iter().map(|s| format!("mcp__{}", s.name)));
    }
    // There is no way to answer a question tool from a headless session.
    args.extend(["--disallowedTools".into(), "AskUserQuestion".into()]);
    args
}

#[derive(Clone)]
pub struct ClaudeClient {
    inner: Arc<Inner>,
}

struct Inner {
    launch: ClaudeLaunch,
    cwd: PathBuf,
    runtime: tokio::runtime::Handle,
    events: async_channel::Sender<AgentEvent>,
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
    version: String,
    auth_failed: AtomicBool,
    models: Mutex<Option<Vec<Model>>>,
    next_request: AtomicU64,
}

struct Session {
    id: String,
    cwd: PathBuf,
    mapper: Mapper,
    settings: SessionSettings,
    /// Claude Code has stored this session, so it is resumed, not created.
    has_history: bool,
    process: Option<Process>,
    /// Bumped whenever the process is replaced, so a dying process's reader
    /// cannot touch its successor's state.
    generation: u64,
}

struct Process {
    settings: SessionSettings,
    outgoing: mpsc::UnboundedSender<Value>,
    child: tokio::process::Child,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl ClaudeClient {
    /// Finds out which Claude Code is installed. No process stays running:
    /// each thread starts its own when it first sends a prompt.
    pub async fn connect(
        launch: ClaudeLaunch,
        cwd: PathBuf,
        runtime: tokio::runtime::Handle,
    ) -> Result<(Self, async_channel::Receiver<AgentEvent>)> {
        let output = tokio::time::timeout(Duration::from_secs(30), launch.command(&cwd).arg("--version").output())
            .await
            .context("`claude --version` did not answer within 30 seconds")?
            .with_context(|| format!("starting {}", launch.program.display()))?;
        if !output.status.success() {
            bail!(
                "`claude --version` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let version = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("unknown")
            .to_string();
        let (events_tx, events_rx) = async_channel::unbounded();
        let client = Self {
            inner: Arc::new(Inner {
                launch,
                cwd,
                runtime,
                events: events_tx,
                sessions: Mutex::new(HashMap::new()),
                version,
                auth_failed: AtomicBool::new(false),
                models: Mutex::new(None),
                next_request: AtomicU64::new(1),
            }),
        };
        Ok((client, events_rx))
    }

    /// Claude Code's version, e.g. "2.1.131".
    pub fn version(&self) -> &str {
        &self.inner.version
    }

    pub fn launch(&self) -> &ClaudeLaunch {
        &self.inner.launch
    }

    fn request_id(&self, prefix: &str) -> String {
        format!("cockpit-{prefix}-{}", self.inner.next_request.fetch_add(1, Ordering::Relaxed))
    }

    fn session(&self, thread_id: &str) -> Result<Arc<Mutex<Session>>> {
        self.inner
            .sessions
            .lock()
            .get(thread_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown Claude thread {thread_id}"))
    }

    fn publish(&self, events: impl IntoIterator<Item = AgentEvent>) {
        for event in events {
            // Unbounded: fails only once nobody listens any more.
            let _ = self.inner.events.try_send(event);
        }
    }

    fn register(&self, id: String, cwd: PathBuf, settings: SessionSettings, has_history: bool) -> Arc<Mutex<Session>> {
        let mut sessions = self.inner.sessions.lock();
        if let Some(existing) = sessions.get(&id) {
            return existing.clone();
        }
        let mut mapper = Mapper::new(id.clone(), cwd.to_string_lossy(), random_hex(3));
        mapper.set_mcp_servers(settings.mcp_servers.clone());
        let session = Arc::new(Mutex::new(Session {
            id: id.clone(),
            cwd,
            mapper,
            settings,
            has_history,
            process: None,
            generation: 0,
        }));
        sessions.insert(id, session.clone());
        session
    }

    fn settings_for(&self, config: &ThreadConfig) -> SessionSettings {
        SessionSettings {
            model: config.model.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            effort: None,
            mode: config.sandbox.map(PermissionMode::from_sandbox_mode).unwrap_or(PermissionMode::Default),
            append_system_prompt: config.developer_instructions.clone(),
            mcp_servers: McpServerSpec::from_dynamic(&config.dynamic_tools),
        }
    }

    /// Starts the thread's process. Called with the session locked.
    fn spawn_process(&self, handle: &Arc<Mutex<Session>>, session: &mut Session) -> Result<()> {
        let _guard = self.inner.runtime.enter();
        let args = process_args(&session.settings, &session.id, session.has_history);
        let mut command = self.inner.launch.command(&session.cwd);
        command.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", self.inner.launch.program.display()))?;
        let stdin = child.stdin.take().context("Claude Code has no stdin")?;
        let stdout = child.stdout.take().context("Claude Code has no stdout")?;
        let stderr = child.stderr.take();

        session.generation += 1;
        let generation = session.generation;
        let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<Value>();

        // Writer: one JSON object per line.
        self.inner.runtime.spawn(async move {
            let mut stdin = stdin;
            while let Some(message) = outgoing_rx.recv().await {
                let mut line = message.to_string().into_bytes();
                line.push(b'\n');
                if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // Stderr: kept for the error shown if the process dies.
        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        if let Some(stderr) = stderr {
            let tail = tail.clone();
            self.inner.runtime.spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "claude", "{line}");
                    let mut tail = tail.lock();
                    if tail.len() == 12 {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }

        // Reader: everything goes through the mapper under the session lock,
        // so events reach the UI in the order Claude Code sent them.
        let client = self.clone();
        let handle = handle.clone();
        let writer = outgoing.clone();
        self.inner.runtime.spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) | Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                let mut session = handle.lock();
                if session.generation != generation {
                    return;
                }
                let outputs = session.mapper.handle_line(&line);
                if session.mapper.take_auth_failure() {
                    client.inner.auth_failed.store(true, Ordering::Relaxed);
                }
                for output in outputs {
                    match output {
                        Output::Event(event) => client.publish([event]),
                        Output::Write(message) => {
                            let _ = writer.send(message);
                        }
                        Output::ControlResponse { request_id, result: Err(error) } => {
                            tracing::warn!(target: "claude", "control request {request_id} failed: {error}");
                        }
                        Output::ControlResponse { .. } => {}
                    }
                }
            }
            // Give stderr a moment to deliver the reason.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut session = handle.lock();
            if session.generation != generation {
                return;
            }
            session.process = None;
            let detail = tail.lock().iter().rev().take(4).rev().cloned().collect::<Vec<_>>().join("\n");
            let reason = if detail.is_empty() {
                "Claude Code exited unexpectedly.".to_string()
            } else {
                format!("Claude Code exited: {detail}")
            };
            let events = session.mapper.process_exited(&reason);
            client.publish(events);
        });

        let _ = outgoing.send(control_request(
            &self.request_id("init"),
            json!({
                "subtype": "initialize",
                "hooks": null,
                "sdkMcpServers": session.settings.mcp_servers.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            }),
        ));
        session.process = Some(Process { settings: session.settings.clone(), outgoing, child });
        Ok(())
    }

    fn submit(&self, prompt: Prompt) -> Result<Turn> {
        let handle = self.session(&prompt.thread_id)?;
        let text = prompt_text(&prompt.input);
        let mut session = handle.lock();
        if session.mapper.is_running() {
            bail!("a turn is already running in this thread");
        }
        session.settings.model = prompt.model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
        session.settings.effort = prompt.effort;
        if let Some(policy) = prompt.sandbox_policy {
            session.settings.mode = PermissionMode::from_sandbox_policy(policy);
        }
        if session.process.as_ref().is_some_and(|p| p.settings != session.settings) {
            // Model, effort or permissions changed: restart with --resume.
            session.process = None;
            session.generation += 1;
        }
        if session.process.is_none() {
            self.spawn_process(&handle, &mut session)?;
        }
        let (turn, events) = session.mapper.begin_turn(&text);
        self.publish(events);
        let process = session.process.as_ref().expect("spawned above");
        if process.outgoing.send(user_message(&text)).is_err() {
            let events = session.mapper.process_exited("Claude Code is not running.");
            self.publish(events);
            bail!("Claude Code is not running");
        }
        // From here on the session exists on disk.
        session.has_history = true;
        Ok(turn)
    }

    fn write(&self, session: &Session, message: Value) -> Result<()> {
        session
            .process
            .as_ref()
            .ok_or_else(|| anyhow!("Claude Code is not running for this thread"))?
            .outgoing
            .send(message)
            .map_err(|_| anyhow!("Claude Code is not running for this thread"))
    }

    /// Runs `future` on the agent runtime, so the returned future can be
    /// awaited from any executor (GPUI's included).
    fn on_runtime<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> BoxFuture<'static, Result<T>> {
        let task = self.inner.runtime.spawn(future);
        Box::pin(async move { task.await.context("agent runtime stopped")? })
    }

    fn sessions(&self) -> Vec<Arc<Mutex<Session>>> {
        self.inner.sessions.lock().values().cloned().collect()
    }

    /// Runs a short-lived Claude Code to read its `initialize` response
    /// (models, account). No prompt is sent, so nothing is billed.
    async fn probe(&self) -> Result<Value> {
        let mut command = self.inner.launch.command(&self.inner.cwd);
        command
            .args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().context("starting Claude Code")?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let request = control_request("cockpit-probe", json!({"subtype": "initialize", "hooks": null}));
        stdin.write_all(format!("{request}\n").as_bytes()).await?;
        stdin.flush().await?;
        let read = async {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                if message["type"] == "control_response" && message["response"]["request_id"] == "cockpit-probe" {
                    return Ok(message["response"]["response"].clone());
                }
            }
            Err(anyhow!("Claude Code exited before answering"))
        };
        let result = tokio::time::timeout(Duration::from_secs(20), read)
            .await
            .unwrap_or_else(|_| Err(anyhow!("Claude Code did not answer within 20 seconds")));
        drop(stdin);
        let _ = child.start_kill();
        result
    }
}

/// The text of a prompt. Images are referred to by path; Claude can read
/// them with its Read tool.
fn prompt_text(input: &[UserInput]) -> String {
    input
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.clone()),
            UserInput::LocalImage { path } => Some(format!("[image: {path}]")),
            UserInput::Image { url } => Some(format!("[image: {url}]")),
            UserInput::Mention { path, .. } => Some(format!("@{path}")),
            UserInput::Unknown => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buffer))
        .is_ok();
    if !filled {
        // No /dev/urandom: time, pid and a counter are unique enough.
        use std::hash::{BuildHasher, Hasher};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        for (ix, chunk) in buffer.chunks_mut(8).enumerate() {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
            hasher.write_u32(std::process::id());
            hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed) + ix as u64);
            let value = hasher.finish().to_le_bytes();
            chunk.copy_from_slice(&value[..chunk.len()]);
        }
    }
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random (version 4) UUID, which Claude Code requires for session ids.
pub fn new_session_id() -> String {
    let hex = random_hex(16);
    let mut bytes: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0)).collect();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

impl AgentBackend for ClaudeClient {
    fn name(&self) -> &str {
        "Claude"
    }

    fn start_thread(&self, config: ThreadConfig) -> BoxFuture<'static, Result<ThreadStartResponse>> {
        let id = new_session_id();
        let cwd = config.cwd.clone().map(PathBuf::from).unwrap_or_else(|| self.inner.cwd.clone());
        let settings = self.settings_for(&config);
        let model = settings.model.clone();
        self.register(id.clone(), cwd.clone(), settings, false);
        let cwd = cwd.to_string_lossy().into_owned();
        Box::pin(async move {
            Ok(ThreadStartResponse {
                thread: Thread { id, model: Some(model.clone()), cwd: Some(cwd.clone()), ..Default::default() },
                model,
                cwd: Some(cwd),
                reasoning_effort: None,
            })
        })
    }

    fn resume_thread(&self, thread_id: String, config: ThreadConfig) -> BoxFuture<'static, Result<ThreadResumeResponse>> {
        let cwd = config.cwd.clone().map(PathBuf::from).unwrap_or_else(|| self.inner.cwd.clone());
        let mut settings = self.settings_for(&config);
        let client = self.clone();
        self.on_runtime(async move {
            let (id, cwd_text) = (thread_id.clone(), cwd.to_string_lossy().into_owned());
            let history = client
                .inner
                .runtime
                .spawn_blocking(move || {
                    let path = history::find_session_file(&id)?;
                    let text = std::fs::read_to_string(path).ok()?;
                    Some(history::turns_from_jsonl(&text, &id, &cwd_text))
                })
                .await
                .context("reading the Claude session")?;
            let found = history.is_some();
            let (turns, last_model) = history.unwrap_or_default();
            if config.model.is_none() {
                if let Some(model) = last_model {
                    settings.model = model;
                }
            }
            let model = settings.model.clone();
            client.register(thread_id.clone(), cwd.clone(), settings, found);
            Ok(ThreadResumeResponse {
                thread: Thread {
                    id: thread_id,
                    model: Some(model.clone()),
                    cwd: Some(cwd.to_string_lossy().into_owned()),
                    turns,
                    ..Default::default()
                },
                model,
                cwd: Some(cwd.to_string_lossy().into_owned()),
                reasoning_effort: None,
            })
        })
    }

    fn submit_prompt(&self, prompt: Prompt) -> BoxFuture<'static, Result<Turn>> {
        let result = self.submit(prompt);
        Box::pin(async move { result })
    }

    fn interrupt(&self, thread_id: String, _turn_id: String) -> BoxFuture<'static, Result<()>> {
        let result = self.session(&thread_id).and_then(|handle| {
            let mut session = handle.lock();
            session.mapper.interrupt();
            self.write(&session, control_request(&self.request_id("interrupt"), json!({"subtype": "interrupt"})))
        });
        Box::pin(async move { result })
    }

    fn resolve_approval(&self, request_id: &RequestId, decision: ApprovalDecision) -> Result<()> {
        let id = request_id.to_string();
        for handle in self.sessions() {
            let mut session = handle.lock();
            if let Some(reply) = session.mapper.approval_reply(&id, decision) {
                return self.write(&session, reply);
            }
        }
        bail!("Claude is no longer waiting for this approval")
    }

    fn respond_tool_call(&self, request_id: &RequestId, response: DynamicToolCallResponse) -> Result<()> {
        let id = request_id.to_string();
        for handle in self.sessions() {
            let mut session = handle.lock();
            if let Some(reply) = session.mapper.tool_reply(&id, &response) {
                return self.write(&session, reply);
            }
        }
        bail!("Claude is no longer waiting for this tool call")
    }

    fn models(&self) -> BoxFuture<'static, Result<Vec<Model>>> {
        let client = self.clone();
        self.on_runtime(async move {
            if let Some(models) = client.inner.models.lock().clone() {
                return Ok(models);
            }
            let probed = client.probe().await;
            if let Err(err) = &probed {
                tracing::info!(target: "claude", "reading Claude Code's model list: {err:#}");
            }
            let models = models::merge_models(probed.as_ref().ok().and_then(|r| r.get("models")));
            *client.inner.models.lock() = Some(models.clone());
            Ok(models)
        })
    }

    fn account(&self) -> BoxFuture<'static, Result<GetAccountResponse>> {
        let client = self.clone();
        self.on_runtime(async move {
            let mut command = client.inner.launch.command(&client.inner.cwd);
            command.args(["auth", "status", "--json"]).stdin(Stdio::null());
            let output = tokio::time::timeout(Duration::from_secs(20), command.output())
                .await
                .context("`claude auth status` did not answer within 20 seconds")??;
            let auth_failed = client.inner.auth_failed.load(Ordering::Relaxed);
            match serde_json::from_slice::<Value>(&output.stdout) {
                Ok(status) => Ok(models::account_from_status(&status, auth_failed)),
                // An older Claude Code without `auth status`: let turns find out.
                Err(_) => Ok(GetAccountResponse {
                    account: (!auth_failed).then_some(Account::Other),
                    requires_openai_auth: auth_failed,
                }),
            }
        })
    }

    fn login(&self, _params: LoginParams) -> BoxFuture<'static, Result<LoginResponse>> {
        Box::pin(async {
            Err(anyhow!("sign in to Claude with `claude auth login`; Cockpit runs it in a terminal"))
        })
    }

    fn forget_auth_failure(&self) {
        self.inner.auth_failed.store(false, Ordering::Relaxed);
    }

    fn shutdown(&self) {
        for handle in self.sessions() {
            let mut session = handle.lock();
            session.generation += 1;
            session.process = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> SessionSettings {
        SessionSettings {
            model: DEFAULT_MODEL.into(),
            effort: Some("high".into()),
            mode: PermissionMode::AcceptEdits,
            append_system_prompt: None,
            mcp_servers: Vec::new(),
        }
    }

    #[test]
    fn command_line_for_new_and_resumed_threads() {
        let args = process_args(&settings(), "sid", false);
        let joined = args.join(" ");
        assert!(joined.starts_with("-p --input-format stream-json --output-format stream-json --verbose"));
        assert!(joined.contains("--permission-prompt-tool stdio"));
        assert!(joined.contains("--model claude-opus-5-5 --effort high --permission-mode acceptEdits"));
        assert!(joined.contains("--session-id sid"));
        assert!(!joined.contains("--mcp-config"));
        assert!(!joined.contains("dangerously"));

        let mut resumed = settings();
        resumed.effort = None;
        resumed.mode = PermissionMode::BypassPermissions;
        resumed.append_system_prompt = Some("Use the IDE browser.".into());
        resumed.mcp_servers = vec![McpServerSpec { name: "ide_browser".into(), tools: Vec::new() }];
        let args = process_args(&resumed, "sid", true);
        let at = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(at("--resume"), "sid");
        assert!(!args.contains(&"--effort".to_string()));
        assert!(args.contains(&"--allow-dangerously-skip-permissions".to_string()));
        assert_eq!(at("--append-system-prompt"), "Use the IDE browser.");
        assert_eq!(
            serde_json::from_str::<Value>(&at("--mcp-config")).unwrap(),
            json!({"mcpServers": {"ide_browser": {"type": "sdk", "name": "ide_browser"}}})
        );
        assert_eq!(at("--allowedTools"), "mcp__ide_browser");
    }

    #[test]
    fn presets_map_to_permission_modes() {
        assert_eq!(PermissionMode::from_sandbox_policy(SandboxPolicy::ReadOnly), PermissionMode::Default);
        assert_eq!(PermissionMode::from_sandbox_policy(SandboxPolicy::WorkspaceWrite), PermissionMode::AcceptEdits);
        assert_eq!(PermissionMode::from_sandbox_mode(SandboxMode::DangerFullAccess), PermissionMode::BypassPermissions);
    }

    #[test]
    fn session_ids_are_v4_uuids() {
        let id = new_session_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, new_session_id());
    }

    /// A stand-in for `claude` that speaks just enough stream-json.
    #[cfg(unix)]
    fn fake_claude(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("claude");
        let body = r#"#!/bin/sh
case "$1" in
  --version) echo "9.9.9 (Claude Code)"; exit 0 ;;
  auth) echo '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"dev@example.com","subscriptionType":"pro"}'; exit 0 ;;
esac
echo "$@" >> "$(dirname "$0")/args.log"
while IFS= read -r line; do
  case "$line" in
    *'"subtype":"initialize"'*)
      echo '{"type":"control_response","response":{"subtype":"success","request_id":"cockpit-probe","response":{"models":[{"value":"opus","displayName":"Opus","description":"Opus 4.7 · Most capable"}]}}}' ;;
    *'"type":"user"'*)
      echo '{"type":"system","subtype":"init","model":"claude-opus-5-5","session_id":"s"}'
      echo '{"type":"assistant","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}'
      echo '{"type":"control_request","request_id":"perm-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"t1"}}' ;;
    *'"behavior":"allow"'*)
      echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"a.txt"}]}}'
      echo '{"type":"assistant","message":{"id":"m2","model":"claude-opus-5-5","content":[{"type":"text","text":"Done."}]}}'
      echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"usage":{"input_tokens":3,"output_tokens":2}}' ;;
  esac
done
"#;
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_turn_through_a_real_process() {
        use crate::events::ApprovalKind;

        let dir = tempfile::tempdir().unwrap();
        let launch = ClaudeLaunch::new(fake_claude(dir.path()));
        let (client, events) = ClaudeClient::connect(launch, dir.path().to_path_buf(), tokio::runtime::Handle::current())
            .await
            .unwrap();
        assert_eq!(client.version(), "9.9.9");

        let account = client.account().await.unwrap();
        assert!(!account.requires_openai_auth);
        let models = client.models().await.unwrap();
        assert_eq!(models[0].id, DEFAULT_MODEL);
        assert!(models.iter().any(|m| m.id == "opus"), "Claude Code's own models are appended");

        let thread = client
            .start_thread(ThreadConfig { sandbox: Some(SandboxMode::ReadOnly), ..Default::default() })
            .await
            .unwrap();
        let turn = client
            .submit_prompt(Prompt {
                thread_id: thread.thread.id.clone(),
                input: vec![UserInput::text("list files")],
                model: None,
                effort: Some("low".into()),
                approval_policy: None,
                sandbox_policy: Some(SandboxPolicy::ReadOnly),
            })
            .await
            .unwrap();

        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv()).await.unwrap().unwrap();
            if let AgentEvent::ApprovalRequested(approval) = &event {
                assert!(matches!(&approval.kind, ApprovalKind::Command { command: Some(c), .. } if c == "ls"));
                client.resolve_approval(&approval.request_id, ApprovalDecision::Accept).unwrap();
            }
            let done = matches!(&event, AgentEvent::TurnCompleted { turn: t, .. } if t.id == turn.id);
            seen.push(event);
            if done {
                break;
            }
        }
        assert!(matches!(&seen[0], AgentEvent::TurnStarted { .. }));
        assert!(seen.iter().any(|e| matches!(e, AgentEvent::ItemCompleted {
            item: ThreadItem::CommandExecution { status: ItemStatus::Completed, .. }, .. })));
        assert!(seen.iter().any(|e| matches!(e, AgentEvent::ItemCompleted {
            item: ThreadItem::AgentMessage { text, .. }, .. } if text == "Done.")));
        assert!(seen.iter().any(|e| matches!(e, AgentEvent::TurnUsage { usage, .. } if usage.total_tokens == 5)));
        assert!(matches!(seen.last(), Some(AgentEvent::TurnCompleted { turn, .. }) if turn.status == TurnStatus::Completed));

        let log = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        let turn_args = log.lines().find(|l| l.contains("--session-id")).expect("the thread's process");
        assert!(turn_args.contains(&format!("--session-id {}", thread.thread.id)));
        assert!(turn_args.contains("--effort low --permission-mode default"));
        client.shutdown();
    }
}

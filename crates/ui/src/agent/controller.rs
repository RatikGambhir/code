//! The agent side of a workspace: connection (Codex or Claude), threads,
//! turns, approvals, browser tool calls and usage accounting. The panel
//! renders it.

use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::Local;
use cockpit_agents::{
    protocol::{
        Account, ApprovalDecision, DynamicToolCallResponse, GetAccountResponse, LoginParams, LoginResponse,
        Model, ThreadItem, TurnStatus, UserInput,
    },
    chatgpt,
    claude::login::{ClaudeLogin, LoginEvent},
    AgentBackend, AgentEvent, ApprovalState, Connection, Prompt, Provider, ToolCall, Transcript,
};
use cockpit_browser::BrowserAction;
use cockpit_orchestrator::{
    admit_turn, Admission, AgentPolicy, AutoDecision, Dispatch, Orchestrator, PermissionPreset,
    QueuedPrompt, ToolDecision, TurnContext,
};
use cockpit_persistence::{now_ts, AgentThreadRecord, TokenCounts, UsageFilter, UsageRecord};
use gpui::{App, AppContext as _, Context, EventEmitter, SharedString, Task};

use crate::state::{AppState, OpenAiSignIn};

/// The key the global agent policy is stored under.
const POLICY_KEY: &str = "global";

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    Idle,
    /// The provider's CLI (`codex`, `claude`) was not found.
    NotInstalled,
    Connecting,
    Connected { user_agent: String },
    Failed(String),
}

pub struct ThreadSession {
    pub id: String,
    pub title: SharedString,
    pub model: Option<String>,
    pub transcript: Transcript,
    /// Seconds since the epoch; the feed lists threads in this order.
    pub created_at: i64,
    /// Breaks ties between threads created in the same second (0 for
    /// threads loaded from disk).
    pub seq: u64,
    /// Last activity, in seconds since the epoch, for ordering and labels.
    pub updated_at: i64,
    /// The transcript reflects the server's history (started here or resumed).
    pub loaded: bool,
    pub loading: bool,
    /// An `@chatgpt` conversation (ChatGPT on the plan, not Codex).
    pub chatgpt: bool,
}

/// A browser tool call waiting for the user's go-ahead.
pub struct PendingToolCall {
    pub call: ToolCall,
    pub action: BrowserAction,
    pub reason: String,
}

pub enum AgentControllerEvent {
    /// Something to show the user outside the panel.
    Notify(SharedString),
    /// Run a browser action for the agent and answer with `respond_tool_call`.
    RunBrowserAction { call: Box<ToolCall>, action: BrowserAction },
    /// The agent edited these files.
    FilesChanged(Vec<PathBuf>),
    /// Run a program in a new terminal tab (Claude Code's interactive
    /// sign-in, which owns the credentials).
    RunInTerminal { program: String, args: Vec<String> },
}

/// Hangar's own ChatGPT sign-in (tokens in the Keychain).
#[derive(Clone, Debug, PartialEq)]
pub enum HangarSignIn {
    /// Not read from the Keychain yet.
    Unknown,
    SignedOut,
    SignedIn(chatgpt::AccountInfo),
}

/// Claude Code's sign-in, running in the background.
#[derive(Clone)]
pub struct ClaudeLoginState {
    login: Arc<ClaudeLogin>,
    /// The fallback link, whose page shows a code to paste.
    pub url: Option<String>,
}

/// A Hangar sign-in waiting for the browser.
#[derive(Clone)]
pub struct HangarLogin {
    pub auth_url: Option<String>,
    cancel: Arc<AtomicBool>,
}

/// The stored provider key of `@chatgpt` threads.
const CHATGPT_KEY: &str = "chatgpt";

type Connecting = Pin<Box<dyn Future<Output = anyhow::Result<Connection>> + Send>>;

pub struct AgentController {
    workspace_id: i64,
    cwd: PathBuf,
    provider: Provider,
    /// Bumped on every (re)connect, so a connection that finishes after the
    /// user switched provider is dropped instead of installed.
    connection_generation: u64,
    state: ConnectionState,
    backend: Option<Arc<dyn AgentBackend>>,
    models: Vec<Model>,
    account: Option<GetAccountResponse>,
    login: Option<LoginResponse>,
    /// The current provider's threads.
    threads: Vec<ThreadSession>,
    /// The other provider's threads, swapped in when the user switches.
    parked_threads: Vec<ThreadSession>,
    active: Option<String>,
    orchestrator: Orchestrator,
    pending_tools: Vec<PendingToolCall>,
    /// The model chosen for new turns (`None` = server default).
    model: Option<String>,
    effort: Option<String>,
    browser_tools: bool,
    /// The URL of the browser tab the agent drives, for tool policy.
    agent_browser_url: Option<String>,
    usage_today: i64,
    /// The next [`ThreadSession::seq`].
    next_seq: u64,
    /// Claude Code's sign-in is running in a terminal.
    terminal_login: bool,
    /// The panel has been shown, so the agent should be connected. Until
    /// then no agent process is started (the panel may never be opened).
    wanted: bool,
    /// Saved threads and policy have been read.
    loaded: bool,
    /// Codex uses Hangar's ChatGPT sign-in, not its own.
    hangar_sign_in: bool,
    hangar_account: HangarSignIn,
    hangar_login: Option<HangarLogin>,
    claude_login: Option<ClaudeLoginState>,
    /// `@chatgpt` threads' backend, made the first time one is needed.
    chat: Option<Arc<dyn AgentBackend>>,
    chat_models: Vec<Model>,
    chat_model: Option<String>,
    chat_effort: Option<String>,
    _events: Option<Task<()>>,
    _chat_events: Option<Task<()>>,
    _login_poll: Option<Task<()>>,
    _hangar_login: Option<Task<()>>,
    _claude_login: Option<Task<()>>,
}

impl EventEmitter<AgentControllerEvent> for AgentController {}

impl AgentController {
    pub fn new(workspace_id: i64, cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let settings = AppState::global(cx).settings.clone();
        let provider = settings.agent_provider;
        let (model, effort) = match provider {
            Provider::OpenAi => (settings.default_model, settings.default_effort),
            Provider::Anthropic => (settings.claude_model, settings.claude_effort),
        };
        let controller = Self {
            workspace_id,
            cwd,
            provider,
            connection_generation: 0,
            state: ConnectionState::Idle,
            backend: None,
            models: Vec::new(),
            account: None,
            login: None,
            threads: Vec::new(),
            parked_threads: Vec::new(),
            active: None,
            orchestrator: Orchestrator::new(AgentPolicy::default()),
            pending_tools: Vec::new(),
            model,
            effort,
            browser_tools: settings.agent_browser_tools,
            agent_browser_url: None,
            usage_today: 0,
            next_seq: 1,
            terminal_login: false,
            wanted: false,
            loaded: false,
            hangar_sign_in: settings.openai_sign_in == OpenAiSignIn::Hangar,
            hangar_account: HangarSignIn::Unknown,
            hangar_login: None,
            claude_login: None,
            chat: None,
            chat_models: Vec::new(),
            chat_model: settings.chatgpt_model.clone(),
            chat_effort: settings.chatgpt_effort.clone(),
            _events: None,
            _chat_events: None,
            _login_poll: None,
            _hangar_login: None,
            _claude_login: None,
        };
        controller.load_saved_state(cx);
        controller
    }

    fn load_saved_state(&self, cx: &mut Context<Self>) {
        let db = AppState::global(cx).db.clone();
        let workspace_id = self.workspace_id;
        let saved = db.read(move |r| {
            Ok((
                r.load_policy::<AgentPolicy>(POLICY_KEY)?,
                r.agent_threads(workspace_id, false)?,
            ))
        });
        cx.spawn(async move |this, cx| {
            let saved = saved.await;
            this.update(cx, |this, cx| {
                match saved {
                    Ok((policy, threads)) => {
                        if let Some(policy) = policy {
                            this.orchestrator.set_policy(policy);
                        }
                        for record in threads {
                            if this.threads.iter().chain(&this.parked_threads).any(|t| t.id == record.thread_id) {
                                continue;
                            }
                            let chatgpt = record.provider.as_deref() == Some(CHATGPT_KEY);
                            let list = if Provider::from_key(record.provider.as_deref()) == this.provider {
                                &mut this.threads
                            } else {
                                &mut this.parked_threads
                            };
                            list.push(ThreadSession {
                                title: record
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| "Untitled thread".into())
                                    .into(),
                                id: record.thread_id,
                                model: record.model,
                                transcript: Transcript::new(),
                                created_at: record.created_at,
                                seq: 0,
                                updated_at: record.updated_at,
                                loaded: false,
                                loading: false,
                                chatgpt,
                            });
                        }
                    }
                    Err(err) => tracing::warn!("loading agent state: {err:#}"),
                }
                this.loaded = true;
                this.refresh_usage(cx);
                if this.wanted {
                    this.connect(cx);
                }
            })
        })
        .detach();
    }

    /// The panel is showing: connect to the agent if that never happened.
    /// Starting `codex app-server` or `claude` is deferred to here so that a
    /// window with the panel closed does not run (or pay for) either.
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        self.wanted = true;
        if self.loaded && self.state == ConnectionState::Idle {
            self.connect(cx);
        }
    }

    // --- accessors ---------------------------------------------------------

    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// "Codex" or "Claude", for messages.
    pub fn agent_name(&self) -> &'static str {
        self.provider.agent_name()
    }

    /// Claude Code's sign-in is running in a terminal tab.
    pub fn terminal_login(&self) -> bool {
        self.terminal_login
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// The open thread is an `@chatgpt` conversation: the model pickers
    /// are ChatGPT's.
    pub fn chat_active(&self) -> bool {
        self.active_thread().is_some_and(|t| t.chatgpt)
    }

    pub fn models(&self) -> &[Model] {
        if self.chat_active() { &self.chat_models } else { &self.models }
    }

    pub fn model(&self) -> Option<&str> {
        if self.chat_active() { self.chat_model.as_deref() } else { self.model.as_deref() }
    }

    pub fn effort(&self) -> Option<&str> {
        if self.chat_active() { self.chat_effort.as_deref() } else { self.effort.as_deref() }
    }

    /// The model new turns will use: the chosen one or the server default.
    pub fn effective_model(&self) -> Option<&Model> {
        let models = self.models();
        match self.model() {
            Some(id) => models.iter().find(|m| m.id == id),
            None => models.iter().find(|m| m.is_default),
        }
    }

    /// "ChatGPT" for `@chatgpt` threads, else the provider's agent.
    pub fn thread_agent_name(&self, thread_id: &str) -> &'static str {
        match self.threads.iter().find(|t| t.id == thread_id) {
            Some(thread) if thread.chatgpt => "ChatGPT",
            _ => self.agent_name(),
        }
    }

    pub fn permissions(&self) -> PermissionPreset {
        self.orchestrator.policy().permissions
    }

    pub fn browser_tools(&self) -> bool {
        self.browser_tools
    }

    pub fn needs_login(&self) -> bool {
        if self.uses_hangar_sign_in() {
            return self.hangar_account == HangarSignIn::SignedOut;
        }
        matches!(&self.account, Some(account) if account.requires_openai_auth && account.account.is_none())
    }

    /// Codex is running on Hangar's ChatGPT sign-in.
    pub fn uses_hangar_sign_in(&self) -> bool {
        self.provider == Provider::OpenAi && self.hangar_sign_in
    }

    pub fn claude_login(&self) -> Option<&ClaudeLoginState> {
        self.claude_login.as_ref()
    }

    pub fn hangar_login(&self) -> Option<&HangarLogin> {
        self.hangar_login.as_ref()
    }

    pub fn login_state(&self) -> Option<&LoginResponse> {
        self.login.as_ref()
    }

    pub fn threads(&self) -> &[ThreadSession] {
        &self.threads
    }

    pub fn active_thread(&self) -> Option<&ThreadSession> {
        let id = self.active.as_ref()?;
        self.threads.iter().find(|t| &t.id == id)
    }

    fn thread_mut(&mut self, id: &str) -> Option<&mut ThreadSession> {
        self.threads.iter_mut().find(|t| t.id == id)
    }

    pub fn pending_tools(&self) -> &[PendingToolCall] {
        &self.pending_tools
    }

    pub fn usage_today(&self) -> i64 {
        self.usage_today
    }

    /// Some thread has a turn running.
    pub fn is_running(&self) -> bool {
        self.threads.iter().any(|t| t.transcript.is_running())
    }

    /// How many threads have a turn running.
    pub fn running_count(&self) -> usize {
        self.threads.iter().filter(|t| t.transcript.is_running()).count()
    }

    /// The open thread has a turn running.
    pub fn active_is_running(&self) -> bool {
        self.active_thread().is_some_and(|t| t.transcript.is_running())
    }

    pub fn queued(&self) -> usize {
        self.active.as_deref().map_or(0, |id| self.queued_for(id))
    }

    /// Prompts waiting for a free slot in `thread_id`.
    pub fn queued_for(&self, thread_id: &str) -> usize {
        self.orchestrator.queued_for(thread_id)
    }

    // --- connection --------------------------------------------------------

    pub fn connect(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ConnectionState::Connecting | ConnectionState::Connected { .. }) {
            return;
        }
        let state = AppState::global(cx);
        let connecting: Option<Connecting> = match self.provider {
            // The experimental API is what allows client-side (browser) tools.
            Provider::OpenAi => state
                .codex_launch()
                .map(|launch| Box::pin(state.agents.connect_codex(launch, self.cwd.clone(), true)) as Connecting),
            Provider::Anthropic => state
                .claude_launch()
                .map(|launch| Box::pin(state.agents.connect_claude(launch, self.cwd.clone())) as Connecting),
        };
        let Some(connecting) = connecting else {
            self.state = ConnectionState::NotInstalled;
            cx.notify();
            return;
        };
        self.connection_generation += 1;
        let generation = self.connection_generation;
        self.state = ConnectionState::Connecting;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let connection = connecting.await;
            this.update(cx, |this, cx| {
                if this.connection_generation != generation {
                    // The user switched provider (or reconnected) meanwhile.
                    if let Ok(connection) = connection {
                        connection.backend.shutdown();
                    }
                    return;
                }
                match connection {
                    Ok(connection) => this.on_connected(connection, cx),
                    Err(err) => {
                        this.state = ConnectionState::Failed(format!("{err:#}"));
                        cx.notify();
                    }
                }
            })
        })
        .detach();
    }

    fn on_connected(&mut self, connection: Connection, cx: &mut Context<Self>) {
        self.state = ConnectionState::Connected {
            user_agent: connection.server.user_agent.clone(),
        };
        self.backend = Some(connection.backend.clone());
        let events = connection.events;
        self._events = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this.update(cx, |this, cx| this.handle_event(event, cx)).is_err() {
                    break;
                }
            }
        }));
        self.refresh_account(cx);

        let models = connection.backend.models();
        cx.spawn(async move |this, cx| {
            let models = models.await;
            this.update(cx, |this, cx| {
                match models {
                    Ok(models) => this.models = models,
                    Err(err) => tracing::warn!("listing models: {err:#}"),
                }
                cx.notify();
            })
        })
        .detach();
        // Saved threads stay closed in the feed: each is read from disk
        // only when its card is opened.
        cx.notify();
    }

    fn refresh_account(&mut self, cx: &mut Context<Self>) {
        if self.provider == Provider::OpenAi {
            self.refresh_hangar_account(cx);
        }
        let Some(backend) = self.backend.clone() else { return };
        let account = backend.account();
        let provider = self.provider;
        cx.spawn(async move |this, cx| {
            let account = account.await;
            this.update(cx, |this, cx| {
                if this.provider != provider {
                    return;
                }
                match account {
                    Ok(account) => {
                        this.account = Some(account);
                        if this.terminal_login && !this.needs_login() {
                            this.terminal_login = false;
                            this._login_poll = None;
                            let name = this.agent_name();
                            cx.emit(AgentControllerEvent::Notify(format!("Signed in to {name}.").into()));
                        }
                    }
                    Err(err) => tracing::warn!("reading the {} account: {err:#}", provider.agent_name()),
                }
                cx.notify();
            })
        })
        .detach();
    }

    /// Drops the connection and everything that depended on it.
    fn disconnect(&mut self) {
        if let Some(backend) = self.backend.take() {
            backend.shutdown();
        }
        self._events = None;
        self._login_poll = None;
        self.terminal_login = false;
        self.connection_generation += 1;
        self.state = ConnectionState::Idle;
        self.pending_tools.clear();
        for thread in &mut self.threads {
            thread.loaded = false;
            thread.loading = false;
            self.orchestrator.cancel_queued(&thread.id);
            self.orchestrator.turn_finished(&thread.id);
        }
    }

    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.disconnect();
        self.connect(cx);
    }

    /// Switches the panel to another provider's agent. Its threads, model
    /// and sign-in are separate; the choice is remembered.
    pub fn set_provider(&mut self, provider: Provider, cx: &mut Context<Self>) {
        if provider == self.provider {
            return;
        }
        self.disconnect();
        self.provider = provider;
        self.models.clear();
        self.account = None;
        self.login = None;
        std::mem::swap(&mut self.threads, &mut self.parked_threads);
        self.active = None;
        let settings = &AppState::global(cx).settings;
        (self.model, self.effort) = match provider {
            Provider::OpenAi => (settings.default_model.clone(), settings.default_effort.clone()),
            Provider::Anthropic => (settings.claude_model.clone(), settings.claude_effort.clone()),
        };
        AppState::save_settings(cx, |settings| settings.agent_provider = provider);
        self.connect(cx);
        cx.notify();
    }

    /// Who the agent is signed in as, for the header menu, e.g.
    /// "you@example.com · Plus". `None` while signed out or unknown.
    pub fn account_label(&self) -> Option<String> {
        if self.uses_hangar_sign_in() {
            let HangarSignIn::SignedIn(account) = &self.hangar_account else { return None };
            let who = account.email.clone().unwrap_or_else(|| "Signed in".into());
            return Some(format!("{who} · ChatGPT plan"));
        }
        let account = self.account.as_ref()?.account.as_ref()?;
        let (who, plan) = match account {
            Account::Chatgpt { email, plan_type } => (email.clone(), plan_type.as_str().map(str::to_string)),
            Account::ApiKey => (Some("OpenAI API key".into()), None),
            Account::Claude { email, method, plan } => (email.clone().or_else(|| method.clone()), plan.clone()),
            Account::Other => (None, None),
        };
        let who = who.unwrap_or_else(|| "Signed in".into());
        Some(match plan.filter(|p| !p.is_empty()) {
            Some(plan) => format!("{who} · {}", capitalize(&plan)),
            None => who,
        })
    }

    /// Signing out is offered when the agent's CLI holds the credentials:
    /// not for Claude on Bedrock, Vertex or Foundry, which use the cloud's.
    pub fn can_sign_out(&self) -> bool {
        if self.uses_hangar_sign_in() {
            return matches!(self.hangar_account, HangarSignIn::SignedIn(_));
        }
        let Some(account) = self.account.as_ref().and_then(|a| a.account.as_ref()) else {
            return false;
        };
        self.backend.is_some()
            && !matches!(account, Account::Claude { method: Some(m), .. }
                if matches!(m.as_str(), "bedrock" | "vertex" | "foundry"))
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        if self.uses_hangar_sign_in() {
            return self.sign_out_of_hangar(cx);
        }
        let Some(backend) = self.backend.clone() else { return };
        let logout = backend.logout();
        let name = self.agent_name();
        cx.spawn(async move |this, cx| {
            let result = logout.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.login = None;
                        cx.emit(AgentControllerEvent::Notify(format!("Signed out of {name}.").into()));
                    }
                    Err(err) => cx.emit(AgentControllerEvent::Notify(format!("Sign-out failed: {err:#}").into())),
                }
                this.refresh_account(cx);
                cx.notify();
            })
        })
        .detach();
    }

    /// Reads Hangar's ChatGPT sign-in from the Keychain (off the UI thread:
    /// macOS may ask the user first).
    fn refresh_hangar_account(&mut self, cx: &mut Context<Self>) {
        let read = cx.background_spawn(async { chatgpt::signed_in_account() });
        cx.spawn(async move |this, cx| {
            let account = read.await;
            this.update(cx, |this, cx| {
                this.hangar_account = match account {
                    Ok(Some(account)) => HangarSignIn::SignedIn(account),
                    Ok(None) => HangarSignIn::SignedOut,
                    Err(err) => {
                        tracing::warn!("reading the ChatGPT sign-in: {err:#}");
                        cx.emit(AgentControllerEvent::Notify(format!("Couldn't read the ChatGPT sign-in: {err:#}").into()));
                        HangarSignIn::SignedOut
                    }
                };
                cx.notify();
            })
        })
        .detach();
    }

    /// Hangar's own Sign in with ChatGPT: the browser signs in, Hangar keeps
    /// the tokens in the Keychain, and Codex is restarted to use them.
    pub fn sign_in_with_hangar(&mut self, cx: &mut Context<Self>) {
        self.sign_in_with_hangar_for(true, cx);
    }

    /// `codex`: also switch Codex to this sign-in (else it is for
    /// `@chatgpt` only).
    fn sign_in_with_hangar_for(&mut self, codex: bool, cx: &mut Context<Self>) {
        if self.hangar_login.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (url_tx, url_rx) = async_channel::bounded::<String>(1);
        let (done_tx, done_rx) = async_channel::bounded(1);
        let thread_cancel = cancel.clone();
        let spawned = std::thread::Builder::new().name("chatgpt-sign-in".into()).spawn(move || {
            let result = chatgpt::begin_sign_in().and_then(|pending| {
                let _ = url_tx.send_blocking(pending.auth_url().to_string());
                pending.finish(&thread_cancel)
            });
            let _ = done_tx.send_blocking(result);
        });
        if let Err(err) = spawned {
            cx.emit(AgentControllerEvent::Notify(format!("Sign-in failed: {err}").into()));
            return;
        }
        self.hangar_login = Some(HangarLogin { auth_url: None, cancel: cancel.clone() });
        cx.notify();
        self._hangar_login = Some(cx.spawn(async move |this, cx| {
            if let Ok(url) = url_rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    cx.open_url(&url);
                    if let Some(login) = &mut this.hangar_login {
                        login.auth_url = Some(url);
                    }
                    cx.notify();
                });
            }
            let result = done_rx.recv().await.unwrap_or_else(|_| Err(anyhow::anyhow!("the sign-in stopped")));
            let _ = this.update(cx, |this, cx| {
                this.hangar_login = None;
                match result {
                    Ok(account) => {
                        let who = account.email.clone().unwrap_or_else(|| "your ChatGPT account".into());
                        this.hangar_account = HangarSignIn::SignedIn(account);
                        if codex {
                            cx.emit(AgentControllerEvent::Notify(
                                format!("Signed in to ChatGPT as {who}. Codex now uses this sign-in.").into(),
                            ));
                            this.set_hangar_sign_in(true, cx);
                        } else {
                            cx.emit(AgentControllerEvent::Notify(
                                format!("Signed in to ChatGPT as {who}. Send your @chatgpt message again.").into(),
                            ));
                        }
                    }
                    Err(_) if cancel.load(Ordering::Relaxed) => {}
                    Err(err) => cx.emit(AgentControllerEvent::Notify(format!("Sign-in failed: {err:#}").into())),
                }
                cx.notify();
            });
        }));
    }

    pub fn cancel_hangar_sign_in(&mut self, cx: &mut Context<Self>) {
        if let Some(login) = self.hangar_login.take() {
            login.cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    /// Switches Codex between Hangar's ChatGPT sign-in and its own, and
    /// restarts it so the change takes effect.
    pub fn set_hangar_sign_in(&mut self, hangar: bool, cx: &mut Context<Self>) {
        let changed = self.hangar_sign_in != hangar;
        self.hangar_sign_in = hangar;
        AppState::save_settings(cx, |settings| {
            settings.openai_sign_in = if hangar { OpenAiSignIn::Hangar } else { OpenAiSignIn::Codex };
        });
        if changed && self.provider == Provider::OpenAi {
            self.account = None;
            self.login = None;
            if self.wanted {
                self.reconnect(cx);
            }
        }
        if hangar {
            self.refresh_hangar_account(cx);
        }
        cx.notify();
    }

    fn sign_out_of_hangar(&mut self, cx: &mut Context<Self>) {
        let sign_out = cx.background_spawn(async { chatgpt::sign_out() });
        cx.spawn(async move |this, cx| {
            let result = sign_out.await;
            this.update(cx, |this, cx| {
                let message = match result {
                    Ok(chatgpt::SignOut::Revoked) => "Signed out of ChatGPT.".to_string(),
                    Ok(chatgpt::SignOut::LocalOnly) => "Signed out of ChatGPT on this Mac, but OpenAI didn't confirm it. You can also disconnect Hangar in ChatGPT's settings.".to_string(),
                    Err(err) => format!("Sign-out failed: {err:#}"),
                };
                cx.emit(AgentControllerEvent::Notify(message.into()));
                this.refresh_hangar_account(cx);
            })
        })
        .detach();
    }

    pub fn login_with_chatgpt(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else { return };
        let login = backend.login(LoginParams::Chatgpt);
        cx.spawn(async move |this, cx| {
            let login = login.await;
            this.update(cx, |this, cx| {
                match login {
                    Ok(LoginResponse::Chatgpt { auth_url, login_id }) => {
                        cx.open_url(&auth_url);
                        this.login = Some(LoginResponse::Chatgpt { auth_url, login_id });
                    }
                    Ok(other) => this.login = Some(other),
                    Err(err) => cx.emit(AgentControllerEvent::Notify(format!("Sign-in failed: {err:#}").into())),
                }
                cx.notify();
            })
        })
        .detach();
    }

    /// Starts Claude Code's own sign-in in the background: claude.ai (a
    /// Claude plan) or, with `console`, an Anthropic Console account billed
    /// per API use. Claude Code opens the browser and keeps the credentials;
    /// the panel waits, and takes a code if the sign-in page shows one.
    pub fn login_with_claude(&mut self, console: bool, cx: &mut Context<Self>) {
        let Some(launch) = AppState::global(cx).claude_launch() else {
            self.state = ConnectionState::NotInstalled;
            cx.notify();
            return;
        };
        self.cancel_terminal_login(cx);
        let (login, events) = match ClaudeLogin::start(&launch, console) {
            Ok(started) => started,
            Err(err) => {
                cx.emit(AgentControllerEvent::Notify(format!("Claude sign-in failed: {err:#}").into()));
                return;
            }
        };
        self.claude_login = Some(ClaudeLoginState { login: Arc::new(login), url: None });
        cx.notify();
        // Replacing the task drops any earlier sign-in's events.
        self._claude_login = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let finished = this
                    .update(cx, |this, cx| {
                        let finished = matches!(event, LoginEvent::Finished(_));
                        match event {
                            LoginEvent::Url(url) => {
                                if let Some(state) = &mut this.claude_login {
                                    state.url = Some(url);
                                }
                            }
                            LoginEvent::Finished(result) => {
                                // Cancelled sign-ins have already been cleared.
                                let cancelled = this.claude_login.take().is_none();
                                match result {
                                    Ok(()) => {
                                        cx.emit(AgentControllerEvent::Notify("Signed in to Claude.".into()));
                                        this.check_login(cx);
                                    }
                                    Err(err) if !cancelled => cx.emit(AgentControllerEvent::Notify(
                                        format!("Claude sign-in failed: {err}").into(),
                                    )),
                                    Err(_) => {}
                                }
                            }
                        }
                        cx.notify();
                        finished
                    })
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        }));
    }

    /// Sends the code the Claude sign-in page showed.
    pub fn submit_claude_code(&mut self, code: String, cx: &mut Context<Self>) {
        let Some(state) = &self.claude_login else { return };
        if let Err(err) = state.login.submit_code(&code) {
            cx.emit(AgentControllerEvent::Notify(format!("Claude sign-in failed: {err:#}").into()));
        }
    }

    pub fn cancel_claude_login(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.claude_login.take() {
            state.login.cancel();
        }
        cx.notify();
    }

    /// The older way: `claude auth login` in a terminal tab.
    pub fn login_with_claude_in_terminal(&mut self, console: bool, cx: &mut Context<Self>) {
        self.cancel_claude_login(cx);
        let Some(launch) = AppState::global(cx).claude_launch() else {
            self.state = ConnectionState::NotInstalled;
            cx.notify();
            return;
        };
        let flag = if console { "--console" } else { "--claudeai" };
        cx.emit(AgentControllerEvent::RunInTerminal {
            program: launch.program.to_string_lossy().into_owned(),
            args: vec!["auth".into(), "login".into(), flag.into()],
        });
        self.terminal_login = true;
        // Notice a fresh sign-in on its own; an expired one needs "Check
        // again" (the status already says signed in).
        self._login_poll = Some(cx.spawn(async move |this, cx| {
            for _ in 0..200 {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        if this.terminal_login && this.needs_login() {
                            this.refresh_account(cx);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// The user finished signing in (in the terminal); look again.
    pub fn check_login(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = &self.backend {
            backend.forget_auth_failure();
        }
        self.refresh_account(cx);
    }

    pub fn cancel_terminal_login(&mut self, cx: &mut Context<Self>) {
        self.terminal_login = false;
        self._login_poll = None;
        cx.notify();
    }

    pub fn login_with_api_key(&mut self, api_key: String, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else { return };
        let login = backend.login(LoginParams::ApiKey { api_key });
        cx.spawn(async move |this, cx| {
            let login = login.await;
            this.update(cx, |this, cx| {
                if let Err(err) = login {
                    cx.emit(AgentControllerEvent::Notify(format!("Sign-in failed: {err:#}").into()));
                }
                this.refresh_account(cx);
            })
        })
        .detach();
    }

    // --- settings ----------------------------------------------------------

    pub fn set_model(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        if self.chat_active() {
            self.chat_model = model.clone();
            self.chat_effort = None;
            AppState::save_settings(cx, |settings| {
                settings.chatgpt_model = model;
                settings.chatgpt_effort = None;
            });
            cx.notify();
            return;
        }
        self.model = model.clone();
        // The previous effort may not exist for the new model.
        self.effort = None;
        let provider = self.provider;
        AppState::save_settings(cx, |settings| match provider {
            Provider::OpenAi => {
                settings.default_model = model;
                settings.default_effort = None;
            }
            Provider::Anthropic => {
                settings.claude_model = model;
                settings.claude_effort = None;
            }
        });
        cx.notify();
    }

    pub fn set_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        if self.chat_active() {
            self.chat_effort = effort.clone();
            AppState::save_settings(cx, |settings| settings.chatgpt_effort = effort);
            cx.notify();
            return;
        }
        self.effort = effort.clone();
        let provider = self.provider;
        AppState::save_settings(cx, |settings| match provider {
            Provider::OpenAi => settings.default_effort = effort,
            Provider::Anthropic => settings.claude_effort = effort,
        });
        cx.notify();
    }

    pub fn set_permissions(&mut self, preset: PermissionPreset, cx: &mut Context<Self>) {
        let mut policy = self.orchestrator.policy().clone();
        policy.permissions = preset;
        self.save_policy(policy, cx);
    }

    pub fn set_browser_tools(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.browser_tools = enabled;
        AppState::save_settings(cx, |settings| settings.agent_browser_tools = enabled);
        cx.notify();
    }

    fn save_policy(&mut self, policy: AgentPolicy, cx: &mut Context<Self>) {
        let started = self.orchestrator.set_policy(policy.clone());
        AppState::global(cx)
            .db
            .write_detached("agent policy", move |w| w.save_policy(POLICY_KEY, &policy));
        for prompt in started {
            self.start_turn(prompt, cx);
        }
        cx.notify();
    }

    pub fn set_agent_browser_url(&mut self, url: Option<String>) {
        self.agent_browser_url = url;
    }

    // --- threads -----------------------------------------------------------

    /// Starts a new thread (for Claude, its own Claude Code process), opens
    /// it and sends `text` as its first prompt. Threads started this way run
    /// side by side, up to the policy's concurrency limit.
    pub fn spawn(&mut self, text: String, cx: &mut Context<Self>) {
        self.spawn_as(text, false, cx);
    }

    /// Starts a new Codex (or Claude) thread, or with `chatgpt` an
    /// `@chatgpt` conversation on Hangar's ChatGPT sign-in.
    pub fn spawn_as(&mut self, text: String, chatgpt: bool, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Check before creating a thread that would only hold the error.
        if let Err(reason) = self.orchestrator.check_prompt(&text) {
            cx.emit(AgentControllerEvent::Notify(reason.into()));
            return;
        }
        if chatgpt {
            if self.hangar_account == HangarSignIn::SignedOut {
                cx.emit(AgentControllerEvent::Notify(
                    "@chatgpt needs Hangar's ChatGPT sign-in. Finish it in your browser, then send again.".into(),
                ));
                self.sign_in_with_hangar_for(false, cx);
                return;
            }
            self.start_thread_on(true, false, text, cx);
        } else {
            self.start_thread(self.browser_tools, text, cx);
        }
    }

    /// The backend for `@chatgpt` threads, made on first use.
    fn chat_backend(&mut self, cx: &mut Context<Self>) -> Arc<dyn AgentBackend> {
        if let Some(chat) = &self.chat {
            return chat.clone();
        }
        let connection = AppState::global(cx).agents.connect_chatgpt();
        let backend = connection.backend.clone();
        let events = connection.events;
        self._chat_events = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this.update(cx, |this, cx| this.handle_event(event, cx)).is_err() {
                    break;
                }
            }
        }));
        let models = backend.models();
        cx.spawn(async move |this, cx| {
            let models = models.await;
            this.update(cx, |this, cx| match models {
                Ok(models) => {
                    this.chat_models = models;
                    cx.notify();
                }
                Err(err) => tracing::warn!("listing ChatGPT models: {err:#}"),
            })
        })
        .detach();
        self.chat = Some(backend.clone());
        backend
    }

    /// The backend that runs `thread_id`.
    fn backend_for(&mut self, thread_id: &str, cx: &mut Context<Self>) -> Option<Arc<dyn AgentBackend>> {
        if self.threads.iter().any(|t| t.id == thread_id && t.chatgpt) {
            Some(self.chat_backend(cx))
        } else {
            self.backend.clone()
        }
    }

    /// `first_prompt` is sent once the thread exists; each call carries its
    /// own, so several threads can be starting at once.
    fn start_thread(&mut self, with_tools: bool, first_prompt: String, cx: &mut Context<Self>) {
        self.start_thread_on(false, with_tools, first_prompt, cx);
    }

    fn start_thread_on(&mut self, chatgpt: bool, with_tools: bool, first_prompt: String, cx: &mut Context<Self>) {
        let backend = if chatgpt { Some(self.chat_backend(cx)) } else { self.backend.clone() };
        let Some(backend) = backend else {
            cx.emit(AgentControllerEvent::Notify(format!("{} is not connected.", self.agent_name()).into()));
            return;
        };
        let config = self.orchestrator.thread_config(
            Some(self.cwd.to_string_lossy().into_owned()),
            if chatgpt { self.chat_model.clone() } else { self.model.clone() },
            with_tools && !chatgpt,
        );
        let request = backend.start_thread(config);
        cx.spawn(async move |this, cx| {
            let response = request.await;
            this.update(cx, |this, cx| match response {
                Ok(response) => {
                    let thread = response.thread;
                    let seq = this.next_seq;
                    this.next_seq += 1;
                    this.threads.insert(
                        0,
                        ThreadSession {
                            id: thread.id.clone(),
                            title: "New thread".into(),
                            model: Some(response.model.clone()),
                            transcript: Transcript::new(),
                            created_at: now_ts(),
                            seq,
                            updated_at: now_ts(),
                            loaded: true,
                            loading: false,
                            chatgpt,
                        },
                    );
                    this.active = Some(thread.id.clone());
                    let record = AgentThreadRecord {
                        thread_id: thread.id.clone(),
                        workspace_id: Some(this.workspace_id),
                        title: None,
                        model: Some(response.model),
                        cwd: Some(this.cwd.to_string_lossy().into_owned()),
                        created_at: now_ts(),
                        updated_at: now_ts(),
                        provider: Some(if chatgpt { CHATGPT_KEY.into() } else { this.provider.key().into() }),
                    };
                    AppState::global(cx)
                        .db
                        .write_detached("agent thread", move |w| w.upsert_agent_thread(&record));
                    this.send_to(thread.id, first_prompt, cx);
                    cx.notify();
                }
                Err(err) if with_tools && !chatgpt && format!("{err:#}").contains("dynamic") => {
                    // An app server without dynamic tool support: run without
                    // the browser tools rather than not at all.
                    tracing::warn!("thread/start rejected dynamic tools: {err:#}");
                    cx.emit(AgentControllerEvent::Notify(
                        "This Codex version does not support IDE browser tools; continuing without them.".into(),
                    ));
                    this.start_thread(false, first_prompt, cx);
                }
                Err(err) => {
                    cx.emit(AgentControllerEvent::Notify(format!("Could not start a thread: {err:#}").into()));
                }
            })
        })
        .detach();
    }

    /// Closes the open thread so the next prompt starts a new one. Nothing
    /// is created until the user sends.
    pub fn start_draft(&mut self, cx: &mut Context<Self>) {
        self.active = None;
        cx.notify();
    }

    pub fn select_thread(&mut self, id: &str, cx: &mut Context<Self>) {
        self.active = Some(id.to_string());
        let needs_resume = self
            .thread_mut(id)
            .is_some_and(|t| !t.loaded && !t.loading);
        if needs_resume {
            self.resume_thread(id.to_string(), cx);
        }
        cx.notify();
    }

    fn resume_thread(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(backend) = self.backend_for(&id, cx) else { return };
        if let Some(thread) = self.thread_mut(&id) {
            thread.loading = true;
        }
        // Codex remembers a thread's tools; Claude Code gets them each time
        // its process starts, so a resumed Claude thread needs them again.
        let with_tools = self.provider == Provider::Anthropic && self.browser_tools;
        let config = self.orchestrator.thread_config(
            Some(self.cwd.to_string_lossy().into_owned()),
            None,
            with_tools,
        );
        let request = backend.resume_thread(id.clone(), config);
        cx.spawn(async move |this, cx| {
            let response = request.await;
            this.update(cx, |this, cx| {
                let Some(thread) = this.thread_mut(&id) else { return };
                thread.loading = false;
                match response {
                    Ok(response) => {
                        thread.transcript = Transcript::from_turns(&response.thread.turns);
                        thread.model = Some(response.model);
                        if let Some(name) = response.thread.name.filter(|n| !n.is_empty()) {
                            thread.title = name.into();
                        }
                        thread.loaded = true;
                    }
                    Err(err) => {
                        let text = format!("{err:#}");
                        // Codex ties a thread to the sign-in that started it.
                        let message = if !text.contains(chatgpt::CODEX_PROVIDER_ID) {
                            format!("Could not resume this thread: {text}")
                        } else if this.hangar_sign_in {
                            "This thread was started with Codex's own sign-in. To continue it, choose \"Use Codex's own sign-in\" in the Codex menu above.".into()
                        } else {
                            "This thread was started with Hangar's ChatGPT sign-in. To continue it, choose \"Use Hangar's ChatGPT sign-in\" in the Codex menu above.".into()
                        };
                        let Some(thread) = this.thread_mut(&id) else { return };
                        thread.transcript.push_error(message);
                    }
                }
                cx.notify();
            })
        })
        .detach();
    }

    pub fn archive_thread(&mut self, id: &str, cx: &mut Context<Self>) {
        self.threads.retain(|t| t.id != id);
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
        let id = id.to_string();
        AppState::global(cx)
            .db
            .write_detached("archive thread", move |w| w.archive_agent_thread(&id));
        cx.notify();
    }

    // --- turns -------------------------------------------------------------

    /// Sends a prompt to the open thread, or starts a new one when none is
    /// open.
    pub fn send(&mut self, text: String, cx: &mut Context<Self>) {
        match self.active.clone() {
            Some(thread_id) => self.send_to(thread_id, text, cx),
            None => self.spawn(text, cx),
        }
    }

    /// Sends a prompt to `thread_id` through policy, budget admission and
    /// the scheduler.
    pub fn send_to(&mut self, thread_id: String, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.backend_for(&thread_id, cx).is_none() {
            cx.emit(AgentControllerEvent::Notify(format!("{} is not connected.", self.agent_name()).into()));
            return;
        }
        if let Err(reason) = self.orchestrator.check_prompt(&text) {
            if let Some(thread) = self.thread_mut(&thread_id) {
                thread.transcript.push_error(reason);
            }
            cx.notify();
            return;
        }

        // Most recent first, as the chat history lists them.
        if let Some(ix) = self.threads.iter().position(|t| t.id == thread_id) {
            let mut thread = self.threads.remove(ix);
            thread.updated_at = now_ts();
            self.threads.insert(0, thread);
        }
        if let Some(thread) = self.thread_mut(&thread_id) {
            if thread.title.as_ref() == "New thread" || thread.title.as_ref() == "Untitled thread" {
                thread.title = cockpit_common::truncate_chars(text.lines().next().unwrap_or(""), 60).into();
                let (id, title) = (thread_id.clone(), thread.title.to_string());
                AppState::global(cx)
                    .db
                    .write_detached("thread title", move |w| w.rename_agent_thread(&id, &title));
            }
        }

        let db = AppState::global(cx).db.clone();
        let model = self.effective_model().map(|m| m.id.clone());
        let workspace_id = self.workspace_id;
        let admission = {
            let (text, thread_id, model) = (text.clone(), thread_id.clone(), model.clone());
            db.read(move |r| {
                let budgets = r.budgets()?;
                admit_turn(
                    &text,
                    TurnContext {
                        workspace_id: Some(workspace_id),
                        thread_id: Some(&thread_id),
                        model: model.as_deref(),
                    },
                    &budgets,
                    Local::now(),
                    |filter| Ok(r.usage_total(filter)?.total_tokens),
                )
            })
        };
        let chatgpt = self.threads.iter().any(|t| t.id == thread_id && t.chatgpt);
        let prompt = QueuedPrompt {
            thread_id: thread_id.clone(),
            text: text.clone(),
            model: if chatgpt { self.chat_model.clone() } else { self.model.clone() },
            effort: if chatgpt { self.chat_effort.clone() } else { self.effort.clone() },
        };
        cx.spawn(async move |this, cx| {
            let admission = admission.await.unwrap_or_else(|err| {
                tracing::warn!("budget check failed, allowing the turn: {err:#}");
                Admission::Allowed
            });
            this.update(cx, |this, cx| {
                let dispatch = this.orchestrator.submit(prompt, admission);
                let Some(thread) = this.thread_mut(&thread_id) else { return };
                match dispatch {
                    Dispatch::Rejected { reason } => thread.transcript.push_error(reason),
                    Dispatch::Start { prompt, warnings } => {
                        thread.transcript.push_local_user(&text);
                        for warning in warnings {
                            thread.transcript.push_notice(warning);
                        }
                        this.start_turn(prompt, cx);
                    }
                    Dispatch::Queued { position, warnings } => {
                        thread.transcript.push_local_user(&text);
                        for warning in warnings {
                            thread.transcript.push_notice(warning);
                        }
                        thread
                            .transcript
                            .push_notice(format!("Queued — starts when a running turn finishes (#{position})."));
                    }
                }
                cx.notify();
            })
        })
        .detach();
    }

    fn start_turn(&mut self, prompt: QueuedPrompt, cx: &mut Context<Self>) {
        let Some(backend) = self.backend_for(&prompt.thread_id, cx) else { return };
        let thread_id = prompt.thread_id.clone();
        // The current preset applies to every turn, so changing it in the
        // panel also changes threads that started under another one.
        let permissions = self.orchestrator.policy().permissions;
        let request = backend.submit_prompt(Prompt {
            thread_id: prompt.thread_id,
            input: vec![UserInput::text(prompt.text)],
            model: prompt.model,
            effort: prompt.effort,
            approval_policy: Some(permissions.approval_policy()),
            sandbox_policy: Some(permissions.sandbox_policy()),
        });
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    let name = this.thread_agent_name(&thread_id);
                    if let Some(thread) = this.thread_mut(&thread_id) {
                        thread.transcript.push_error(format!("{name} did not start the turn: {err:#}"));
                    }
                    // Free the slot so queued prompts are not stuck.
                    if let Some(next) = this.orchestrator.turn_finished(&thread_id) {
                        this.start_turn(next, cx);
                    }
                    cx.notify();
                }
            })
        })
        .detach();
    }

    /// Stops the open thread's turn and drops its queued prompts.
    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.active.clone() {
            self.interrupt_thread(&id, cx);
        }
    }

    /// Stops `thread_id`'s turn and drops its queued prompts.
    pub fn interrupt_thread(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.iter().find(|t| t.id == thread_id) else { return };
        let thread_id = thread.id.clone();
        let turn_id = thread.transcript.active_turn().map(str::to_string);
        let cancelled = self.orchestrator.cancel_queued(&thread_id);
        if cancelled > 0 {
            if let Some(thread) = self.thread_mut(&thread_id) {
                thread.transcript.push_notice(format!("Dropped {cancelled} queued prompt(s)."));
            }
        }
        if let (Some(backend), Some(turn_id)) = (self.backend_for(&thread_id, cx), turn_id) {
            let request = backend.interrupt(thread_id, turn_id);
            cx.background_spawn(async move {
                if let Err(err) = request.await {
                    tracing::warn!("interrupting the turn: {err:#}");
                }
            })
            .detach();
        }
        cx.notify();
    }

    // --- approvals and tools -----------------------------------------------

    pub fn resolve_approval(
        &mut self,
        thread_id: &str,
        request_id: &cockpit_agents::rpc::RequestId,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        if let Some(backend) = &self.backend {
            if let Err(err) = backend.resolve_approval(request_id, decision) {
                let name = self.agent_name();
                cx.emit(AgentControllerEvent::Notify(format!("Could not answer {name}: {err:#}").into()));
                return;
            }
        }
        let state = match decision {
            ApprovalDecision::Accept => ApprovalState::Accepted,
            ApprovalDecision::AcceptForSession => ApprovalState::AcceptedForSession,
            ApprovalDecision::Decline | ApprovalDecision::Cancel => ApprovalState::Declined,
        };
        if let Some(thread) = self.thread_mut(thread_id) {
            thread.transcript.set_approval_state(request_id, state);
        }
        cx.notify();
    }

    fn on_tool_call(&mut self, call: ToolCall, cx: &mut Context<Self>) {
        match self.orchestrator.browser_call(&call.params, self.agent_browser_url.as_deref()) {
            Err(message) => self.respond_tool_call(&call, DynamicToolCallResponse::text(message, false)),
            Ok((action, ToolDecision::Allow)) => {
                cx.emit(AgentControllerEvent::RunBrowserAction { call: Box::new(call), action });
            }
            Ok((_, ToolDecision::Deny(reason))) => {
                self.respond_tool_call(&call, DynamicToolCallResponse::text(reason, false));
            }
            Ok((action, ToolDecision::NeedsApproval(reason))) => {
                cx.emit(AgentControllerEvent::Notify(format!("{} asks: {reason}", self.agent_name()).into()));
                self.pending_tools.push(PendingToolCall { call, action, reason });
            }
        }
    }

    /// The user's answer to a browser tool approval.
    pub fn answer_tool(&mut self, index: usize, approved: bool, cx: &mut Context<Self>) {
        if index >= self.pending_tools.len() {
            return;
        }
        let pending = self.pending_tools.remove(index);
        if approved {
            cx.emit(AgentControllerEvent::RunBrowserAction {
                call: Box::new(pending.call),
                action: pending.action,
            });
        } else {
            self.respond_tool_call(
                &pending.call,
                DynamicToolCallResponse::text("The user declined this browser action.", false),
            );
        }
        cx.notify();
    }

    pub fn respond_tool_call(&self, call: &ToolCall, response: DynamicToolCallResponse) {
        if let Some(backend) = &self.backend {
            if let Err(err) = backend.respond_tool_call(&call.request_id, response) {
                tracing::warn!("answering a tool call: {err:#}");
            }
        }
    }

    // --- events ------------------------------------------------------------

    fn handle_event(&mut self, event: AgentEvent, cx: &mut Context<Self>) {
        self.log_event(&event, cx);

        // Side effects that need the event before the transcript sees it.
        match &event {
            AgentEvent::ToolCallRequested(call) => self.on_tool_call(call.clone(), cx),
            AgentEvent::TokenUsage { thread_id, turn_id, usage } => {
                self.record_usage(thread_id.clone(), turn_id.clone(), usage.total, cx)
            }
            AgentEvent::TurnUsage { thread_id, turn_id, usage, .. } => {
                self.record_turn_usage(thread_id.clone(), turn_id.clone(), *usage, cx)
            }
            AgentEvent::ThreadNameUpdated { thread_id, name: Some(name) } => {
                if let Some(thread) = self.thread_mut(thread_id) {
                    thread.title = name.clone().into();
                }
                let (id, title) = (thread_id.clone(), name.clone());
                AppState::global(cx)
                    .db
                    .write_detached("thread title", move |w| w.rename_agent_thread(&id, &title));
            }
            AgentEvent::ItemCompleted { item: ThreadItem::FileChange { changes, .. }, .. } => {
                let paths = changes.iter().map(|c| self.cwd.join(&c.path)).collect();
                cx.emit(AgentControllerEvent::FilesChanged(paths));
            }
            AgentEvent::LoginCompleted { success, error } => {
                self.login = None;
                let message = if *success {
                    "Signed in to Codex.".to_string()
                } else {
                    format!("Sign-in failed: {}", error.clone().unwrap_or_default())
                };
                cx.emit(AgentControllerEvent::Notify(message.into()));
                self.refresh_account(cx);
            }
            AgentEvent::AccountChanged => self.refresh_account(cx),
            AgentEvent::Disconnected { reason } => {
                self.state = ConnectionState::Failed(reason.clone());
                self.backend = None;
                self.pending_tools.clear();
                for thread in &mut self.threads {
                    thread.transcript.apply(&event);
                    thread.loaded = false;
                    self.orchestrator.cancel_queued(&thread.id);
                    self.orchestrator.turn_finished(&thread.id);
                }
                cx.notify();
                return;
            }
            _ => {}
        }

        let thread_id = event.thread_id().map(str::to_string);
        if let Some(thread_id) = &thread_id {
            if let Some(thread) = self.thread_mut(thread_id) {
                thread.transcript.apply(&event);
            }
        }

        // Side effects that need the transcript updated first.
        match &event {
            AgentEvent::ApprovalRequested(approval) => {
                let decision = self.orchestrator.auto_decide(approval);
                let thread_id = approval.thread_id.clone();
                match decision {
                    AutoDecision::AskUser => cx.emit(AgentControllerEvent::Notify(
                        format!("{} is waiting for your approval.", self.agent_name()).into(),
                    )),
                    AutoDecision::Accept => {
                        self.resolve_approval(&thread_id, &approval.request_id, ApprovalDecision::Accept, cx);
                        if let Some(thread) = self.thread_mut(&thread_id) {
                            thread.transcript.push_notice("Auto-approved a read-only command.");
                        }
                    }
                    AutoDecision::Decline { reason } => {
                        self.resolve_approval(&thread_id, &approval.request_id, ApprovalDecision::Decline, cx);
                        if let Some(thread) = self.thread_mut(&thread_id) {
                            thread.transcript.push_notice(format!("Declined automatically: {reason}"));
                        }
                    }
                }
            }
            AgentEvent::TurnCompleted { thread_id, turn } => {
                // A failed turn may mean the Hangar sign-in ended (revoked
                // or expired): look again, so the sign-in card comes back.
                if self.uses_hangar_sign_in() && turn.status == TurnStatus::Failed {
                    self.refresh_hangar_account(cx);
                }
                if let Some(next) = self.orchestrator.turn_finished(thread_id) {
                    self.start_turn(next, cx);
                }
                let id = thread_id.clone();
                AppState::global(cx)
                    .db
                    .write_detached("touch thread", move |w| w.touch_agent_thread(&id));
            }
            _ => {}
        }
        cx.notify();
    }

    fn log_event(&self, event: &AgentEvent, cx: &App) {
        if !event.is_loggable() {
            return;
        }
        let Some(thread_id) = event.thread_id().map(str::to_string) else { return };
        let turn_id = match event {
            AgentEvent::TurnStarted { turn, .. } | AgentEvent::TurnCompleted { turn, .. } => Some(turn.id.clone()),
            AgentEvent::ItemStarted { turn_id, .. }
            | AgentEvent::ItemCompleted { turn_id, .. }
            | AgentEvent::TokenUsage { turn_id, .. }
            | AgentEvent::TurnUsage { turn_id, .. }
            | AgentEvent::Error { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        };
        let kind = event.kind();
        let payload = event.log_payload();
        AppState::global(cx).db.write_detached("agent event", move |w| {
            w.append_agent_event(&thread_id, turn_id.as_deref(), kind, &payload)
                .map(|_| ())
        });
    }

    fn record_usage(
        &mut self,
        thread_id: String,
        turn_id: String,
        total: cockpit_agents::protocol::TokenUsageBreakdown,
        cx: &mut Context<Self>,
    ) {
        let model = self
            .threads
            .iter()
            .find(|t| t.id == thread_id)
            .and_then(|t| t.model.clone())
            .or_else(|| self.effective_model().map(|m| m.id.clone()))
            .unwrap_or_else(|| "unknown".into());
        let counts = TokenCounts {
            input_tokens: total.input_tokens,
            cached_input_tokens: total.cached_input_tokens,
            output_tokens: total.output_tokens,
            reasoning_output_tokens: total.reasoning_output_tokens,
            total_tokens: total.total_tokens,
        };
        let workspace_id = self.workspace_id;
        let write = AppState::global(cx).db.write(move |w| {
            w.record_thread_usage_total(&thread_id, Some(&turn_id), Some(workspace_id), &model, counts)
        });
        cx.spawn(async move |this, cx| {
            if let Err(err) = write.await {
                tracing::warn!("recording token usage: {err:#}");
            }
            this.update(cx, |this, cx| this.refresh_usage(cx)).ok();
        })
        .detach();
    }

    /// Records one turn's usage as reported by a backend that counts per
    /// turn (Claude), rather than as a running total.
    fn record_turn_usage(
        &mut self,
        thread_id: String,
        turn_id: String,
        usage: cockpit_agents::protocol::TokenUsageBreakdown,
        cx: &mut Context<Self>,
    ) {
        let model = self
            .threads
            .iter()
            .find(|t| t.id == thread_id)
            .and_then(|t| t.model.clone())
            .or_else(|| self.effective_model().map(|m| m.id.clone()))
            .unwrap_or_else(|| "unknown".into());
        let record = UsageRecord {
            thread_id: Some(thread_id),
            turn_id: Some(turn_id),
            workspace_id: Some(self.workspace_id),
            model,
            counts: TokenCounts {
                input_tokens: usage.input_tokens,
                cached_input_tokens: usage.cached_input_tokens,
                output_tokens: usage.output_tokens,
                reasoning_output_tokens: usage.reasoning_output_tokens,
                total_tokens: usage.total_tokens,
            },
        };
        let write = AppState::global(cx).db.write(move |w| w.record_usage(&record));
        cx.spawn(async move |this, cx| {
            if let Err(err) = write.await {
                tracing::warn!("recording token usage: {err:#}");
            }
            this.update(cx, |this, cx| this.refresh_usage(cx)).ok();
        })
        .detach();
    }

    fn refresh_usage(&mut self, cx: &mut Context<Self>) {
        let since = cockpit_orchestrator::budget::period_start(cockpit_persistence::BudgetPeriod::Day, Local::now());
        let read = AppState::global(cx)
            .db
            .read(move |r| r.usage_total(&UsageFilter { since, ..Default::default() }));
        cx.spawn(async move |this, cx| {
            if let Ok(total) = read.await {
                this.update(cx, |this, cx| {
                    this.usage_today = total.total_tokens;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn shutdown(&mut self) {
        if let Some(backend) = self.backend.take() {
            backend.shutdown();
        }
    }
}

/// "plus" → "Plus", for plan names.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

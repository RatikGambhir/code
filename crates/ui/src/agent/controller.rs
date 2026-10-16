//! The Codex side of a workspace: connection, threads, turns, approvals,
//! browser tool calls and usage accounting. The panel renders it.

use std::{path::PathBuf, sync::Arc};

use chrono::Local;
use cockpit_agents::{
    protocol::{
        ApprovalDecision, DynamicToolCallResponse, GetAccountResponse, LoginParams, LoginResponse,
        Model, ThreadItem, UserInput,
    },
    AgentBackend, AgentEvent, ApprovalState, Connection, Prompt, ToolCall, Transcript,
};
use cockpit_browser::BrowserAction;
use cockpit_orchestrator::{
    admit_turn, Admission, AgentPolicy, AutoDecision, Dispatch, Orchestrator, PermissionPreset,
    QueuedPrompt, ToolDecision, TurnContext,
};
use cockpit_persistence::{now_ts, AgentThreadRecord, TokenCounts, UsageFilter};
use gpui::{App, AppContext as _, Context, EventEmitter, SharedString, Task};

use crate::state::AppState;

/// The key the global agent policy is stored under.
const POLICY_KEY: &str = "global";

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    Idle,
    /// `codex` was not found.
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
    /// The transcript reflects the server's history (started here or resumed).
    pub loaded: bool,
    pub loading: bool,
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
    /// Run a browser action for Codex and answer with `respond_tool_call`.
    RunBrowserAction { call: ToolCall, action: BrowserAction },
    /// Codex edited these files.
    FilesChanged(Vec<PathBuf>),
}

pub struct AgentController {
    workspace_id: i64,
    cwd: PathBuf,
    state: ConnectionState,
    backend: Option<Arc<dyn AgentBackend>>,
    models: Vec<Model>,
    account: Option<GetAccountResponse>,
    login: Option<LoginResponse>,
    threads: Vec<ThreadSession>,
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
    /// A prompt typed before any thread existed; sent once one is created.
    first_prompt: Option<String>,
    _events: Option<Task<()>>,
}

impl EventEmitter<AgentControllerEvent> for AgentController {}

impl AgentController {
    pub fn new(workspace_id: i64, cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let settings = AppState::global(cx).settings.clone();
        let controller = Self {
            workspace_id,
            cwd,
            state: ConnectionState::Idle,
            backend: None,
            models: Vec::new(),
            account: None,
            login: None,
            threads: Vec::new(),
            active: None,
            orchestrator: Orchestrator::new(AgentPolicy::default()),
            pending_tools: Vec::new(),
            model: settings.default_model,
            effort: settings.default_effort,
            browser_tools: settings.agent_browser_tools,
            agent_browser_url: None,
            usage_today: 0,
            first_prompt: None,
            _events: None,
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
                            if this.threads.iter().any(|t| t.id == record.thread_id) {
                                continue;
                            }
                            this.threads.push(ThreadSession {
                                title: record
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| "Untitled thread".into())
                                    .into(),
                                id: record.thread_id,
                                model: record.model,
                                transcript: Transcript::new(),
                                loaded: false,
                                loading: false,
                            });
                        }
                    }
                    Err(err) => tracing::warn!("loading agent state: {err:#}"),
                }
                this.refresh_usage(cx);
                this.connect(cx);
            })
        })
        .detach();
    }

    // --- accessors ---------------------------------------------------------

    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn models(&self) -> &[Model] {
        &self.models
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// The model new turns will use: the chosen one or the server default.
    pub fn effective_model(&self) -> Option<&Model> {
        match &self.model {
            Some(id) => self.models.iter().find(|m| &m.id == id),
            None => self.models.iter().find(|m| m.is_default),
        }
    }

    pub fn permissions(&self) -> PermissionPreset {
        self.orchestrator.policy().permissions
    }

    pub fn browser_tools(&self) -> bool {
        self.browser_tools
    }

    pub fn needs_login(&self) -> bool {
        matches!(&self.account, Some(account) if account.requires_openai_auth && account.account.is_none())
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

    pub fn is_running(&self) -> bool {
        self.active_thread().is_some_and(|t| t.transcript.is_running())
    }

    pub fn queued(&self) -> usize {
        self.active
            .as_deref()
            .map(|id| self.orchestrator.queued_for(id))
            .unwrap_or(0)
    }

    // --- connection --------------------------------------------------------

    pub fn connect(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ConnectionState::Connecting | ConnectionState::Connected { .. }) {
            return;
        }
        let state = AppState::global(cx);
        let Some(launch) = state.codex_launch() else {
            self.state = ConnectionState::NotInstalled;
            cx.notify();
            return;
        };
        // The experimental API is what allows client-side (browser) tools.
        let connecting = state.agents.connect_codex(launch, self.cwd.clone(), true);
        self.state = ConnectionState::Connecting;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let connection = connecting.await;
            this.update(cx, |this, cx| match connection {
                Ok(connection) => this.on_connected(connection, cx),
                Err(err) => {
                    this.state = ConnectionState::Failed(format!("{err:#}"));
                    cx.notify();
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

        // Reopen the most recent thread so the panel is not empty.
        if self.active.is_none() {
            if let Some(id) = self.threads.first().map(|t| t.id.clone()) {
                self.select_thread(&id, cx);
            }
        }
        cx.notify();
    }

    fn refresh_account(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else { return };
        let account = backend.account();
        cx.spawn(async move |this, cx| {
            let account = account.await;
            this.update(cx, |this, cx| {
                match account {
                    Ok(account) => this.account = Some(account),
                    Err(err) => tracing::warn!("reading the Codex account: {err:#}"),
                }
                cx.notify();
            })
        })
        .detach();
    }

    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = self.backend.take() {
            backend.shutdown();
        }
        self._events = None;
        self.state = ConnectionState::Idle;
        for thread in &mut self.threads {
            thread.loaded = false;
        }
        self.connect(cx);
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
        self.model = model.clone();
        // The previous effort may not exist for the new model.
        self.effort = None;
        AppState::save_settings(cx, |settings| {
            settings.default_model = model;
            settings.default_effort = None;
        });
        cx.notify();
    }

    pub fn set_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        self.effort = effort.clone();
        AppState::save_settings(cx, |settings| settings.default_effort = effort);
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

    pub fn new_thread(&mut self, cx: &mut Context<Self>) {
        self.start_thread(self.browser_tools, cx);
    }

    fn start_thread(&mut self, with_tools: bool, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            cx.emit(AgentControllerEvent::Notify("Codex is not connected.".into()));
            return;
        };
        let config = self.orchestrator.thread_config(
            Some(self.cwd.to_string_lossy().into_owned()),
            self.model.clone(),
            with_tools,
        );
        let request = backend.start_thread(config);
        cx.spawn(async move |this, cx| {
            let response = request.await;
            this.update(cx, |this, cx| match response {
                Ok(response) => {
                    let thread = response.thread;
                    this.threads.insert(
                        0,
                        ThreadSession {
                            id: thread.id.clone(),
                            title: "New thread".into(),
                            model: Some(response.model.clone()),
                            transcript: Transcript::new(),
                            loaded: true,
                            loading: false,
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
                    };
                    AppState::global(cx)
                        .db
                        .write_detached("agent thread", move |w| w.upsert_agent_thread(&record));
                    if let Some(prompt) = this.first_prompt.take() {
                        this.send(prompt, cx);
                    }
                    cx.notify();
                }
                Err(err) if with_tools && format!("{err:#}").contains("dynamic") => {
                    // An app server without dynamic tool support: run without
                    // the browser tools rather than not at all.
                    tracing::warn!("thread/start rejected dynamic tools: {err:#}");
                    cx.emit(AgentControllerEvent::Notify(
                        "This Codex version does not support IDE browser tools; continuing without them.".into(),
                    ));
                    this.start_thread(false, cx);
                }
                Err(err) => {
                    this.first_prompt = None;
                    cx.emit(AgentControllerEvent::Notify(format!("Could not start a thread: {err:#}").into()));
                }
            })
        })
        .detach();
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
        let Some(backend) = self.backend.clone() else { return };
        if let Some(thread) = self.thread_mut(&id) {
            thread.loading = true;
        }
        let config = self.orchestrator.thread_config(
            Some(self.cwd.to_string_lossy().into_owned()),
            None,
            false,
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
                        thread.transcript.push_error(format!("Could not resume this thread: {err:#}"));
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
            self.active = self.threads.first().map(|t| t.id.clone());
        }
        let id = id.to_string();
        AppState::global(cx)
            .db
            .write_detached("archive thread", move |w| w.archive_agent_thread(&id));
        cx.notify();
    }

    // --- turns -------------------------------------------------------------

    /// Sends a prompt to the active thread (creating one if needed), through
    /// policy, budget admission and the scheduler.
    pub fn send(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.backend.is_none() {
            cx.emit(AgentControllerEvent::Notify("Codex is not connected.".into()));
            return;
        }
        let Some(thread_id) = self.active.clone() else {
            self.first_prompt = Some(text);
            self.new_thread(cx);
            return;
        };
        if let Err(reason) = self.orchestrator.check_prompt(&text) {
            if let Some(thread) = self.thread_mut(&thread_id) {
                thread.transcript.push_error(reason);
            }
            cx.notify();
            return;
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
        let prompt = QueuedPrompt {
            thread_id: thread_id.clone(),
            text: text.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
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
        let Some(backend) = self.backend.clone() else { return };
        let thread_id = prompt.thread_id.clone();
        let request = backend.submit_prompt(Prompt {
            thread_id: prompt.thread_id,
            input: vec![UserInput::text(prompt.text)],
            model: prompt.model,
            effort: prompt.effort,
            approval_policy: None,
        });
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    if let Some(thread) = this.thread_mut(&thread_id) {
                        thread.transcript.push_error(format!("Codex did not start the turn: {err:#}"));
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

    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.active_thread() else { return };
        let thread_id = thread.id.clone();
        let turn_id = thread.transcript.active_turn().map(str::to_string);
        let cancelled = self.orchestrator.cancel_queued(&thread_id);
        if cancelled > 0 {
            if let Some(thread) = self.thread_mut(&thread_id) {
                thread.transcript.push_notice(format!("Dropped {cancelled} queued prompt(s)."));
            }
        }
        if let (Some(backend), Some(turn_id)) = (self.backend.clone(), turn_id) {
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
                cx.emit(AgentControllerEvent::Notify(format!("Could not answer Codex: {err:#}").into()));
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
                cx.emit(AgentControllerEvent::RunBrowserAction { call, action });
            }
            Ok((_, ToolDecision::Deny(reason))) => {
                self.respond_tool_call(&call, DynamicToolCallResponse::text(reason, false));
            }
            Ok((action, ToolDecision::NeedsApproval(reason))) => {
                cx.emit(AgentControllerEvent::Notify(format!("Codex asks: {reason}").into()));
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
                call: pending.call,
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
                    AutoDecision::AskUser => {
                        cx.emit(AgentControllerEvent::Notify("Codex is waiting for your approval.".into()))
                    }
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
            AgentEvent::TurnCompleted { thread_id, .. } => {
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

//! Running and debugging: the workspace's breakpoints, run configurations
//! and the debug session, which builds a target with Cargo and drives
//! `lldb-dap` through it.
//!
//! [`Debugger`] is the model, one per workspace. The debug panel draws it,
//! editors draw its breakpoints and paused line (see [`editor`]), and the
//! workspace routes commands to it.

pub mod editor;
pub mod panel;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use cockpit_debugger::{
    breakpoints::LineBreakpoint,
    find_lldb_dap, find_runnables,
    protocol::{EvaluateResponse, FunctionBreakpoint, Scope, StackFrame, StoppedEvent, Thread, Variable},
    values::pretty_type,
    workspace_targets, Build, BuildEvent, BreakpointSet, CargoTarget, DapClient, Event, RunConfig, Runnable,
    RustToolchain, TargetKind,
};
use futures::channel::oneshot;
use gpui::{AppContext as _, Context, EventEmitter, SharedString, Task};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::AppState;

/// Lines kept in the console.
const CONSOLE_LIMIT: usize = 5_000;
/// Stack frames fetched per stop.
const FRAME_LIMIT: u32 = 200;
/// LLDB warns about Rust on every launch; the formatters cover it.
const NOISE: &[&str] = &["has no plugin for the language \"rust\""];

pub enum DebuggerEvent {
    /// Breakpoints, runnables, the paused line or values changed.
    DecorationsChanged,
    /// Show this place in an editor (zero-based line).
    Reveal { path: PathBuf, line: u32 },
    /// A session started: show the debug panel.
    SessionStarted,
    Notify(String),
    /// Run without the debugger, in a terminal.
    RunInTerminal { program: String, args: Vec<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Building,
    Starting,
    Running,
    Paused,
    Ended { exit_code: Option<i64> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleKind {
    /// Cargo's output.
    Build,
    Stdout,
    Stderr,
    /// Notes from Hangar or LLDB.
    Info,
    Error,
    /// An expression typed in the console.
    Input,
    /// Its value.
    Result,
}

#[derive(Clone, Debug)]
pub struct ConsoleLine {
    pub kind: ConsoleKind,
    pub text: String,
    /// More of this line may follow (program output without a newline).
    open: bool,
}

pub struct Session {
    pub id: u64,
    pub config: RunConfig,
    pub phase: Phase,
    client: Option<Arc<DapClient>>,
    build: Option<Build>,
    initialized: Option<oneshot::Sender<()>>,
    panic_breakpoint: Option<i64>,
    pub console: Vec<ConsoleLine>,
    pub threads: Vec<Thread>,
    pub thread_id: Option<i64>,
    pub stop_description: Option<String>,
    pub frames: Vec<StackFrame>,
    pub selected_frame: usize,
    pub scopes: Vec<Scope>,
    /// Children of each expandable value, by `variablesReference`.
    pub children: HashMap<i64, Vec<Variable>>,
    /// Expanded tree rows, by path (`Locals/p/x`), kept across stops.
    pub expanded: HashSet<String>,
    pub watch_values: HashMap<String, Result<EvaluateResponse, String>>,
    /// The file whose breakpoints include a temporary run-to-cursor one.
    run_to_cursor: Option<PathBuf>,
    /// Bumped on every stop and frame change, so late answers are dropped.
    generation: u64,
    _tasks: Vec<Task<()>>,
}

impl Session {
    fn new(id: u64, config: RunConfig) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert("Locals".to_string());
        Self {
            id,
            config,
            phase: Phase::Building,
            client: None,
            build: None,
            initialized: None,
            panic_breakpoint: None,
            console: Vec::new(),
            threads: Vec::new(),
            thread_id: None,
            stop_description: None,
            frames: Vec::new(),
            selected_frame: 0,
            scopes: Vec::new(),
            children: HashMap::new(),
            expanded,
            watch_values: HashMap::new(),
            run_to_cursor: None,
            generation: 0,
            _tasks: Vec::new(),
        }
    }

    pub fn is_live(&self) -> bool {
        !matches!(self.phase, Phase::Ended { .. })
    }

    pub fn is_paused(&self) -> bool {
        self.phase == Phase::Paused
    }

    pub fn frame(&self) -> Option<&StackFrame> {
        self.frames.get(self.selected_frame)
    }

    fn frame_id(&self) -> Option<i64> {
        self.is_paused().then(|| self.frame().map(|f| f.id)).flatten()
    }

    pub fn push_console(&mut self, kind: ConsoleKind, text: &str) {
        let mut pieces = text.split('\n').peekable();
        while let Some(piece) = pieces.next() {
            let last = pieces.peek().is_none();
            if last && piece.is_empty() {
                // `text` ended with a newline: close the line.
                if let Some(line) = self.console.last_mut().filter(|l| l.kind == kind) {
                    line.open = false;
                }
                break;
            }
            let piece = piece.trim_end_matches('\r');
            match self.console.last_mut() {
                Some(line) if line.open && line.kind == kind => line.text.push_str(piece),
                _ => self.console.push(ConsoleLine { kind, text: piece.to_string(), open: true }),
            }
            if !last {
                if let Some(line) = self.console.last_mut() {
                    line.open = false;
                }
            }
        }
        if self.console.len() > CONSOLE_LIMIT {
            self.console.drain(..self.console.len() - CONSOLE_LIMIT);
        }
    }

    /// The top-level values of the selected frame's locals, for inline
    /// display: (name, value).
    pub fn locals(&self) -> Vec<(String, String)> {
        let Some(scope) = self.scopes.iter().find(|s| is_locals(s)) else { return Vec::new() };
        self.children
            .get(&scope.variables_reference)
            .map(|vars| vars.iter().map(|v| (v.name.clone(), v.value.clone())).collect())
            .unwrap_or_default()
    }
}

fn is_locals(scope: &Scope) -> bool {
    scope.presentation_hint.as_deref() == Some("locals") || scope.name == "Locals"
}

/// What survives a restart, per workspace.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    breakpoints: BreakpointSet,
    watches: Vec<String>,
    break_on_panic: Option<bool>,
    selected: Option<RunConfig>,
    recent: Vec<RunConfig>,
}

pub struct Debugger {
    root: PathBuf,
    settings_key: String,
    breakpoints: BreakpointSet,
    pub break_on_panic: bool,
    pub watches: Vec<String>,
    targets: Vec<CargoTarget>,
    targets_error: Option<String>,
    selected: Option<RunConfig>,
    /// Tests and other configurations run lately, newest first.
    recent: Vec<RunConfig>,
    session: Option<Session>,
    next_session_id: u64,
    save_task: Option<Task<()>>,
    _targets_task: Option<Task<()>>,
}

impl EventEmitter<DebuggerEvent> for Debugger {}

impl Debugger {
    pub fn new(root: PathBuf, workspace_id: i64, cx: &mut Context<Self>) -> Self {
        let settings_key = format!("debugger.{workspace_id}");
        let key = settings_key.clone();
        let persisted: Persisted = AppState::global(cx)
            .db
            .read_blocking(move |r| r.setting(&key))
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let mut debugger = Self {
            root,
            settings_key,
            breakpoints: persisted.breakpoints,
            break_on_panic: persisted.break_on_panic.unwrap_or(true),
            watches: persisted.watches,
            targets: Vec::new(),
            targets_error: None,
            selected: persisted.selected,
            recent: persisted.recent,
            session: None,
            next_session_id: 1,
            save_task: None,
            _targets_task: None,
        };
        debugger.refresh_targets(cx);
        debugger
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let persisted = Persisted {
            breakpoints: self.breakpoints.clone(),
            watches: self.watches.clone(),
            break_on_panic: Some(self.break_on_panic),
            selected: self.selected.clone(),
            recent: self.recent.clone(),
        };
        let key = self.settings_key.clone();
        self.save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let Ok(json) = serde_json::to_string(&persisted) else { return };
            cx.update(|cx| {
                AppState::global(cx).db.write_detached("debugger", move |w| w.set_setting(&key, &json));
            })
            .ok();
        }));
    }

    // --- cargo targets and run configurations ------------------------------

    /// Reads the workspace's targets again (after `Cargo.toml` changed).
    pub fn refresh_targets(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        if !root.join("Cargo.toml").exists() {
            self.targets.clear();
            self.targets_error = None;
            return;
        }
        let task = cx.background_spawn(async move { workspace_targets(&root) });
        self._targets_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(targets) => {
                        this.targets = targets;
                        this.targets_error = None;
                    }
                    Err(err) => this.targets_error = Some(format!("{err:#}")),
                }
                if this.selected.as_ref().is_none_or(|s| !this.config_exists(s)) {
                    this.selected = this.configs().into_iter().next();
                }
                cx.emit(DebuggerEvent::DecorationsChanged);
                cx.notify();
            })
            .ok();
        }));
    }

    fn config_exists(&self, config: &RunConfig) -> bool {
        self.targets
            .iter()
            .any(|t| t.package == config.package && t.name == config.target && t.kind == config.target_kind)
    }

    pub fn has_cargo_project(&self) -> bool {
        !self.targets.is_empty()
    }

    /// The workspace's Cargo targets (empty until `cargo metadata` answers).
    pub fn targets(&self) -> &[CargoTarget] {
        &self.targets
    }

    pub fn targets_error(&self) -> Option<&str> {
        self.targets_error.as_deref()
    }

    /// Binaries and examples, then recent tests.
    pub fn configs(&self) -> Vec<RunConfig> {
        let mut configs: Vec<RunConfig> = self
            .targets
            .iter()
            .filter(|t| matches!(t.kind, TargetKind::Bin | TargetKind::Example))
            .map(RunConfig::binary)
            .collect();
        // Binaries first, keeping cargo's order otherwise.
        configs.sort_by_key(|c| c.target_kind != TargetKind::Bin);
        for recent in &self.recent {
            if !configs.iter().any(|c| same_config(c, recent)) {
                configs.push(recent.clone());
            }
        }
        // Arguments live on the selected entry.
        if let Some(selected) = &self.selected {
            for config in &mut configs {
                if same_config(config, selected) {
                    config.args = selected.args.clone();
                }
            }
        }
        configs
    }

    pub fn selected_config(&self) -> Option<&RunConfig> {
        self.selected.as_ref()
    }

    pub fn select_config(&mut self, config: RunConfig, cx: &mut Context<Self>) {
        self.selected = Some(config);
        self.save(cx);
        cx.notify();
    }

    pub fn set_program_args(&mut self, args: &str, cx: &mut Context<Self>) {
        if let Some(selected) = &mut self.selected {
            selected.args = split_args(args);
            let selected = selected.clone();
            if let Some(recent) = self.recent.iter_mut().find(|r| same_config(r, &selected)) {
                recent.args = selected.args.clone();
            }
            self.save(cx);
            cx.notify();
        }
    }

    pub fn runnables(&self, path: &Path, text: &str) -> Vec<Runnable> {
        if self.targets.is_empty() || path.extension().is_none_or(|e| e != "rs") {
            return Vec::new();
        }
        find_runnables(path, text, &self.targets)
    }

    fn remember(&mut self, config: &RunConfig) {
        self.selected = Some(config.clone());
        if config.test || !self.config_exists(config) {
            self.recent.retain(|r| !same_config(r, config));
            self.recent.insert(0, config.clone());
            self.recent.truncate(8);
        }
    }

    // --- breakpoints -------------------------------------------------------

    pub fn breakpoints(&self) -> &BreakpointSet {
        &self.breakpoints
    }

    pub fn toggle_breakpoint(&mut self, path: &Path, line: usize, cx: &mut Context<Self>) {
        self.breakpoints.toggle(path, line);
        self.breakpoints_changed(path, cx);
    }

    pub fn toggle_breakpoint_enabled(&mut self, path: &Path, line: usize, cx: &mut Context<Self>) {
        match self.breakpoints.get_mut(path, line) {
            Some(bp) => bp.enabled = !bp.enabled,
            None => {
                self.breakpoints.insert(path, LineBreakpoint { enabled: false, ..LineBreakpoint::new(line) });
            }
        }
        self.breakpoints_changed(path, cx);
    }

    pub fn update_breakpoint(&mut self, path: &Path, breakpoint: LineBreakpoint, cx: &mut Context<Self>) {
        self.breakpoints.insert(path, breakpoint);
        self.breakpoints_changed(path, cx);
    }

    pub fn remove_breakpoint(&mut self, path: &Path, line: usize, cx: &mut Context<Self>) {
        self.breakpoints.remove(path, line);
        self.breakpoints_changed(path, cx);
    }

    pub fn remove_all_breakpoints(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self.breakpoints.files().map(|(p, _)| p.to_path_buf()).collect();
        self.breakpoints.clear();
        for path in paths {
            self.send_breakpoints(&path, None, cx);
        }
        self.save(cx);
        cx.emit(DebuggerEvent::DecorationsChanged);
        cx.notify();
    }

    pub fn set_break_on_panic(&mut self, on: bool, cx: &mut Context<Self>) {
        self.break_on_panic = on;
        self.send_function_breakpoints(cx);
        self.save(cx);
        cx.notify();
    }

    /// Keeps `path`'s breakpoints on their lines through an edit.
    pub fn reanchor(&mut self, path: &Path, old: &str, new: &str, cx: &mut Context<Self>) {
        if self.breakpoints.reanchor(path, old, new) {
            self.save(cx);
            cx.emit(DebuggerEvent::DecorationsChanged);
            cx.notify();
        }
    }

    fn breakpoints_changed(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.send_breakpoints(path, None, cx);
        self.save(cx);
        cx.emit(DebuggerEvent::DecorationsChanged);
        cx.notify();
    }

    /// Sends `path`'s enabled breakpoints (plus a temporary one) to the
    /// adapter, if a session is running.
    fn send_breakpoints(&mut self, path: &Path, extra_line: Option<usize>, cx: &mut Context<Self>) {
        let Some(client) = self.session.as_ref().filter(|s| s.is_live()).and_then(|s| s.client.clone()) else {
            return;
        };
        let mut breakpoints: Vec<_> = self
            .breakpoints
            .for_file(path)
            .iter()
            .filter(|b| b.enabled)
            .map(LineBreakpoint::to_source_breakpoint)
            .collect();
        if let Some(line) = extra_line {
            if !breakpoints.iter().any(|b| b.line == line as u32 + 1) {
                breakpoints.push(LineBreakpoint::new(line).to_source_breakpoint());
            }
        }
        let request = client.set_breakpoints(path, &breakpoints);
        cx.spawn(async move |_, _| {
            if let Err(err) = request.await {
                tracing::warn!("setting breakpoints: {err:#}");
            }
        })
        .detach();
    }

    fn send_function_breakpoints(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().filter(|s| s.is_live()) else { return };
        let Some(client) = session.client.clone() else { return };
        let id = session.id;
        let request = client.set_function_breakpoints(&panic_breakpoints(self.break_on_panic));
        cx.spawn(async move |this, cx| {
            if let Ok(set) = request.await {
                this.update(cx, |this, _| {
                    if let Some(session) = this.session.as_mut().filter(|s| s.id == id) {
                        session.panic_breakpoint = set.first().and_then(|b| b.id);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    // --- watches -----------------------------------------------------------

    pub fn add_watch(&mut self, expression: &str, cx: &mut Context<Self>) {
        let expression = expression.trim();
        if expression.is_empty() || self.watches.iter().any(|w| w == expression) {
            return;
        }
        self.watches.push(expression.to_string());
        self.save(cx);
        self.refresh_watches(cx);
        cx.notify();
    }

    pub fn remove_watch(&mut self, expression: &str, cx: &mut Context<Self>) {
        self.watches.retain(|w| w != expression);
        self.save(cx);
        cx.notify();
    }

    // --- sessions ----------------------------------------------------------

    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    pub fn is_live(&self) -> bool {
        self.session.as_ref().is_some_and(Session::is_live)
    }

    pub fn is_paused(&self) -> bool {
        self.session.as_ref().is_some_and(Session::is_paused)
    }

    /// Runs `config` in a terminal, without the debugger.
    pub fn run(&mut self, config: RunConfig, cx: &mut Context<Self>) {
        self.remember(&config);
        self.save(cx);
        cx.emit(DebuggerEvent::RunInTerminal { program: "cargo".into(), args: config.cargo_run_args() });
        cx.notify();
    }

    pub fn run_selected(&mut self, cx: &mut Context<Self>) {
        match self.selected.clone() {
            Some(config) => self.run(config, cx),
            None => self.no_config(cx),
        }
    }

    pub fn debug_selected(&mut self, cx: &mut Context<Self>) {
        match self.selected.clone() {
            Some(config) => self.debug(config, cx),
            None => self.no_config(cx),
        }
    }

    fn no_config(&mut self, cx: &mut Context<Self>) {
        let message = match &self.targets_error {
            Some(err) => format!("Cargo could not read this workspace: {err}"),
            None if !self.root.join("Cargo.toml").exists() => "Run and Debug work with Cargo projects; this folder has no Cargo.toml.".to_string(),
            None => "Nothing to run yet: this workspace has no binaries or examples. Use the ▶ next to a test.".to_string(),
        };
        cx.emit(DebuggerEvent::Notify(message));
    }

    /// Builds `config` and starts debugging it.
    pub fn debug(&mut self, config: RunConfig, cx: &mut Context<Self>) {
        self.stop(cx);
        self.remember(&config);
        self.save(cx);
        let id = self.next_session_id;
        self.next_session_id += 1;
        let mut session = Session::new(id, config.clone());
        // Expansions carry over from the last session of the same target.
        if let Some(previous) = &self.session {
            if same_config(&previous.config, &config) {
                session.expanded = previous.expanded.clone();
            }
        }
        session.push_console(ConsoleKind::Info, &format!("Building {}…\n", config.label()));
        session.push_console(ConsoleKind::Build, &format!("cargo {}\n", config.cargo_build_args().join(" ")));
        match Build::start(&config, &self.root) {
            Ok((build, events)) => {
                session.build = Some(build);
                session._tasks.push(cx.spawn(async move |this, cx| {
                    while let Ok(event) = events.recv().await {
                        let alive = this.update(cx, |this, cx| this.on_build_event(id, event, cx));
                        if alive.is_err() {
                            break;
                        }
                    }
                }));
            }
            Err(err) => {
                session.push_console(ConsoleKind::Error, &format!("{err:#}\n"));
                session.phase = Phase::Ended { exit_code: None };
            }
        }
        self.session = Some(session);
        cx.emit(DebuggerEvent::SessionStarted);
        cx.emit(DebuggerEvent::DecorationsChanged);
        cx.notify();
    }

    fn session_mut(&mut self, id: u64) -> Option<&mut Session> {
        self.session.as_mut().filter(|s| s.id == id)
    }

    fn on_build_event(&mut self, id: u64, event: BuildEvent, cx: &mut Context<Self>) {
        let Some(session) = self.session_mut(id) else { return };
        match event {
            BuildEvent::Line(line) => session.push_console(ConsoleKind::Build, &format!("{line}\n")),
            BuildEvent::Finished(Ok(program)) => {
                session.build = None;
                session.phase = Phase::Starting;
                session.push_console(ConsoleKind::Info, &format!("Debugging {}\n", program.display()));
                self.launch(id, program, cx);
            }
            BuildEvent::Finished(Err(err)) => {
                session.build = None;
                session.push_console(ConsoleKind::Error, &format!("Build failed: {err}\n"));
                session.phase = Phase::Ended { exit_code: None };
                cx.emit(DebuggerEvent::Notify(format!("Build failed: {err}. The Debug panel's console has cargo's output.")));
                cx.emit(DebuggerEvent::DecorationsChanged);
            }
        }
        cx.notify();
    }

    fn launch(&mut self, id: u64, program: PathBuf, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let Some(config) = self.session_mut(id).map(|s| s.config.clone()) else { return };
        let args = config.program_args(true);
        // `cargo test` runs tests from their package's directory; `cargo run`
        // from where it was started (the workspace).
        let cwd = if config.test {
            self.targets
                .iter()
                .find(|t| t.package == config.package)
                .map(|t| t.package_dir.clone())
                .filter(|dir| dir.is_dir())
                .unwrap_or_else(|| root.clone())
        } else {
            root.clone()
        };
        let adapter = match find_lldb_dap() {
            Ok(adapter) => adapter,
            Err(err) => {
                self.end_with_error(id, format!("{err:#}"), cx);
                return;
            }
        };
        let (client, events) = match DapClient::spawn(&adapter, &root) {
            Ok(spawned) => spawned,
            Err(err) => {
                self.end_with_error(id, format!("{err:#}"), cx);
                return;
            }
        };
        let (initialized_tx, initialized_rx) = oneshot::channel();
        let Some(session) = self.session_mut(id) else { return };
        session.client = Some(client.clone());
        session.initialized = Some(initialized_tx);
        session._tasks.push(cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let ended = matches!(event, Event::AdapterExited);
                let alive = this.update(cx, |this, cx| this.on_adapter_event(id, event, cx));
                if alive.is_err() || ended {
                    break;
                }
            }
        }));

        let toolchain_root = root.clone();
        let toolchain = cx.background_spawn(async move { RustToolchain::detect(&toolchain_root) });
        let task = cx.spawn(async move |this, cx| {
            let result: Result<()> = async {
                client.initialize().await?;
                let toolchain = toolchain.await;
                let launch = client.launch(json!({
                    "program": program,
                    "args": args,
                    "cwd": cwd,
                    "env": ["RUST_BACKTRACE=1"],
                    "initCommands": toolchain.init_commands(),
                    "stopOnEntry": false,
                }));
                let _ = initialized_rx.await;
                // Breakpoints, then go.
                let (files, panic) = this.update(cx, |this, _| {
                    let files: Vec<(PathBuf, Vec<_>)> = this
                        .breakpoints
                        .files()
                        .map(|(path, bps)| {
                            (
                                path.to_path_buf(),
                                bps.iter().filter(|b| b.enabled).map(LineBreakpoint::to_source_breakpoint).collect(),
                            )
                        })
                        .collect();
                    (files, panic_breakpoints(this.break_on_panic))
                })?;
                for (path, breakpoints) in files {
                    if let Err(err) = client.set_breakpoints(&path, &breakpoints).await {
                        tracing::warn!("breakpoints in {}: {err:#}", path.display());
                    }
                }
                let panic_set = client.set_function_breakpoints(&panic).await.unwrap_or_default();
                this.update(cx, |this, _| {
                    if let Some(session) = this.session_mut(id) {
                        session.panic_breakpoint = panic_set.first().and_then(|b| b.id);
                    }
                })?;
                client.configuration_done().await?;
                launch.await?;
                Ok(())
            }
            .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    if let Some(session) = this.session_mut(id) {
                        if session.phase == Phase::Starting {
                            session.phase = Phase::Running;
                        }
                    }
                    cx.notify();
                }
                Err(err) => this.end_with_error(id, format!("Could not start the debugger: {err:#}"), cx),
            })
            .ok();
        });
        if let Some(session) = self.session_mut(id) {
            session._tasks.push(task);
        }
    }

    fn end_with_error(&mut self, id: u64, message: String, cx: &mut Context<Self>) {
        let Some(session) = self.session_mut(id) else { return };
        session.push_console(ConsoleKind::Error, &format!("{message}\n"));
        session.phase = Phase::Ended { exit_code: None };
        if let Some(client) = session.client.take() {
            client.kill();
        }
        cx.emit(DebuggerEvent::Notify(message));
        cx.emit(DebuggerEvent::DecorationsChanged);
        cx.notify();
    }

    fn on_adapter_event(&mut self, id: u64, event: Event, cx: &mut Context<Self>) {
        let Some(session) = self.session_mut(id) else { return };
        match event {
            Event::Initialized => {
                if let Some(tx) = session.initialized.take() {
                    let _ = tx.send(());
                }
            }
            Event::Output(output) => {
                let kind = match output.category.as_deref() {
                    Some("stdout") => ConsoleKind::Stdout,
                    Some("stderr") => ConsoleKind::Stderr,
                    Some("important") => ConsoleKind::Error,
                    // `console` (LLDB's own chatter) and anything else.
                    _ => ConsoleKind::Info,
                };
                if NOISE.iter().any(|n| output.output.contains(n)) {
                    return;
                }
                // LLDB echoes the init commands; nobody needs them.
                if kind == ConsoleKind::Info
                    && (output.output.starts_with("(lldb)") || output.output.starts_with("Running initCommands"))
                {
                    return;
                }
                session.push_console(kind, &output.output);
            }
            Event::Process(process) => {
                if let Some(pid) = process.system_process_id {
                    session.push_console(ConsoleKind::Info, &format!("Process {pid} started\n"));
                }
                session.phase = Phase::Running;
            }
            Event::Stopped(stopped) => {
                self.on_stopped(id, stopped, cx);
                return;
            }
            Event::Continued { .. } => {
                if session.phase == Phase::Paused {
                    session.phase = Phase::Running;
                    clear_pause_state(session);
                    cx.emit(DebuggerEvent::DecorationsChanged);
                }
            }
            Event::Exited { exit_code } => {
                session.push_console(ConsoleKind::Info, &format!("\nProcess finished with exit code {exit_code}\n"));
                session.phase = Phase::Ended { exit_code: Some(exit_code) };
                clear_pause_state(session);
                cx.emit(DebuggerEvent::DecorationsChanged);
            }
            Event::Terminated | Event::AdapterExited => {
                if session.is_live() {
                    session.phase = Phase::Ended { exit_code: None };
                    session.push_console(ConsoleKind::Info, "Debugging stopped\n");
                }
                clear_pause_state(session);
                if let Some(client) = session.client.take() {
                    // Let the adapter finish on its own; kill it if it lingers.
                    cx.background_spawn(async move {
                        let _ = client.disconnect(false).await;
                    })
                    .detach();
                }
                cx.emit(DebuggerEvent::DecorationsChanged);
            }
            Event::Thread(_) | Event::Breakpoint(_) | Event::Other { .. } => return,
        }
        cx.notify();
    }

    fn on_stopped(&mut self, id: u64, stopped: StoppedEvent, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let Some(session) = self.session_mut(id) else { return };
        let Some(client) = session.client.clone() else { return };
        session.phase = Phase::Paused;
        session.generation += 1;
        let generation = session.generation;
        let panic_hit = session.panic_breakpoint.is_some_and(|bp| stopped.hit_breakpoint_ids.contains(&bp));
        session.stop_description = Some(match (panic_hit, stopped.reason.as_str()) {
            (true, _) => "Paused on a panic".to_string(),
            (_, "breakpoint") => "Paused at a breakpoint".to_string(),
            (_, "step") => "Paused after a step".to_string(),
            (_, "pause") => "Paused".to_string(),
            (_, "exception") | (_, "signal") => {
                format!("Paused: {}", stopped.description.clone().or(stopped.text.clone()).unwrap_or(stopped.reason.clone()))
            }
            (_, reason) => format!("Paused ({reason})"),
        });
        if panic_hit {
            session.push_console(ConsoleKind::Error, "The program panicked; paused before unwinding.\n");
        }
        // A run-to-cursor breakpoint has done its job.
        let restore = session.run_to_cursor.take();
        let thread_hint = stopped.thread_id;

        if let Some(path) = restore {
            self.send_breakpoints(&path, None, cx);
        }
        let task = cx.spawn(async move |this, cx| {
            let threads = client.threads().await.unwrap_or_default();
            let thread_id = thread_hint.or_else(|| threads.first().map(|t| t.id));
            let frames = match thread_id {
                Some(thread) => client.stack_trace(thread, FRAME_LIMIT).await.unwrap_or_default(),
                None => Vec::new(),
            };
            let selected = pick_frame(&frames, &root, panic_hit);
            this.update(cx, |this, cx| {
                let Some(session) = this.session_mut(id).filter(|s| s.generation == generation) else { return };
                session.threads = threads;
                session.thread_id = thread_id;
                session.frames = frames;
                session.selected_frame = selected;
                this.load_frame(cx);
            })
            .ok();
        });
        if let Some(session) = self.session_mut(id) {
            session._tasks.push(task);
        }
        cx.notify();
    }

    /// Loads scopes, the expanded part of the variables tree and watches for
    /// the selected frame, then reveals it.
    fn load_frame(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut() else { return };
        let Some(client) = session.client.clone() else { return };
        let Some(frame) = session.frame().cloned() else {
            cx.emit(DebuggerEvent::DecorationsChanged);
            cx.notify();
            return;
        };
        session.generation += 1;
        let generation = session.generation;
        let id = session.id;
        let expanded = session.expanded.clone();
        if let Some(path) = frame.path() {
            if Path::new(path).exists() {
                cx.emit(DebuggerEvent::Reveal { path: PathBuf::from(path), line: frame.line.saturating_sub(1) });
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let scopes = client.scopes(frame.id).await.unwrap_or_default();
            let children = load_tree(&client, &scopes, &expanded).await;
            this.update(cx, |this, cx| {
                let Some(session) = this.session_mut(id).filter(|s| s.generation == generation) else { return };
                session.scopes = scopes;
                session.children = children;
                this.refresh_watches(cx);
                cx.emit(DebuggerEvent::DecorationsChanged);
                cx.notify();
            })
            .ok();
        });
        if let Some(session) = self.session.as_mut() {
            session._tasks.push(task);
        }
        cx.notify();
    }

    fn refresh_watches(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else { return };
        let (Some(client), Some(frame_id)) = (session.client.clone(), session.frame_id()) else { return };
        let id = session.id;
        let generation = session.generation;
        let watches = self.watches.clone();
        cx.spawn(async move |this, cx| {
            let mut values = HashMap::new();
            for watch in watches {
                let value = client.evaluate(&watch, Some(frame_id), "watch").await.map_err(|e| clean_error(&e));
                values.insert(watch, value);
            }
            this.update(cx, |this, cx| {
                if let Some(session) = this.session_mut(id).filter(|s| s.generation == generation) {
                    session.watch_values = values;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn select_frame(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut() else { return };
        if index >= session.frames.len() {
            return;
        }
        session.selected_frame = index;
        self.load_frame(cx);
    }

    pub fn select_thread(&mut self, thread_id: i64, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut() else { return };
        let Some(client) = session.client.clone() else { return };
        session.thread_id = Some(thread_id);
        session.generation += 1;
        let (id, generation) = (session.id, session.generation);
        cx.spawn(async move |this, cx| {
            let frames = client.stack_trace(thread_id, FRAME_LIMIT).await.unwrap_or_default();
            this.update(cx, |this, cx| {
                let Some(session) = this.session_mut(id).filter(|s| s.generation == generation) else { return };
                session.frames = frames;
                session.selected_frame = 0;
                this.load_frame(cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Expands or collapses a tree row; loads its children the first time.
    pub fn toggle_expanded(&mut self, key: &str, reference: i64, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut() else { return };
        if !session.expanded.remove(key) {
            session.expanded.insert(key.to_string());
            if reference > 0 && !session.children.contains_key(&reference) {
                if let Some(client) = session.client.clone() {
                    let (id, generation) = (session.id, session.generation);
                    cx.spawn(async move |this, cx| {
                        let vars = client.variables(reference).await.unwrap_or_default();
                        this.update(cx, |this, cx| {
                            if let Some(session) = this.session_mut(id).filter(|s| s.generation == generation) {
                                session.children.insert(reference, vars);
                                cx.notify();
                            }
                        })
                        .ok();
                    })
                    .detach();
                }
            }
        }
        cx.notify();
    }

    /// Sets a variable (the child `name` of `parent`) to what `value`
    /// evaluates to, then reloads the frame.
    pub fn set_value(&mut self, parent: i64, name: &str, value: &str, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().filter(|s| s.is_paused()) else { return };
        let Some(client) = session.client.clone() else { return };
        let request = client.set_variable(parent, name, value);
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    cx.emit(DebuggerEvent::Notify(format!("Could not set the value: {}", clean_error(&err))));
                }
                this.load_frame(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Evaluates `expression` in the console.
    pub fn evaluate_in_console(&mut self, expression: &str, cx: &mut Context<Self>) {
        let expression = expression.trim().to_string();
        if expression.is_empty() {
            return;
        }
        let Some(session) = self.session.as_mut().filter(|s| s.is_live()) else {
            cx.emit(DebuggerEvent::Notify("Start debugging to evaluate expressions.".into()));
            return;
        };
        session.push_console(ConsoleKind::Input, &format!("> {expression}\n"));
        let Some(client) = session.client.clone() else { return };
        let frame_id = session.frame_id();
        let id = session.id;
        cx.spawn(async move |this, cx| {
            let result = client.evaluate(&expression, frame_id, "repl").await;
            this.update(cx, |this, cx| {
                if let Some(session) = this.session_mut(id) {
                    match result {
                        Ok(value) => {
                            let text = match value.type_name.as_deref().filter(|t| !t.is_empty()) {
                                Some(ty) => format!("{} : {}\n", value.result, pretty_type(ty)),
                                None => format!("{}\n", value.result),
                            };
                            session.push_console(ConsoleKind::Result, &text);
                        }
                        Err(err) => session.push_console(ConsoleKind::Error, &format!("{}\n", clean_error(&err))),
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// A value for the editor's hover, while paused.
    pub fn evaluate_for_hover(&self, expression: &str) -> Option<impl std::future::Future<Output = Option<String>> + 'static> {
        let session = self.session.as_ref().filter(|s| s.is_paused())?;
        let client = session.client.clone()?;
        let frame_id = session.frame_id()?;
        let request = client.evaluate(expression, Some(frame_id), "hover");
        let expression = expression.to_string();
        Some(async move {
            let value = request.await.ok()?;
            let ty = value.type_name.as_deref().map(pretty_type).unwrap_or_default();
            Some(if ty.is_empty() {
                format!("{expression} = {}", value.result)
            } else {
                format!("{expression}: {ty} = {}", value.result)
            })
        })
    }

    // --- execution control -------------------------------------------------

    fn paused_thread(&self) -> Option<(Arc<DapClient>, i64)> {
        let session = self.session.as_ref().filter(|s| s.is_paused())?;
        Some((session.client.clone()?, session.thread_id?))
    }

    fn step(
        &mut self,
        cx: &mut Context<Self>,
        request: impl FnOnce(&DapClient, i64) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value>> + Send>>,
    ) {
        let Some((client, thread)) = self.paused_thread() else { return };
        let future = request(&client, thread);
        if let Some(session) = self.session.as_mut() {
            session.phase = Phase::Running;
            clear_pause_state(session);
        }
        cx.emit(DebuggerEvent::DecorationsChanged);
        cx.notify();
        cx.spawn(async move |this, cx| {
            if let Err(err) = future.await {
                this.update(cx, |_, cx| cx.emit(DebuggerEvent::Notify(clean_error(&err)))).ok();
            }
        })
        .detach();
    }

    pub fn resume(&mut self, cx: &mut Context<Self>) {
        self.step(cx, |c, t| Box::pin(c.continue_(t)));
    }

    pub fn step_over(&mut self, cx: &mut Context<Self>) {
        self.step(cx, |c, t| Box::pin(c.next(t)));
    }

    pub fn step_into(&mut self, cx: &mut Context<Self>) {
        self.step(cx, |c, t| Box::pin(c.step_in(t)));
    }

    pub fn step_out(&mut self, cx: &mut Context<Self>) {
        self.step(cx, |c, t| Box::pin(c.step_out(t)));
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().filter(|s| s.phase == Phase::Running) else { return };
        let Some(client) = session.client.clone() else { return };
        let thread = session.thread_id;
        cx.spawn(async move |_, _| {
            let thread = match thread {
                Some(thread) => thread,
                None => match client.threads().await.ok().and_then(|t| t.first().map(|t| t.id)) {
                    Some(thread) => thread,
                    None => return,
                },
            };
            let _ = client.pause(thread).await;
        })
        .detach();
    }

    /// Continues to `line` of `path` (zero-based) with a one-off breakpoint.
    pub fn run_to_cursor(&mut self, path: &Path, line: usize, cx: &mut Context<Self>) {
        if self.paused_thread().is_none() {
            return;
        }
        self.send_breakpoints(path, Some(line), cx);
        if let Some(session) = self.session.as_mut() {
            session.run_to_cursor = Some(path.to_path_buf());
        }
        self.resume(cx);
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut().filter(|s| s.is_live()) else { return };
        if let Some(build) = session.build.take() {
            build.cancel();
        }
        session.phase = Phase::Ended { exit_code: None };
        session.push_console(ConsoleKind::Info, "Stopped\n");
        clear_pause_state(session);
        if let Some(client) = session.client.take() {
            cx.background_spawn(async move {
                let disconnect = client.disconnect(true);
                let timeout = async {
                    smol_timer(Duration::from_secs(2)).await;
                };
                futures::pin_mut!(disconnect, timeout);
                let _ = futures::future::select(disconnect, timeout).await;
                client.kill();
            })
            .detach();
        }
        cx.emit(DebuggerEvent::DecorationsChanged);
        cx.notify();
    }

    pub fn restart(&mut self, cx: &mut Context<Self>) {
        let config = self.session.as_ref().map(|s| s.config.clone()).or_else(|| self.selected.clone());
        match config {
            Some(config) => self.debug(config, cx),
            None => self.no_config(cx),
        }
    }

    /// The selected frame's place, when paused: (path, zero-based line, is
    /// the top frame).
    pub fn paused_location(&self) -> Option<(PathBuf, usize, bool)> {
        let session = self.session.as_ref().filter(|s| s.is_paused())?;
        let frame = session.frame()?;
        Some((PathBuf::from(frame.path()?), frame.line.saturating_sub(1) as usize, session.selected_frame == 0))
    }

    pub fn clear_console(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.session.as_mut() {
            session.console.clear();
            cx.notify();
        }
    }
}

fn clear_pause_state(session: &mut Session) {
    session.generation += 1;
    session.frames.clear();
    session.scopes.clear();
    session.children.clear();
    session.watch_values.clear();
    session.stop_description = None;
}

fn panic_breakpoints(on: bool) -> Vec<FunctionBreakpoint> {
    if on {
        vec![FunctionBreakpoint { name: "rust_panic".into(), condition: None }]
    } else {
        Vec::new()
    }
}

/// The frame to show first: on a panic the first one in the workspace's own
/// code, else the first with a source file on disk.
fn pick_frame(frames: &[StackFrame], root: &Path, prefer_workspace: bool) -> usize {
    let exists = |f: &StackFrame| f.path().is_some_and(|p| Path::new(p).exists());
    if prefer_workspace {
        if let Some(ix) = frames.iter().position(|f| f.path().is_some_and(|p| Path::new(p).starts_with(root)) && exists(f)) {
            return ix;
        }
    }
    if frames.first().is_some_and(exists) {
        return 0;
    }
    frames.iter().position(exists).unwrap_or(0)
}

/// Children of the expanded scopes and values, fetched breadth first.
async fn load_tree(client: &DapClient, scopes: &[Scope], expanded: &HashSet<String>) -> HashMap<i64, Vec<Variable>> {
    let mut children = HashMap::new();
    let mut queue: Vec<(String, i64)> = scopes
        .iter()
        .filter(|s| s.variables_reference > 0 && expanded.contains(&s.name))
        .map(|s| (s.name.clone(), s.variables_reference))
        .collect();
    let mut fetched = 0;
    while let Some((path, reference)) = queue.pop() {
        // Deep, wide expansions are cut short rather than stalling a step.
        if fetched > 200 {
            break;
        }
        fetched += 1;
        let vars = client.variables(reference).await.unwrap_or_default();
        for var in &vars {
            let key = format!("{path}/{}", var.name);
            if var.variables_reference > 0 && expanded.contains(&key) {
                queue.push((key, var.variables_reference));
            }
        }
        children.insert(reference, vars);
    }
    children
}

/// Two configurations name the same thing (arguments aside).
pub fn same_config(a: &RunConfig, b: &RunConfig) -> bool {
    a.package == b.package
        && a.target == b.target
        && a.target_kind == b.target_kind
        && a.test == b.test
        && a.test_filter == b.test_filter
}

/// Splits program arguments on spaces, honouring double quotes.
pub fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut has_arg = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                has_arg = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has_arg {
                    args.push(std::mem::take(&mut current));
                    has_arg = false;
                }
            }
            c => {
                current.push(c);
                has_arg = true;
            }
        }
    }
    if has_arg {
        args.push(current);
    }
    args
}

pub fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| if a.is_empty() || a.contains(char::is_whitespace) { format!("\"{a}\"") } else { a.clone() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The adapter's message without LLDB's caret diagram.
fn clean_error(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    let text = text.split_once(": ").filter(|(head, _)| !head.contains(' ')).map(|(_, rest)| rest).unwrap_or(&text);
    text.lines().next().unwrap_or("").trim().to_string()
}

async fn smol_timer(duration: Duration) {
    let (tx, rx) = oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        let _ = tx.send(());
    });
    let _ = rx.await;
}

/// A short status for the title and status bars.
pub fn phase_label(phase: &Phase) -> SharedString {
    match phase {
        Phase::Building => "Building…".into(),
        Phase::Starting => "Starting…".into(),
        Phase::Running => "Running".into(),
        Phase::Paused => "Paused".into(),
        Phase::Ended { exit_code: Some(code) } => format!("Exited ({code})").into(),
        Phase::Ended { exit_code: None } => "Stopped".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_arguments() {
        assert_eq!(split_args(r#"--port 80 "a b" """#), ["--port", "80", "a b", ""]);
        assert_eq!(join_args(&split_args(r#"x "a b""#)), r#"x "a b""#);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn console_joins_partial_lines() {
        let mut session = Session::new(1, RunConfig {
            package: "p".into(),
            target_kind: TargetKind::Bin,
            target: "p".into(),
            test: false,
            test_filter: None,
            exact: false,
            args: vec![],
        });
        session.push_console(ConsoleKind::Stdout, "hel");
        session.push_console(ConsoleKind::Stdout, "lo\nworld\n");
        session.push_console(ConsoleKind::Stderr, "oops");
        session.push_console(ConsoleKind::Stdout, "!\n");
        let lines: Vec<_> = session.console.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(lines, ["hello", "world", "oops", "!"]);
    }

    #[test]
    fn cleans_lldb_errors() {
        let err = anyhow::anyhow!("evaluate: error: <user expression 0>:1:1: use of undeclared identifier 'p'\n    1 | p\n      | ^");
        assert_eq!(clean_error(&err), "error: <user expression 0>:1:1: use of undeclared identifier 'p'");
    }
}

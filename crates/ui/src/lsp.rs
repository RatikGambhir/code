//! Language servers for a workspace.
//!
//! When a file opens, its project is detected (Node, Bun or Deno; Go
//! modules and workspaces; frameworks and linters) and the servers that
//! apply are planned: a primary server, plus linters and add-ons such as
//! ESLint or Tailwind. Each server runs once per project root, started on
//! first use, so opening a Go file starts gopls for its module and nothing
//! for TypeScript. Editors attach to every server for their file;
//! diagnostics are kept per server and merged for display. Each attached
//! editor gets an [`LspDocument`] per server that keeps the server's copy
//! of the file in step with the buffer, and the providers the code editor
//! calls for completion, hover, definitions and code actions.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use cockpit_editor::{
    fuzzy,
    lsp::{
        plan, uri_to_path, Install, LspClient, PositionEncoding, ResolveError, Role, ServerEvent, ServerPlan,
        ServerSpec, ToolDirs,
    },
    project::{FileContext, ProjectTask},
};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, SharedString, Task, WeakEntity, Window};
use gpui_component::input::{
    CodeActionProvider, CompletionProvider, DefinitionProvider, HoverProvider, InputState, RopeExt as _,
};
use gpui_component::Rope;
use lsp_types::{
    CodeAction, CodeActionOrCommand, CompletionContext, CompletionItem, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind,
    GotoDefinitionResponse, LocationLink, OneOf, TextEdit, WorkspaceEdit,
};

use crate::editor_view::EditorView;

/// Completion lists longer than this are cut; the server ranks them first.
const MAX_COMPLETIONS: usize = 100;

pub enum LspEvent {
    /// New diagnostics for a file.
    Diagnostics(PathBuf),
    /// A server started, stopped or reported progress, or a file's project
    /// was detected; the status bar shows it.
    StatusChanged,
    /// The primary server for a file is not installed but can be (said
    /// once per server).
    Missing { server: &'static str, hint: String },
    /// An install finished: what was installed, or why it failed.
    Installed(Result<Vec<String>, String>),
}

/// A file's diagnostics, by the server (its plan key) that reported them.
pub type FileDiagnostics = BTreeMap<String, Vec<lsp_types::Diagnostic>>;

/// A place in a file. Lines and columns are 0-based; columns count characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

struct Server {
    client: Arc<LspClient>,
    /// Work in progress by token, as display text.
    progress: BTreeMap<String, String>,
    /// rust-analyzer is still loading the workspace.
    loading: bool,
    /// rust-analyzer's own report of a problem (a broken Cargo.toml, say).
    problem: Option<String>,
}

enum Slot {
    Starting(Vec<WeakEntity<EditorView>>),
    Ready(Server),
    Failed(String),
    /// A linter or add-on that is not installed: nothing to show.
    Unavailable,
}

/// A server process for one root, in whatever state it is in.
struct Entry {
    spec: &'static ServerSpec,
    slot: Slot,
}

pub struct LspManager {
    root: PathBuf,
    tools: ToolDirs,
    /// By plan key (`gopls@/path/to/module`).
    servers: HashMap<String, Entry>,
    /// Each opened file's detected project, for the status bar and tasks.
    contexts: HashMap<PathBuf, FileContext>,
    /// Servers that were needed but are not installed and can be installed
    /// automatically, by server id.
    missing: BTreeMap<&'static str, Install>,
    /// Servers the user has already been told are missing.
    announced: BTreeSet<&'static str>,
    installing: bool,
    diagnostics: HashMap<PathBuf, FileDiagnostics>,
    /// Errors and warnings across `diagnostics`, kept up to date as they
    /// arrive: the status bar shows them on every redraw.
    counts: (usize, usize),
    last_status: Option<String>,
}

impl EventEmitter<LspEvent> for LspManager {}

/// Why a server did not start.
enum StartFailure {
    Resolve(ResolveError),
    Start(anyhow::Error),
}

impl LspManager {
    pub fn new(root: PathBuf, tools: ToolDirs) -> Self {
        Self {
            root,
            tools,
            servers: HashMap::new(),
            contexts: HashMap::new(),
            missing: BTreeMap::new(),
            announced: BTreeSet::new(),
            installing: false,
            diagnostics: HashMap::new(),
            counts: (0, 0),
            last_status: None,
        }
    }

    /// A file's diagnostics from every server.
    pub fn diagnostics(&self, path: &Path) -> FileDiagnostics {
        self.diagnostics.get(path).cloned().unwrap_or_default()
    }

    /// Counts of (errors, warnings) across the workspace.
    pub fn diagnostic_counts(&self) -> (usize, usize) {
        self.counts
    }

    fn count(diagnostics: &[lsp_types::Diagnostic]) -> (usize, usize) {
        diagnostics.iter().fold((0, 0), |(errors, warnings), diagnostic| match diagnostic.severity {
            Some(lsp_types::DiagnosticSeverity::ERROR) => (errors + 1, warnings),
            Some(lsp_types::DiagnosticSeverity::WARNING) => (errors, warnings + 1),
            _ => (errors, warnings),
        })
    }

    /// The detected project of an open file: "Bun · TypeScript · Next.js".
    pub fn project_label(&self, path: &Path) -> Option<String> {
        self.contexts.get(path).and_then(FileContext::label)
    }

    /// Commands the project of `path` offers, or the workspace's when no
    /// file is given (or its project is not known yet).
    pub fn tasks(&self, path: Option<&Path>) -> Vec<ProjectTask> {
        if let Some(context) = path.and_then(|path| self.contexts.get(path)) {
            let tasks = context.tasks();
            if !tasks.is_empty() {
                return tasks;
            }
        }
        // As if a Go and a TypeScript file sat at the root.
        let go = FileContext::detect(&self.root.join("main.go"), "go", &self.root);
        let js = FileContext::detect(&self.root.join("index.ts"), "typescript", &self.root);
        go.tasks().into_iter().chain(js.tasks()).collect()
    }

    /// Servers that were needed and can be installed automatically.
    pub fn missing_servers(&self) -> Vec<&'static str> {
        self.missing.keys().copied().collect()
    }

    pub fn is_installing(&self) -> bool {
        self.installing
    }

    /// One line for the status bar.
    pub fn status(&self) -> Option<String> {
        let mut parts: BTreeSet<String> = self
            .servers
            .values()
            .filter_map(|entry| {
                let id = entry.spec.id;
                Some(match &entry.slot {
                    Slot::Starting(_) => format!("{id}: starting…"),
                    Slot::Ready(server) => {
                        let name = server.client.name();
                        if let Some(work) = server.progress.values().next_back() {
                            format!("{name}: {work}")
                        } else if server.loading {
                            format!("{name}: loading workspace…")
                        } else if let Some(problem) = &server.problem {
                            format!("{name}: {}", cockpit_common::truncate_chars(problem, 60))
                        } else {
                            name.to_string()
                        }
                    }
                    Slot::Failed(err) => format!("{id}: {}", cockpit_common::truncate_chars(err, 80)),
                    Slot::Unavailable => return None,
                })
            })
            .collect();
        if self.installing {
            parts.insert("installing language servers…".into());
        }
        (!parts.is_empty()).then(|| parts.into_iter().collect::<Vec<_>>().join(", "))
    }

    /// Emits `StatusChanged` only when the status line text changes; servers
    /// report progress many times a second while indexing.
    fn status_changed(&mut self, cx: &mut Context<Self>) {
        let status = self.status();
        if status != self.last_status {
            self.last_status = status;
            cx.emit(LspEvent::StatusChanged);
        }
    }

    /// Attaches `editor`, showing `path`, to every server for the file,
    /// starting those that are not running. The project is detected off
    /// the main thread; the editor is called back for each server once it
    /// is ready.
    pub fn attach(&mut self, path: PathBuf, language: &'static str, editor: WeakEntity<EditorView>, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let detect = cx.background_spawn(async move {
            let context = FileContext::detect(&path, language, &root);
            let plans = plan(&context);
            (context, plans)
        });
        cx.spawn(async move |this, cx| {
            let (context, plans) = detect.await;
            this.update(cx, |this, cx| {
                let label_changed = this.project_label(&context.path) != context.label();
                this.contexts.insert(context.path.clone(), context);
                for plan in plans {
                    this.attach_to(plan, editor.clone(), cx);
                }
                if label_changed {
                    cx.emit(LspEvent::StatusChanged);
                }
            })
            .ok();
        })
        .detach();
    }

    fn attach_to(&mut self, plan: ServerPlan, editor: WeakEntity<EditorView>, cx: &mut Context<Self>) {
        let key = plan.key();
        let role = plan.role();
        match self.servers.get_mut(&key).map(|entry| &mut entry.slot) {
            Some(Slot::Ready(server)) => {
                let client = server.client.clone();
                cx.defer(move |cx| {
                    editor.update(cx, |editor, cx| editor.attach_lsp(client, key, role, cx)).ok();
                });
                return;
            }
            Some(Slot::Starting(waiting)) => {
                waiting.push(editor);
                return;
            }
            Some(Slot::Failed(_) | Slot::Unavailable) => return,
            None => {}
        }

        let spec = plan.spec;
        self.servers.insert(key.clone(), Entry { spec, slot: Slot::Starting(vec![editor]) });
        self.status_changed(cx);

        let tools = self.tools.clone();
        let start = cx.background_spawn(async move {
            // Resolving runs some servers once (`--version`), so it stays
            // off the main thread too.
            let config = plan.resolve(&tools).map_err(StartFailure::Resolve)?;
            LspClient::start(&config, &plan.root).await.map_err(StartFailure::Start)
        });
        cx.spawn(async move |this, cx| {
            let started = start.await;
            this.update(cx, |this, cx| {
                let waiting = match this.servers.get_mut(&key).map(|entry| &mut entry.slot) {
                    Some(Slot::Starting(waiting)) => std::mem::take(waiting),
                    // Restarted while starting: this client is not wanted.
                    _ => return,
                };
                let slot = match started {
                    Ok((client, events)) => {
                        for editor in waiting {
                            let client = client.clone();
                            let key = key.clone();
                            editor.update(cx, |editor, cx| editor.attach_lsp(client, key, role, cx)).ok();
                        }
                        this.listen(key.clone(), client.clone(), events, cx);
                        Slot::Ready(Server { client, progress: BTreeMap::new(), loading: false, problem: None })
                    }
                    Err(StartFailure::Resolve(ResolveError::NotInstalled { install })) => {
                        this.not_installed(spec, install, cx)
                    }
                    Err(StartFailure::Resolve(err)) => {
                        tracing::warn!("{key}: {err}");
                        Slot::Failed(err.to_string())
                    }
                    Err(StartFailure::Start(err)) => {
                        tracing::warn!("language server {key} failed to start: {err:#}");
                        Slot::Failed(format!("{err:#}"))
                    }
                };
                if let Some(entry) = this.servers.get_mut(&key) {
                    entry.slot = slot;
                }
                this.status_changed(cx);
            })
        })
        .detach();
    }

    /// Remembers a missing server for "Install Missing Language Servers"
    /// and decides how loudly to say so: a language's main server is shown
    /// (and announced once), a linter's or a config-file helper's is not.
    fn not_installed(&mut self, spec: &'static ServerSpec, install: Option<Install>, cx: &mut Context<Self>) -> Slot {
        let automatic = install.as_ref().is_some_and(Install::is_automatic);
        if let Some(install) = install.clone().filter(|_| automatic) {
            self.missing.insert(spec.id, install);
        }
        if !spec.is_essential() {
            return Slot::Unavailable;
        }
        let message = match &install {
            Some(_) if automatic => "not installed (run Install Missing Language Servers)".to_string(),
            Some(install) => format!("not installed; {}", install.hint()),
            None => return Slot::Unavailable,
        };
        if self.announced.insert(spec.id) {
            cx.emit(LspEvent::Missing { server: spec.id, hint: message.clone() });
        }
        Slot::Failed(message)
    }

    /// Installs every missing server that can be installed. The caller
    /// restarts the servers once [`LspEvent::Installed`] reports success.
    pub fn install_missing(&mut self, cx: &mut Context<Self>) {
        if self.installing || self.missing.is_empty() {
            return;
        }
        let Some(managed) = self.tools.managed.clone() else {
            cx.emit(LspEvent::Installed(Err("there is nowhere to install servers".into())));
            return;
        };
        self.installing = true;
        self.status_changed(cx);
        let installs: Vec<Install> = self.missing.values().cloned().collect();
        let tools = self.tools.clone();
        let task = cx.background_spawn(async move { managed.install(&installs, |name| tools.find(name)) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.installing = false;
                if result.is_ok() {
                    this.missing.clear();
                    this.announced.clear();
                }
                this.status_changed(cx);
                cx.emit(LspEvent::Installed(result.map_err(|err| format!("{err:#}"))));
            })
            .ok();
        })
        .detach();
    }

    fn listen(
        &mut self,
        key: String,
        client: Arc<LspClient>,
        events: async_channel::Receiver<ServerEvent>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let keep_going = this.update(cx, |this, cx| {
                    // Events from a client that has since been restarted.
                    let server = match this.servers.get_mut(&key).map(|entry| &mut entry.slot) {
                        Some(Slot::Ready(server)) if Arc::ptr_eq(&server.client, &client) => server,
                        _ => return false,
                    };
                    match event {
                        ServerEvent::Diagnostics(params) => {
                            if let Some(path) = uri_to_path(&params.uri) {
                                this.set_diagnostics(path, &key, params.diagnostics, cx);
                            }
                        }
                        ServerEvent::Progress { token, title, message, percentage, done } => {
                            if done {
                                server.progress.remove(&token);
                            } else {
                                let previous = server.progress.get(&token).cloned();
                                let text = progress_text(previous.as_deref(), title, message, percentage);
                                server.progress.insert(token, text);
                            }
                            this.status_changed(cx);
                        }
                        ServerEvent::Status { quiescent, healthy, message } => {
                            server.loading = !quiescent;
                            server.problem = if healthy { None } else { message.or(Some("error".into())) };
                            this.status_changed(cx);
                        }
                        ServerEvent::Message { kind, text } => {
                            if kind == lsp_types::MessageType::ERROR {
                                tracing::warn!("{}: {text}", client.name());
                            }
                        }
                        ServerEvent::Exited => {
                            tracing::warn!("{} exited", client.name());
                            if let Some(entry) = this.servers.get_mut(&key) {
                                entry.slot = Slot::Failed("stopped; run Restart Language Servers".into());
                            }
                            this.clear_server_diagnostics(&key, cx);
                            this.status_changed(cx);
                            return false;
                        }
                    }
                    true
                });
                if !matches!(keep_going, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Replaces what server `key` reports for `path`, keeping the counts.
    fn set_diagnostics(&mut self, path: PathBuf, key: &str, diagnostics: Vec<lsp_types::Diagnostic>, cx: &mut Context<Self>) {
        let added = Self::count(&diagnostics);
        let file = self.diagnostics.entry(path.clone()).or_default();
        let removed = if diagnostics.is_empty() {
            file.remove(key)
        } else {
            file.insert(key.to_string(), diagnostics)
        }
        .map_or((0, 0), |old| Self::count(&old));
        if file.is_empty() {
            self.diagnostics.remove(&path);
        }
        self.counts = (self.counts.0 + added.0 - removed.0, self.counts.1 + added.1 - removed.1);
        cx.emit(LspEvent::Diagnostics(path));
    }

    /// Drops everything a server that went away reported.
    fn clear_server_diagnostics(&mut self, key: &str, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .diagnostics
            .iter()
            .filter(|(_, file)| file.contains_key(key))
            .map(|(path, _)| path.clone())
            .collect();
        for path in paths {
            self.set_diagnostics(path, key, Vec::new(), cx);
        }
    }

    /// Stops every server and forgets failures, so the next `attach` starts
    /// fresh (after installing a server, or when one got stuck).
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.shutdown(cx);
        self.missing.clear();
        let paths: Vec<PathBuf> = self.diagnostics.drain().map(|(path, _)| path).collect();
        self.counts = (0, 0);
        for path in paths {
            cx.emit(LspEvent::Diagnostics(path));
        }
        self.status_changed(cx);
    }

    pub fn shutdown(&mut self, cx: &mut App) {
        for entry in self.servers.drain().map(|(_, entry)| entry) {
            if let Slot::Ready(server) = entry.slot {
                cx.background_spawn(async move {
                    let _ = server.client.shutdown().await;
                })
                .detach();
            }
        }
    }
}

/// "Indexing 42% 12/30 (serde)" from a progress report. Reports after the
/// first usually omit the title, so the previous text's title is kept.
fn progress_text(
    previous: Option<&str>,
    title: Option<String>,
    message: Option<String>,
    percentage: Option<u32>,
) -> String {
    let title = title.or_else(|| {
        previous.map(|p| p.split(" · ").next().unwrap_or(p).to_string())
    });
    let mut parts = Vec::new();
    parts.extend(title);
    if let Some(percentage) = percentage {
        parts.push(format!("{percentage}%"));
    }
    if let Some(message) = message.filter(|m| !m.is_empty()) {
        parts.push(cockpit_common::truncate_chars(&message, 40).to_string());
    }
    // The first separator marks where the title ends, for the next report.
    match parts.split_first() {
        Some((title, rest)) if !rest.is_empty() => format!("{title} · {}", rest.join(" ")),
        Some((title, _)) => title.clone(),
        None => "working…".into(),
    }
}

/// An editor's file as the language server sees it.
///
/// Everything here runs on the main thread, so plain cells are enough.
/// How far the server's lines are from the editor's: a notebook cell is
/// shown to the server inside fixed text (see [`LspDocument::open_cell`]),
/// so its line 0 is the server's line `n`. Columns are the same.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineShift(pub u32);

impl LineShift {
    pub fn to_server(self, position: lsp_types::Position) -> lsp_types::Position {
        lsp_types::Position::new(position.line + self.0, position.character)
    }

    /// `None` for a position in the text around the editor's.
    pub fn from_server(self, position: lsp_types::Position) -> Option<lsp_types::Position> {
        let line = position.line.checked_sub(self.0)?;
        Some(lsp_types::Position::new(line, position.character))
    }

    pub fn range_from_server(self, range: lsp_types::Range) -> Option<lsp_types::Range> {
        Some(lsp_types::Range::new(self.from_server(range.start)?, self.from_server(range.end)?))
    }

    /// Completion edits in editor lines. An import the server would add
    /// above the cell goes at the cell's top instead; other edits outside
    /// it are dropped.
    fn completions(self, response: CompletionResponse) -> CompletionResponse {
        if self.0 == 0 {
            return response;
        }
        let map = |items: Vec<CompletionItem>| -> Vec<CompletionItem> {
            items
                .into_iter()
                .map(|mut item| {
                    item.text_edit = item.text_edit.and_then(|edit| match edit {
                        CompletionTextEdit::Edit(mut edit) => {
                            edit.range = self.range_from_server(edit.range)?;
                            Some(CompletionTextEdit::Edit(edit))
                        }
                        CompletionTextEdit::InsertAndReplace(mut edit) => {
                            edit.insert = self.range_from_server(edit.insert)?;
                            edit.replace = self.range_from_server(edit.replace)?;
                            Some(CompletionTextEdit::InsertAndReplace(edit))
                        }
                    });
                    item.additional_text_edits = item.additional_text_edits.map(|edits| {
                        edits
                            .into_iter()
                            .filter_map(|mut edit| match self.range_from_server(edit.range) {
                                Some(range) => {
                                    edit.range = range;
                                    Some(edit)
                                }
                                None if edit.range.start == edit.range.end => {
                                    edit.range = lsp_types::Range::default();
                                    Some(edit)
                                }
                                None => None,
                            })
                            .collect()
                    });
                    item
                })
                .collect()
        };
        match response {
            CompletionResponse::Array(items) => CompletionResponse::Array(map(items)),
            CompletionResponse::List(mut list) => {
                list.items = map(list.items);
                CompletionResponse::List(list)
            }
        }
    }
}

pub struct LspDocument {
    pub client: Arc<LspClient>,
    /// The server's plan key, which its diagnostics are filed under.
    pub key: String,
    pub path: PathBuf,
    /// Text the server sees before and after the editor's (notebook cells).
    surround: Option<(String, String)>,
    pub shift: LineShift,
    version: Cell<i32>,
    /// The text the server has; compared before each request so the server
    /// never answers for stale text (completion is asked for before the
    /// editor reports the keystroke that triggered it).
    synced: RefCell<Rope>,
    /// This server's diagnostics for the file, sent back with code action
    /// requests so it can offer its fixes.
    diagnostics: RefCell<Vec<lsp_types::Diagnostic>>,
    /// Definitions under the mouse while ⌘ is held, for ⌘-click.
    hovered_definitions: Arc<Mutex<Vec<LocationLink>>>,
}

impl LspDocument {
    /// Opens `path`, whose LSP language id is `language`, on `client`.
    pub fn open(client: Arc<LspClient>, key: String, path: PathBuf, language: &str, text: &Rope) -> Result<Rc<Self>> {
        client.did_open(&path, language, 1, &text.to_string())?;
        Ok(Rc::new(Self {
            client,
            key,
            path,
            surround: None,
            shift: LineShift::default(),
            version: Cell::new(1),
            synced: RefCell::new(text.clone()),
            diagnostics: RefCell::default(),
            hovered_definitions: Arc::default(),
        }))
    }

    /// A notebook cell, shown to the server as `prefix + text + suffix` in
    /// `path` (a file the editor does not have open).
    pub fn open_cell(
        client: Arc<LspClient>,
        path: PathBuf,
        language: &str,
        text: &Rope,
        prefix: String,
        suffix: String,
    ) -> Result<Rc<Self>> {
        let shift = LineShift(prefix.matches('\n').count() as u32);
        client.did_open(&path, cockpit_editor::lsp_language_id(language), 1, &format!("{prefix}{text}{suffix}"))?;
        Ok(Rc::new(Self {
            client,
            key: String::new(),
            path,
            surround: Some((prefix, suffix)),
            shift,
            version: Cell::new(1),
            synced: RefCell::new(text.clone()),
            diagnostics: RefCell::default(),
            hovered_definitions: Arc::default(),
        }))
    }

    /// The whole text the server has for `text`.
    fn server_text(&self, text: &Rope) -> String {
        match &self.surround {
            Some((prefix, suffix)) => format!("{prefix}{text}{suffix}"),
            None => text.to_string(),
        }
    }

    /// Sends the text to the server if it changed since the last sync.
    ///
    /// Called on every keystroke, so servers that take edits get just the
    /// one that happened (a few bytes) instead of the whole file, which
    /// would be serialized, piped and re-read by the server each time.
    pub fn sync(&self, text: &Rope) {
        let edit = {
            let synced = self.synced.borrow();
            match rope_diff(&synced, text) {
                None => return,
                Some((old_range, new_range)) => self.incremental().then(|| {
                    let encoding = self.client.position_encoding();
                    let range = lsp_types::Range::new(
                        self.shift.to_server(lsp_position(&synced, old_range.start, encoding)),
                        self.shift.to_server(lsp_position(&synced, old_range.end, encoding)),
                    );
                    (range, text.slice(new_range).to_string())
                }),
            }
        };
        let version = self.version.get() + 1;
        self.version.set(version);
        let sent = match edit {
            Some((range, new_text)) => self.client.did_change_range(&self.path, version, range, &new_text),
            None => self.client.did_change(&self.path, version, &self.server_text(text)),
        };
        if let Err(err) = sent {
            tracing::warn!("didChange {}: {err:#}", self.path.display());
        }
        *self.synced.borrow_mut() = text.clone();
    }

    /// Edits can be sent as ranges: the server takes them, and a cell's
    /// surrounding text starts it on a line of its own (columns line up).
    fn incremental(&self) -> bool {
        self.client.incremental_sync()
            && self
                .surround
                .as_ref()
                .is_none_or(|(prefix, _)| prefix.is_empty() || prefix.ends_with('\n'))
    }

    /// What this server reports for the file.
    pub fn set_diagnostics(&self, diagnostics: Vec<lsp_types::Diagnostic>) {
        *self.diagnostics.borrow_mut() = diagnostics;
    }

    pub fn saved(&self, text: &str) {
        let _ = self.client.did_save(&self.path, text);
    }

    pub fn close(&self) {
        let _ = self.client.did_close(&self.path);
    }

    /// The definitions found under the mouse while ⌘ was held, if any.
    pub fn hovered_definitions(&self) -> Vec<LocationLink> {
        self.hovered_definitions.lock().map(|links| links.clone()).unwrap_or_default()
    }

    pub fn clear_hovered_definitions(&self) {
        if let Ok(mut links) = self.hovered_definitions.lock() {
            links.clear();
        }
    }

    /// Definitions of the symbol at `position`.
    pub fn definitions(&self, position: lsp_types::Position) -> impl std::future::Future<Output = Result<Vec<Target>>> {
        let request = self.client.definition(&self.path, position);
        async move { Ok(definition_targets(request.await?)) }
    }

    /// References to the symbol at `position`, declaration included.
    pub fn references(&self, position: lsp_types::Position) -> impl std::future::Future<Output = Result<Vec<Target>>> {
        let request = self.client.references(&self.path, position);
        async move {
            Ok(request
                .await?
                .unwrap_or_default()
                .into_iter()
                .filter_map(|location| {
                    Some(Target {
                        path: uri_to_path(&location.uri)?,
                        line: location.range.start.line,
                        column: location.range.start.character,
                    })
                })
                .collect())
        }
    }
}

fn definition_links(response: Option<GotoDefinitionResponse>) -> Vec<LocationLink> {
    let location_link = |location: lsp_types::Location| LocationLink {
        origin_selection_range: None,
        target_uri: location.uri,
        target_range: location.range,
        target_selection_range: location.range,
    };
    match response {
        None => Vec::new(),
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location_link(location)],
        Some(GotoDefinitionResponse::Array(locations)) => locations.into_iter().map(location_link).collect(),
        Some(GotoDefinitionResponse::Link(links)) => links,
    }
}

pub fn link_targets(links: &[LocationLink]) -> Vec<Target> {
    links
        .iter()
        .filter_map(|link| {
            Some(Target {
                path: uri_to_path(&link.target_uri)?,
                // The selection range is the name; the target range can start
                // at doc comments and attributes.
                line: link.target_selection_range.start.line,
                column: link.target_selection_range.start.character,
            })
        })
        .collect()
}

fn definition_targets(response: Option<GotoDefinitionResponse>) -> Vec<Target> {
    link_targets(&definition_links(response))
}

/// The one contiguous edit that turns `old` into `new`: the byte range it
/// replaced in `old` and the range of the replacement in `new`. `None` when
/// they are the same. Both ranges fall on character boundaries and never
/// split a `\r\n`.
fn rope_diff(old: &Rope, new: &Rope) -> Option<(Range<usize>, Range<usize>)> {
    let mut prefix = common_len(old.chunks(), new.chunks());
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    fn reversed(rope: &Rope) -> impl Iterator<Item = &str> {
        rope.chunks_at(rope.len()).0.reversed()
    }
    let mut suffix = common_len_reversed(reversed(old), reversed(new)).min(max_suffix);

    let splits = |rope: &Rope, at: usize| {
        !rope.is_char_boundary(at) || (at > 0 && at < rope.len() && rope.byte(at - 1) == b'\r' && rope.byte(at) == b'\n')
    };
    while prefix > 0 && (splits(old, prefix) || splits(new, prefix)) {
        prefix -= 1;
    }
    while suffix > 0 && (splits(old, old.len() - suffix) || splits(new, new.len() - suffix)) {
        suffix -= 1;
    }
    Some((prefix..old.len() - suffix, prefix..new.len() - suffix))
}

/// How many bytes two chunk streams share at their start.
fn common_len<'a>(mut a: impl Iterator<Item = &'a str>, mut b: impl Iterator<Item = &'a str>) -> usize {
    let (mut left, mut right): (&[u8], &[u8]) = (&[], &[]);
    let mut total = 0;
    loop {
        if left.is_empty() {
            match a.next() {
                Some(chunk) => left = chunk.as_bytes(),
                None => return total,
            }
            continue;
        }
        if right.is_empty() {
            match b.next() {
                Some(chunk) => right = chunk.as_bytes(),
                None => return total,
            }
            continue;
        }
        let len = left.len().min(right.len());
        if left[..len] != right[..len] {
            return total + left.iter().zip(right).take_while(|(x, y)| x == y).count();
        }
        total += len;
        left = &left[len..];
        right = &right[len..];
    }
}

/// How many bytes two chunk streams share at their end; the streams yield
/// chunks back to front.
fn common_len_reversed<'a>(mut a: impl Iterator<Item = &'a str>, mut b: impl Iterator<Item = &'a str>) -> usize {
    let (mut left, mut right): (&[u8], &[u8]) = (&[], &[]);
    let mut total = 0;
    loop {
        if left.is_empty() {
            match a.next() {
                Some(chunk) => left = chunk.as_bytes(),
                None => return total,
            }
            continue;
        }
        if right.is_empty() {
            match b.next() {
                Some(chunk) => right = chunk.as_bytes(),
                None => return total,
            }
            continue;
        }
        let len = left.len().min(right.len());
        let (l, r) = (&left[left.len() - len..], &right[right.len() - len..]);
        if l != r {
            return total + l.iter().rev().zip(r.iter().rev()).take_while(|(x, y)| x == y).count();
        }
        total += len;
        left = &left[..left.len() - len];
        right = &right[..right.len() - len];
    }
}

/// The LSP position of byte `offset` in `text`, its column counted the way
/// the server asked.
fn lsp_position(text: &Rope, offset: usize, encoding: PositionEncoding) -> lsp_types::Position {
    // Offsets from `rope_diff` are character boundaries already.
    let point = text.offset_to_point(offset);
    let (line, start) = (point.row, offset - point.column);
    let column = match encoding {
        PositionEncoding::Utf8 => offset - start,
        PositionEncoding::Utf16 => text.byte_to_utf16_idx(offset) - text.byte_to_utf16_idx(start),
        PositionEncoding::Utf32 => text.slice(start..offset).chars().count(),
    };
    lsp_types::Position::new(line as u32, column as u32)
}

/// Completion through the language server.
pub struct LspCompletion {
    doc: Rc<LspDocument>,
    triggers: Vec<char>,
}

impl LspCompletion {
    /// `None` when the server does not complete.
    fn new(doc: &Rc<LspDocument>) -> Option<Self> {
        let provider = doc.client.capabilities()?.completion_provider.as_ref()?;
        let triggers = provider
            .trigger_characters
            .iter()
            .flatten()
            .filter_map(|t| t.chars().last())
            .collect();
        Some(Self { doc: doc.clone(), triggers })
    }
}

impl CompletionProvider for LspCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionResponse>> {
        self.doc.sync(text);
        let position = text.offset_to_position(offset);
        // The editor passes the typed word as the "trigger character";
        // servers act on the real one (rust-analyzer only completes after
        // `::`, not `:`), so work it out here.
        let line_start = text.line_start_offset(position.line as usize);
        let before_cursor = text.slice(line_start..offset).to_string();
        let previous = before_cursor.chars().next_back();
        let word = typed_word(&before_cursor).to_string();
        let after_trigger = previous.is_some_and(|c| self.triggers.contains(&c));
        if word.is_empty() && !after_trigger {
            // `<`, a space, `;`…: nothing to complete. An empty answer makes
            // the editor close the menu instead of leaving stale items up.
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        }
        let context = match previous.filter(|c| self.triggers.contains(c)) {
            Some(c) => CompletionContext {
                trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(c.to_string()),
            },
            None => CompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            },
        };
        let shift = self.doc.shift;
        let request = self.doc.client.completion(&self.doc.path, shift.to_server(position), Some(context));
        cx.background_spawn(async move {
            let response = request.await?.unwrap_or(CompletionResponse::Array(Vec::new()));
            // Unranked: the menu narrows and orders it (`filter_completions`)
            // for whatever has been typed by the time it is shown.
            Ok(shift.completions(response))
        })
    }

    fn filter_completions(&self, items: &[&CompletionItem], word: &str) -> Vec<usize> {
        rank_completions(items, word)
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut Context<InputState>) -> bool {
        // Every single typed character, so the menu follows (or closes on)
        // each keystroke; `completions` decides whether there is anything to
        // ask. Loading or reloading a file also reports its whole text as
        // inserted, and pastes should not pop up a menu either.
        let mut chars = new_text.chars();
        matches!((chars.next(), chars.next()), (Some(_), None))
    }
}

/// The identifier being typed at the end of `before_cursor`.
fn typed_word(before_cursor: &str) -> &str {
    let start = before_cursor
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(before_cursor.len(), |(ix, _)| ix);
    &before_cursor[start..]
}

/// The editor shows completions as given, without filtering or sorting, and
/// servers such as rust-analyzer send everything in scope expecting the
/// client to narrow it down. So: keep the items whose filter text fuzzily
/// matches the typed `word`, put those that start with it first, then follow
/// the server's relevance (`sortText`, preselected first), and keep the best.
/// Returns indices into `items`. Runs on every keystroke over everything the
/// server sent (thousands of items), so it allocates nothing per item.
fn rank_completions(items: &[&CompletionItem], word: &str) -> Vec<usize> {
    let mut ranked: Vec<_> = items
        .iter()
        .enumerate()
        .filter_map(|(ix, item)| {
            let filter = item.filter_text.as_deref().unwrap_or(&item.label);
            let score = if word.is_empty() { 0 } else { fuzzy::score(word, filter)? };
            // 0: is the word, 1: starts with it as typed, 2: ignoring case,
            // 3: fuzzy.
            let tier = if !word.is_empty() && filter == word {
                0
            } else if filter.starts_with(word) {
                1
            } else if starts_with_ignoring_case(filter, word) {
                2
            } else {
                3
            };
            let key = (
                tier,
                !item.preselect.unwrap_or(false),
                item.sort_text.as_deref().unwrap_or(&item.label),
                std::cmp::Reverse(score),
                ix,
            );
            Some(key)
        })
        .collect();
    if ranked.len() > MAX_COMPLETIONS {
        ranked.select_nth_unstable(MAX_COMPLETIONS - 1);
        ranked.truncate(MAX_COMPLETIONS);
    }
    ranked.sort_unstable();
    ranked.into_iter().map(|key| key.4).collect()
}

fn starts_with_ignoring_case(text: &str, prefix: &str) -> bool {
    if text.is_ascii() && prefix.is_ascii() {
        return text.len() >= prefix.len() && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes());
    }
    let mut text = text.chars().flat_map(char::to_lowercase);
    prefix.chars().flat_map(char::to_lowercase).all(|c| text.next() == Some(c))
}

/// Hover documentation through the language server.
pub struct LspHover {
    doc: Rc<LspDocument>,
}

impl HoverProvider for LspHover {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>> {
        self.doc.sync(text);
        let shift = self.doc.shift;
        let position = shift.to_server(text.offset_to_position(offset));
        let request = self.doc.client.hover(&self.doc.path, position);
        cx.background_spawn(async move {
            let hover = request.await?;
            Ok(hover.map(|mut hover| {
                hover.range = hover.range.and_then(|range| shift.range_from_server(range));
                hover
            }))
        })
    }
}

/// Definitions under the mouse while ⌘ is held: the editor underlines the
/// symbol, and the result is kept for [`EditorView`] to jump on ⌘-click,
/// since the editor itself can only move within its own buffer.
pub struct LspDefinitions {
    doc: Rc<LspDocument>,
}

impl DefinitionProvider for LspDefinitions {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<LocationLink>>> {
        self.doc.sync(text);
        self.doc.clear_hovered_definitions();
        let position = text.offset_to_position(offset);
        let request = self.doc.client.definition(&self.doc.path, position);
        let hovered = self.doc.hovered_definitions.clone();
        cx.background_spawn(async move {
            let links = definition_links(request.await?);
            if let Ok(mut hovered) = hovered.lock() {
                *hovered = links.clone();
            }
            Ok(links)
        })
    }
}

/// Quick fixes and refactors (⌘.), applied to this file.
pub struct LspCodeActions {
    doc: Rc<LspDocument>,
}

impl CodeActionProvider for LspCodeActions {
    fn id(&self) -> SharedString {
        // One provider per server; actions are routed back by this id.
        format!("lsp:{}", self.doc.key).into()
    }

    fn code_actions(
        &self,
        state: Entity<InputState>,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        let text = state.read(cx).text().clone();
        self.doc.sync(&text);
        let range = lsp_types::Range {
            start: text.offset_to_position(range.start),
            end: text.offset_to_position(range.end),
        };
        let diagnostics: Vec<_> = self
            .doc
            .diagnostics
            .borrow()
            .iter()
            .filter(|d| d.range.start.line <= range.end.line && d.range.end.line >= range.start.line)
            .cloned()
            .collect();
        let request = self.doc.client.code_actions(&self.doc.path, range, diagnostics);
        cx.background_spawn(async move {
            Ok(request
                .await?
                .unwrap_or_default()
                .into_iter()
                .filter_map(|action| match action {
                    // Bare commands need server-side execution, which the
                    // client does not offer.
                    CodeActionOrCommand::Command(_) => None,
                    CodeActionOrCommand::CodeAction(action) => {
                        (action.disabled.is_none() && action.edit.is_some()).then_some(action)
                    }
                })
                .collect())
        })
    }

    fn perform_code_action(
        &self,
        state: Entity<InputState>,
        action: CodeAction,
        _push_to_history: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let Some(edit) = action.edit.as_ref() else {
            return Task::ready(Ok(()));
        };
        let (edits, other_files) = edits_for_path(edit, &self.doc.path);
        if other_files > 0 {
            tracing::warn!(
                "code action \"{}\" also edits {other_files} other file(s); only this file was changed",
                action.title
            );
        }
        let edits = order_edits(edits);
        state.update(cx, |state, cx| state.apply_lsp_edits(&edits, window, cx));
        Task::ready(Ok(()))
    }
}

/// The edits a workspace edit makes to `path`, and how many other files it
/// touches.
fn edits_for_path(edit: &WorkspaceEdit, path: &Path) -> (Vec<TextEdit>, usize) {
    let mut edits = Vec::new();
    let mut others = std::collections::HashSet::new();
    let mut add = |uri: &lsp_types::Uri, file_edits: Vec<TextEdit>| match uri_to_path(uri) {
        Some(p) if p == path => edits.extend(file_edits),
        _ => {
            others.insert(uri.as_str().to_string());
        }
    };
    if let Some(changes) = &edit.changes {
        for (uri, file_edits) in changes {
            add(uri, file_edits.clone());
        }
    }
    let document_edits: Vec<&lsp_types::TextDocumentEdit> = match &edit.document_changes {
        None => Vec::new(),
        Some(lsp_types::DocumentChanges::Edits(edits)) => edits.iter().collect(),
        Some(lsp_types::DocumentChanges::Operations(ops)) => ops
            .iter()
            .filter_map(|op| match op {
                lsp_types::DocumentChangeOperation::Edit(edit) => Some(edit),
                lsp_types::DocumentChangeOperation::Op(_) => None,
            })
            .collect(),
    };
    for document_edit in document_edits {
        let file_edits = document_edit
            .edits
            .iter()
            .map(|edit| match edit {
                OneOf::Left(edit) => edit.clone(),
                OneOf::Right(annotated) => annotated.text_edit.clone(),
            })
            .collect();
        add(&document_edit.text_document.uri, file_edits);
    }
    (edits, others.len())
}

/// LSP edits all refer to the original text, but the editor applies them one
/// after another; going from the end of the file backwards keeps the earlier
/// positions valid. Edits at the same spot keep their relative order.
fn order_edits(edits: Vec<TextEdit>) -> Vec<TextEdit> {
    let mut indexed: Vec<(usize, TextEdit)> = edits.into_iter().enumerate().collect();
    indexed.sort_by(|(i, a), (j, b)| {
        let start = |e: &TextEdit| (e.range.start.line, e.range.start.character);
        start(b).cmp(&start(a)).then(j.cmp(i))
    });
    indexed.into_iter().map(|(_, edit)| edit).collect()
}

/// Attaches every provider the server supports to `input`.
pub fn install_providers(doc: &Rc<LspDocument>, input: &mut InputState) {
    install_all(Some(doc), &[], input);
}

/// Attaches providers for every server of a file: completion, hover and
/// definitions from the primary server, with add-ons' completions and
/// hovers merged in, and each server's code actions.
pub fn install_all(primary: Option<&Rc<LspDocument>>, others: &[(Role, Rc<LspDocument>)], input: &mut InputState) {
    remove_providers(input);
    let addons = others.iter().filter(|(role, _)| *role == Role::Addon).map(|(_, doc)| doc);
    let completing: Vec<&Rc<LspDocument>> = primary.into_iter().chain(addons.clone()).collect();

    let completions: Vec<LspCompletion> = completing.iter().filter_map(|doc| LspCompletion::new(doc)).collect();
    input.lsp.completion_provider = match completions.len() {
        0 => None,
        1 => completions.into_iter().next().map(|c| Rc::new(c) as Rc<dyn CompletionProvider>),
        _ => Some(Rc::new(MergedCompletion(completions))),
    };

    let hovers: Vec<LspHover> = completing
        .iter()
        .filter(|doc| supports_hover(doc))
        .map(|doc| LspHover { doc: Rc::clone(doc) })
        .collect();
    input.lsp.hover_provider = match hovers.len() {
        0 => None,
        1 => hovers.into_iter().next().map(|h| Rc::new(h) as Rc<dyn HoverProvider>),
        _ => Some(Rc::new(FirstHover(hovers))),
    };

    input.lsp.definition_provider = primary
        .filter(|doc| supported(doc.client.capabilities().and_then(|c| c.definition_provider.as_ref())))
        .map(|doc| Rc::new(LspDefinitions { doc: doc.clone() }) as Rc<dyn DefinitionProvider>);

    input.lsp.code_action_providers = primary
        .into_iter()
        .chain(others.iter().map(|(_, doc)| doc))
        .filter(|doc| supports_code_actions(doc))
        .map(|doc| Rc::new(LspCodeActions { doc: doc.clone() }) as Rc<dyn CodeActionProvider>)
        .collect();
}

fn supported<T>(provider: Option<&OneOf<bool, T>>) -> bool {
    match provider {
        Some(OneOf::Left(enabled)) => *enabled,
        Some(OneOf::Right(_)) => true,
        None => false,
    }
}

fn supports_hover(doc: &LspDocument) -> bool {
    doc.client.capabilities().and_then(|c| c.hover_provider.as_ref()).is_some_and(|hover| match hover {
        lsp_types::HoverProviderCapability::Simple(enabled) => *enabled,
        lsp_types::HoverProviderCapability::Options(_) => true,
    })
}

fn supports_code_actions(doc: &LspDocument) -> bool {
    doc.client.capabilities().and_then(|c| c.code_action_provider.as_ref()).is_some_and(|actions| match actions {
        lsp_types::CodeActionProviderCapability::Simple(enabled) => *enabled,
        lsp_types::CodeActionProviderCapability::Options(_) => true,
    })
}

/// Completions from several servers in one menu: the primary server's and
/// add-ons' such as Tailwind's class names. A server that fails or is
/// slow to start just adds nothing.
struct MergedCompletion(Vec<LspCompletion>);

impl CompletionProvider for MergedCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionResponse>> {
        let requests: Vec<_> = self
            .0
            .iter()
            .map(|provider| provider.completions(text, offset, trigger.clone(), window, cx))
            .collect();
        cx.background_spawn(async move {
            let mut items = Vec::new();
            let mut incomplete = false;
            for response in futures::future::join_all(requests).await {
                match response {
                    Ok(CompletionResponse::Array(more)) => items.extend(more),
                    Ok(CompletionResponse::List(list)) => {
                        incomplete |= list.is_incomplete;
                        items.extend(list.items);
                    }
                    Err(err) => tracing::debug!("completion: {err:#}"),
                }
            }
            Ok(CompletionResponse::List(lsp_types::CompletionList { is_incomplete: incomplete, items }))
        })
    }

    fn filter_completions(&self, items: &[&CompletionItem], word: &str) -> Vec<usize> {
        rank_completions(items, word)
    }

    fn is_completion_trigger(&self, offset: usize, new_text: &str, cx: &mut Context<InputState>) -> bool {
        self.0.iter().any(|provider| provider.is_completion_trigger(offset, new_text, cx))
    }
}

/// Hover from the first server with something to say, in order: the
/// primary's, then add-ons' (Tailwind's CSS for a class name).
struct FirstHover(Vec<LspHover>);

impl HoverProvider for FirstHover {
    fn hover(&self, text: &Rope, offset: usize, window: &mut Window, cx: &mut App) -> Task<Result<Option<lsp_types::Hover>>> {
        let requests: Vec<_> = self.0.iter().map(|provider| provider.hover(text, offset, window, cx)).collect();
        cx.background_spawn(async move {
            for request in requests {
                if let Ok(Some(hover)) = request.await {
                    return Ok(Some(hover));
                }
            }
            Ok(None)
        })
    }
}

/// Completion and hover for a notebook cell. Go to definition and code
/// actions are left out: they would act on the server's text around the
/// cell.
pub fn install_cell_providers(doc: &Rc<LspDocument>, input: &mut InputState) {
    install_providers(doc, input);
    input.lsp.definition_provider = None;
    input.lsp.code_action_providers.clear();
}

pub fn remove_providers(input: &mut InputState) {
    input.lsp.completion_provider = None;
    input.lsp.hover_provider = None;
    input.lsp.definition_provider = None;
    input.lsp.code_action_providers.clear();
}

/// Whether the server for `doc` answers references requests.
pub fn supports_references(doc: &LspDocument) -> bool {
    match doc.client.capabilities().and_then(|c| c.references_provider.as_ref()) {
        Some(OneOf::Left(enabled)) => *enabled,
        Some(OneOf::Right(_)) => true,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{CompletionItem, Position, Range as LspRange};

    fn edit(line: u32, character: u32, text: &str) -> TextEdit {
        TextEdit {
            range: LspRange::new(Position::new(line, character), Position::new(line, character)),
            new_text: text.into(),
        }
    }

    /// Applies the edit `rope_diff` reports and checks it rebuilds `new`.
    fn diff(old: &str, new: &str) -> Option<(std::ops::Range<usize>, String)> {
        let (old_rope, new_rope) = (Rope::from(old), Rope::from(new));
        let (old_range, new_range) = rope_diff(&old_rope, &new_rope)?;
        let replacement = new[new_range].to_string();
        let mut rebuilt = old.to_string();
        rebuilt.replace_range(old_range.clone(), &replacement);
        assert_eq!(rebuilt, new);
        Some((old_range, replacement))
    }

    #[test]
    fn rope_diff_finds_the_single_edit() {
        assert_eq!(diff("fn main() {}", "fn main() {}"), None);
        assert_eq!(diff("let x = 1;", "let xy = 1;"), Some((5..5, "y".into())));
        assert_eq!(diff("let xy = 1;", "let x = 1;"), Some((5..6, String::new())));
        assert_eq!(diff("", "a"), Some((0..0, "a".into())));
        assert_eq!(diff("a", ""), Some((0..1, String::new())));
        assert_eq!(diff("abc", "xabc"), Some((0..0, "x".into())));
        assert_eq!(diff("abc", "abcx"), Some((3..3, "x".into())));
        // Repeated text: the edit is placed somewhere that rebuilds `new`.
        assert!(diff("aaaa", "aaaaa").is_some());
        // Never inside a character…
        assert_eq!(diff("é", "è"), Some((0..2, "è".into())));
        assert_eq!(diff("aéb", "aèb"), Some((1..3, "è".into())));
        // …nor between \r and \n.
        let (range, _) = diff("a\r\nb", "a\rx\nb").unwrap();
        assert!(range.start <= 1);
    }

    #[test]
    fn rope_diff_handles_text_longer_than_a_chunk() {
        let old: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let mut new = old.clone();
        new.insert_str(20_000, "inserted");
        assert_eq!(diff(&old, &new), Some((20_000..20_000, "inserted".into())));
        // Repeated lines make where a deletion sits ambiguous; `diff` checks
        // it rebuilds the text, so check the edit is the smallest one.
        let mut shorter = old.clone();
        shorter.replace_range(30_000..30_010, "");
        let (range, replacement) = diff(&old, &shorter).unwrap();
        assert_eq!((range.len(), replacement.as_str()), (10, ""));
    }

    #[test]
    fn lsp_positions_count_columns_as_the_server_asked() {
        // "😀" is 4 bytes, 2 UTF-16 units and 1 character.
        let text = Rope::from("ab\n😀x\n");
        let offset = "ab\n😀".len();
        assert_eq!(lsp_position(&text, offset, PositionEncoding::Utf8), Position::new(1, 4));
        assert_eq!(lsp_position(&text, offset, PositionEncoding::Utf16), Position::new(1, 2));
        assert_eq!(lsp_position(&text, offset, PositionEncoding::Utf32), Position::new(1, 1));
        assert_eq!(lsp_position(&text, 0, PositionEncoding::Utf16), Position::new(0, 0));
        assert_eq!(lsp_position(&text, text.len(), PositionEncoding::Utf16), Position::new(2, 0));
        let crlf = Rope::from("ab\r\ncd");
        assert_eq!(lsp_position(&crlf, 5, PositionEncoding::Utf16), Position::new(1, 1));
    }

    #[test]
    fn edits_apply_back_to_front_keeping_ties_in_order() {
        let ordered = order_edits(vec![edit(0, 0, "a"), edit(3, 1, "b"), edit(0, 0, "c"), edit(1, 0, "d")]);
        let texts: Vec<_> = ordered.iter().map(|e| e.new_text.as_str()).collect();
        // Applying "c" then "a" at the same spot leaves "ac", the order given.
        assert_eq!(texts, ["b", "d", "c", "a"]);
    }

    #[test]
    fn workspace_edits_are_split_by_file() {
        let here = PathBuf::from("/tmp/project/src/main.rs");
        let there = PathBuf::from("/tmp/project/src/lib.rs");
        let uri = |p: &Path| cockpit_editor::lsp::path_to_uri(p).unwrap();
        let workspace_edit = WorkspaceEdit {
            changes: Some(HashMap::from([
                (uri(&here), vec![edit(0, 0, "use std::fmt;\n")]),
                (uri(&there), vec![edit(2, 0, "x")]),
            ])),
            document_changes: Some(lsp_types::DocumentChanges::Edits(vec![lsp_types::TextDocumentEdit {
                text_document: lsp_types::OptionalVersionedTextDocumentIdentifier { uri: uri(&here), version: None },
                edits: vec![OneOf::Left(edit(4, 2, "y"))],
            }])),
            change_annotations: None,
        };
        let (edits, others) = edits_for_path(&workspace_edit, &here);
        assert_eq!(edits.len(), 2);
        assert_eq!(others, 1);
    }

    #[test]
    fn cell_positions_shift_by_the_prefix() {
        use lsp_types::{Position, Range};
        let shift = LineShift(4);
        assert_eq!(shift.to_server(Position::new(0, 3)), Position::new(4, 3));
        assert_eq!(shift.from_server(Position::new(5, 2)), Some(Position::new(1, 2)));
        assert_eq!(shift.from_server(Position::new(3, 0)), None);

        let item = |range: Range, import: Range| CompletionItem {
            label: "HashMap".into(),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range, "HashMap".into()))),
            additional_text_edits: Some(vec![TextEdit::new(import, "use std::collections::HashMap;\n".into())]),
            ..Default::default()
        };
        let at = |line, col| Position::new(line, col);
        let response = CompletionResponse::Array(vec![
            item(Range::new(at(6, 4), at(6, 8)), Range::new(at(1, 0), at(1, 0))),
            // An edit that would change the hidden text is dropped.
            item(Range::new(at(1, 0), at(1, 4)), Range::new(at(1, 0), at(2, 0))),
        ]);
        let CompletionResponse::Array(items) = shift.completions(response) else { panic!() };
        let Some(CompletionTextEdit::Edit(edit)) = &items[0].text_edit else { panic!() };
        assert_eq!(edit.range, Range::new(at(2, 4), at(2, 8)));
        // The import goes at the top of the cell.
        assert_eq!(items[0].additional_text_edits.as_ref().unwrap()[0].range, Range::default());
        assert!(items[1].text_edit.is_none());
        assert!(items[1].additional_text_edits.as_ref().unwrap().is_empty());
    }

    #[test]
    fn completions_follow_sort_text_and_are_capped() {
        let item = |label: &str, sort: Option<&str>, preselect: bool| CompletionItem {
            label: label.into(),
            sort_text: sort.map(Into::into),
            preselect: preselect.then_some(true),
            ..Default::default()
        };
        let mut items = vec![item("zeta", Some("0001"), false), item("alpha", Some("0002"), false), item("pick", Some("9"), true)];
        items.extend((0..200).map(|i| item(&format!("x{i}"), Some("5"), false)));
        let refs: Vec<&CompletionItem> = items.iter().collect();
        let ranked = rank_completions(&refs, "");
        let labels: Vec<_> = ranked.iter().take(3).map(|&ix| items[ix].label.as_str()).collect();
        assert_eq!(labels, ["pick", "zeta", "alpha"]);
        assert_eq!(ranked.len(), MAX_COMPLETIONS);
    }

    #[test]
    fn completions_are_filtered_by_the_typed_word() {
        let item = |label: &str, sort: &str| CompletionItem {
            label: label.into(),
            sort_text: Some(sort.into()),
            ..Default::default()
        };
        // What rust-analyzer sends after `Vec`: everything in scope.
        let items = vec![
            item("None", "1"),
            item("Err", "1"),
            item("VecDeque", "3"),
            item("vec!", "2"),
            item("Vec", "4"),
            item("ValueError", "1"),
            item("VerticalCenter", "1"),
            item("main", "1"),
        ];
        let refs: Vec<&CompletionItem> = items.iter().collect();
        let labels: Vec<_> = rank_completions(&refs, "Vec").into_iter().map(|ix| items[ix].label.as_str()).collect();
        // The exact match, then prefix matches as typed (in the server's
        // order), then ignoring case, then fuzzy; non-matches are gone.
        assert_eq!(labels, ["Vec", "VecDeque", "vec!", "VerticalCenter"]);
    }

    #[test]
    fn case_is_ignored_for_any_script() {
        assert!(starts_with_ignoring_case("VecDeque", "vecd"));
        assert!(starts_with_ignoring_case("Éclair", "éc"));
        assert!(!starts_with_ignoring_case("as", "assert"));
        assert!(starts_with_ignoring_case("anything", ""));
    }

    #[test]
    fn typed_word_is_the_identifier_before_the_cursor() {
        assert_eq!(typed_word("    let g = gre"), "gre");
        assert_eq!(typed_word("foo."), "");
        assert_eq!(typed_word("Self::new_é"), "new_é");
        assert_eq!(typed_word(""), "");
    }

    #[test]
    fn progress_keeps_its_title_across_reports() {
        let first = progress_text(None, Some("Indexing".into()), Some("1/40 (core)".into()), Some(2));
        assert_eq!(first, "Indexing · 2% 1/40 (core)");
        let next = progress_text(Some(&first), None, Some("20/40 (serde)".into()), Some(50));
        assert_eq!(next, "Indexing · 50% 20/40 (serde)");
        assert_eq!(progress_text(None, Some("Roots Scanned".into()), None, None), "Roots Scanned");
    }

    #[test]
    fn definition_links_target_the_name() {
        let uri = cockpit_editor::lsp::path_to_uri(Path::new("/tmp/a.rs")).unwrap();
        let link = LocationLink {
            origin_selection_range: None,
            target_uri: uri.clone(),
            target_range: LspRange::new(Position::new(3, 0), Position::new(9, 1)),
            target_selection_range: LspRange::new(Position::new(5, 7), Position::new(5, 10)),
        };
        let targets = definition_targets(Some(GotoDefinitionResponse::Link(vec![link])));
        assert_eq!(targets, vec![Target { path: "/tmp/a.rs".into(), line: 5, column: 7 }]);
        let scalar = definition_targets(Some(GotoDefinitionResponse::Scalar(lsp_types::Location {
            uri,
            range: LspRange::new(Position::new(1, 2), Position::new(1, 4)),
        })));
        assert_eq!(scalar[0].line, 1);
    }
}

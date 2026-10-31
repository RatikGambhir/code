//! Language servers for a workspace.
//!
//! One server per language family, started on first use. Editors attach
//! when they open a file; diagnostics are routed back to the editor showing
//! the file. Each attached editor gets an [`LspDocument`] that keeps the
//! server's copy of the file in step with the buffer, and the providers the
//! code editor calls for completion, hover, definitions and code actions.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Result};
use cockpit_editor::{
    fuzzy,
    lsp::{resolve_server, server_key, uri_to_path, LspClient, ServerEvent},
    lsp_language_id,
};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, SharedString, Task, WeakEntity, Window};
use gpui_component::input::{
    CodeActionProvider, CompletionProvider, DefinitionProvider, HoverProvider, InputState, RopeExt as _,
};
use gpui_component::Rope;
use lsp_types::{
    CodeAction, CodeActionOrCommand, CompletionContext, CompletionResponse, CompletionTriggerKind,
    GotoDefinitionResponse, LocationLink, OneOf, TextEdit, WorkspaceEdit,
};

use crate::editor_view::EditorView;

/// Completion lists longer than this are cut; the server ranks them first.
const MAX_COMPLETIONS: usize = 100;

pub enum LspEvent {
    /// New diagnostics for a file.
    Diagnostics(PathBuf),
    /// A server started, stopped or reported progress; the status bar shows it.
    StatusChanged,
}

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
    /// No server is installed for the language; nothing to show.
    Unavailable,
}

pub struct LspManager {
    root: PathBuf,
    servers: HashMap<String, Slot>,
    diagnostics: HashMap<PathBuf, Vec<lsp_types::Diagnostic>>,
    last_status: Option<String>,
}

impl EventEmitter<LspEvent> for LspManager {}

impl LspManager {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            servers: HashMap::new(),
            diagnostics: HashMap::new(),
            last_status: None,
        }
    }

    pub fn diagnostics(&self, path: &Path) -> &[lsp_types::Diagnostic] {
        self.diagnostics.get(path).map(Vec::as_slice).unwrap_or_default()
    }

    /// Counts of (errors, warnings) across the workspace.
    pub fn diagnostic_counts(&self) -> (usize, usize) {
        let mut errors = 0;
        let mut warnings = 0;
        for diagnostic in self.diagnostics.values().flatten() {
            match diagnostic.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => errors += 1,
                Some(lsp_types::DiagnosticSeverity::WARNING) => warnings += 1,
                _ => {}
            }
        }
        (errors, warnings)
    }

    /// One line for the status bar.
    pub fn status(&self) -> Option<String> {
        let mut parts: Vec<String> = self
            .servers
            .iter()
            .filter_map(|(key, slot)| {
                Some(match slot {
                    Slot::Starting(_) => format!("{key}: starting…"),
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
                    Slot::Failed(err) => format!("{key}: {}", cockpit_common::truncate_chars(err, 80)),
                    Slot::Unavailable => return None,
                })
            })
            .collect();
        parts.sort();
        (!parts.is_empty()).then(|| parts.join(", "))
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

    /// Attaches `editor` to the server for `language`, starting it if needed.
    /// The editor is called back with the client once it is ready.
    pub fn attach(&mut self, language: &str, editor: WeakEntity<EditorView>, cx: &mut Context<Self>) {
        let key = server_key(language).to_string();
        match self.servers.get_mut(&key) {
            Some(Slot::Ready(server)) => {
                let client = server.client.clone();
                cx.defer(move |cx| {
                    editor.update(cx, |editor, cx| editor.attach_lsp(client, cx)).ok();
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

        self.servers.insert(key.clone(), Slot::Starting(vec![editor]));
        self.status_changed(cx);

        let root = self.root.clone();
        let language = language.to_string();
        let start = cx.background_spawn(async move {
            // Looking the server up runs it once (`--version`), so it stays
            // off the main thread too.
            let search_dirs = cockpit_common::shell_env::common_tool_dirs();
            let Some(config) = resolve_server(&language, &search_dirs).map_err(|err| anyhow!(err))? else {
                return Ok(None);
            };
            LspClient::start(&config, &root).await.map(Some)
        });
        cx.spawn(async move |this, cx| {
            let started = start.await;
            this.update(cx, |this, cx| {
                let waiting = match this.servers.remove(&key) {
                    Some(Slot::Starting(waiting)) => waiting,
                    // Restarted while starting: this client is not wanted.
                    _ => return,
                };
                match started {
                    Ok(Some((client, events))) => {
                        this.servers.insert(
                            key.clone(),
                            Slot::Ready(Server {
                                client: client.clone(),
                                progress: BTreeMap::new(),
                                loading: false,
                                problem: None,
                            }),
                        );
                        for editor in waiting {
                            let client = client.clone();
                            editor.update(cx, |editor, cx| editor.attach_lsp(client, cx)).ok();
                        }
                        this.listen(key.clone(), client, events, cx);
                    }
                    Ok(None) => {
                        this.servers.insert(key.clone(), Slot::Unavailable);
                    }
                    Err(err) => {
                        tracing::warn!("language server for {key} failed to start: {err:#}");
                        this.servers.insert(key.clone(), Slot::Failed(format!("{err:#}")));
                    }
                }
                this.status_changed(cx);
            })
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
                    let server = match this.servers.get_mut(&key) {
                        Some(Slot::Ready(server)) if Arc::ptr_eq(&server.client, &client) => server,
                        _ => return false,
                    };
                    match event {
                        ServerEvent::Diagnostics(params) => {
                            if let Some(path) = uri_to_path(&params.uri) {
                                this.diagnostics.insert(path.clone(), params.diagnostics);
                                cx.emit(LspEvent::Diagnostics(path));
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
                            this.servers.insert(
                                key.clone(),
                                Slot::Failed("stopped; run Restart Language Servers".into()),
                            );
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

    /// Stops every server and forgets failures, so the next `attach` starts
    /// fresh (after installing a server, or when one got stuck).
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.shutdown(cx);
        let paths: Vec<PathBuf> = self.diagnostics.drain().map(|(path, _)| path).collect();
        for path in paths {
            cx.emit(LspEvent::Diagnostics(path));
        }
        self.status_changed(cx);
    }

    pub fn shutdown(&mut self, cx: &mut App) {
        for slot in self.servers.drain().map(|(_, slot)| slot) {
            if let Slot::Ready(server) = slot {
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
pub struct LspDocument {
    pub client: Arc<LspClient>,
    pub path: PathBuf,
    version: Cell<i32>,
    /// The text the server has; compared before each request so the server
    /// never answers for stale text (completion is asked for before the
    /// editor reports the keystroke that triggered it).
    synced: RefCell<Rope>,
    diagnostics: Rc<RefCell<Vec<lsp_types::Diagnostic>>>,
    /// Definitions under the mouse while ⌘ is held, for ⌘-click.
    hovered_definitions: Arc<Mutex<Vec<LocationLink>>>,
}

impl LspDocument {
    pub fn open(
        client: Arc<LspClient>,
        path: PathBuf,
        language: &str,
        text: &Rope,
        diagnostics: Rc<RefCell<Vec<lsp_types::Diagnostic>>>,
    ) -> Result<Rc<Self>> {
        client.did_open(&path, lsp_language_id(language), 1, &text.to_string())?;
        Ok(Rc::new(Self {
            client,
            path,
            version: Cell::new(1),
            synced: RefCell::new(text.clone()),
            diagnostics,
            hovered_definitions: Arc::default(),
        }))
    }

    /// Sends the text to the server if it changed since the last sync.
    pub fn sync(&self, text: &Rope) {
        if *self.synced.borrow() == *text {
            return;
        }
        let version = self.version.get() + 1;
        self.version.set(version);
        if let Err(err) = self.client.did_change(&self.path, version, &text.to_string()) {
            tracing::warn!("didChange {}: {err:#}", self.path.display());
        }
        *self.synced.borrow_mut() = text.clone();
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

/// Completion through the language server.
pub struct LspCompletion {
    doc: Rc<LspDocument>,
    triggers: Vec<char>,
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
        let request = self.doc.client.completion(&self.doc.path, position, Some(context));
        cx.background_spawn(async move {
            let response = request.await?.unwrap_or(CompletionResponse::Array(Vec::new()));
            Ok(rank_completions(response, &word))
        })
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

/// The part of `label` the menu highlights as matching `word`: the label's
/// own first characters when it starts with the word (ignoring case), else
/// nothing.
///
/// The editor's completion menu highlights the first `filter_text.len()`
/// bytes of the label (or the typed text's length when there is no
/// `filter_text`), and GPUI panics when that is not a character boundary of
/// the label, which servers' filter texts often are not. Built from the
/// label itself, this always fits.
fn highlight_prefix(label: &str, word: &str) -> String {
    let mut prefix_len = 0;
    let mut label_chars = label.char_indices();
    for typed in word.chars() {
        match label_chars.next() {
            Some((ix, c)) if c.to_lowercase().eq(typed.to_lowercase()) => prefix_len = ix + c.len_utf8(),
            _ => return String::new(),
        }
    }
    label[..prefix_len].to_string()
}

/// The editor shows completions as given, without filtering or sorting, and
/// servers such as rust-analyzer send everything in scope expecting the
/// client to narrow it down. So: keep the items whose filter text fuzzily
/// matches the typed `word`, put those that start with it first, then follow
/// the server's relevance (`sortText`, preselected first), and keep the best.
///
/// `filter_text` is then replaced with what the menu should highlight (see
/// [`highlight_prefix`]); the menu uses it for nothing else.
fn rank_completions(response: CompletionResponse, word: &str) -> CompletionResponse {
    let (items, incomplete) = match response {
        CompletionResponse::Array(items) => (items, false),
        CompletionResponse::List(list) => (list.items, list.is_incomplete),
    };
    let word_lower = word.to_lowercase();
    let mut ranked: Vec<_> = items
        .into_iter()
        .filter_map(|item| {
            let filter = item.filter_text.as_deref().unwrap_or(&item.label);
            let score = if word.is_empty() { 0 } else { fuzzy::score(word, filter)? };
            // 0: is the word, 1: starts with it as typed, 2: ignoring case,
            // 3: fuzzy.
            let tier = if !word.is_empty() && filter == word {
                0
            } else if filter.starts_with(word) {
                1
            } else if filter.to_lowercase().starts_with(&word_lower) {
                2
            } else {
                3
            };
            let key = (
                tier,
                !item.preselect.unwrap_or(false),
                item.sort_text.clone().unwrap_or_else(|| item.label.clone()),
                std::cmp::Reverse(score),
            );
            Some((key, item))
        })
        .collect();
    ranked.sort_by(|(a, _), (b, _)| a.cmp(b));
    let mut items: Vec<_> = ranked.into_iter().map(|(_, item)| item).take(MAX_COMPLETIONS).collect();
    for item in &mut items {
        item.filter_text = Some(highlight_prefix(&item.label, word));
    }
    CompletionResponse::List(lsp_types::CompletionList { is_incomplete: incomplete, items })
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
        let position = text.offset_to_position(offset);
        let request = self.doc.client.hover(&self.doc.path, position);
        cx.background_spawn(request)
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
        "lsp".into()
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
    let capabilities = doc.client.capabilities();
    let triggers = capabilities
        .and_then(|c| c.completion_provider.as_ref())
        .and_then(|c| c.trigger_characters.clone())
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t.chars().last())
        .collect();
    let supported = |provider: Option<&OneOf<bool, _>>| match provider {
        Some(OneOf::Left(enabled)) => *enabled,
        Some(OneOf::Right(_)) => true,
        None => false,
    };
    let hover = capabilities.and_then(|c| c.hover_provider.as_ref()).is_some_and(|hover| match hover {
        lsp_types::HoverProviderCapability::Simple(enabled) => *enabled,
        lsp_types::HoverProviderCapability::Options(_) => true,
    });
    let definition = supported(capabilities.and_then(|c| c.definition_provider.as_ref()));
    let code_actions = capabilities.and_then(|c| c.code_action_provider.as_ref()).is_some_and(|actions| match actions {
        lsp_types::CodeActionProviderCapability::Simple(enabled) => *enabled,
        lsp_types::CodeActionProviderCapability::Options(_) => true,
    });

    input.lsp.completion_provider = Some(Rc::new(LspCompletion { doc: doc.clone(), triggers }));
    input.lsp.hover_provider = hover.then(|| Rc::new(LspHover { doc: doc.clone() }) as Rc<dyn HoverProvider>);
    input.lsp.definition_provider =
        definition.then(|| Rc::new(LspDefinitions { doc: doc.clone() }) as Rc<dyn DefinitionProvider>);
    input.lsp.code_action_providers = if code_actions {
        vec![Rc::new(LspCodeActions { doc: doc.clone() }) as Rc<dyn CodeActionProvider>]
    } else {
        Vec::new()
    };
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
    fn completions_follow_sort_text_and_are_capped() {
        let item = |label: &str, sort: Option<&str>, preselect: bool| CompletionItem {
            label: label.into(),
            sort_text: sort.map(Into::into),
            preselect: preselect.then_some(true),
            ..Default::default()
        };
        let mut items = vec![item("zeta", Some("0001"), false), item("alpha", Some("0002"), false), item("pick", Some("9"), true)];
        items.extend((0..200).map(|i| item(&format!("x{i}"), Some("5"), false)));
        let CompletionResponse::List(list) = rank_completions(CompletionResponse::Array(items), "") else { panic!() };
        let labels: Vec<_> = list.items.iter().take(3).map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["pick", "zeta", "alpha"]);
        assert_eq!(list.items.len(), MAX_COMPLETIONS);
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
        let CompletionResponse::List(list) = rank_completions(CompletionResponse::Array(items), "Vec") else {
            panic!()
        };
        let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
        // The exact match, then prefix matches as typed (in the server's
        // order), then ignoring case, then fuzzy; non-matches are gone.
        assert_eq!(labels, ["Vec", "VecDeque", "vec!", "VerticalCenter"]);
    }

    #[test]
    fn highlights_always_fit_the_label() {
        assert_eq!(highlight_prefix("greet()", "gre"), "gre");
        assert_eq!(highlight_prefix("Greeter", "gree"), "Gree");
        // Typed text longer than the label, or not its start: no highlight.
        assert_eq!(highlight_prefix("as", "assert"), "");
        assert_eq!(highlight_prefix("into", "nto"), "");
        // Multi-byte characters are taken whole.
        assert_eq!(highlight_prefix("éclair", "e"), "");
        assert_eq!(highlight_prefix("éclair", "éc"), "éc");
        assert_eq!(highlight_prefix("Some(…)", "Some"), "Some");

        // Server filter texts that would cut a character are replaced.
        let item = CompletionItem {
            label: "café…".into(),
            filter_text: Some("cafe_long_filter".into()),
            ..Default::default()
        };
        let CompletionResponse::List(list) = rank_completions(CompletionResponse::Array(vec![item]), "caf") else {
            panic!()
        };
        let label = &list.items[0].label;
        let highlight = list.items[0].filter_text.as_deref().unwrap();
        assert!(label.is_char_boundary(highlight.len()) && label.starts_with(highlight));
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

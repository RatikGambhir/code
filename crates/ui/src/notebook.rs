//! A notebook tab: cells of code that call a function from the workspace,
//! each with its output and returned value. Running is up to the notebook's
//! [`Kernel`]; this view knows nothing about any one language.
//!
//! When the notebook has [`CellAnalysis`], the tab starts its own language
//! server and shows each cell to it, so cells get completion, hover and
//! errors as you type. The server stops when the tab closes.

use std::{
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use cockpit_editor::{
    lsp::{resolve_server, uri_to_path, LspClient, ServerEvent},
    lsp_language_id,
};
use cockpit_notebook::{CellAnalysis, Kernel, Notebook, RunEvent, RunHandle, RunOutcome, Snippet, Value};
use gpui::{
    div, prelude::*, px, AnyElement, App, ClipboardItem, Context, Entity, EntityInputHandler as _, EventEmitter,
    FocusHandle, Focusable, Hsla, ScrollHandle, SharedString, Subscription, Task, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, RopeExt as _, TabSize},
    v_flex, Sizable as _,
};
use serde_json::Value as Json;

use crate::{
    actions::{self, NOTEBOOK_CELL_CONTEXT},
    assets::icon,
    chrome::{self, key_chip},
    lsp::{self, LspDocument, Target},
    theme::{tokens, Tokens, RADIUS, TEXT_BODY, TEXT_MONO, TEXT_SMALL, TEXT_TITLE},
};

/// Lines of printed output kept per cell; older ones are dropped.
const MAX_OUTPUT_LINES: usize = 2_000;
/// Lines of compiler output kept per cell.
const MAX_BUILD_LINES: usize = 1_000;
/// A cell's editor grows with its text up to this many lines, then scrolls.
const MAX_CELL_LINES: usize = 30;
const MIN_CELL_LINES: usize = 3;

pub enum NotebookEvent {
    /// The title or running state changed.
    Changed,
    /// Show the function the notebook calls.
    OpenSource(Target),
}

pub struct NotebookView {
    title: SharedString,
    subject: SharedString,
    signature: Option<SharedString>,
    notes: Vec<SharedString>,
    /// The file's functions, each a button that adds a cell calling it.
    functions: Vec<Snippet>,
    source: Target,
    language: SharedString,
    /// Stopped first when the tab closes (see `Drop`), before the kernel
    /// and analysis clean up their files.
    server: Server,
    /// Versions of the document tying the cells together, as last sent.
    root_version: i32,
    kernel: Arc<dyn Kernel>,
    analysis: Option<Arc<dyn CellAnalysis>>,
    cells: Vec<Cell>,
    next_cell_id: usize,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
}

/// The cells' language server.
enum Server {
    /// The notebook's language has no analysis.
    Off,
    Starting,
    Ready {
        client: Arc<LspClient>,
        /// Still loading the project (dependencies, build scripts).
        loading: bool,
        /// What it reports working on ("Indexing 40%").
        progress: Option<String>,
    },
    Failed(SharedString),
}

struct Cell {
    id: usize,
    input: Entity<InputState>,
    run: Option<ActiveRun>,
    output: Option<CellOutput>,
    /// The cell as the language server sees it.
    doc: Option<Rc<LspDocument>>,
    /// Why the last Format did nothing; cleared on the next edit.
    format_error: Option<SharedString>,
    formatting: Option<Task<()>>,
    _subscription: Subscription,
}

struct ActiveRun {
    handle: RunHandle,
    _task: Task<()>,
}

#[derive(Default)]
struct CellOutput {
    build: Vec<SharedString>,
    printed: Vec<(SharedString, bool)>,
    /// Printed lines dropped to stay under [`MAX_OUTPUT_LINES`].
    printed_dropped: usize,
    phase: Phase,
    started: Option<Instant>,
    elapsed: Option<Duration>,
    /// Show compiler output even though the build worked.
    show_build: bool,
}

#[derive(Default)]
enum Phase {
    #[default]
    Building,
    Running,
    Done(RunOutcome),
}

impl EventEmitter<NotebookEvent> for NotebookView {}

impl Focusable for NotebookView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.cells.first() {
            Some(cell) => cell.input.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl NotebookView {
    /// A notebook for `notebook`, opened from `source`.
    pub fn new(notebook: Notebook, source: Target, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            title: notebook.title.into(),
            subject: notebook.subject.into(),
            signature: notebook.signature.map(SharedString::from),
            notes: notebook.notes.into_iter().map(SharedString::from).collect(),
            functions: notebook.functions,
            source,
            language: notebook.cell_language.into(),
            server: Server::Off,
            root_version: 0,
            kernel: notebook.kernel,
            analysis: notebook.analysis,
            cells: Vec::new(),
            next_cell_id: 0,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
        };
        view.add_cell(notebook.cell, None, window, cx);
        view.start_server(cx);
        view
    }

    pub fn title(&self) -> SharedString {
        format!("{} · notebook", self.title).into()
    }

    pub fn subject(&self) -> SharedString {
        self.subject.clone()
    }

    pub fn is_running(&self) -> bool {
        self.cells.iter().any(|c| c.run.is_some())
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cell) = self.cells.first() {
            cell.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Adds a cell after `after` (or at the end) and focuses it.
    fn add_cell(&mut self, code: String, after: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let language = self.language.clone();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .line_number(true)
                .soft_wrap(false)
                .tab_size(TabSize { tab_size: 4, hard_tabs: false })
                .default_value(code)
        });
        let subscription = cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                if let Some(cell) = this.cells.iter_mut().find(|c| c.input == input) {
                    cell.format_error = None;
                    // Keep the server's copy current, so its errors are.
                    if let Some(doc) = &cell.doc {
                        doc.sync(input.read(cx).text());
                    }
                }
                // The editor's height follows its line count.
                cx.notify();
            }
        });
        let id = self.next_cell_id;
        self.next_cell_id += 1;
        let index = after
            .and_then(|after| self.index_of(after))
            .map_or(self.cells.len(), |ix| ix + 1);
        self.cells.insert(
            index,
            Cell {
                id,
                input: input.clone(),
                run: None,
                output: None,
                doc: None,
                format_error: None,
                formatting: None,
                _subscription: subscription,
            },
        );
        // The server learns of the cell's module before the cell itself.
        self.sync_root_document();
        self.connect_cell(index, cx);
        if self.cells.len() > 1 {
            input.update(cx, |input, cx| input.focus(window, cx));
            if index + 1 == self.cells.len() {
                self.scroll.scroll_to_bottom();
            }
        }
        cx.notify();
    }

    fn index_of(&self, id: usize) -> Option<usize> {
        self.cells.iter().position(|c| c.id == id)
    }

    fn remove_cell(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else { return };
        if self.cells.len() == 1 {
            return;
        }
        let cell = self.cells.remove(index);
        if let Some(run) = cell.run {
            run.handle.stop();
        }
        if let Some(doc) = &cell.doc {
            doc.close();
        }
        self.sync_root_document();
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    // --- language server ---------------------------------------------------

    fn start_server(&mut self, cx: &mut Context<Self>) {
        let Some(analysis) = self.analysis.clone() else { return };
        self.server = Server::Starting;
        let start = cx.background_spawn(async move {
            // Looking the server up runs it once (`--version`).
            let search_dirs = cockpit_common::shell_env::common_tool_dirs();
            let Some(mut config) = resolve_server(analysis.language(), &search_dirs).map_err(|err| anyhow!(err))? else {
                return Ok(None);
            };
            if let Some(overrides) = analysis.settings() {
                let base = config.settings.take().unwrap_or_else(|| Json::Object(Default::default()));
                config.settings = Some(merge_settings(base, overrides));
            }
            LspClient::start(&config, analysis.root()).await.map(Some)
        });
        cx.spawn(async move |this, cx| {
            let started: anyhow::Result<Option<(Arc<LspClient>, async_channel::Receiver<ServerEvent>)>> = start.await;
            this.update(cx, |this, cx| {
                match started {
                    Ok(Some((client, events))) => this.server_ready(client, events, cx),
                    Ok(None) => this.server = Server::Failed("no language server is installed for it".into()),
                    Err(err) => this.server = Server::Failed(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn server_ready(&mut self, client: Arc<LspClient>, events: async_channel::Receiver<ServerEvent>, cx: &mut Context<Self>) {
        self.server = Server::Ready { client, loading: true, progress: None };
        self.root_version = 0;
        self.sync_root_document();
        for index in 0..self.cells.len() {
            self.connect_cell(index, cx);
        }
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let listening = this.update(cx, |this, cx| this.on_server_event(event, cx)).unwrap_or(false);
                if !listening {
                    break;
                }
            }
        })
        .detach();
    }

    /// Sends the document that ties the cells together, for the cells there
    /// are now.
    fn sync_root_document(&mut self) {
        let (Some(analysis), Server::Ready { client, .. }) = (&self.analysis, &self.server) else { return };
        let ids: Vec<usize> = self.cells.iter().map(|c| c.id).collect();
        let Some((path, text)) = analysis.root_document(&ids) else { return };
        self.root_version += 1;
        let sent = if self.root_version == 1 {
            client.did_open(&path, lsp_language_id(analysis.language()), 1, &text)
        } else {
            client.did_change(&path, self.root_version, &text)
        };
        if let Err(err) = sent {
            tracing::warn!("notebook: sending {} to the language server: {err:#}", path.display());
        }
    }

    /// Shows cell `index` to the server and gives it completion and hover.
    fn connect_cell(&mut self, index: usize, cx: &mut Context<Self>) {
        let (Some(analysis), Server::Ready { client, .. }) = (&self.analysis, &self.server) else { return };
        let cell = &mut self.cells[index];
        if cell.doc.is_some() {
            return;
        }
        let spec = analysis.cell_document(cell.id);
        let text = cell.input.read(cx).text().clone();
        match LspDocument::open_cell(client.clone(), spec.path, analysis.language(), &text, spec.prefix, spec.suffix) {
            Ok(doc) => {
                cell.input.update(cx, |input, _| lsp::install_cell_providers(&doc, input));
                cell.doc = Some(doc);
            }
            Err(err) => tracing::warn!("notebook: opening a cell in the language server: {err:#}"),
        }
    }

    /// `false` once the server is gone.
    fn on_server_event(&mut self, event: ServerEvent, cx: &mut Context<Self>) -> bool {
        if let ServerEvent::Diagnostics(params) = event {
            self.on_diagnostics(params, cx);
            return true;
        }
        let Server::Ready { loading, progress, .. } = &mut self.server else { return false };
        match event {
            ServerEvent::Progress { title, message, percentage, done, .. } => {
                *progress = (!done).then(|| {
                    let what = title.or(message).unwrap_or_else(|| "working".into());
                    match percentage {
                        Some(p) => format!("{what} {p}%"),
                        None => what,
                    }
                });
            }
            ServerEvent::Status { quiescent, .. } => *loading = !quiescent,
            ServerEvent::Exited => {
                self.server = Server::Failed("the language server stopped; reopen the notebook to restart it".into());
                for cell in &mut self.cells {
                    cell.doc = None;
                    cell.input.update(cx, |input, _| lsp::remove_providers(input));
                }
                cx.notify();
                return false;
            }
            ServerEvent::Diagnostics(_) | ServerEvent::Message { .. } => {}
        }
        cx.notify();
        true
    }

    /// The server's errors for a cell, on the cell's own lines.
    fn on_diagnostics(&mut self, params: lsp_types::PublishDiagnosticsParams, cx: &mut Context<Self>) {
        let Some(path) = uri_to_path(&params.uri) else { return };
        let Some(cell) = self.cells.iter().find(|c| c.doc.as_ref().is_some_and(|d| d.path == path)) else { return };
        let Some(shift) = cell.doc.as_ref().map(|d| d.shift) else { return };
        let lines = cell.input.read(cx).text().lines_len() as u32;
        let mapped: Vec<lsp_types::Diagnostic> = params
            .diagnostics
            .into_iter()
            .filter_map(|mut diagnostic| {
                diagnostic.range = shift.range_from_server(diagnostic.range)?;
                // Problems in the text after the cell are not the cell's.
                (diagnostic.range.start.line < lines).then_some(diagnostic)
            })
            .collect();
        cell.input.update(cx, |input, cx| {
            if let Some(set) = input.diagnostics_mut() {
                set.clear();
                set.extend(mapped);
            }
            cx.notify();
        });
    }

    fn server_status(&self, t: &Tokens) -> Option<(String, Hsla)> {
        let name = || match self.language.as_ref() {
            "rust" => "rust-analyzer".to_string(),
            other => format!("{other} language server"),
        };
        Some(match &self.server {
            Server::Off => return None,
            Server::Starting => (format!("Starting {} for completions…", name()), t.fg_subtle),
            Server::Ready { progress: Some(work), .. } => (format!("{}: {work}", name()), t.fg_subtle),
            Server::Ready { loading: true, .. } => (format!("{}: loading the project…", name()), t.fg_subtle),
            Server::Ready { .. } => (format!("{}: completions and errors as you type", name()), t.fg_subtle),
            Server::Failed(why) => (format!("No completions in cells: {why}"), t.warning),
        })
    }

    // --- formatting and running --------------------------------------------

    /// Formats the cell; with `then_run`, runs it afterwards (formatted or,
    /// if formatting failed, as written: the build reports the problem).
    fn format_cell(&mut self, id: usize, then_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else { return };
        let code = self.cells[index].input.read(cx).text().to_string();
        let kernel = self.kernel.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let formatted = cx.background_spawn(async move { kernel.format(&code).map(|f| (code, f)) }).await;
            this.update_in(cx, |this, window, cx| {
                let Some(index) = this.index_of(id) else { return };
                this.cells[index].formatting = None;
                match formatted {
                    Ok((original, Some(new))) => this.apply_format(index, &original, &new, window, cx),
                    Ok((_, None)) => {}
                    Err(err) if !then_run => {
                        this.cells[index].format_error = Some(format!("Couldn't format: {err:#}").into());
                    }
                    Err(_) => {}
                }
                if then_run {
                    this.start_run(id, cx);
                }
                cx.notify();
            })
            .ok();
        });
        self.cells[index].formatting = Some(task);
        cx.notify();
    }

    /// Puts the formatted text in, as one edit that ⌘Z undoes, unless the
    /// cell changed while it was being formatted.
    fn apply_format(&mut self, index: usize, original: &str, formatted: &str, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.cells[index].input.clone();
        input.update(cx, |input, cx| {
            let current = input.text().to_string();
            if current != original || current == formatted {
                return;
            }
            let focused = input.focus_handle(cx).is_focused(window);
            let cursor = input.cursor_position();
            let all = 0..current.encode_utf16().count();
            input.replace_text_in_range(Some(all), formatted, window, cx);
            // Restoring the cursor focuses the editor; only when it had focus.
            if focused {
                input.set_cursor_position(cursor, window, cx);
            }
        });
    }

    fn run_cell(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Cells are tidied up as they run.
        self.format_cell(id, true, window, cx);
    }

    fn start_run(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else { return };
        let code = self.cells[index].input.read(cx).text().to_string();
        if let Some(previous) = self.cells[index].run.take() {
            previous.handle.stop();
        }
        let (handle, events) = match self.kernel.run(&code) {
            Ok(started) => started,
            Err(err) => {
                self.cells[index].output = Some(CellOutput {
                    phase: Phase::Done(RunOutcome::Failed(format!("{err:#}"))),
                    ..CellOutput::default()
                });
                cx.notify();
                return;
            }
        };
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let finished = matches!(event, RunEvent::Finished(_));
                let alive = this.update(cx, |this, cx| this.on_run_event(id, event, cx)).is_ok();
                if finished || !alive {
                    break;
                }
            }
        });
        let cell = &mut self.cells[index];
        cell.output = Some(CellOutput { started: Some(Instant::now()), ..CellOutput::default() });
        cell.run = Some(ActiveRun { handle, _task: task });
        cx.emit(NotebookEvent::Changed);
        cx.notify();
    }

    fn stop_cell(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else { return };
        if let Some(run) = &self.cells[index].run {
            run.handle.stop();
        }
        cx.notify();
    }

    fn on_run_event(&mut self, id: usize, event: RunEvent, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else { return };
        let cell = &mut self.cells[index];
        let Some(output) = cell.output.as_mut() else { return };
        match event {
            RunEvent::Build(line) => {
                if output.build.len() < MAX_BUILD_LINES {
                    output.build.push(line.into());
                }
            }
            RunEvent::Output { line, stderr } => {
                if matches!(output.phase, Phase::Building) {
                    output.phase = Phase::Running;
                }
                output.printed.push((line.replace('\t', "    ").into(), stderr));
                if output.printed.len() > MAX_OUTPUT_LINES {
                    let excess = output.printed.len() - MAX_OUTPUT_LINES;
                    output.printed.drain(..excess);
                    output.printed_dropped += excess;
                }
            }
            RunEvent::Finished(outcome) => {
                output.elapsed = output.started.map(|s| s.elapsed());
                output.phase = Phase::Done(outcome);
                cell.run = None;
                cx.emit(NotebookEvent::Changed);
            }
        }
        cx.notify();
    }

    // --- rendering ---------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let source = self.source.clone();
        v_flex()
            .gap(px(6.))
            .pb(px(10.))
            .border_b_1()
            .border_color(t.border)
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(icon("notebook-pen").size(px(15.)).text_color(t.fg_muted))
                    .child(
                        div()
                            .id("nb-subject")
                            .font_family(t.mono.clone())
                            .text_size(TEXT_TITLE)
                            .text_color(t.fg)
                            .cursor_pointer()
                            .hover(|s| s.text_color(t.accent_text))
                            .tooltip(chrome::tooltip("Go to the function"))
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(NotebookEvent::OpenSource(source.clone()))))
                            .child(self.subject.clone()),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("nb-add-cell")
                            .label("Add cell")
                            .icon(icon("plus"))
                            .small()
                            .ghost()
                            .tooltip("Add a cell at the end (⌘⇧↵ from a cell adds one below it)")
                            .on_click(cx.listener(|this, _, window, cx| this.add_cell(String::new(), None, window, cx))),
                    ),
            )
            .children(self.signature.clone().map(|signature| {
                div().font_family(t.mono.clone()).text_size(TEXT_SMALL).text_color(t.fg_muted).child(signature)
            }))
            .child(div().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(if self.signature.is_some() {
                "Fill in the arguments and run a cell. Its last line's value is shown below it; cells don't share variables."
            } else {
                "Write code that calls into this file and run it. The last line's value is shown below the cell; cells don't share variables."
            }))
            .when(!self.functions.is_empty(), |el| el.child(self.render_functions(cx)))
            .children(self.server_status(&t).map(|(text, color)| {
                div().text_size(TEXT_SMALL).text_color(color).child(text)
            }))
            .children(self.notes.iter().map(|note| {
                div()
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(RADIUS)
                    .bg(t.warning.opacity(0.1))
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg)
                    .child(note.clone())
            }))
    }

    /// The file's functions; clicking one adds a cell that calls it.
    fn render_functions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        h_flex()
            .flex_wrap()
            .items_center()
            .gap(px(4.))
            .child(div().text_size(TEXT_SMALL).text_color(t.fg_subtle).mr(px(2.)).child("Try a function:"))
            .children(self.functions.iter().enumerate().map(|(ix, function)| {
                let code = function.cell.clone();
                div()
                    .id(("nb-function", ix))
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(RADIUS)
                    .border_1()
                    .border_color(t.border)
                    .font_family(t.mono.clone())
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.hover).text_color(t.fg))
                    .tooltip(chrome::tooltip(format!("Add a cell calling {}", function.signature)))
                    .on_click(cx.listener(move |this, _, window, cx| this.add_cell(code.clone(), None, window, cx)))
                    .child(function.label.clone())
            }))
    }

    fn render_cell(&self, index: usize, cell: &Cell, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let id = cell.id;
        let running = cell.run.is_some();
        let font_size = px(crate::state::AppState::global(cx).settings.editor_font_size);
        let line_height = font_size * 1.6;
        let lines = cell.input.read(cx).text().lines_len().clamp(MIN_CELL_LINES, MAX_CELL_LINES);
        let focused = cell.input.focus_handle(cx).contains_focused(window, cx);
        let can_remove = self.cells.len() > 1;

        let toolbar = h_flex()
            .gap(px(6.))
            .items_center()
            .child(
                div()
                    .font_family(t.mono.clone())
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_subtle)
                    .child(format!("[{}]", index + 1)),
            )
            .child(if running {
                Button::new(("nb-stop", id))
                    .label("Stop")
                    .icon(icon("square"))
                    .small()
                    .on_click(cx.listener(move |this, _, _, cx| this.stop_cell(id, cx)))
                    .into_any_element()
            } else {
                Button::new(("nb-run", id))
                    .label("Run")
                    .icon(icon("play"))
                    .small()
                    .primary()
                    .on_click(cx.listener(move |this, _, window, cx| this.run_cell(id, window, cx)))
                    .into_any_element()
            })
            .when(focused && !running, |el| el.child(key_chip("⌘↵", cx)))
            .child(
                Button::new(("nb-format", id))
                    .label("Format")
                    .small()
                    .ghost()
                    .loading(cell.formatting.is_some())
                    .tooltip("Format this cell with the language's formatter (⇧⌥F). Cells are also formatted when they run.")
                    .on_click(cx.listener(move |this, _, window, cx| this.format_cell(id, false, window, cx))),
            )
            .child(div().flex_1())
            .children(cell.output.as_ref().map(|output| render_status(output, &t)))
            .when(can_remove, |el| {
                el.child(
                    Button::new(("nb-remove", id))
                        .icon(icon("x"))
                        .small()
                        .ghost()
                        .tooltip("Remove this cell")
                        .on_click(cx.listener(move |this, _, _, cx| this.remove_cell(id, cx))),
                )
            });

        let editor = div()
            .h(line_height * lines as f32 + px(12.))
            .rounded(RADIUS)
            .border_1()
            .border_color(if focused { t.border_strong } else { t.border })
            .bg(t.surface)
            .overflow_hidden()
            .font_family(t.mono.clone())
            .text_size(font_size)
            .line_height(line_height)
            .child(
                Input::new(&cell.input)
                    .h_full()
                    .appearance(false)
                    .bordered(false)
                    .focus_bordered(false),
            );

        v_flex()
            .id(("nb-cell", id))
            .key_context(NOTEBOOK_CELL_CONTEXT)
            .on_action(cx.listener(move |this, _: &actions::RunCell, window, cx| this.run_cell(id, window, cx)))
            .on_action(cx.listener(move |this, _: &actions::FormatCell, window, cx| {
                this.format_cell(id, false, window, cx);
            }))
            .on_action(cx.listener(move |this, _: &actions::AddCell, window, cx| {
                this.add_cell(String::new(), Some(id), window, cx);
            }))
            .gap(px(6.))
            .child(toolbar)
            .child(editor)
            .children(cell.format_error.clone().map(|error| {
                div().text_size(TEXT_SMALL).text_color(t.warning).font_family(t.mono.clone()).child(error)
            }))
            .children(cell.output.as_ref().map(|output| self.render_output(id, output, cx)))
            .into_any_element()
    }

    fn render_output(&self, id: usize, output: &CellOutput, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let build_failed = matches!(output.phase, Phase::Done(RunOutcome::BuildFailed));
        let show_build = build_failed || output.show_build;
        let mut body = v_flex().gap(px(6.)).font_family(t.mono.clone()).text_size(TEXT_MONO);

        // While building, the latest compiler line is the progress.
        if matches!(output.phase, Phase::Building) && !show_build {
            if let Some(last) = output.build.last() {
                body = body.child(div().text_color(t.fg_subtle).truncate().child(last.trim().to_string()));
            }
        }
        if show_build && !output.build.is_empty() {
            body = body.child(lines_box(
                ("nb-build", id),
                output.build.iter().map(|line| (line.clone(), build_line_color(line, &t))),
                &t,
            ));
        }
        if output.printed_dropped > 0 {
            body = body.child(
                div()
                    .text_color(t.fg_subtle)
                    .child(format!("… {} earlier lines not shown", output.printed_dropped)),
            );
        }
        if !output.printed.is_empty() {
            body = body.child(lines_box(
                ("nb-printed", id),
                output.printed.iter().map(|(line, stderr)| (line.clone(), if *stderr { t.danger } else { t.fg })),
                &t,
            ));
        }
        match &output.phase {
            Phase::Done(RunOutcome::Returned(Some(value))) => body = body.child(self.render_value(id, value, cx)),
            Phase::Done(RunOutcome::Failed(message)) => {
                body = body.child(div().text_color(t.danger).font_family(tokens(cx).mono.clone()).child(message.clone()));
            }
            _ => {}
        }

        let has_build = !output.build.is_empty() && !build_failed;
        let finished = matches!(output.phase, Phase::Done(_));
        v_flex()
            .gap(px(4.))
            .pl(px(10.))
            .border_l_2()
            .border_color(t.border)
            .child(body)
            .when(has_build && finished, |el| {
                el.child(
                    div()
                        .id(("nb-toggle-build", id))
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_subtle)
                        .cursor_pointer()
                        .hover(|s| s.text_color(t.fg))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(output) = this.index_of(id).and_then(|ix| this.cells[ix].output.as_mut()) {
                                output.show_build = !output.show_build;
                                cx.notify();
                            }
                        }))
                        .child(if output.show_build { "Hide build output" } else { "Show build output" }),
                )
            })
            .into_any_element()
    }

    fn render_value(&self, id: usize, value: &Value, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let copy_text = value.text.clone();
        v_flex()
            .rounded(RADIUS)
            .border_1()
            .border_color(t.accent.opacity(0.35))
            .bg(t.accent_soft.opacity(0.35))
            .child(
                h_flex()
                    .px(px(8.))
                    .py(px(4.))
                    .gap(px(6.))
                    .items_center()
                    .border_b_1()
                    .border_color(t.accent.opacity(0.2))
                    .child(div().text_size(TEXT_SMALL).text_color(t.accent_text).font_family(t.mono.clone()).child("returned"))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_muted)
                            .child(value.type_name.clone()),
                    )
                    .children(copy_text.map(|text| {
                        Button::new(("nb-copy", id))
                            .label("Copy")
                            .xsmall()
                            .ghost()
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
                    })),
            )
            .child(match &value.text {
                Some(text) => div()
                    .id(("nb-value", id))
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .px(px(8.))
                    .py(px(6.))
                    .children(text.lines().map(|line| {
                        div().whitespace_nowrap().text_color(t.fg).child(if line.is_empty() {
                            " ".to_string()
                        } else {
                            line.replace('\t', "    ")
                        })
                    }))
                    .into_any_element(),
                None => div()
                    .px(px(8.))
                    .py(px(6.))
                    .font_family(t.mono.clone())
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_subtle)
                    .child("This type doesn't implement Debug, so only its name is shown.")
                    .into_any_element(),
            })
            .into_any_element()
    }
}

fn render_status(output: &CellOutput, t: &Tokens) -> AnyElement {
    let (text, color): (String, Hsla) = match &output.phase {
        Phase::Building => ("Building…".into(), t.fg_subtle),
        Phase::Running => ("Running…".into(), t.fg_subtle),
        Phase::Done(outcome) => {
            let took = output.elapsed.map(format_duration).unwrap_or_default();
            match outcome {
                RunOutcome::Returned(_) => (format!("Done in {took}"), t.success),
                RunOutcome::BuildFailed => ("Didn't compile".into(), t.danger),
                RunOutcome::Failed(_) => (format!("Failed after {took}"), t.danger),
                RunOutcome::Stopped => ("Stopped".into(), t.fg_subtle),
            }
        }
    };
    div().text_size(TEXT_SMALL).text_color(color).child(text).into_any_element()
}

fn format_duration(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.1}s", d.as_secs_f32())
    }
}

fn build_line_color(line: &str, t: &Tokens) -> Hsla {
    let trimmed = line.trim_start();
    if trimmed.starts_with("error") {
        t.danger
    } else if trimmed.starts_with("warning") {
        t.warning
    } else if trimmed.starts_with("-->") {
        t.accent_text
    } else {
        t.fg_muted
    }
}

/// Lines of output in a scrollable box.
fn lines_box(
    id: impl Into<gpui::ElementId>,
    lines: impl Iterator<Item = (SharedString, Hsla)>,
    t: &Tokens,
) -> impl IntoElement {
    div()
        .id(id)
        .max_h(px(280.))
        .overflow_y_scroll()
        .px(px(8.))
        .py(px(6.))
        .rounded(RADIUS)
        .bg(t.raised)
        .children(lines.map(|(line, color)| {
            div()
                .whitespace_nowrap()
                .text_color(color)
                .child(if line.is_empty() { SharedString::from(" ") } else { line })
        }))
}

impl Drop for NotebookView {
    fn drop(&mut self) {
        // The cells' editors hold the server too, and go later; stop it now,
        // before the kernel and analysis delete the files it reads.
        if let Server::Ready { client, .. } = &self.server {
            client.kill();
        }
        // The kernel cleans up once its last run ends; end them now.
        for cell in &self.cells {
            if let Some(run) = &cell.run {
                run.handle.stop();
            }
        }
    }
}

impl Render for NotebookView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let cells: Vec<AnyElement> = self
            .cells
            .iter()
            .enumerate()
            .map(|(ix, cell)| self.render_cell(ix, cell, window, cx))
            .collect();
        div()
            .id("notebook")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .bg(t.surface)
            .text_size(TEXT_BODY)
            .text_color(t.fg)
            .child(
                v_flex()
                    .max_w(px(980.))
                    .px(px(20.))
                    .py(px(16.))
                    .gap(px(18.))
                    .child(self.render_header(cx))
                    .children(cells),
            )
    }
}


/// `overrides` laid over `base`, object by object.
fn merge_settings(mut base: Json, overrides: Json) -> Json {
    match (&mut base, overrides) {
        (Json::Object(base_map), Json::Object(overrides)) => {
            for (key, value) in overrides {
                let merged = match base_map.remove(&key) {
                    Some(existing) => merge_settings(existing, value),
                    None => value,
                };
                base_map.insert(key, merged);
            }
            base
        }
        (_, overrides) => overrides,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn settings_merge_object_by_object() {
        let merged = super::merge_settings(
            json!({"cargo": {"buildScripts": {"enable": true}, "allTargets": true}, "checkOnSave": true}),
            json!({"cargo": {"allTargets": false, "targetDir": "/t"}, "checkOnSave": false}),
        );
        assert_eq!(
            merged,
            json!({"cargo": {"buildScripts": {"enable": true}, "allTargets": false, "targetDir": "/t"}, "checkOnSave": false})
        );
    }
}

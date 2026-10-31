//! A code editor tab: gpui-component's rope-based editor bound to a file.

use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

use anyhow::Result;
use cockpit_debugger::Runnable;
use cockpit_editor::{lsp::LspClient, Document};
use gpui::{
    div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, MouseMoveEvent, SharedString, Subscription, WeakEntity, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{self, EditorDecorations, GutterEvent, HoverProvider, Input, InputEvent, InputState, Position, Rope, RopeExt as _},
    v_flex, ActiveTheme as _, Sizable as _,
};

use crate::{
    actions::{self, CODE_EDITOR_CONTEXT},
    debug::{editor::DebugHover, Debugger},
    lsp::{self, LspDocument, LspManager, Target},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationKind {
    Definition,
    References,
}

pub enum EditorEvent {
    /// Title or dirty state changed.
    Changed,
    Saved(PathBuf),
    /// The language server answered a go-to-definition or find-references
    /// request; the workspace jumps or lists the places.
    Locations {
        kind: LocationKind,
        symbol: String,
        origin: Target,
        targets: Vec<Target>,
    },
    /// Something to tell the user ("No definition found").
    Notify(String),
    /// A press in the gutter (breakpoints, run icons).
    Gutter(GutterEvent),
    /// The text changed; `old` is the text before.
    Edited { old: Rope },
}

pub struct EditorView {
    document: Document,
    input: Entity<InputState>,
    dirty: bool,
    /// The file changed on disk while it had unsaved edits.
    conflict: bool,
    read_only: bool,
    lsp: Option<Rc<LspDocument>>,
    /// The file's diagnostics, shared with the code action provider.
    diagnostics: Rc<RefCell<Vec<lsp_types::Diagnostic>>>,
    /// The text as of the last `Edited`, to tell what an edit changed.
    last_text: Rope,
    /// Lines with a run icon, and what they run.
    runnables: Vec<Runnable>,
    /// Hover shows values from this debugger while it is paused.
    debugger: Option<WeakEntity<Debugger>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Focusable for EditorView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl EditorView {
    pub fn open(
        path: PathBuf,
        lsp: Option<&Entity<LspManager>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self> {
        let document = if path.exists() {
            Document::open(&path)?
        } else {
            Document::new_file(&path)
        };
        let view = Self::with_document(document, false, window, cx);
        if let Some(lsp) = lsp {
            view.connect_lsp(lsp, cx);
        }
        Ok(view)
    }

    /// Asks the workspace's language servers for this file's server; it calls
    /// back [`Self::attach_lsp`] once that is running.
    pub fn connect_lsp(&self, lsp: &Entity<LspManager>, cx: &mut Context<Self>) {
        let language = self.document.language();
        if self.read_only || language == "text" {
            return;
        }
        let this = cx.entity().downgrade();
        lsp.update(cx, |lsp, cx| lsp.attach(language, this, cx));
    }

    /// A read-only buffer that is not backed by a file the user edits (diffs).
    /// `extension` picks the highlighting (`"diff"` for diffs).
    pub fn read_only(title: &str, extension: &str, text: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let document = Document::new_file(PathBuf::from(format!("{title}.{extension}")));
        let view = Self::with_document(document, true, window, cx);
        view.input.update(cx, |input, cx| input.set_value(text, window, cx));
        view
    }

    fn with_document(document: Document, read_only: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let language = document.language();
        let text = document.saved_text().to_string();
        let text_for_last = text.clone();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .line_number(true)
                .searchable(true)
                .soft_wrap(false)
                .default_value(text)
        });
        let subscription = cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _window, cx| {
            if let InputEvent::Change = event {
                if !this.read_only {
                    if !this.dirty {
                        this.dirty = true;
                        cx.emit(EditorEvent::Changed);
                    }
                    this.sync_lsp(cx);
                    let new = input.read(cx).text().clone();
                    let old = std::mem::replace(&mut this.last_text, new);
                    cx.emit(EditorEvent::Edited { old });
                }
            }
        });
        if !read_only {
            if let Some(language) = cockpit_notebook::language_for(document.path()) {
                let label = SharedString::from(format!("Open {} Notebook", language.name()));
                input.update(cx, |input, _| {
                    input.set_context_menu_extras(vec![(label, Box::new(actions::OpenNotebook))]);
                });
            }
        }
        let gutter = cx.subscribe(&input, |this, _, event: &GutterEvent, cx| {
            if !this.read_only {
                cx.emit(EditorEvent::Gutter(event.clone()));
            }
        });
        Self {
            document,
            input,
            dirty: false,
            conflict: false,
            read_only,
            lsp: None,
            diagnostics: Rc::default(),
            last_text: Rope::from(text_for_last.as_str()),
            runnables: Vec::new(),
            debugger: None,
            _subscriptions: vec![subscription, gutter],
        }
    }

    pub fn path(&self) -> &std::path::Path {
        self.document.path()
    }

    pub fn title(&self) -> SharedString {
        self.document.file_name().into()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn language(&self) -> &'static str {
        self.document.language()
    }

    pub fn line_ending(&self) -> &'static str {
        self.document.line_ending().label()
    }

    pub fn cursor(&self, cx: &App) -> Position {
        self.input.read(cx).cursor_position()
    }

    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).text().to_string()
    }

    pub fn save(&mut self, cx: &mut Context<Self>) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        let text = self.text(cx);
        self.document.save(&text)?;
        self.dirty = false;
        self.conflict = false;
        if let Some(doc) = &self.lsp {
            doc.saved(&text);
        }
        cx.emit(EditorEvent::Saved(self.document.path().to_path_buf()));
        cx.emit(EditorEvent::Changed);
        cx.notify();
        Ok(())
    }

    /// Called when the file may have changed on disk (an agent edited it, a
    /// git checkout, another editor).
    pub fn on_disk_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only || !self.document.changed_on_disk() {
            return;
        }
        if !self.document.path().exists() {
            return;
        }
        if self.dirty {
            self.conflict = true;
            cx.notify();
        } else {
            self.reload(window, cx);
        }
    }

    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = match self.document.reload() {
            Ok(text) => text.to_string(),
            Err(err) => {
                tracing::warn!("reloading {}: {err:#}", self.document.path().display());
                return;
            }
        };
        let focused = self.input.focus_handle(cx).is_focused(window);
        self.input.update(cx, |input, cx| {
            let cursor = input.cursor_position();
            input.set_value(text, window, cx);
            // Restoring the cursor focuses the editor, so only do it when the
            // user is already here.
            if focused {
                input.set_cursor_position(cursor, window, cx);
            }
        });
        self.dirty = false;
        self.conflict = false;
        self.sync_lsp(cx);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn go_to(&mut self, line: u32, column: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_cursor_position(Position::new(line, column), window, cx);
        });
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    // --- language server ---------------------------------------------------

    pub fn attach_lsp(&mut self, client: Arc<LspClient>, cx: &mut Context<Self>) {
        if self.lsp.is_some() {
            return;
        }
        let path = self.document.path().to_path_buf();
        let text = self.input.read(cx).text().clone();
        let doc = match LspDocument::open(client, path, self.document.language(), &text, self.diagnostics.clone()) {
            Ok(doc) => doc,
            Err(err) => {
                tracing::warn!("didOpen {}: {err:#}", self.document.path().display());
                return;
            }
        };
        self.input.update(cx, |input, _| lsp::install_providers(&doc, input));
        self.lsp = Some(doc);
        self.install_debug_hover(cx);
    }

    /// Disconnects from the language server (it is restarting).
    pub fn detach_lsp(&mut self, cx: &mut Context<Self>) {
        self.detach();
        self.input.update(cx, |input, _| lsp::remove_providers(input));
        self.install_debug_hover(cx);
    }

    // --- debugger ----------------------------------------------------------

    /// Shows values from `debugger` on hover while it is paused.
    pub fn set_debugger(&mut self, debugger: WeakEntity<Debugger>, cx: &mut Context<Self>) {
        self.debugger = Some(debugger);
        self.install_debug_hover(cx);
    }

    /// Wraps the current hover provider (the language server's, if any) in
    /// one that shows debugger values first.
    fn install_debug_hover(&mut self, cx: &mut Context<Self>) {
        let Some(debugger) = self.debugger.clone() else { return };
        self.input.update(cx, |input, _| {
            let inner = input.lsp.hover_provider.take();
            input.lsp.hover_provider = Some(Rc::new(DebugHover { debugger, inner }) as Rc<dyn HoverProvider>);
        });
    }

    pub fn set_decorations(&mut self, decorations: EditorDecorations, runnables: Vec<Runnable>, cx: &mut Context<Self>) {
        self.runnables = runnables;
        self.input.update(cx, |input, cx| input.set_decorations(decorations, cx));
    }

    pub fn runnables(&self) -> &[Runnable] {
        &self.runnables
    }

    /// The buffer's text (cheap: ropes share structure).
    pub fn rope(&self, cx: &App) -> Rope {
        self.input.read(cx).text().clone()
    }

    fn sync_lsp(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.lsp else { return };
        doc.sync(self.input.read(cx).text());
    }

    /// The cursor as a [`Target`], where a jump returns to.
    fn cursor_target(&self, cx: &App) -> Target {
        let cursor = self.cursor(cx);
        Target {
            path: self.document.path().to_path_buf(),
            line: cursor.line,
            column: cursor.character,
        }
    }

    /// The identifier at the cursor, for titles.
    fn symbol_at_cursor(&self, cx: &App) -> String {
        let input = self.input.read(cx);
        input.text().word_at(input.cursor())
    }

    /// F12 and the context menu's Go to Definition.
    pub fn go_to_definition(&mut self, cx: &mut Context<Self>) {
        self.request_locations(LocationKind::Definition, cx);
    }

    /// ⇧F12.
    pub fn find_references(&mut self, cx: &mut Context<Self>) {
        self.request_locations(LocationKind::References, cx);
    }

    fn request_locations(&mut self, kind: LocationKind, cx: &mut Context<Self>) {
        let Some(doc) = self.lsp.clone() else {
            let message = match self.document.language() {
                "text" => "No language server for this file.".to_string(),
                language => format!("No language server is running for {language}."),
            };
            cx.emit(EditorEvent::Notify(message));
            return;
        };
        if kind == LocationKind::References && !lsp::supports_references(&doc) {
            cx.emit(EditorEvent::Notify(format!("{} cannot find references.", doc.client.name())));
            return;
        }
        doc.sync(self.input.read(cx).text());
        let origin = self.cursor_target(cx);
        let symbol = self.symbol_at_cursor(cx);
        let position = lsp_types::Position::new(origin.line, origin.column);
        let request: std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Target>>>>> = match kind {
            LocationKind::Definition => Box::pin(doc.definitions(position)),
            LocationKind::References => Box::pin(doc.references(position)),
        };
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |_, cx| match result {
                Ok(targets) => cx.emit(EditorEvent::Locations { kind, symbol, origin, targets }),
                Err(err) => cx.emit(EditorEvent::Notify(format!("{}: {err:#}", doc.client.name()))),
            })
            .ok();
        })
        .detach();
    }

    /// ⌘-click on a symbol the editor underlined: jump to its definition.
    /// The editor would only move within this buffer, so the click is taken
    /// here before it reaches the editor.
    fn on_mouse_down_capture(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left || !event.modifiers.secondary() {
            return;
        }
        let Some(doc) = &self.lsp else { return };
        let targets = lsp::link_targets(&doc.hovered_definitions());
        if targets.is_empty() {
            return;
        }
        cx.stop_propagation();
        doc.clear_hovered_definitions();
        let origin = self.cursor_target(cx);
        let symbol = self.symbol_at_cursor(cx);
        cx.emit(EditorEvent::Locations {
            kind: LocationKind::Definition,
            symbol,
            origin,
            targets,
        });
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, _: &mut Context<Self>) {
        // Without ⌘ the editor drops its underline; so does the ⌘-click target.
        if !event.modifiers.secondary() {
            if let Some(doc) = &self.lsp {
                doc.clear_hovered_definitions();
            }
        }
    }

    pub fn set_diagnostics(&mut self, diagnostics: &[lsp_types::Diagnostic], cx: &mut Context<Self>) {
        *self.diagnostics.borrow_mut() = diagnostics.to_vec();
        let diagnostics = diagnostics.to_vec();
        self.input.update(cx, |input, cx| {
            if let Some(set) = input.diagnostics_mut() {
                set.clear();
                set.extend(diagnostics);
            }
            cx.notify();
        });
    }

    pub fn detach(&mut self) {
        if let Some(doc) = self.lsp.take() {
            doc.close();
        }
    }

    fn render_conflict(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .font_family(cx.theme().font_family.clone())
            .text_size(crate::theme::TEXT_BODY)
            .bg(cx.theme().warning.opacity(0.1))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().flex_1().child("This file changed on disk while you had unsaved edits."))
            .child(
                Button::new("reload-from-disk")
                    .label("Use disk version")
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
            .child(
                Button::new("keep-mine")
                    .label("Keep mine")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.conflict = false;
                        cx.notify();
                    })),
            )
    }
}

impl Drop for EditorView {
    fn drop(&mut self) {
        self.detach();
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_size = px(crate::state::AppState::global(cx).settings.editor_font_size);
        v_flex()
            .key_context(CODE_EDITOR_CONTEXT)
            .size_full()
            .capture_any_mouse_down(cx.listener(Self::on_mouse_down_capture))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_action(cx.listener(|this, _: &actions::GoToDefinition, _, cx| this.go_to_definition(cx)))
            .on_action(cx.listener(|this, _: &actions::FindReferences, _, cx| this.find_references(cx)))
            // The editor's context menu sends its own Go to Definition, which
            // only works within the buffer; answer it here instead.
            .capture_action(cx.listener(|this, _: &input::GoToDefinition, _, cx| {
                cx.stop_propagation();
                this.go_to_definition(cx);
            }))
            .font_family(crate::theme::tokens(cx).mono.clone())
            .text_size(font_size)
            .line_height(font_size * 1.6)
            .when(self.conflict, |el| el.child(self.render_conflict(cx)))
            .child(
                div().flex_1().min_h(px(0.)).child(
                    Input::new(&self.input)
                        .h_full()
                        .appearance(false)
                        .bordered(false)
                        .focus_bordered(false)
                        .disabled(self.read_only),
                ),
            )
    }
}

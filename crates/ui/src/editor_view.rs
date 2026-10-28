//! A code editor tab: gpui-component's rope-based editor bound to a file.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use cockpit_editor::{lsp::LspClient, Document};
use gpui::{
    div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, Position},
    v_flex, ActiveTheme as _, Sizable as _,
};

use crate::lsp::{self, LspManager};

pub enum EditorEvent {
    /// Title or dirty state changed.
    Changed,
    Saved(PathBuf),
}

pub struct EditorView {
    document: Document,
    input: Entity<InputState>,
    dirty: bool,
    /// The file changed on disk while it had unsaved edits.
    conflict: bool,
    read_only: bool,
    lsp: Option<Arc<LspClient>>,
    lsp_version: i32,
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
            let language = view.document.language();
            if language != "text" {
                let this = cx.entity().downgrade();
                lsp.update(cx, |lsp, cx| lsp.attach(language, this, cx));
            }
        }
        Ok(view)
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
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .line_number(true)
                .searchable(true)
                .soft_wrap(false)
                .default_value(text)
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _window, cx| {
            if let InputEvent::Change = event {
                if !this.read_only {
                    if !this.dirty {
                        this.dirty = true;
                        cx.emit(EditorEvent::Changed);
                    }
                    this.sync_lsp(cx);
                }
            }
        });
        Self {
            document,
            input,
            dirty: false,
            conflict: false,
            read_only,
            lsp: None,
            lsp_version: 0,
            _subscriptions: vec![subscription],
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
        if let Some(client) = &self.lsp {
            let _ = client.did_save(self.document.path(), &text);
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
        let path = self.document.path().to_path_buf();
        self.lsp_version = 1;
        let text = self.text(cx);
        if let Err(err) = client.did_open(&path, lsp::language_id(self.document.language()), 1, &text) {
            tracing::warn!("didOpen {}: {err:#}", path.display());
            return;
        }
        let (completion, hover) = lsp::providers(&client, &path);
        self.input.update(cx, |input, _| {
            input.lsp.completion_provider = Some(completion);
            input.lsp.hover_provider = hover;
        });
        self.lsp = Some(client);
    }

    fn sync_lsp(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.lsp else { return };
        self.lsp_version += 1;
        let text = self.input.read(cx).text().to_string();
        let _ = client.did_change(self.document.path(), self.lsp_version, &text);
    }

    pub fn set_diagnostics(&mut self, diagnostics: &[lsp_types::Diagnostic], cx: &mut Context<Self>) {
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
        if let Some(client) = self.lsp.take() {
            let _ = client.did_close(self.document.path());
        }
    }

    fn render_conflict(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .text_sm()
            .bg(cx.theme().warning.opacity(0.15))
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
        v_flex()
            .size_full()
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

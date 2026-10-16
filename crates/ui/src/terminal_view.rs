//! The terminal panel: tabs of PTY-backed shells rendered from vt100
//! snapshots.

use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

use cockpit_terminal::{
    keys::Key, urls::detect_local_urls, Snapshot, Terminal, TerminalEvent, TerminalOptions,
};
use gpui::{
    canvas, div, prelude::*, px, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, KeyDownEvent, Pixels, ScrollDelta, ScrollWheelEvent,
    SharedString, StyledText, Subscription, Task, TextRun, UnderlineStyle, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, ActiveTheme as _, IconName, Sizable as _,
};

use crate::{actions, assets::icon, state::AppState, theme};

const PADDING: f32 = 8.0;

pub enum TerminalViewEvent {
    /// Open a URL (a dev server the shell printed) in the browser.
    OpenUrl(String),
}

pub struct TerminalView {
    terminal: Option<Arc<Terminal>>,
    error: Option<SharedString>,
    title: SharedString,
    snapshot: Snapshot,
    urls: Vec<String>,
    exited: Option<Option<u32>>,
    focus_handle: FocusHandle,
    /// The (rows, cols) last sent to the PTY.
    size: Rc<Cell<(u16, u16)>>,
    _reader: Option<Task<()>>,
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    pub fn new(cwd: PathBuf, program: Option<(String, Vec<String>)>, cx: &mut Context<Self>) -> Self {
        let mut options = TerminalOptions::shell(cwd);
        options.program = program;
        let mut view = Self {
            terminal: None,
            error: None,
            title: "Terminal".into(),
            snapshot: Snapshot::default(),
            urls: Vec::new(),
            exited: None,
            focus_handle: cx.focus_handle(),
            size: Rc::new(Cell::new((options.rows, options.cols))),
            _reader: None,
        };
        match Terminal::spawn(options) {
            Ok((terminal, events)) => {
                let terminal = Arc::new(terminal);
                view.snapshot = terminal.snapshot();
                view._reader = Some(cx.spawn({
                    let terminal = terminal.clone();
                    async move |this, cx| {
                        while let Ok(event) = events.recv().await {
                            let alive = this.update(cx, |this, cx| this.on_event(&terminal, event, cx));
                            if alive.is_err() {
                                break;
                            }
                        }
                    }
                }));
                view.terminal = Some(terminal);
            }
            Err(err) => view.error = Some(format!("Could not start a shell: {err:#}").into()),
        }
        view
    }

    fn on_event(&mut self, terminal: &Terminal, event: TerminalEvent, cx: &mut Context<Self>) {
        match event {
            TerminalEvent::Updated => {
                self.snapshot = terminal.snapshot();
                let title = terminal.title();
                if !title.is_empty() {
                    self.title = title.into();
                }
                let text: String = self
                    .snapshot
                    .lines
                    .iter()
                    .map(|line| line.text() + "\n")
                    .collect();
                for url in detect_local_urls(&text) {
                    if !self.urls.contains(&url) {
                        self.urls.push(url);
                    }
                }
                if self.urls.len() > 3 {
                    self.urls.drain(..self.urls.len() - 3);
                }
            }
            TerminalEvent::Exited(code) => self.exited = Some(code),
            TerminalEvent::Bell => {}
        }
        cx.notify();
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub fn has_exited(&self) -> bool {
        self.exited.is_some()
    }

    /// Runs `command` in this terminal as if typed.
    pub fn run_command(&self, command: &str) {
        if let Some(terminal) = &self.terminal {
            terminal.write(format!("{command}\r").as_bytes());
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = &self.terminal else { return };
        let keystroke = &event.keystroke;
        let key = Key {
            key: &keystroke.key,
            text: keystroke.key_char.as_deref(),
            ctrl: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            shift: keystroke.modifiers.shift,
            platform: keystroke.modifiers.platform,
        };
        if terminal.send_key(&key) {
            cx.stop_propagation();
        }
    }

    fn send_tab(&mut self, backwards: bool) {
        if let Some(terminal) = &self.terminal {
            terminal.write(if backwards { b"\x1b[Z" } else { b"\t" });
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let (Some(terminal), Some(item)) = (&self.terminal, cx.read_from_clipboard()) else {
            return;
        };
        if let Some(text) = item.text() {
            terminal.paste(&text);
        }
    }

    fn copy_screen(&mut self, cx: &mut Context<Self>) {
        let text = self
            .snapshot
            .lines
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text.trim_end().to_string()));
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = &self.terminal else { return };
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y.round() as i32,
            ScrollDelta::Pixels(delta) => (f32::from(delta.y) / 18.0).round() as i32,
        };
        if lines != 0 {
            terminal.scroll(lines);
            self.snapshot = terminal.snapshot();
            cx.notify();
        }
    }

    fn render_lines(&self, font: &Font, font_size: Pixels, line_height: Pixels, cx: &App) -> Vec<gpui::AnyElement> {
        let background = cx.theme().background;
        self.snapshot
            .lines
            .iter()
            .map(|line| {
                let mut text = String::new();
                let mut runs = Vec::with_capacity(line.runs.len());
                for run in &line.runs {
                    let mut fg = theme::terminal_color(run.style.fg, true, cx).unwrap_or(cx.theme().foreground);
                    let mut bg = theme::terminal_color(run.style.bg, false, cx);
                    if run.style.inverse {
                        let new_fg = bg.unwrap_or(background);
                        bg = Some(fg);
                        fg = new_fg;
                    }
                    if run.style.dim {
                        fg = fg.opacity(0.6);
                    }
                    text.push_str(&run.text);
                    runs.push(TextRun {
                        len: run.text.len(),
                        font: Font {
                            weight: if run.style.bold { FontWeight::BOLD } else { FontWeight::NORMAL },
                            style: if run.style.italic { FontStyle::Italic } else { FontStyle::Normal },
                            ..font.clone()
                        },
                        color: fg,
                        background_color: bg,
                        underline: run.style.underline.then(|| UnderlineStyle {
                            thickness: px(1.),
                            color: None,
                            wavy: false,
                        }),
                        strikethrough: None,
                    });
                }
                if text.is_empty() {
                    text.push(' ');
                    runs.push(TextRun {
                        len: 1,
                        font: font.clone(),
                        color: cx.theme().foreground,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                }
                div()
                    .h(line_height)
                    .text_size(font_size)
                    .line_height(line_height)
                    .whitespace_nowrap()
                    .child(StyledText::new(text).with_runs(runs))
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(error) = &self.error {
            return div()
                .size_full()
                .p_4()
                .text_color(cx.theme().danger)
                .child(error.clone())
                .into_any_element();
        }

        let font_size = px(AppState::global(cx).settings.terminal_font_size);
        let font = gpui::font(cx.theme().mono_font_family.clone());
        let font_id = window.text_system().resolve_font(&font);
        let cell_width = window
            .text_system()
            .advance(font_id, font_size, 'm')
            .map(|size| size.width)
            .unwrap_or(font_size * 0.6);
        let line_height = (font_size * 1.35).round();

        let terminal = self.terminal.clone();
        let size = self.size.clone();
        let measure = canvas(
            move |bounds, _, _| {
                let cols = ((f32::from(bounds.size.width) - PADDING * 2.0) / f32::from(cell_width)).floor().max(2.0) as u16;
                let rows = ((f32::from(bounds.size.height) - PADDING * 2.0) / f32::from(line_height)).floor().max(1.0) as u16;
                if size.get() != (rows, cols) {
                    size.set((rows, cols));
                    if let Some(terminal) = &terminal {
                        terminal.resize(rows, cols);
                    }
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let focused = self.focus_handle.is_focused(window);
        let cursor = self.snapshot.cursor.filter(|_| self.exited.is_none()).map(|(row, col)| {
            div()
                .absolute()
                .left(px(PADDING) + cell_width * col as f32)
                .top(px(PADDING) + line_height * row as f32)
                .w(cell_width)
                .h(line_height)
                .when(focused, |el| el.bg(cx.theme().caret.opacity(0.7)))
                .when(!focused, |el| el.border_1().border_color(cx.theme().caret))
        });

        let lines = self.render_lines(&font, font_size, line_height, cx);

        v_flex()
            .size_full()
            .child(
                div()
                    .id("terminal")
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .bg(cx.theme().background)
                    .font_family(font.family.clone())
                    .track_focus(&self.focus_handle)
                    .key_context(actions::TERMINAL_CONTEXT)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_scroll_wheel(cx.listener(Self::on_scroll))
                    .on_action(cx.listener(|this, _: &actions::TerminalPaste, _, cx| this.paste(cx)))
                    .on_action(cx.listener(|this, _: &actions::TerminalCopyScreen, _, cx| this.copy_screen(cx)))
                    .on_action(cx.listener(|this, _: &actions::TerminalTab, _, _| this.send_tab(false)))
                    .on_action(cx.listener(|this, _: &actions::TerminalBackTab, _, _| this.send_tab(true)))
                    .on_action(cx.listener(|this, _: &actions::TerminalClear, _, _| {
                        // Ctrl-L asks the shell to clear and redraw its prompt.
                        if let Some(terminal) = &this.terminal {
                            terminal.write(b"\x0c");
                        }
                    }))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, _| window.focus(&this.focus_handle)),
                    )
                    .child(measure)
                    .child(v_flex().p(px(PADDING)).children(lines))
                    .children(cursor),
            )
            .when(!self.urls.is_empty() || self.exited.is_some(), |el| {
                el.child(
                    h_flex()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .when_some(self.exited, |el, code| {
                            el.child(SharedString::from(match code {
                                Some(code) => format!("Process exited with code {code}"),
                                None => "Process exited".to_string(),
                            }))
                        })
                        .children(self.urls.iter().enumerate().map(|(ix, url)| {
                            let url = url.clone();
                            Button::new(("open-url", ix))
                                .label(SharedString::from(format!("Open {url}")))
                                .icon(IconName::Globe)
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(TerminalViewEvent::OpenUrl(url.clone()));
                                }))
                        })),
                )
            })
            .into_any_element()
    }
}

pub enum TerminalPanelEvent {
    OpenUrl(String),
}

/// Tabs of terminals for one workspace.
pub struct TerminalPanel {
    cwd: PathBuf,
    terminals: Vec<Entity<TerminalView>>,
    active: usize,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.terminals.get(self.active) {
            Some(terminal) => terminal.focus_handle(cx),
            None => cx.focus_handle(),
        }
    }
}

impl TerminalPanel {
    pub fn new(cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut panel = Self {
            cwd,
            terminals: Vec::new(),
            active: 0,
            _subscriptions: Vec::new(),
        };
        panel.add_terminal(None, cx);
        panel
    }

    pub fn add_terminal(&mut self, program: Option<(String, Vec<String>)>, cx: &mut Context<Self>) -> Entity<TerminalView> {
        let cwd = self.cwd.clone();
        let terminal = cx.new(|cx| TerminalView::new(cwd, program, cx));
        self._subscriptions.push(cx.subscribe(&terminal, |_, _, event: &TerminalViewEvent, cx| match event {
            TerminalViewEvent::OpenUrl(url) => cx.emit(TerminalPanelEvent::OpenUrl(url.clone())),
        }));
        self._subscriptions.push(cx.observe(&terminal, |_, _, cx| cx.notify()));
        self.terminals.push(terminal.clone());
        self.active = self.terminals.len() - 1;
        cx.notify();
        terminal
    }

    pub fn active_terminal(&self) -> Option<&Entity<TerminalView>> {
        self.terminals.get(self.active)
    }

    fn close(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.terminals.len() {
            self.terminals.remove(ix);
            if self.active >= self.terminals.len() {
                self.active = self.terminals.len().saturating_sub(1);
            }
            cx.notify();
        }
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminals.get(self.active) {
            window.focus(&terminal.focus_handle(cx));
        }
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self.terminals.iter().enumerate().map(|(ix, terminal)| {
            let active = ix == self.active;
            let title = terminal.read(cx).title();
            h_flex()
                .id(("terminal-tab", ix))
                .gap_1()
                .px_2()
                .h_full()
                .text_xs()
                .cursor_pointer()
                .when(active, |el| el.bg(cx.theme().tab_active).text_color(cx.theme().tab_active_foreground))
                .when(!active, |el| el.text_color(cx.theme().muted_foreground))
                .child(icon("terminal").xsmall())
                .child(div().max_w(px(160.)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
                .child(
                    Button::new(("close-terminal", ix))
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(move |this, _, _, cx| this.close(ix, cx))),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.active = ix;
                    this.focus_active(window, cx);
                    cx.notify();
                }))
        });

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(30.))
                    .flex_none()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().tab_bar)
                    .child(div().px_3().text_xs().text_color(cx.theme().muted_foreground).child("TERMINAL"))
                    .children(tabs)
                    .child(div().flex_1())
                    .child(
                        Button::new("new-terminal")
                            .icon(icon("plus"))
                            .ghost()
                            .xsmall()
                            .tooltip("New terminal")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_terminal(None, cx);
                                this.focus_active(window, cx);
                            })),
                    ),
            )
            .child(match self.terminals.get(self.active) {
                Some(terminal) => div().flex_1().min_h(px(0.)).child(terminal.clone()).into_any_element(),
                None => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No terminals. Press ctrl-` to open one.")
                    .into_any_element(),
            })
    }
}

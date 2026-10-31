//! The debug panel, laid out like RustRover's Debug tool window: a toolbar,
//! frames beside variables (with watches and an expression field on top),
//! the program's console, and every breakpoint.

use std::{ops::Range, path::PathBuf};

use cockpit_debugger::{protocol::Variable, values::pretty_type};
use gpui::{
    div, prelude::*, px, uniform_list, Action, AnyElement, App, Context, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Hsla, ScrollStrategy, SharedString, Subscription, UniformListScrollHandle, Window,
};
use gpui_component::{
    checkbox::Checkbox,
    h_flex,
    input::{Escape, Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel, ResizableState},
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Icon, IconName, Sizable as _,
};

use super::{phase_label, ConsoleKind, Debugger, Phase};
use crate::{
    actions::{self, DEBUG_PANEL_CONTEXT},
    assets::icon,
    chrome,
    theme::{tokens, RADIUS, TEXT_BODY, TEXT_MONO, TEXT_SMALL},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelTab {
    Debugger,
    Console,
    Breakpoints,
}

pub enum DebugPanelEvent {
    /// Open a file at a zero-based line.
    Open { path: PathBuf, line: u32 },
}

/// A value being edited in place (Set Value).
struct Editing {
    key: String,
    parent: i64,
    name: String,
    input: Entity<InputState>,
}

pub struct DebugPanel {
    debugger: Entity<Debugger>,
    tab: PanelTab,
    eval_input: Entity<InputState>,
    console_input: Entity<InputState>,
    editing: Option<Editing>,
    console_scroll: UniformListScrollHandle,
    console_len: usize,
    sizes: Entity<ResizableState>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DebugPanelEvent> for DebugPanel {}

impl Focusable for DebugPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// One row of the variables tree.
struct Row {
    key: String,
    depth: usize,
    name: SharedString,
    value: SharedString,
    type_name: Option<SharedString>,
    reference: i64,
    /// The container this value is a child of, for Set Value.
    parent: Option<i64>,
    kind: RowKind,
}

#[derive(Clone, PartialEq)]
enum RowKind {
    Scope,
    Variable,
    Watch(String),
    WatchError(String),
    Note,
}

impl DebugPanel {
    pub fn new(debugger: Entity<Debugger>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let eval_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Evaluate expression (↩) or add a watch (⌘↩)")
        });
        let console_input = cx.new(|cx| InputState::new(window, cx).placeholder("Evaluate an expression"));
        let subscriptions = vec![
            cx.observe(&debugger, |this, debugger, cx| {
                let len = debugger.read(cx).session().map(|s| s.console.len()).unwrap_or(0);
                if len != this.console_len {
                    this.console_len = len;
                    if len > 0 {
                        this.console_scroll.scroll_to_item(len - 1, ScrollStrategy::Bottom);
                    }
                }
                cx.notify();
            }),
            cx.subscribe_in(&eval_input, window, |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { secondary } = event {
                    let text = input.read(cx).value().to_string();
                    if text.trim().is_empty() {
                        return;
                    }
                    if *secondary {
                        this.debugger.update(cx, |d, cx| d.add_watch(&text, cx));
                    } else {
                        // Evaluate once: show it in the console.
                        this.debugger.update(cx, |d, cx| d.evaluate_in_console(&text, cx));
                        this.tab = PanelTab::Console;
                    }
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                }
            }),
            cx.subscribe_in(&console_input, window, |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = input.read(cx).value().to_string();
                    this.debugger.update(cx, |d, cx| d.evaluate_in_console(&text, cx));
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                }
            }),
        ];
        Self {
            debugger,
            tab: PanelTab::Debugger,
            eval_input,
            console_input,
            editing: None,
            console_scroll: UniformListScrollHandle::new(),
            console_len: 0,
            sizes: cx.new(|_| ResizableState::default()),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    pub fn show_tab(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    /// ⌥F8: the expression field, with `text` (the selection) in it.
    pub fn focus_evaluate(&mut self, text: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = PanelTab::Debugger;
        self.eval_input.update(cx, |input, cx| {
            if let Some(text) = text {
                input.set_value(text, window, cx);
            }
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn start_editing(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = row.parent else { return };
        let value = row.value.to_string();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
        self._subscriptions.push(cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| {
            match event {
                InputEvent::PressEnter { .. } => {
                    if let Some(editing) = this.editing.take() {
                        let value = input.read(cx).value().to_string();
                        this.debugger.update(cx, |d, cx| d.set_value(editing.parent, &editing.name, &value, cx));
                    }
                    cx.notify();
                }
                InputEvent::Blur => {
                    this.editing = None;
                    cx.notify();
                }
                _ => {}
            }
        }));
        input.update(cx, |input, cx| input.focus(window, cx));
        self.editing = Some(Editing { key: row.key.clone(), parent, name: row.name.to_string(), input });
        cx.notify();
    }

    // --- rows --------------------------------------------------------------

    fn variable_rows(&self, cx: &App) -> Vec<Row> {
        let debugger = self.debugger.read(cx);
        let mut rows = Vec::new();
        let Some(session) = debugger.session() else { return rows };
        let paused = session.is_paused();

        for watch in &debugger.watches {
            let key = format!("watch:{watch}");
            match session.watch_values.get(watch) {
                Some(Ok(value)) => {
                    rows.push(Row {
                        key: key.clone(),
                        depth: 0,
                        name: watch.clone().into(),
                        value: value.result.clone().into(),
                        type_name: value.type_name.as_deref().map(|t| pretty_type(t).into()),
                        reference: value.variables_reference,
                        parent: None,
                        kind: RowKind::Watch(watch.clone()),
                    });
                    if value.variables_reference > 0 && session.expanded.contains(&key) {
                        push_children(&mut rows, session, &key, value.variables_reference, 1);
                    }
                }
                Some(Err(err)) => rows.push(Row {
                    key,
                    depth: 0,
                    name: watch.clone().into(),
                    value: err.clone().into(),
                    type_name: None,
                    reference: 0,
                    parent: None,
                    kind: RowKind::WatchError(watch.clone()),
                }),
                None => rows.push(Row {
                    key,
                    depth: 0,
                    name: watch.clone().into(),
                    value: if paused { "…".into() } else { "not available".into() },
                    type_name: None,
                    reference: 0,
                    parent: None,
                    kind: RowKind::WatchError(watch.clone()),
                }),
            }
        }

        for scope in &session.scopes {
            if scope.variables_reference == 0 || (scope.name == "Globals" && scope.named_variables == Some(0)) {
                continue;
            }
            let key = scope.name.clone();
            rows.push(Row {
                key: key.clone(),
                depth: 0,
                name: scope.name.clone().into(),
                value: "".into(),
                type_name: None,
                reference: scope.variables_reference,
                parent: None,
                kind: RowKind::Scope,
            });
            if session.expanded.contains(&key) {
                if session.children.get(&scope.variables_reference).is_some_and(|c| c.is_empty()) {
                    rows.push(note(1, "No variables"));
                }
                push_children(&mut rows, session, &key, scope.variables_reference, 1);
            }
        }
        rows
    }

    // --- rendering ---------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let debugger = self.debugger.read(cx);
        let phase = debugger.session().map(|s| s.phase.clone());
        let live = debugger.is_live();
        let paused = debugger.is_paused();
        let running = phase == Some(Phase::Running);
        let t = tokens(cx).clone();

        let button = |id: &'static str, icon: Icon, enabled: bool, color: Option<Hsla>, tip: &'static str, action: Box<dyn Action>| {
            let (hover, muted) = (t.hover, t.fg_subtle);
            div()
                .id(id)
                .flex_none()
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .text_color(if enabled { color.unwrap_or(t.fg_muted) } else { muted.alpha(0.5) })
                .when(enabled, |el| el.cursor_pointer().hover(move |s| s.bg(hover)))
                .tooltip(chrome::tooltip(tip))
                .child(icon.size(px(14.)))
                .when(enabled, move |el| {
                    el.on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
                })
        };
        let separator = || div().flex_none().w(px(1.)).h(px(16.)).mx(px(4.)).bg(t.border);

        h_flex()
            .gap(px(1.))
            .child(button("dbg-rerun", icon("rotate-cw"), debugger.selected_config().is_some() || debugger.session().is_some(), Some(t.success), "Rerun (⌘F5)", Box::new(actions::RestartDebugging)))
            .child(button("dbg-resume", icon("play"), paused, Some(t.success), "Resume (⌥⌘R)", Box::new(actions::Resume)))
            .child(button("dbg-pause", icon("pause"), running, None, "Pause", Box::new(actions::Pause)))
            .child(button("dbg-stop", icon("square"), live, Some(t.danger), "Stop (⌘F2)", Box::new(actions::StopRun)))
            .child(separator())
            .child(button("dbg-over", icon("redo-dot"), paused, Some(t.accent_text), "Step Over (F8)", Box::new(actions::StepOver)))
            .child(button("dbg-into", icon("arrow-down-to-dot"), paused, Some(t.accent_text), "Step Into (F7)", Box::new(actions::StepInto)))
            .child(button("dbg-out", icon("arrow-up-from-dot"), paused, Some(t.accent_text), "Step Out (⇧F8)", Box::new(actions::StepOut)))
            .child(button("dbg-cursor", icon("arrow-right-to-line"), paused, Some(t.accent_text), "Run to Cursor (⌥F9)", Box::new(actions::RunToCursor)))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let debugger = self.debugger.read(cx);
        let breakpoint_count = debugger.breakpoints().len();
        let status = debugger.session().map(|s| {
            let color = match s.phase {
                Phase::Paused => cx.theme().info,
                Phase::Running | Phase::Starting | Phase::Building => t.success,
                Phase::Ended { exit_code: Some(0) } => t.fg_subtle,
                Phase::Ended { .. } => t.fg_subtle,
            };
            let text = match (&s.phase, &s.stop_description) {
                (Phase::Paused, Some(description)) => format!("{} · {description}", s.config.label()),
                (phase, _) => format!("{} · {}", s.config.label(), phase_label(phase)),
            };
            (SharedString::from(text), color)
        });
        let tab = |id: &'static str, label: SharedString, which: PanelTab, cx: &mut Context<Self>| {
            chrome::underline_tab(id, self.tab == which, cx)
                .text_size(TEXT_BODY)
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.show_tab(which, cx)))
        };
        h_flex()
            .h(px(34.))
            .flex_none()
            .px(px(6.))
            .gap(px(8.))
            .border_b_1()
            .border_color(t.border)
            .child(tab("dbg-tab-debugger", "Debugger".into(), PanelTab::Debugger, cx))
            .child(tab("dbg-tab-console", "Console".into(), PanelTab::Console, cx))
            .child(tab(
                "dbg-tab-breakpoints",
                if breakpoint_count > 0 { format!("Breakpoints ({breakpoint_count})").into() } else { "Breakpoints".into() },
                PanelTab::Breakpoints,
                cx,
            ))
            .child(div().w(px(1.)).h(px(16.)).bg(t.border))
            .child(self.render_toolbar(cx))
            .child(div().flex_1())
            .when_some(status, |el, (text, color)| {
                el.child(
                    h_flex()
                        .min_w_0()
                        .gap(px(6.))
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_muted)
                        .child(chrome::status_dot(color))
                        .child(div().truncate().child(text)),
                )
            })
    }

    fn render_empty(&self, message: impl Into<SharedString>, cx: &App) -> AnyElement {
        let t = tokens(cx);
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .px(px(16.))
            .text_size(TEXT_BODY)
            .text_color(t.fg_muted)
            .child(message.into())
            .into_any_element()
    }

    fn render_frames(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let debugger = self.debugger.read(cx);
        let Some(session) = debugger.session() else {
            return self.render_empty("No frames", cx);
        };
        if !session.is_paused() || session.frames.is_empty() {
            let message = match session.phase {
                Phase::Running => "Running…",
                Phase::Building => "Building…",
                Phase::Starting => "Starting…",
                Phase::Paused => "Loading frames…",
                Phase::Ended { .. } => "Not running",
            };
            return self.render_empty(message, cx);
        }
        let root = debugger_root(debugger);
        let threads = session.threads.clone();
        let thread_id = session.thread_id;
        let selected = session.selected_frame;
        let frames = session.frames.clone();
        let paused_color = cx.theme().info;

        let thread_bar = (threads.len() > 1).then(|| {
            h_flex()
                .id("dbg-threads")
                .flex_none()
                .gap(px(4.))
                .px(px(8.))
                .py(px(4.))
                .overflow_x_scroll()
                .border_b_1()
                .border_color(t.border)
                .children(threads.iter().map(|thread| {
                    let id = thread.id;
                    let active = Some(id) == thread_id;
                    let name = if thread.name.is_empty() { format!("Thread {id}") } else { thread.name.clone() };
                    div()
                        .id(("dbg-thread", id as u64))
                        .flex_none()
                        .px(px(7.))
                        .py(px(1.))
                        .rounded(RADIUS)
                        .text_size(TEXT_SMALL)
                        .cursor_pointer()
                        .when(active, |el| el.bg(t.accent_soft).text_color(t.accent_text))
                        .when(!active, |el| el.text_color(t.fg_muted).hover(|s| s.bg(t.hover)))
                        .child(name)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.debugger.update(cx, |d, cx| d.select_thread(id, cx));
                        }))
                }))
        });

        v_flex()
            .size_full()
            .children(thread_bar)
            .child(
                v_flex()
                    .id("dbg-frames")
                    .flex_1()
                    .min_h_0()
                    .p(px(4.))
                    .overflow_y_scrollbar()
                    .children(frames.into_iter().enumerate().map(|(ix, frame)| {
                        let path = frame.path().map(PathBuf::from);
                        let own = path.as_ref().is_some_and(|p| p.starts_with(&root));
                        let location = path
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| format!("{}:{}", n.to_string_lossy(), frame.line))
                            .unwrap_or_else(|| "no source".into());
                        let name = short_function(&frame.name);
                        let is_selected = ix == selected;
                        chrome::list_row(("dbg-frame", ix), is_selected, cx)
                            .h(px(22.))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(10.))
                                    .when(ix == 0, |el| el.child(chrome::status_dot(paused_color))),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(TEXT_BODY)
                                    .text_color(if own { t.fg } else { t.fg_subtle })
                                    .when(own, |el| el.font_weight(FontWeight::MEDIUM))
                                    .child(name),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(TEXT_SMALL)
                                    .text_color(t.fg_subtle)
                                    .child(location),
                            )
                            .tooltip(chrome::tooltip(frame.name.clone()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.debugger.update(cx, |d, cx| d.select_frame(ix, cx));
                            }))
                    })),
            )
            .into_any_element()
    }

    fn render_variables(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let debugger = self.debugger.read(cx);
        let live = debugger.is_live();
        let paused = debugger.is_paused();
        let has_watches = !debugger.watches.is_empty();
        let rows = self.variable_rows(cx);
        let message = match debugger.session().map(|s| &s.phase) {
            None | Some(Phase::Ended { .. }) => Some("Start debugging (⌃D, or the bug in the title bar) to see variables."),
            Some(Phase::Building) => Some("Building…"),
            Some(Phase::Starting) => Some("Starting the debugger…"),
            Some(Phase::Running) => Some("The program is running. It pauses at a breakpoint, or press Pause."),
            Some(Phase::Paused) if rows.is_empty() => Some("Loading variables…"),
            Some(Phase::Paused) => None,
        };
        let _ = live;

        let body = v_flex()
            .id("dbg-variables")
            .flex_1()
            .min_h_0()
            .py(px(4.))
            .overflow_y_scrollbar()
            .font_family(t.mono.clone())
            .text_size(TEXT_MONO)
            .children(rows.into_iter().enumerate().map(|(ix, row)| self.render_row(ix, row, window, cx)))
            .when_some(message.filter(|_| !paused || !has_watches), |el, message| {
                el.child(
                    div()
                        .px(px(12.))
                        .py(px(10.))
                        .font_family(cx.theme().font_family.clone())
                        .text_size(TEXT_BODY)
                        .text_color(t.fg_muted)
                        .child(message),
                )
            });

        v_flex()
            .size_full()
            .child(
                div()
                    .flex_none()
                    .px(px(6.))
                    .py(px(5.))
                    .border_b_1()
                    .border_color(t.border)
                    .child(Input::new(&self.eval_input).small().cleanable(true)),
            )
            .child(body)
            .into_any_element()
    }

    fn render_row(&self, ix: usize, row: Row, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let expanded = self
            .debugger
            .read(cx)
            .session()
            .is_some_and(|s| s.expanded.contains(&row.key));
        let expandable = row.reference > 0;
        let editing = self.editing.as_ref().filter(|e| e.key == row.key).map(|e| e.input.clone());
        let indent = px(8. + row.depth as f32 * 14.);
        let hover = t.hover;

        let chevron = div().flex_none().w(px(14.)).when(expandable, |el| {
            el.child(
                Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                    .size(px(12.))
                    .text_color(t.fg_subtle),
            )
        });
        let (name_color, value_color) = match &row.kind {
            RowKind::Scope => (t.fg, t.fg),
            RowKind::Note => (t.fg_subtle, t.fg_subtle),
            RowKind::WatchError(_) => (t.accent_text, t.danger),
            _ => (t.accent_text, t.fg),
        };
        let key = row.key.clone();
        let reference = row.reference;
        let watch = match &row.kind {
            RowKind::Watch(w) | RowKind::WatchError(w) => Some(w.clone()),
            _ => None,
        };
        let is_watch = watch.is_some();
        let can_edit = row.parent.is_some();

        let mut line = h_flex()
            .id(("dbg-row", ix))
            .group("dbg-row")
            .w_full()
            .min_h(px(22.))
            .pl(indent)
            .pr(px(6.))
            .gap(px(2.))
            .hover(move |s| s.bg(hover))
            .child(chevron);
        if row.kind == RowKind::Scope {
            line = line.child(
                div()
                    .font_family(cx.theme().font_family.clone())
                    .text_size(TEXT_SMALL)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.fg_muted)
                    .child(row.name.clone()),
            );
        } else {
            if is_watch {
                line = line.child(
                    Icon::new(IconName::Eye).size(px(12.)).text_color(t.fg_subtle).mr(px(4.)),
                );
            }
            line = line
                .child(div().flex_none().text_color(name_color).child(row.name.clone()))
                .when(row.kind != RowKind::Note, |el| el.child(div().flex_none().text_color(t.fg_subtle).child(" = ")));
            line = match editing {
                Some(input) => line.child(div().flex_1().child(Input::new(&input).xsmall())),
                None => line
                    .when_some(row.type_name.clone().filter(|t| !t.is_empty()), |el, ty| {
                        el.child(div().flex_none().text_color(t.fg_subtle).child(format!("{{{ty}}} ")))
                    })
                    .child(div().min_w_0().truncate().text_color(value_color).child(row.value.clone())),
            };
        }
        line = line.child(div().flex_1());
        if let Some(watch) = watch {
            line = line.child(
                div()
                    .id(("dbg-unwatch", ix))
                    .flex_none()
                    .invisible()
                    .group_hover("dbg-row", |s| s.visible())
                    .cursor_pointer()
                    .text_color(t.fg_subtle)
                    .child(Icon::new(IconName::Close).size(px(12.)))
                    .tooltip(chrome::tooltip("Remove watch"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.debugger.update(cx, |d, cx| d.remove_watch(&watch, cx));
                    })),
            );
        }
        let row_for_edit = Row { kind: row.kind.clone(), ..row };
        line.when(can_edit, |el| el.tooltip(chrome::tooltip("Double-click to set the value")))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                if event.click_count() >= 2 && can_edit {
                    this.start_editing(&row_for_edit, window, cx);
                } else if expandable {
                    let key = key.clone();
                    this.debugger.update(cx, |d, cx| d.toggle_expanded(&key, reference, cx));
                }
            }))
            .into_any_element()
    }

    fn render_debugger_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let frames = self.render_frames(cx);
        let variables = self.render_variables(window, cx);
        let t = tokens(cx);
        h_resizable("dbg-columns")
            .with_state(&self.sizes)
            .child(
                resizable_panel()
                    .size(px(320.))
                    .size_range(px(160.)..px(900.))
                    .child(
                        v_flex()
                            .size_full()
                            .child(
                                h_flex()
                                    .h(px(26.))
                                    .flex_none()
                                    .px(px(10.))
                                    .gap(px(6.))
                                    .border_b_1()
                                    .border_color(t.border)
                                    .child(icon("list-tree").size(px(12.)).text_color(t.fg_subtle))
                                    .child(chrome::caps_label("Frames", cx)),
                            )
                            .child(div().flex_1().min_h_0().child(frames)),
                    ),
            )
            .child(resizable_panel().child(variables))
            .into_any_element()
    }

    fn render_console_rows(&mut self, range: Range<usize>, _window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = tokens(cx).clone();
        let info = cx.theme().info;
        let Some(session) = self.debugger.read(cx).session() else { return Vec::new() };
        range
            .filter_map(|ix| session.console.get(ix).map(|line| (ix, line)))
            .map(|(ix, line)| {
                let color = match line.kind {
                    ConsoleKind::Stdout => t.fg,
                    ConsoleKind::Stderr | ConsoleKind::Error => t.danger,
                    ConsoleKind::Build => t.fg_muted,
                    ConsoleKind::Info => t.fg_subtle,
                    ConsoleKind::Input => info,
                    ConsoleKind::Result => t.accent_text,
                };
                div()
                    .id(("dbg-console", ix))
                    .h(px(19.))
                    .px(px(10.))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_color(color)
                    .child(if line.text.is_empty() { " ".to_string() } else { line.text.replace('\t', "    ") })
                    .into_any_element()
            })
            .collect()
    }

    fn render_console_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let count = self.debugger.read(cx).session().map(|s| s.console.len()).unwrap_or(0);
        v_flex()
            .size_full()
            .child(if count == 0 {
                div().flex_1().child(self.render_empty("The program's output appears here.", cx)).into_any_element()
            } else {
                uniform_list("dbg-console-rows", count, cx.processor(Self::render_console_rows))
                    .track_scroll(self.console_scroll.clone())
                    .flex_1()
                    .py(px(4.))
                    .font_family(t.mono.clone())
                    .text_size(TEXT_MONO)
                    .into_any_element()
            })
            .child(
                h_flex()
                    .flex_none()
                    .px(px(6.))
                    .py(px(5.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(t.border)
                    .child(div().flex_none().font_family(t.mono.clone()).text_color(t.fg_subtle).child(">"))
                    .child(div().flex_1().child(Input::new(&self.console_input).small()))
                    .child(
                        div()
                            .id("dbg-clear-console")
                            .flex_none()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(RADIUS)
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(t.hover))
                            .child("Clear")
                            .on_click(cx.listener(|this, _, _, cx| this.debugger.update(cx, |d, cx| d.clear_console(cx)))),
                    ),
            )
            .into_any_element()
    }

    fn render_breakpoints_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let debugger = self.debugger.read(cx);
        let root = debugger_root(debugger);
        let break_on_panic = debugger.break_on_panic;
        let files: Vec<(PathBuf, Vec<cockpit_debugger::LineBreakpoint>)> = debugger
            .breakpoints()
            .files()
            .map(|(p, b)| (p.to_path_buf(), b.to_vec()))
            .collect();
        let any = !files.is_empty();

        let mut list = v_flex()
            .id("dbg-breakpoints")
            .flex_1()
            .min_h_0()
            .p(px(4.))
            .overflow_y_scrollbar()
            .child(
                h_flex()
                    .h(px(26.))
                    .px(px(8.))
                    .gap(px(8.))
                    .child(
                        Checkbox::new("dbg-break-on-panic")
                            .checked(break_on_panic)
                            .label("Pause when the program panics")
                            .small()
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let on = *checked;
                                this.debugger.update(cx, |d, cx| d.set_break_on_panic(on, cx));
                            })),
                    )
                    .child(div().flex_1())
                    .when(any, |el| {
                        el.child(
                            div()
                                .id("dbg-remove-all")
                                .px(px(6.))
                                .py(px(2.))
                                .rounded(RADIUS)
                                .text_size(TEXT_SMALL)
                                .text_color(t.fg_muted)
                                .cursor_pointer()
                                .hover(|s| s.bg(t.hover))
                                .child("Remove all")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.debugger.update(cx, |d, cx| d.remove_all_breakpoints(cx));
                                })),
                        )
                    }),
            );
        if !any {
            list = list.child(
                div()
                    .px(px(12.))
                    .py(px(10.))
                    .text_size(TEXT_BODY)
                    .text_color(t.fg_muted)
                    .child("No breakpoints. Click in the gutter beside a line number, or press ⌘F8, to add one."),
            );
        }
        let mut ix = 0usize;
        for (path, breakpoints) in files {
            let relative = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().into_owned();
            for bp in breakpoints {
                ix += 1;
                let line = bp.line;
                let (open_path, toggle_path, remove_path) = (path.clone(), path.clone(), path.clone());
                let detail = match (&bp.condition, &bp.hit_condition, &bp.log_message) {
                    (_, _, Some(log)) => Some(format!("logs \"{log}\"")),
                    (Some(c), Some(h), _) => Some(format!("when {c}, hit {h}")),
                    (Some(c), None, _) => Some(format!("when {c}")),
                    (None, Some(h), _) => Some(format!("hit {h}")),
                    _ => None,
                };
                list = list.child(
                    chrome::list_row(("dbg-bp", ix), false, cx)
                        .group("dbg-bp-row")
                        .child(
                            Checkbox::new(("dbg-bp-enabled", ix))
                                .checked(bp.enabled)
                                .small()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let path = toggle_path.clone();
                                    this.debugger.update(cx, |d, cx| d.toggle_breakpoint_enabled(&path, line, cx));
                                })),
                        )
                        .child(
                            div()
                                .text_size(TEXT_BODY)
                                .text_color(if bp.enabled { t.fg } else { t.fg_subtle })
                                .child(format!("{relative}:{}", line + 1)),
                        )
                        .when_some(detail, |el, detail| {
                            el.child(div().min_w_0().truncate().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(detail))
                        })
                        .child(div().flex_1())
                        .child(
                            div()
                                .id(("dbg-bp-remove", ix))
                                .invisible()
                                .group_hover("dbg-bp-row", |s| s.visible())
                                .text_color(t.fg_subtle)
                                .cursor_pointer()
                                .child(Icon::new(IconName::Close).size(px(12.)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    let path = remove_path.clone();
                                    this.debugger.update(cx, |d, cx| d.remove_breakpoint(&path, line, cx));
                                })),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(DebugPanelEvent::Open { path: open_path.clone(), line: line as u32 });
                        })),
                );
            }
        }
        list.into_any_element()
    }
}

fn note(depth: usize, text: &str) -> Row {
    Row {
        key: format!("note:{text}:{depth}"),
        depth,
        name: text.to_string().into(),
        value: "".into(),
        type_name: None,
        reference: 0,
        parent: None,
        kind: RowKind::Note,
    }
}

fn push_children(rows: &mut Vec<Row>, session: &super::Session, parent_key: &str, reference: i64, depth: usize) {
    let Some(children) = session.children.get(&reference) else {
        rows.push(note(depth, "Loading…"));
        return;
    };
    for var in children {
        let key = format!("{parent_key}/{}", var.name);
        rows.push(variable_row(var, &key, reference, depth));
        if var.variables_reference > 0 && session.expanded.contains(&key) && depth < 24 {
            push_children(rows, session, &key, var.variables_reference, depth + 1);
        }
    }
}

fn variable_row(var: &Variable, key: &str, parent: i64, depth: usize) -> Row {
    Row {
        key: key.to_string(),
        depth,
        name: var.name.clone().into(),
        value: var.value.clone().into(),
        type_name: var.type_name.as_deref().map(|t| pretty_type(t).into()),
        reference: var.variables_reference,
        parent: Some(parent),
        kind: RowKind::Variable,
    }
}

/// `demo::parse::lexer::next` → `lexer::next`; generics and closures kept.
fn short_function(name: &str) -> String {
    let base = name.split('<').next().unwrap_or(name);
    let parts: Vec<&str> = base.split("::").collect();
    if parts.len() <= 2 {
        return name.to_string();
    }
    let tail = parts[parts.len() - 2..].join("::");
    format!("{tail}{}", &name[base.len()..])
}

fn debugger_root(debugger: &Debugger) -> PathBuf {
    debugger.root.clone()
}

impl Render for DebugPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            PanelTab::Debugger => self.render_debugger_tab(window, cx),
            PanelTab::Console => self.render_console_tab(cx),
            PanelTab::Breakpoints => self.render_breakpoints_tab(cx),
        };
        v_flex()
            .key_context(DEBUG_PANEL_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.editing.take().is_some() {
                    window.focus(&this.focus_handle);
                    cx.notify();
                }
            }))
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::short_function;

    #[test]
    fn shortens_function_names() {
        assert_eq!(short_function("demo::parse::lexer::next"), "lexer::next");
        assert_eq!(short_function("demo::main"), "demo::main");
        assert_eq!(short_function("core::ops::function::FnOnce::call_once<fn()>"), "FnOnce::call_once<fn()>");
    }
}

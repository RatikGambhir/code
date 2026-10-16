//! A pane: a row of tabs (files, diffs, browser pages) and the active one.

use std::path::Path;

use gpui::{
    div, prelude::*, px, AnyElement, AnyView, App, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, MouseButton, PromptLevel, SharedString, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, ActiveTheme as _, Icon, IconName, Sizable as _,
};

use crate::{
    assets::icon,
    browser_view::{BrowserEvent, BrowserView},
    editor_view::{EditorEvent, EditorView},
};

#[derive(Clone)]
pub enum PaneItem {
    Editor(Entity<EditorView>),
    Browser(Entity<BrowserView>),
}

impl PaneItem {
    pub fn entity_id(&self) -> EntityId {
        match self {
            PaneItem::Editor(editor) => editor.entity_id(),
            PaneItem::Browser(browser) => browser.entity_id(),
        }
    }

    pub fn title(&self, cx: &App) -> SharedString {
        match self {
            PaneItem::Editor(editor) => editor.read(cx).title(),
            PaneItem::Browser(browser) => browser.read(cx).title(),
        }
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        match self {
            PaneItem::Editor(editor) => editor.read(cx).is_dirty(),
            PaneItem::Browser(_) => false,
        }
    }

    fn tooltip(&self, cx: &App) -> SharedString {
        match self {
            PaneItem::Editor(editor) => editor.read(cx).path().to_string_lossy().into_owned().into(),
            PaneItem::Browser(browser) => browser.read(cx).url().to_string().into(),
        }
    }

    fn icon(&self, cx: &App) -> Icon {
        match self {
            PaneItem::Editor(editor) if editor.read(cx).is_read_only() => icon("git-compare"),
            PaneItem::Editor(_) => Icon::new(IconName::File),
            PaneItem::Browser(_) => Icon::new(IconName::Globe),
        }
    }

    fn view(&self) -> AnyView {
        match self {
            PaneItem::Editor(editor) => editor.clone().into(),
            PaneItem::Browser(browser) => browser.clone().into(),
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        match self {
            PaneItem::Editor(editor) => editor.update(cx, |editor, cx| editor.focus(window, cx)),
            PaneItem::Browser(browser) => window.focus(&browser.focus_handle(cx)),
        }
    }
}

pub enum PaneEvent {
    /// The user interacted with this pane.
    Focused,
    /// The set of tabs or the active tab changed (for persistence and the
    /// title bar).
    Changed,
    /// The last tab closed.
    Empty,
}

pub struct Pane {
    items: Vec<PaneItem>,
    active: usize,
    focus_handle: FocusHandle,
    subscriptions: Vec<(EntityId, Subscription)>,
}

impl EventEmitter<PaneEvent> for Pane {}

impl Focusable for Pane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.active_item() {
            Some(PaneItem::Editor(editor)) => editor.focus_handle(cx),
            Some(PaneItem::Browser(browser)) => browser.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl Pane {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            items: Vec::new(),
            active: 0,
            focus_handle: cx.focus_handle(),
            subscriptions: Vec::new(),
        }
    }

    pub fn items(&self) -> &[PaneItem] {
        &self.items
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active_item(&self) -> Option<&PaneItem> {
        self.items.get(self.active)
    }

    pub fn active_editor(&self) -> Option<&Entity<EditorView>> {
        match self.active_item()? {
            PaneItem::Editor(editor) => Some(editor),
            PaneItem::Browser(_) => None,
        }
    }

    pub fn active_browser(&self) -> Option<&Entity<BrowserView>> {
        match self.active_item()? {
            PaneItem::Browser(browser) => Some(browser),
            PaneItem::Editor(_) => None,
        }
    }

    pub fn editors(&self) -> impl Iterator<Item = &Entity<EditorView>> {
        self.items.iter().filter_map(|item| match item {
            PaneItem::Editor(editor) => Some(editor),
            PaneItem::Browser(_) => None,
        })
    }

    pub fn browsers(&self) -> impl Iterator<Item = &Entity<BrowserView>> {
        self.items.iter().filter_map(|item| match item {
            PaneItem::Browser(browser) => Some(browser),
            PaneItem::Editor(_) => None,
        })
    }

    /// The index of the editable tab showing `path`.
    pub fn find_editor(&self, path: &Path, cx: &App) -> Option<usize> {
        self.items.iter().position(|item| match item {
            PaneItem::Editor(editor) => {
                let editor = editor.read(cx);
                !editor.is_read_only() && editor.path() == path
            }
            PaneItem::Browser(_) => false,
        })
    }

    pub fn position_of(&self, id: EntityId) -> Option<usize> {
        self.items.iter().position(|item| item.entity_id() == id)
    }

    /// Adds a tab after the active one and activates it.
    pub fn add_item(&mut self, item: PaneItem, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let subscription = match &item {
            PaneItem::Editor(editor) => cx.subscribe(editor, |_, _, event: &EditorEvent, cx| match event {
                EditorEvent::Changed | EditorEvent::Saved(_) => cx.notify(),
            }),
            PaneItem::Browser(browser) => cx.subscribe(browser, |_, _, event: &BrowserEvent, cx| {
                if matches!(event, BrowserEvent::Changed | BrowserEvent::Navigated { .. }) {
                    cx.notify();
                }
            }),
        };
        self.subscriptions.push((item.entity_id(), subscription));
        let index = if self.items.is_empty() { 0 } else { self.active + 1 };
        self.items.insert(index, item);
        self.activate(index, focus, window, cx);
    }

    pub fn activate(&mut self, index: usize, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.items.len() {
            return;
        }
        self.active = index;
        if focus {
            self.items[index].focus(window, cx);
        }
        cx.emit(PaneEvent::Changed);
        cx.notify();
    }

    pub fn activate_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.items.is_empty() {
            self.activate((self.active + 1) % self.items.len(), true, window, cx);
        }
    }

    pub fn activate_previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.items.is_empty() {
            let len = self.items.len();
            self.activate((self.active + len - 1) % len, true, window, cx);
        }
    }

    /// Closes a tab, asking first when it has unsaved changes.
    pub fn close_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index).cloned() else { return };
        if !item.is_dirty(cx) {
            self.remove_item(item.entity_id(), window, cx);
            return;
        }
        let title = item.title(cx);
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Save changes to {title}?"),
            Some("Your changes will be lost if you don't save them."),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        let id = item.entity_id();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else { return };
            this.update_in(cx, |this, window, cx| {
                match answer {
                    0 => {
                        if let PaneItem::Editor(editor) = &item {
                            if let Err(err) = editor.update(cx, |editor, cx| editor.save(cx)) {
                                drop(window.prompt(
                                    PromptLevel::Critical,
                                    &format!("Could not save {title}"),
                                    Some(&format!("{err:#}")),
                                    &["OK"],
                                    cx,
                                ));
                                return;
                            }
                        }
                    }
                    1 => {}
                    _ => return,
                }
                this.remove_item(id, window, cx);
            })
            .ok();
        })
        .detach();
    }

    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.items.is_empty() {
            self.close_item(self.active, window, cx);
        }
    }

    /// Removes a tab without closing what it shows (to move it elsewhere).
    pub fn take_item(&mut self, id: EntityId, cx: &mut Context<Self>) -> Option<PaneItem> {
        let index = self.position_of(id)?;
        let item = self.items.remove(index);
        self.subscriptions.retain(|(sub_id, _)| *sub_id != id);
        if self.active > index || self.active >= self.items.len() {
            self.active = self.active.saturating_sub(1);
        }
        cx.emit(PaneEvent::Changed);
        cx.notify();
        Some(item)
    }

    /// Removes a tab without asking.
    pub fn remove_item(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.position_of(id) else { return };
        let had_focus = self.focus_handle(cx).contains_focused(window, cx);
        let item = self.items.remove(index);
        if let PaneItem::Editor(editor) = &item {
            editor.update(cx, |editor, _| editor.detach());
        }
        if let PaneItem::Browser(browser) = &item {
            browser.update(cx, |browser, _| browser.close());
        }
        self.subscriptions.retain(|(sub_id, _)| *sub_id != id);
        if self.items.is_empty() {
            self.active = 0;
            cx.emit(PaneEvent::Changed);
            cx.emit(PaneEvent::Empty);
            cx.notify();
            return;
        }
        if self.active > index || self.active >= self.items.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.activate(self.active, had_focus, window, cx);
    }

    fn render_tab(&self, index: usize, item: &PaneItem, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let active = index == self.active;
        let dirty = item.is_dirty(cx);
        let title = item.title(cx);
        let tooltip = item.tooltip(cx);
        let (bg, fg) = if active {
            (theme.tab_active, theme.tab_active_foreground)
        } else {
            (theme.tab, theme.tab_foreground)
        };
        h_flex()
            .id(("pane-tab", index))
            .group("pane-tab")
            .h_full()
            .flex_none()
            .max_w(px(220.))
            .pl_3()
            .pr_1()
            .gap_1p5()
            .text_sm()
            .bg(bg)
            .text_color(fg)
            .border_r_1()
            .border_color(theme.border)
            .when(active, |el| el.border_t_2().border_color(theme.primary))
            .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, window, cx| this.activate(index, true, window, cx)))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, window, cx| this.close_item(index, window, cx)),
            )
            .child(item.icon(cx).small().text_color(theme.muted_foreground))
            .child(div().truncate().child(title))
            .child(
                div()
                    .size(px(18.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Button::new(("close-tab", index))
                            .icon(if dirty { IconName::Dash } else { IconName::Close })
                            .ghost()
                            .xsmall()
                            .when(!active && !dirty, |b| b.invisible().group_hover("pane-tab", |s| s.visible()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_item(index, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let hint = |keys: &'static str, label: &'static str| {
            h_flex()
                .gap_4()
                .child(div().w(px(160.)).text_right().child(label))
                .child(div().font_family(cx.theme().mono_font_family.clone()).child(keys))
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .text_sm()
            .text_color(muted)
            .child(hint("⌘P", "Go to file"))
            .child(hint("⇧⌘P", "Command palette"))
            .child(hint("⌘T", "New browser tab"))
            .child(hint("⌘L", "Ask Codex"))
            .child(hint("⌘J", "Toggle terminal"))
    }
}

impl Render for Pane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tab_bar_bg = theme.tab_bar;
        let border = theme.border;
        let mut tabs = h_flex().id("pane-tabs").h_full().overflow_x_scroll();
        for (index, item) in self.items.iter().enumerate() {
            tabs = tabs.child(self.render_tab(index, item, cx));
        }
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .capture_any_mouse_down(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Focused)))
            .when(!self.items.is_empty(), |el| {
                el.child(
                    h_flex()
                        .h(px(34.))
                        .flex_none()
                        .bg(tab_bar_bg)
                        .border_b_1()
                        .border_color(border)
                        .child(tabs),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .map(|el| match self.active_item() {
                        Some(item) => el.child(item.view()),
                        None => el.child(self.render_empty(cx)),
                    }),
            )
    }
}

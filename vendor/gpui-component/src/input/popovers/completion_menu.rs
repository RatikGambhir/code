use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, AppContext, Bounds, Context, DismissEvent, Empty, Entity,
    EventEmitter, HighlightStyle, InteractiveElement as _, IntoElement, ParentElement, Pixels,
    Point, Render, RenderOnce, SharedString, Styled, StyledText, Subscription, Window, canvas,
    deferred, div, prelude::FluentBuilder, px, relative,
};
use lsp_types::CompletionItem;

const MAX_MENU_WIDTH: Pixels = px(320.);
const MAX_MENU_HEIGHT: Pixels = px(240.);
const POPOVER_GAP: Pixels = px(4.);

use crate::{
    ActiveTheme, IndexPath, Selectable, actions, h_flex,
    input::{
        self, CompletionProvider, InputState,
        popovers::{editor_popover, render_markdown},
    },
    label::Label,
    list::{List, ListDelegate, ListEvent, ListState},
};

struct ContextMenuDelegate {
    query: SharedString,
    menu: Entity<CompletionMenu>,
    items: Vec<Rc<CompletionItem>>,
    selected_ix: usize,
}

impl ContextMenuDelegate {
    fn set_items(&mut self, items: Vec<Rc<CompletionItem>>) {
        self.items = items;
        self.selected_ix = 0;
    }

    fn selected_item(&self) -> Option<&Rc<CompletionItem>> {
        self.items.get(self.selected_ix)
    }
}

#[derive(IntoElement)]
struct CompletionMenuItem {
    ix: usize,
    item: Rc<CompletionItem>,
    children: Vec<AnyElement>,
    selected: bool,
    highlight_prefix: SharedString,
}

impl CompletionMenuItem {
    fn new(ix: usize, item: Rc<CompletionItem>) -> Self {
        Self {
            ix,
            item,
            children: vec![],
            selected: false,
            highlight_prefix: "".into(),
        }
    }

    fn highlight_prefix(mut self, s: impl Into<SharedString>) -> Self {
        self.highlight_prefix = s.into();
        self
    }
}
impl Selectable for CompletionMenuItem {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl ParentElement for CompletionMenuItem {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for CompletionMenuItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let item = self.item;

        let deprecated = item.deprecated.unwrap_or(false);
        // cockpit patch: highlight the label's own characters that spell
        // the typed word. A length taken from `filter_text` or the query
        // could fall inside a character of the label, which panics.
        let matched_len = matched_prefix_len(&item.label, &self.highlight_prefix);

        let highlights = vec![(
            0..matched_len,
            HighlightStyle {
                color: Some(cx.theme().blue),
                ..Default::default()
            },
        )];

        h_flex()
            .id(self.ix)
            .gap_2()
            .p_1()
            .text_xs()
            .line_height(relative(1.))
            .rounded_sm()
            .when(item.deprecated.unwrap_or(false), |this| this.line_through())
            .hover(|this| this.bg(cx.theme().accent.opacity(0.8)))
            .when(self.selected, |this| {
                this.bg(cx.theme().accent)
                    .text_color(cx.theme().accent_foreground)
            })
            .child(div().child(StyledText::new(item.label.clone()).with_highlights(highlights)))
            .when(item.detail.is_some(), |this| {
                this.child(
                    Label::new(item.detail.as_deref().unwrap_or("").to_string())
                        .text_color(cx.theme().muted_foreground)
                        .when(deprecated, |this| this.line_through())
                        .italic(),
                )
            })
            .children(self.children)
    }
}

impl EventEmitter<DismissEvent> for ContextMenuDelegate {}

impl ListDelegate for ContextMenuDelegate {
    type Item = CompletionMenuItem;

    fn items_count(&self, _: usize, _: &gpui::App) -> usize {
        self.items.len()
    }

    fn render_item(
        &mut self,
        ix: crate::IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let item = self.items.get(ix.row)?;
        Some(CompletionMenuItem::new(ix.row, item.clone()).highlight_prefix(self.query.clone()))
    }

    fn set_selected_index(
        &mut self,
        ix: Option<crate::IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected_ix = ix.map(|i| i.row).unwrap_or(0);
        cx.notify();
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(item) = self.selected_item().cloned() else {
            return;
        };

        self.menu.update(cx, |this, cx| {
            this.select_item(&item, false, window, cx);
        });
    }
}

/// A context menu for code completions and code actions.
pub struct CompletionMenu {
    offset: usize,
    editor: Entity<InputState>,
    list: Entity<ListState<ContextMenuDelegate>>,
    open: bool,
    bounds: Bounds<Pixels>,

    /// The offset of the first character that triggered the completion.
    pub(crate) trigger_start_offset: Option<usize>,
    query: SharedString,
    /// cockpit patch: the server's last answer, unfiltered; typing narrows
    /// it locally until the next answer arrives.
    all_items: Vec<Rc<CompletionItem>>,
    /// cockpit patch: completion requests made, the one whose answer is
    /// shown, and the last one made before the menu was dismissed (answers
    /// to those must not bring it back).
    requests: usize,
    shown_request: usize,
    dismissed_request: usize,
    _subscriptions: Vec<Subscription>,
}

impl CompletionMenu {
    /// Creates a new `CompletionMenu` with the given offset and completion items.
    ///
    /// NOTE: This element should not call from InputState::new, unless that will stack overflow.
    pub(crate) fn new(
        editor: Entity<InputState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let view = cx.entity();
            let menu = ContextMenuDelegate {
                query: SharedString::default(),
                menu: view,
                items: vec![],
                selected_ix: 0,
            };

            let list = cx.new(|cx| ListState::new(menu, window, cx));

            let _subscriptions =
                vec![
                    cx.subscribe(&list, |this: &mut Self, _, ev: &ListEvent, cx| {
                        match ev {
                            ListEvent::Confirm(_) => {
                                this.hide(cx);
                            }
                            _ => {}
                        }
                        cx.notify();
                    }),
                ];

            Self {
                offset: 0,
                editor,
                list,
                open: false,
                trigger_start_offset: None,
                query: SharedString::default(),
                all_items: Vec::new(),
                requests: 0,
                shown_request: 0,
                dismissed_request: 0,
                bounds: Bounds::default(),
                _subscriptions,
            }
        })
    }

    /// Accepts `item`: replacing the rest of the word (Tab, `replace`) or
    /// inserting before it (Enter, a click).
    fn select_item(
        &mut self,
        item: &CompletionItem,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let item = item.clone();
        let editor = self.editor.clone();

        // The editor may be mid-update (this runs from its key handling).
        cx.spawn_in(window, async move |_, cx| {
            editor.update_in(cx, |editor, window, cx| {
                editor.completion_inserting = true;
                // cockpit patch: see `InputState::apply_completion`.
                editor.apply_completion(&item, replace, window, cx);
                editor.completion_inserting = false;
                // FIXME: Input not get the focus
                editor.focus(window, cx);
            })
        })
        .detach();

        self.dismiss(cx);
    }

    pub(crate) fn handle_action(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.open {
            return false;
        }

        cx.propagate();
        if action.partial_eq(&input::Enter { secondary: false }) {
            self.on_action_enter(false, window, cx);
        } else if action.partial_eq(&input::IndentInline) {
            // cockpit patch: Tab accepts too, replacing the whole word.
            self.on_action_enter(true, window, cx);
        } else if action.partial_eq(&input::Escape) {
            self.on_action_escape(window, cx);
        } else if action.partial_eq(&input::MoveUp) {
            self.on_action_up(window, cx);
        } else if action.partial_eq(&input::MoveDown) {
            self.on_action_down(window, cx);
        } else {
            return false;
        }

        true
    }

    fn on_action_enter(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.list.read(cx).delegate().selected_item().cloned() else {
            return;
        };
        self.select_item(&item, replace, window, cx);
    }

    fn on_action_escape(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);
    }

    fn on_action_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |this, cx| {
            this.on_action_select_prev(&actions::SelectUp, window, cx)
        });
    }

    fn on_action_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |this, cx| {
            this.on_action_select_next(&actions::SelectDown, window, cx)
        });
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Hide the completion menu and reset the trigger start offset.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.trigger_start_offset = None;
        cx.notify();
    }

    /// cockpit patch: closes the menu at the user's request (Escape, an
    /// accepted item); answers to requests made so far are ignored.
    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.dismissed_request = self.requests;
        self.hide(cx);
    }

    /// cockpit patch: numbers a new completion request.
    pub(crate) fn next_request(&mut self) -> usize {
        self.requests += 1;
        self.requests
    }

    /// cockpit patch: whether the answer to `request` should be shown.
    pub(crate) fn wants_answer(&self, request: usize) -> bool {
        request > self.dismissed_request && request >= self.shown_request
    }

    /// cockpit patch: shows the answer to `request`, narrowed to `word`.
    pub(crate) fn set_items(
        &mut self,
        request: usize,
        offset: usize,
        items: Vec<CompletionItem>,
        provider: &dyn CompletionProvider,
        word: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shown_request = request;
        self.all_items = items.into_iter().map(Rc::new).collect();
        self.refilter(provider, word, offset, window, cx);
    }

    /// cockpit patch: narrows the last answer to `word` without asking the
    /// server; hides the menu when nothing is left.
    pub(crate) fn refilter(
        &mut self,
        provider: &dyn CompletionProvider,
        word: &str,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let indices = {
            let all: Vec<&CompletionItem> = self.all_items.iter().map(|item| item.as_ref()).collect();
            provider.filter_completions(&all, word)
        };
        let items: Vec<Rc<CompletionItem>> = indices
            .into_iter()
            .filter_map(|ix| self.all_items.get(ix).cloned())
            .collect();
        if items.is_empty() {
            self.hide(cx);
            return;
        }
        self.query = SharedString::from(word.to_string());
        self.show(offset, items, window, cx);
    }

    /// Sets the trigger start offset if it is not already set.
    pub(crate) fn update_query(&mut self, start_offset: usize, query: impl Into<SharedString>) {
        if self.trigger_start_offset.is_none() {
            self.trigger_start_offset = Some(start_offset);
        }
        self.query = query.into();
    }

    pub(crate) fn show(
        &mut self,
        offset: usize,
        items: Vec<Rc<CompletionItem>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.offset = offset;
        self.open = true;
        self.list.update(cx, |this, cx| {
            let longest_ix = items
                .iter()
                .enumerate()
                .max_by_key(|(_, item)| {
                    item.label.len() + item.detail.as_ref().map(|d| d.len()).unwrap_or(0)
                })
                .map(|(ix, _)| ix)
                .unwrap_or(0);

            this.delegate_mut().query = self.query.clone();
            this.delegate_mut().set_items(items);
            this.set_selected_index(Some(IndexPath::new(0)), window, cx);
            this.set_item_to_measure_index(IndexPath::new(longest_ix), window, cx);
        });

        cx.notify();
    }

    fn origin(&self, cx: &App) -> Option<Point<Pixels>> {
        let editor = self.editor.read(cx);
        let Some(last_layout) = editor.last_layout.as_ref() else {
            return None;
        };
        let Some(cursor_origin) = last_layout.cursor_bounds.map(|b| b.origin) else {
            return None;
        };

        let scroll_origin = self.editor.read(cx).scroll_handle.offset();

        Some(
            scroll_origin + cursor_origin - editor.input_bounds.origin
                + Point::new(-px(4.), last_layout.line_height + px(4.)),
        )
    }
}

impl Render for CompletionMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return Empty.into_any_element();
        }

        if self.list.read(cx).delegate().items.is_empty() {
            self.open = false;
            return Empty.into_any_element();
        }

        let view = cx.entity();

        let Some(pos) = self.origin(cx) else {
            return Empty.into_any_element();
        };

        let selected_documentation = self
            .list
            .read(cx)
            .delegate()
            .selected_item()
            .and_then(|item| item.documentation.clone());

        let max_width = MAX_MENU_WIDTH.min(window.bounds().size.width - pos.x);
        let abs_pos = self.editor.read(cx).input_bounds.origin + pos;
        let vertical_layout =
            abs_pos.x + MAX_MENU_WIDTH + POPOVER_GAP + MAX_MENU_WIDTH + POPOVER_GAP
                > window.bounds().size.width;

        deferred(
            div()
                .absolute()
                .left(pos.x)
                .top(pos.y)
                .flex()
                .flex_row()
                .gap(POPOVER_GAP)
                .items_start()
                .when(vertical_layout, |this| this.flex_col())
                .child(
                    editor_popover("completion-menu", cx)
                        .max_w(max_width)
                        .min_w(px(120.))
                        .child(List::new(&self.list).max_h(MAX_MENU_HEIGHT))
                        .child(
                            canvas(
                                move |bounds, _, cx| view.update(cx, |r, _| r.bounds = bounds),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        ),
                )
                .when_some(selected_documentation, |this, documentation| {
                    let mut doc = match documentation {
                        lsp_types::Documentation::String(s) => s.clone(),
                        lsp_types::Documentation::MarkupContent(mc) => mc.value.clone(),
                    };
                    if vertical_layout {
                        doc = doc.split("\n").next().unwrap_or_default().to_string();
                    }

                    this.child(
                        div().child(
                            editor_popover("completion-menu", cx)
                                .w(MAX_MENU_WIDTH)
                                .px_2()
                                .child(render_markdown("doc", doc, window, cx)),
                        ),
                    )
                })
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.hide(cx);
                })),
        )
        .into_any_element()
    }
}

/// cockpit patch: how many bytes at the start of `label` spell `typed`,
/// ignoring case (always a character boundary of `label`).
fn matched_prefix_len(label: &str, typed: &str) -> usize {
    let mut typed = typed.chars().flat_map(char::to_lowercase);
    let mut len = 0;
    for c in label.chars() {
        let mut lower = c.to_lowercase();
        let matches = lower.all(|l| typed.next() == Some(l));
        if !matches {
            break;
        }
        len += c.len_utf8();
    }
    len
}

#[cfg(test)]
mod tests {
    use super::matched_prefix_len;

    #[test]
    fn highlight_covers_the_typed_prefix_only() {
        assert_eq!(matched_prefix_len("HashMap", "has"), 3);
        assert_eq!(matched_prefix_len("HashMap", ""), 0);
        assert_eq!(matched_prefix_len("HashMap", "map"), 0);
        assert_eq!(matched_prefix_len("é_test", "É"), 2);
        assert_eq!(matched_prefix_len("ab", "abc"), 2);
    }
}

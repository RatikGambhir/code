//! Find in files.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use cockpit_editor::search::{search, SearchMatch, SearchOptions};
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, EventEmitter, FocusHandle,
    Focusable, HighlightStyle, SharedString, StyledText, Subscription, Task, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    v_flex, IconName, Selectable as _, Sizable as _,
};

use crate::{
    chrome::{self, panel_header},
    theme::{tokens, RADIUS, TEXT_MONO, TEXT_SMALL},
};

const DEBOUNCE: Duration = Duration::from_millis(200);
const MAX_RESULTS: usize = 2000;

pub enum SearchPanelEvent {
    /// Open `path` at the 0-based `line` and `column`.
    OpenMatch { path: PathBuf, line: u32, column: u32 },
}

pub struct SearchPanel {
    root: PathBuf,
    query: Entity<InputState>,
    options: SearchOptions,
    results: Vec<SearchMatch>,
    error: Option<String>,
    searching: bool,
    cancel: Arc<AtomicBool>,
    _search: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SearchPanelEvent> for SearchPanel {}

impl Focusable for SearchPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl SearchPanel {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscriptions = vec![cx.subscribe(&query, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => this.schedule(DEBOUNCE, cx),
            InputEvent::PressEnter { .. } => this.schedule(Duration::ZERO, cx),
            _ => {}
        })];
        Self {
            root,
            query,
            options: SearchOptions { max_results: MAX_RESULTS, ..Default::default() },
            results: Vec::new(),
            error: None,
            searching: false,
            cancel: Arc::new(AtomicBool::new(false)),
            _search: None,
            _subscriptions: subscriptions,
        }
    }

    /// Focuses the query and selects it, optionally seeding it with `text`.
    pub fn focus_query(&mut self, text: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |state, cx| {
            if let Some(text) = text.filter(|t| !t.is_empty() && !t.contains('\n')) {
                state.set_value(text.to_string(), window, cx);
            }
            state.focus(window, cx);
        });
        window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
    }

    fn schedule(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();

        let query = self.query.read(cx).value().to_string();
        if query.is_empty() {
            self.results.clear();
            self.error = None;
            self.searching = false;
            self._search = None;
            cx.notify();
            return;
        }
        let root = self.root.clone();
        let options = self.options.clone();
        self.searching = true;
        cx.notify();
        self._search = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let results = cx
                .background_spawn({
                    let cancel = cancel.clone();
                    async move { search(&root, &query, &options, cancel) }
                })
                .await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            this.update(cx, |this, cx| {
                this.searching = false;
                match results {
                    Ok(results) => {
                        this.results = results;
                        this.error = None;
                    }
                    Err(err) => {
                        this.results.clear();
                        this.error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn toggle_option(&mut self, option: fn(&mut SearchOptions) -> &mut bool, cx: &mut Context<Self>) {
        let value = option(&mut self.options);
        *value = !*value;
        self.schedule(Duration::ZERO, cx);
    }

    fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let highlight = HighlightStyle {
            background_color: Some(t.accent.alpha(if t.dark { 0.35 } else { 0.2 })),
            color: Some(t.fg),
            ..Default::default()
        };
        let mut list = v_flex().px(px(6.)).pb(px(8.));
        let mut current: Option<&PathBuf> = None;
        for (ix, found) in self.results.iter().enumerate() {
            if current != Some(&found.path) {
                current = Some(&found.path);
                let count = self.results.iter().filter(|m| m.path == found.path).count();
                list = list.child(
                    h_flex()
                        .h(px(28.))
                        .mt(px(4.))
                        .px(px(8.))
                        .gap_2()
                        .font_family(t.mono.clone())
                        .text_size(TEXT_MONO)
                        .text_color(t.fg)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(found.path.to_string_lossy().into_owned()),
                        )
                        .child(
                            div()
                                .px(px(6.))
                                .rounded_full()
                                .bg(t.raised)
                                .text_size(px(10.5))
                                .text_color(t.fg_muted)
                                .child(count.to_string()),
                        ),
                );
            }
            // Trim leading whitespace but keep the highlight aligned.
            let trimmed = found.line_text.trim_start();
            let offset = found.line_text.len() - trimmed.len();
            let (text, range) = clip_line(trimmed, found.range.start.saturating_sub(offset)..found.range.end.saturating_sub(offset));
            let path = self.root.join(&found.path);
            let line = found.line_number.saturating_sub(1) as u32;
            let column = found.line_text[..found.range.start.min(found.line_text.len())].chars().count() as u32;
            list = list.child(
                chrome::list_row(("search-match", ix), false, cx)
                    .h(px(24.))
                    .pl(px(12.))
                    .font_family(t.mono.clone())
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_muted)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(SearchPanelEvent::OpenMatch { path: path.clone(), line, column });
                    }))
                    .child(div().w(px(30.)).flex_none().text_color(t.fg_subtle).child(found.line_number.to_string()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(StyledText::new(text).with_highlights(vec![(range, highlight)])),
                    ),
            );
        }
        list.into_any_element()
    }
}

/// Keeps long lines readable: show some context before the match.
fn clip_line(line: &str, range: std::ops::Range<usize>) -> (SharedString, std::ops::Range<usize>) {
    const CONTEXT: usize = 40;
    let range = range.start.min(line.len())..range.end.min(line.len());
    if range.start <= CONTEXT {
        return (line.to_string().into(), range);
    }
    let mut start = range.start - CONTEXT;
    while !line.is_char_boundary(start) {
        start += 1;
    }
    let text = format!("…{}", &line[start..]);
    let ellipsis = '…'.len_utf8();
    (text.into(), range.start - start + ellipsis..range.end - start + ellipsis)
}

impl Render for SearchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let option = |id: &'static str, label: &'static str, tooltip: &'static str, on: bool| {
            Button::new(id).label(label).ghost().xsmall().selected(on).tooltip(tooltip)
        };
        let summary = if self.searching {
            "Searching…".to_string()
        } else if let Some(err) = &self.error {
            err.clone()
        } else if self.query.read(cx).value().is_empty() {
            String::new()
        } else {
            let files = {
                let mut paths: Vec<&PathBuf> = self.results.iter().map(|m| &m.path).collect();
                paths.dedup();
                paths.len()
            };
            let capped = if self.results.len() >= MAX_RESULTS { "+" } else { "" };
            format!("{}{capped} results in {files} files", self.results.len())
        };

        v_flex()
            .size_full()
            .child(panel_header("Search", cx))
            .child(
                v_flex()
                    .px(px(12.))
                    .pb(px(8.))
                    .gap(px(6.))
                    .child(
                        div()
                            .rounded(RADIUS)
                            .border_1()
                            .border_color(t.border_strong)
                            .child(Input::new(&self.query).small().appearance(false).cleanable(true).prefix(IconName::Search)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                option("search-case", "Aa", "Match case", self.options.case_sensitive)
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| &mut o.case_sensitive, cx))),
                            )
                            .child(
                                option("search-word", "ab", "Whole word", self.options.whole_word)
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| &mut o.whole_word, cx))),
                            )
                            .child(
                                option("search-regex", ".*", "Regular expression", self.options.regex)
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_option(|o| &mut o.regex, cx))),
                            )
                            .child(div().flex_1())
                            .child(div().text_size(TEXT_SMALL).text_color(t.fg_subtle).truncate().child(summary)),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .id("search-results")
                        .size_full()
                        .border_t_1()
                        .border_color(t.border)
                        .overflow_y_scrollbar()
                        .child(self.render_results(cx)),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_keeps_the_match_highlighted() {
        let line = format!("{}needle tail", "x".repeat(100));
        let (text, range) = clip_line(&line, 100..106);
        assert!(text.starts_with('…'));
        assert_eq!(&text[range], "needle");

        let (text, range) = clip_line("short needle", 6..12);
        assert_eq!(&text[range], "needle");
    }
}

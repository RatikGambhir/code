//! The chat history: the sidebar list of conversations shown in chat mode.

use chrono::{DateTime, Local, NaiveDate, TimeZone as _};
use cockpit_persistence::now_ts;
use gpui::{
    div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle,
    SharedString, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    v_flex, IconName, Sizable as _,
};

use crate::{
    agent::AgentController,
    assets::icon,
    chrome::{self, caps_label, panel_header},
    theme::{tokens, RADIUS, TEXT_BODY, TEXT_SMALL},
};

pub enum ChatHistoryEvent {
    /// A conversation was opened or a new one started; focus the composer.
    Opened,
}

pub struct ChatHistory {
    controller: Entity<AgentController>,
    filter: Entity<InputState>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ChatHistoryEvent> for ChatHistory {}

impl ChatHistory {
    pub fn new(controller: Entity<AgentController>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search chats"));
        let subscriptions = vec![
            cx.observe(&controller, |_, _, cx| cx.notify()),
            cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        Self {
            controller,
            filter,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    fn new_chat(&mut self, cx: &mut Context<Self>) {
        self.controller.update(cx, |c, cx| c.start_draft(cx));
        cx.emit(ChatHistoryEvent::Opened);
    }

    fn open(&mut self, id: &str, cx: &mut Context<Self>) {
        self.controller.update(cx, |c, cx| c.select_thread(id, cx));
        cx.emit(ChatHistoryEvent::Opened);
    }

    fn archive(&mut self, id: &str, cx: &mut Context<Self>) {
        self.controller.update(cx, |c, cx| c.archive_thread(id, cx));
    }

    fn render_row(&self, row: Row, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let (open_id, archive_id) = (row.id.clone(), row.id.clone());
        let group = SharedString::from(format!("chat-row-{}", row.id));
        chrome::list_row(SharedString::from(format!("chat-{}", row.id)), row.active, cx)
            .group(group.clone())
            .h(px(30.))
            .pr(px(4.))
            .on_click(cx.listener(move |this, _, _, cx| this.open(&open_id, cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(TEXT_BODY)
                    .text_color(if row.active { t.fg } else { t.fg_muted })
                    .child(row.title),
            )
            .map(|el| match (row.running, row.needs_you) {
                (_, true) => el.child(chrome::status_dot(t.warning)),
                (true, false) => el.child(Spinner::new().xsmall().color(t.accent_text)),
                _ => el,
            })
            // The age, swapped for the archive button on hover.
            .child(
                div()
                    .relative()
                    .flex_none()
                    .min_w(px(24.))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(
                        div()
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_subtle)
                            .group_hover(group.clone(), |s| s.invisible())
                            .child(relative_time(now, row.updated_at)),
                    )
                    .child(
                        div().absolute().right_0().invisible().group_hover(group, |s| s.visible()).child(
                            Button::new(SharedString::from(format!("archive-{}", row.id)))
                                .icon(icon("archive"))
                                .ghost()
                                .xsmall()
                                .tooltip("Archive")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.archive(&archive_id, cx);
                                })),
                        ),
                    ),
            )
    }

    fn render_empty(&self, filtered: bool, cx: &App) -> impl IntoElement {
        let t = tokens(cx);
        v_flex()
            .px(px(14.))
            .py(px(10.))
            .gap(px(4.))
            .text_size(TEXT_SMALL)
            .text_color(t.fg_subtle)
            .child(if filtered { "No chats match." } else { "No chats yet." })
            .when(!filtered, |el| el.child("Conversations you start appear here."))
    }
}

/// One conversation as the list shows it.
struct Row {
    id: String,
    title: SharedString,
    updated_at: i64,
    active: bool,
    running: bool,
    needs_you: bool,
}

impl Focusable for ChatHistory {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ChatHistory {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let query = self.filter.read(cx).value().trim().to_lowercase();
        let controller = self.controller.read(cx);
        let active = controller.active_thread().map(|t| t.id.clone());
        let rows: Vec<Row> = controller
            .threads()
            .iter()
            .filter(|thread| query.is_empty() || thread.title.to_lowercase().contains(&query))
            .map(|thread| Row {
                id: thread.id.clone(),
                title: thread.title.clone(),
                updated_at: thread.updated_at,
                active: active.as_deref() == Some(thread.id.as_str()),
                running: thread.transcript.is_running(),
                needs_you: thread.transcript.pending_approvals().next().is_some(),
            })
            .collect();
        let drafting = active.is_none();

        let now = now_ts();
        let today = Local::now().date_naive();
        let mut list = v_flex().px(px(6.)).pb(px(10.)).gap(px(1.));
        if rows.is_empty() {
            list = list.child(self.render_empty(!query.is_empty(), cx));
        }
        let mut last_bucket = None;
        for row in rows {
            let bucket = Bucket::of(today, local_date(row.updated_at));
            if last_bucket != Some(bucket) {
                list = list.child(caps_label(bucket.label(), cx).px(px(8.)).pt(px(10.)).pb(px(4.)));
                last_bucket = Some(bucket);
            }
            list = list.child(self.render_row(row, now, cx));
        }

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(
                panel_header("Chats", cx).child(
                    Button::new("chat-new")
                        .icon(icon("square-pen"))
                        .ghost()
                        .xsmall()
                        .tooltip("New chat")
                        .on_click(cx.listener(|this, _, _, cx| this.new_chat(cx))),
                ),
            )
            .child(
                v_flex().px(px(12.)).pb(px(6.)).gap(px(6.)).child(
                    div()
                        .rounded(RADIUS)
                        .border_1()
                        .border_color(t.border_strong)
                        .child(Input::new(&self.filter).small().appearance(false).cleanable(true).prefix(IconName::Search)),
                ),
            )
            // A draft (new chat, nothing sent yet) gets a row of its own.
            .when(drafting, |el| {
                el.child(
                    div().px(px(6.)).child(
                        chrome::list_row("chat-draft", true, cx)
                            .h(px(30.))
                            .child(icon("square-pen").size(px(13.)).text_color(t.accent_text))
                            .child(div().text_size(TEXT_BODY).text_color(t.fg).child("New chat")),
                    ),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("chat-history-list")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .child(list),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
    }
}

/// Which heading a conversation falls under.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Bucket {
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    Older,
}

impl Bucket {
    fn of(today: NaiveDate, date: NaiveDate) -> Self {
        match (today - date).num_days() {
            ..=0 => Self::Today,
            1 => Self::Yesterday,
            2..=6 => Self::ThisWeek,
            7..=29 => Self::ThisMonth,
            _ => Self::Older,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Yesterday => "Yesterday",
            Self::ThisWeek => "Previous 7 days",
            Self::ThisMonth => "Previous 30 days",
            Self::Older => "Older",
        }
    }
}

fn local_date(ts: i64) -> NaiveDate {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt: DateTime<Local>| dt.date_naive())
        .unwrap_or_default()
}

/// "now", "5m", "3h", "2d", "6w".
fn relative_time(now: i64, then: i64) -> String {
    let secs = now.saturating_sub(then).max(0);
    match secs {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..604_800 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_steps() {
        assert_eq!(relative_time(1_000, 990), "now");
        assert_eq!(relative_time(1_000, 1_100), "now", "clock skew is not negative");
        assert_eq!(relative_time(10_000, 10_000 - 5 * 60), "5m");
        assert_eq!(relative_time(100_000, 100_000 - 3 * 3_600), "3h");
        assert_eq!(relative_time(1_000_000, 1_000_000 - 2 * 86_400), "2d");
        assert_eq!(relative_time(10_000_000, 10_000_000 - 14 * 86_400), "2w");
    }

    #[test]
    fn buckets_by_calendar_day() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        let day = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        assert_eq!(Bucket::of(today, day(30)), Bucket::Today);
        assert_eq!(Bucket::of(today, day(29)), Bucket::Yesterday);
        assert_eq!(Bucket::of(today, day(25)), Bucket::ThisWeek);
        assert_eq!(Bucket::of(today, day(10)), Bucket::ThisMonth);
        assert_eq!(Bucket::of(today, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()), Bucket::Older);
    }
}

//! The agent panel, laid out like a chat channel: every thread is a card in
//! one feed, and `@claude <prompt>` in the composer starts another agent
//! beside the others. One card is open at a time (an accordion); it shows
//! the full transcript, approvals and the composer replies to it.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use cockpit_agents::{
    protocol::{ApprovalDecision, ItemStatus, LoginResponse, PlanStepStatus},
    rpc::RequestId,
    transcript::ChangedFile,
    ApprovalKind, ApprovalState, Entry, EntryKind, Provider,
};
use cockpit_orchestrator::PermissionPreset;
use cockpit_persistence::now_ts;
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, ScrollHandle, SharedString, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Enter, IndentInline, Input, InputEvent, InputState, MoveDown, MoveUp},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    switch::Switch,
    text::TextView,
    v_flex, Disableable as _, IconName, Selectable as _, Sizable as _, StyledExt as _,
};

use super::{
    feed::{self, CardStatus, Cards},
    mention::{self, Handle},
    AgentController, ConnectionState,
};
use crate::{
    actions,
    assets::icon,
    chat::{
        diff::{chat_changes, count_changes, DiffSummary},
        relative_time,
    },
    chrome::{self, caps_label, key_chip},
    theme::{tokens, Tokens, RADIUS, RADIUS_LG, TEXT_BODY, TEXT_MONO, TEXT_SMALL, TEXT_TITLE},
};

/// Output shown for a command before the card is expanded.
const COLLAPSED_OUTPUT_LINES: usize = 6;
/// Output kept on screen for an expanded command (the tail).
const MAX_OUTPUT_CHARS: usize = 20_000;
/// The reading width of the conversation when it fills the window.
const CHAT_COLUMN_WIDTH: f32 = 780.;
/// How close to the bottom counts as "following" new output.
const FOLLOW_SLACK: f32 = 48.;

pub enum AgentPanelEvent {
    OpenFile(PathBuf),
    /// The diff button in chat mode was pressed.
    ToggleDiff,
}

pub struct AgentPanel {
    controller: Entity<AgentController>,
    composer: Entity<InputState>,
    api_key: Entity<InputState>,
    /// The code Claude's sign-in page shows when it can't finish on its own.
    claude_code: Entity<InputState>,
    scroll: ScrollHandle,
    /// Entries the user expanded (commands, diffs, reasoning).
    expanded: HashSet<String>,
    /// What the feed showed last frame, to decide how to scroll.
    seen: FeedSeen,
    /// Collapsed-card summaries, rebuilt only when a transcript changes.
    cards: Cards,
    /// The provider and open-thread state the composer's placeholder is for.
    placeholder_for: Option<(Provider, bool)>,
    /// A note under the composer, cleared when the text changes.
    hint: Option<SharedString>,
    /// The highlighted row of the `@` suggestion list.
    mention_selected: usize,
    /// Shown full width in the center (chat mode) rather than docked.
    full: bool,
    /// The diff panel beside the chat is open (for the toggle's state).
    diff_open: bool,
    /// Changed-file totals of the active thread, keyed by (thread, revision).
    diff_summary: ((Option<String>, u64), DiffSummary),
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AgentPanelEvent> for AgentPanel {}

impl AgentPanel {
    pub fn new(controller: Entity<AgentController>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(1, 12)
                .placeholder("Ask Codex anything")
        });
        let api_key = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("sk-…"));
        let claude_code = cx.new(|cx| InputState::new(window, cx).placeholder("Code from the sign-in page"));
        let subscriptions = vec![
            cx.observe(&controller, |_, _, cx| cx.notify()),
            cx.subscribe(&composer, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.hint = None;
                    this.mention_selected = 0;
                    this.highlight_mention(cx);
                    cx.notify();
                }
            }),
        ];
        Self {
            controller,
            composer,
            api_key,
            claude_code,
            scroll: ScrollHandle::new(),
            expanded: HashSet::new(),
            seen: FeedSeen::default(),
            cards: Cards::default(),
            placeholder_for: None,
            hint: None,
            mention_selected: 0,
            full: false,
            diff_open: false,
            diff_summary: ((None, 0), DiffSummary::default()),
            _subscriptions: subscriptions,
        }
    }

    pub fn controller(&self) -> &Entity<AgentController> {
        &self.controller
    }

    /// Switches between the docked panel and the full-width chat, and tells
    /// the chat whether its diff panel is open.
    pub fn set_chat_mode(&mut self, full: bool, diff_open: bool, cx: &mut Context<Self>) {
        if (self.full, self.diff_open) != (full, diff_open) {
            self.full = full;
            self.diff_open = diff_open;
            cx.notify();
        }
    }

    /// Totals of what the active thread changed, recomputed only when one
    /// of its file changes moves (not on every streamed token).
    fn refresh_diff_summary(&mut self, cx: &App) {
        let controller = self.controller.read(cx);
        let thread = controller.active_thread();
        let revision = thread.map_or(0, |t| t.transcript.changes_revision());
        if self.diff_summary.0 .0.as_deref() == thread.map(|t| t.id.as_str()) && self.diff_summary.0 .1 == revision {
            return;
        }
        let key = (thread.map(|t| t.id.clone()), revision);
        let summary = controller
            .active_thread()
            .map(|t| DiffSummary::of(&chat_changes(t.transcript.entries())))
            .unwrap_or_default();
        self.diff_summary = (key, summary);
    }

    /// Centers `el` in a reading-width column when the chat fills the window.
    ///
    /// The column sits in a full-width block wrapper rather than being a
    /// `w_full().max_w()` flex item: taffy measures a flex item's height at
    /// its unclamped width, so wrapped text came out too short and spilled
    /// past the card. Block layout applies `max_w` before measuring.
    fn column<E: Styled + IntoElement>(&self, el: E) -> AnyElement {
        if self.full {
            div()
                .flex_none()
                .w_full()
                .child(el.max_w(px(CHAT_COLUMN_WIDTH)).mx_auto())
                .into_any_element()
        } else {
            el.into_any_element()
        }
    }

    /// Puts text in the composer (e.g. "explain this selection") and focuses it.
    pub fn insert_prompt(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.composer.read(cx).value().to_string();
        let value = if current.trim().is_empty() {
            text.to_string()
        } else {
            format!("{current}\n\n{text}")
        };
        self.composer.update(cx, |state, cx| {
            state.set_value(value, window, cx);
            state.focus(window, cx);
        });
    }

    /// `@claude …` starts a new agent; anything else replies to the open
    /// thread, or starts an agent when none is open.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value();
        if text.trim().is_empty() {
            return;
        }
        let provider = self.controller.read(cx).provider();
        match mention::parse(&text) {
            Some(m) if m.handle.provider() != provider => {
                self.hint = Some(
                    format!(
                        "Switch to {} in the header to mention @{}.",
                        m.handle.provider().agent_name(),
                        m.handle.name()
                    )
                    .into(),
                );
                cx.notify();
                return;
            }
            // A bare "@claude": wait for the prompt.
            Some(m) if m.prompt.is_empty() => return,
            Some(m) => {
                let prompt = m.prompt.to_string();
                let chatgpt = m.handle == Handle::ChatGpt;
                self.controller.update(cx, |c, cx| c.spawn_as(prompt, chatgpt, cx));
            }
            None => {
                let text = text.to_string();
                self.controller.update(cx, |c, cx| c.send(text, cx));
            }
        }
        self.composer.update(cx, |state, cx| state.set_value("", window, cx));
    }

    /// Puts `@claude ` at the start of the composer, ready for a prompt.
    pub fn start_mention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = mention::handle(self.controller.read(cx).provider());
        let current = self.composer.read(cx).value();
        let current = current.trim();
        let mentioned = mention::parse(current).is_some_and(|m| m.prompt.len() < current.len());
        self.composer.update(cx, |state, cx| {
            if !mentioned {
                let text = if current.is_empty() { format!("@{handle} ") } else { format!("@{handle} {current}") };
                // Inserting (rather than setting) leaves the cursor at the end.
                state.set_value("", window, cx);
                state.insert(text, window, cx);
            }
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// The handles that could finish the `@…` being typed at the cursor
    /// (both `@codex` and `@chatgpt` after `@c`), while the composer has
    /// focus, with what was typed after the `@`.
    fn mention_suggestions(&self, window: &Window, cx: &App) -> Option<(String, Vec<Handle>)> {
        let state = self.composer.read(cx);
        if !state.focus_handle(cx).is_focused(window) {
            return None;
        }
        let text = state.value();
        let partial = mention::partial_before_cursor(&text, state.cursor())?;
        let handles = mention::completions(self.controller.read(cx).provider(), partial);
        (!handles.is_empty()).then(|| (partial.to_string(), handles))
    }

    /// Finishes the handle being typed with `pick` (else the highlighted
    /// suggestion). False when nothing is being typed.
    fn complete_mention(&mut self, pick: Option<Handle>, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((partial, handles)) = self.mention_suggestions(window, cx) else {
            return false;
        };
        let handle = pick.unwrap_or(handles[self.mention_selected.min(handles.len() - 1)]);
        let Some(rest) = mention::rest_of(handle, &partial) else {
            return false;
        };
        self.composer.update(cx, |state, cx| {
            state.insert(format!("{rest} "), window, cx);
            state.focus(window, cx);
        });
        self.mention_selected = 0;
        self.highlight_mention(cx);
        cx.notify();
        true
    }

    /// Moves the highlight in the suggestion list. False when it is closed.
    fn move_mention_selection(&mut self, down: bool, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some((_, handles)) = self.mention_suggestions(window, cx) else {
            return false;
        };
        let len = handles.len();
        self.mention_selected = if down {
            (self.mention_selected + 1) % len
        } else {
            (self.mention_selected + len - 1) % len
        };
        cx.notify();
        true
    }

    /// Colors the message's `@handle` (the one that will take effect).
    fn highlight_mention(&mut self, cx: &mut Context<Self>) {
        let provider = self.controller.read(cx).provider();
        let text = self.composer.read(cx).value();
        let highlights = match mention::parse(&text) {
            Some(m) if m.handle.provider() == provider => {
                let blue = mention_blue(tokens(cx).dark);
                let style = gpui::HighlightStyle {
                    color: Some(blue),
                    background_color: Some(blue.alpha(0.12)),
                    ..Default::default()
                };
                vec![(m.range, style)]
            }
            _ => Vec::new(),
        };
        self.composer.update(cx, |state, cx| state.set_text_highlights(highlights, cx));
    }

    fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_string());
        }
        cx.notify();
    }

    /// Opens a card (closing the open one) or closes it.
    fn toggle_card(&mut self, thread_id: &str, open: bool, cx: &mut Context<Self>) {
        self.controller.update(cx, |c, cx| {
            if open {
                c.start_draft(cx);
            } else {
                c.select_thread(thread_id, cx);
            }
        });
    }

    /// Scrolls the feed: a new thread lands at the bottom like a new message,
    /// a card just opened scrolls to its top, and otherwise the feed stays
    /// pinned to the bottom while output arrives, unless the user scrolled
    /// up to read.
    fn follow_output(&mut self, threads: usize, open: Option<&str>, revision: u64, open_ix: Option<usize>) {
        let seen = &self.seen;
        if seen.threads == threads && seen.open.as_deref() == open && seen.revision == revision {
            return;
        }
        let offset = -f32::from(self.scroll.offset().y);
        let max = f32::from(self.scroll.max_offset().height);
        if threads > seen.threads {
            self.scroll.scroll_to_bottom();
        } else if let (true, Some(ix)) = (seen.open.as_deref() != open, open_ix) {
            self.scroll.scroll_to_top_of_item(ix);
        } else if max - offset < FOLLOW_SLACK {
            self.scroll.scroll_to_bottom();
        }
        self.seen = FeedSeen { threads, open: open.map(str::to_string), revision };
    }

    /// Keeps the composer's placeholder in step with the provider and with
    /// whether a message replies or starts an agent.
    fn sync_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let controller = self.controller.read(cx);
        let key = (controller.provider(), controller.active_thread().is_some());
        if self.placeholder_for == Some(key) {
            return;
        }
        self.placeholder_for = Some(key);
        let handle = mention::handle(key.0);
        let text = match (key.0, key.1) {
            (Provider::OpenAi, true) => "Reply, or @codex / @chatgpt to start another".to_string(),
            (Provider::OpenAi, false) => "Message @codex, or @chatgpt to ask ChatGPT".to_string(),
            (_, true) => format!("Reply, or @{handle} to start another agent"),
            (_, false) => format!("Message @{handle} to start an agent"),
        };
        self.composer.update(cx, |state, cx| state.set_placeholder(text, window, cx));
    }

    // --- header ------------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.read(cx);
        let title: SharedString = controller
            .active_thread()
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "Agents".into());
        let name = controller.agent_name();
        let handle_name = mention::handle(controller.provider());
        let status: SharedString = match controller.state() {
            ConnectionState::Connected { user_agent } => format!("Connected · {user_agent}").into(),
            ConnectionState::Connecting => format!("Starting {name}…").into(),
            ConnectionState::Idle => "Not running".into(),
            ConnectionState::NotInstalled => format!("{name} not installed").into(),
            ConnectionState::Failed(err) => format!("Disconnected: {err}").into(),
        };
        let provider = controller.provider();
        let handle = self.controller.clone();
        let panel = cx.entity().downgrade();
        let t = tokens(cx).clone();
        let running = controller.running_count();
        let (label, label_color): (SharedString, Hsla) = match controller.state() {
            ConnectionState::Connected { .. } if running == 1 => ("Working".into(), t.accent_text),
            ConnectionState::Connected { .. } if running > 1 => (format!("{running} working").into(), t.accent_text),
            ConnectionState::Connected { .. } => ("Ready".into(), t.fg_subtle),
            ConnectionState::Connecting => ("Starting".into(), t.fg_subtle),
            ConnectionState::Idle => ("Stopped".into(), t.fg_subtle),
            ConnectionState::NotInstalled | ConnectionState::Failed(_) => ("Offline".into(), t.danger),
        };

        let full = self.full;
        let summary = self.diff_summary.1;
        let title_label = div()
            .flex_shrink()
            .min_w_0()
            .pl(px(6.))
            .truncate()
            .text_size(TEXT_TITLE)
            .font_weight(gpui::FontWeight::MEDIUM)
            .child(title.clone());
        let diff_button = Button::new("chat-diff-toggle")
            .icon(icon("file-diff"))
            .ghost()
            .small()
            .selected(self.diff_open)
            .tooltip(if self.diff_open { "Hide changes (⇧⌘D)" } else { "Show changes (⇧⌘D)" })
            .when(summary.files > 0, |b| b.label(format!("+{} −{}", summary.added, summary.removed)))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(AgentPanelEvent::ToggleDiff)));

        h_flex()
            .id("agent-header")
            .h(px(40.))
            .flex_none()
            .pl(px(8.))
            .pr(px(8.))
            .gap(px(4.))
            .border_b_1()
            .border_color(t.border)
            .when(full, |el| el.child(title_label))
            .when(!full, |el| el.child(
                // Jumps to a thread: opens its card in the feed.
                Button::new("thread-picker")
                    .label("Threads")
                    .ghost()
                    .small()
                    .dropdown_caret(true)
                    .dropdown_menu(move |mut menu, _, cx| {
                        let controller = handle.read(cx);
                        let active = controller.active_thread().map(|t| t.id.clone());
                        for thread in controller.threads().iter().take(30) {
                            let id = thread.id.clone();
                            let handle = handle.clone();
                            menu = menu.item(
                                PopupMenuItem::new(thread.title.clone())
                                    .checked(active.as_deref() == Some(id.as_str()))
                                    .on_click(move |_, _, cx| {
                                        handle.update(cx, |c, cx| c.select_thread(&id, cx));
                                    }),
                            );
                        }
                        if !controller.threads().is_empty() {
                            menu = menu.separator();
                        }
                        let new_panel = panel.clone();
                        menu = menu.item(PopupMenuItem::new(format!("New agent (@{handle_name})")).on_click(
                            move |_, window, cx| {
                                new_panel.update(cx, |p, cx| p.start_mention(window, cx)).ok();
                            },
                        ));
                        if let Some(id) = active {
                            let close = handle.clone();
                            menu = menu.item(PopupMenuItem::new("Close this thread").on_click(move |_, _, cx| {
                                close.update(cx, |c, cx| c.start_draft(cx));
                            }));
                            let handle = handle.clone();
                            menu = menu.item(PopupMenuItem::new("Archive this thread").on_click(move |_, _, cx| {
                                handle.update(cx, |c, cx| c.archive_thread(&id, cx));
                            }));
                        }
                        menu.max_h(px(420.)).scrollable(true)
                    }),
            ))
            .child(div().flex_1())
            .child(provider_menu(self.controller.clone(), provider))
            .child(
                h_flex()
                    .id("agent-status")
                    .gap(px(6.))
                    .px(px(4.))
                    .text_size(TEXT_SMALL)
                    .text_color(label_color)
                    .tooltip(chrome::tooltip(status))
                    .child(chrome::status_dot(label_color))
                    .child(label),
            )
            .when(!full, |el| el.child(
                Button::new("new-thread")
                    .icon(icon("plus"))
                    .ghost()
                    .xsmall()
                    .tooltip(format!("New agent: @{handle_name} (⇧⌘N)"))
                    .disabled(!matches!(self.controller.read(cx).state(), ConnectionState::Connected { .. }))
                    .on_click(cx.listener(|this, _, window, cx| this.start_mention(window, cx))),
            ))
            .when(full, |el| el.child(diff_button))
    }

    // --- connection states ---------------------------------------------------

    fn render_connection(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.controller.read(cx).state().clone();
        let provider = self.controller.read(cx).provider();
        let name = provider.agent_name();
        let muted = tokens(cx).fg_muted;
        let card = || notice_card(tokens(cx));
        let retry = |label: &'static str| {
            Button::new("agent-retry")
                .label(label)
                .small()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.update(cx, |c, cx| c.reconnect(cx));
                }))
        };
        let element = match state {
            ConnectionState::Connected { .. } => return None,
            ConnectionState::Connecting => h_flex()
                .m(px(14.))
                .gap(px(8.))
                .text_size(TEXT_BODY)
                .text_color(muted)
                .child(Spinner::new().small())
                .child(match provider {
                    Provider::OpenAi => "Starting the Codex app server…",
                    Provider::Anthropic => "Starting Claude Code…",
                })
                .into_any_element(),
            ConnectionState::Idle => card()
                .child(format!("{name} is not running."))
                .child(retry(match provider {
                    Provider::OpenAi => "Start Codex",
                    Provider::Anthropic => "Start Claude",
                }))
                .into_any_element(),
            ConnectionState::NotInstalled => card()
                .child(div().text_size(TEXT_TITLE).font_semibold().child(match provider {
                    Provider::OpenAi => "Codex CLI not found",
                    Provider::Anthropic => "Claude Code not found",
                }))
                .child(
                    div()
                        .text_color(muted)
                        .child("Install it, then press Retry. Hangar looks on your login shell's PATH and in the usual install locations, or you can set its path in settings."),
                )
                .child(
                    div()
                        .font_family(tokens(cx).mono.clone())
                        .text_size(TEXT_MONO)
                        .p(px(10.))
                        .rounded(RADIUS)
                        .bg(tokens(cx).raised)
                        .child(match provider {
                            Provider::OpenAi => "npm install -g @openai/codex\n# or\nbrew install codex",
                            Provider::Anthropic => {
                                "curl -fsSL https://claude.ai/install.sh | bash\n# or\nnpm install -g @anthropic-ai/claude-code"
                            }
                        }),
                )
                .child(retry("Retry"))
                .into_any_element(),
            ConnectionState::Failed(err) => card()
                .child(div().text_size(TEXT_TITLE).font_semibold().child(format!("{name} stopped")))
                .child(div().text_color(muted).child(err))
                .child(retry(match provider {
                    Provider::OpenAi => "Restart Codex",
                    Provider::Anthropic => "Restart Claude",
                }))
                .into_any_element(),
        };
        Some(element)
    }

    fn render_login(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let controller = self.controller.read(cx);
        let hangar_login = controller.hangar_login().cloned();
        if !controller.needs_login() && hangar_login.is_none() && controller.claude_login().is_none() {
            return None;
        }
        if controller.provider() == Provider::Anthropic {
            return Some(self.render_claude_login(cx));
        }
        let hangar = controller.uses_hangar_sign_in();
        let waiting = controller.login_state().cloned();
        let muted = tokens(cx).fg_muted;
        let mut card = notice_card(tokens(cx))
            .child(div().text_size(TEXT_TITLE).font_semibold().child("Sign in to Codex"))
            .child(div().text_color(muted).child("Use your ChatGPT plan or an OpenAI API key."));
        if let Some(login) = hangar_login {
            card = card
                .child(
                    h_flex()
                        .gap_2()
                        .text_color(muted)
                        .child(Spinner::new().small())
                        .child("Finish signing in in your browser…"),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .when_some(login.auth_url, |el, url| {
                            el.child(
                                Button::new("reopen-hangar-login")
                                    .label("Open the sign-in page again")
                                    .small()
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                        })
                        .child(
                            Button::new("cancel-hangar-login")
                                .label("Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |c, cx| c.cancel_hangar_sign_in(cx));
                                })),
                        ),
                );
            return Some(card.into_any_element());
        }
        // Hangar's sign-in leads; Codex's own (and an API key) stay available.
        let hangar_button = v_flex()
            .gap_1()
            .child(
                Button::new("login-hangar")
                    .label("Continue with ChatGPT")
                    .primary()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |c, cx| c.sign_in_with_hangar(cx));
                    })),
            )
            .child(
                div()
                    .text_size(TEXT_SMALL)
                    .text_color(muted)
                    .child("Hangar keeps the sign-in in your Keychain and hands Codex a token when it needs one."),
            );
        if hangar {
            card = card.child(hangar_button).child(
                Button::new("use-codex-login")
                    .label("Use Codex's own sign-in instead")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |c, cx| c.set_hangar_sign_in(false, cx));
                    })),
            );
            return Some(card.into_any_element());
        }
        match waiting {
            Some(LoginResponse::Chatgpt { auth_url, .. }) => {
                card = card
                    .child(
                        h_flex()
                            .gap_2()
                            .text_color(muted)
                            .child(Spinner::new().small())
                            .child("Finish signing in in your browser…"),
                    )
                    .child(
                        Button::new("reopen-login")
                            .label("Open the sign-in page again")
                            .small()
                            .on_click(move |_, _, cx| cx.open_url(&auth_url)),
                    );
            }
            Some(LoginResponse::ChatgptDeviceCode { verification_url, user_code, .. }) => {
                card = card.child(format!("Open {verification_url} and enter {user_code}."));
            }
            _ => {
                card = card
                    .child(hangar_button)
                    .child(
                        Button::new("login-chatgpt")
                            .label("Sign in through Codex instead")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |c, cx| c.login_with_chatgpt(cx));
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.api_key).small()))
                            .child(
                                Button::new("login-api-key")
                                    .label("Use API key")
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let key = this.api_key.read(cx).value().trim().to_string();
                                        if key.is_empty() {
                                            return;
                                        }
                                        this.api_key.update(cx, |state, cx| state.set_value("", window, cx));
                                        this.controller.update(cx, |c, cx| c.login_with_api_key(key, cx));
                                    })),
                            ),
                    );
            }
        }
        Some(card.into_any_element())
    }

    /// Claude Code signs in through its own interactive flow, so Hangar
    /// runs `claude auth login` in a terminal tab and never handles the
    /// credentials.
    fn render_claude_login(&self, cx: &mut Context<Self>) -> AnyElement {
        let controller = self.controller.read(cx);
        let terminal = controller.terminal_login();
        let background = controller.claude_login().cloned();
        let muted = tokens(cx).fg_muted;
        let mut card = notice_card(tokens(cx))
            .child(div().text_size(TEXT_TITLE).font_semibold().child("Sign in to Claude"))
            .child(div().text_color(muted).child(
                "Use your Claude plan, or an Anthropic Console account billed by API use. Claude Code signs in and keeps the credentials; Hangar never sees them.",
            ));
        if let Some(login) = background {
            let submit = cx.listener(|this, _, window, cx| {
                let code = this.claude_code.read(cx).value().trim().to_string();
                if code.is_empty() {
                    return;
                }
                this.claude_code.update(cx, |state, cx| state.set_value("", window, cx));
                this.controller.update(cx, |c, cx| c.submit_claude_code(code, cx));
            });
            card = card
                .child(
                    h_flex()
                        .gap_2()
                        .text_color(muted)
                        .child(Spinner::new().small())
                        .child("Finish signing in in your browser…"),
                )
                .child(
                    div()
                        .text_size(TEXT_SMALL)
                        .text_color(muted)
                        .child("If the page shows a code instead, paste it here."),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.claude_code).small()))
                        .child(Button::new("claude-submit-code").label("Continue").primary().small().on_click(submit)),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .when_some(login.url, |el, url| {
                            el.child(
                                Button::new("claude-reopen-login")
                                    .label("Open the sign-in page again")
                                    .small()
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                        })
                        .child(
                            Button::new("claude-cancel-login")
                                .label("Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |c, cx| c.cancel_claude_login(cx));
                                })),
                        ),
                );
        } else if terminal {
            card = card
                .child(
                    h_flex()
                        .gap_2()
                        .text_color(muted)
                        .child(Spinner::new().small())
                        .child("Finish signing in in the terminal below, then come back here."),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("claude-check-login")
                                .label("I've signed in")
                                .primary()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |c, cx| c.check_login(cx));
                                })),
                        )
                        .child(
                            Button::new("claude-cancel-terminal-login")
                                .label("Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |c, cx| c.cancel_terminal_login(cx));
                                })),
                        ),
                );
        } else {
            card = card
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("login-claude")
                                .label("Sign in with Claude")
                                .primary()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |c, cx| c.login_with_claude(false, cx));
                                })),
                        )
                        .child(
                            Button::new("login-console")
                                .label("Use an Anthropic Console account")
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |c, cx| c.login_with_claude(true, cx));
                                })),
                        ),
                )
                .child(div().text_size(TEXT_SMALL).text_color(muted).child(
                    "An ANTHROPIC_API_KEY, Bedrock or Vertex set up for Claude Code works too. Already signed in elsewhere? Check again.",
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("claude-recheck")
                                .label("Check again")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |c, cx| c.check_login(cx));
                                })),
                        )
                        .child(
                            Button::new("claude-terminal-login")
                                .label("Sign in in a terminal")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |c, cx| c.login_with_claude_in_terminal(false, cx));
                                })),
                        ),
                );
        }
        card.into_any_element()
    }

    // --- transcript ----------------------------------------------------------

    /// Every thread as a card, oldest first. Only the open card renders its
    /// transcript; the others show a cached one-line summary.
    fn render_feed(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let controller = self.controller.read(cx);
        let threads = controller.threads();
        let open_id = controller.active_thread().map(|t| t.id.as_str());
        let order = feed::order(threads, open_id);
        if order.is_empty() {
            return self.render_empty(cx).into_any_element();
        }
        self.cards.refresh(threads, &order);

        let mut views = Vec::with_capacity(order.len());
        let mut revision = 0u64;
        let mut open_ix = None;
        for thread in order.iter().map(|&ix| &threads[ix]) {
            revision = revision.wrapping_add(thread.transcript.revision());
            let Some(card) = self.cards.get(&thread.id) else { continue };
            let open = open_id == Some(thread.id.as_str());
            if open {
                open_ix = Some(views.len());
            }
            views.push(CardView {
                pos: views.len(),
                thread_id: card.thread_id.clone(),
                title: thread.title.clone(),
                status: feed::status(thread, card.needs_approval, controller.queued_for(&thread.id)),
                updated_at: thread.updated_at,
                preview: card.preview.clone(),
                meta: card.meta.clone(),
                open,
            });
        }
        // Copy only what the open card shows.
        let mut open_body = open_ix.and_then(|_| controller.active_thread()).map(|thread| OpenBody {
            entries: thread.transcript.entries().iter().map(displayed_part).collect(),
            loading: thread.loading,
            running: thread.transcript.is_running(),
        });
        let cwd = controller.cwd().to_path_buf();
        self.follow_output(order.len(), open_id, revision, open_ix);

        let now = now_ts();
        let mut cards = Vec::with_capacity(views.len());
        for view in views {
            let body = if view.open { open_body.take() } else { None };
            cards.push(self.render_card(view, body, &cwd, now, window, cx));
        }

        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                // The cards are the scroll container's direct children, so
                // an opened card can be scrolled to by index.
                v_flex()
                    .id("agent-feed")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px(px(10.))
                    .py(px(10.))
                    .gap(px(6.))
                    .when(self.full, |el| el.px(px(24.)).py(px(20.)).gap(px(8.)))
                    .children(cards),
            )
            .vertical_scrollbar(&self.scroll)
            .into_any_element()
    }

    /// A thread as a chat message: who, when and where it stands, its prompt
    /// and latest line when closed; its whole transcript when open.
    fn render_card(
        &self,
        view: CardView,
        body: Option<OpenBody>,
        cwd: &Path,
        now: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = tokens(cx).clone();
        let name = self.controller.read(cx).thread_agent_name(&view.thread_id);
        let open = view.open;
        let toggle_id = view.thread_id.clone();
        let stop_id = view.thread_id.clone();
        let working = matches!(view.status, CardStatus::Working | CardStatus::NeedsApproval);
        let hover_bg = t.raised;

        let top_line = h_flex()
            .gap(px(6.))
            .text_size(TEXT_SMALL)
            .child(div().flex_none().font_weight(gpui::FontWeight::MEDIUM).text_color(t.fg).child(name))
            .child(div().flex_none().text_color(t.fg_subtle).child(relative_time(now, view.updated_at)))
            .when_some(view.meta, |el, meta| {
                el.child(div().min_w_0().truncate().text_color(t.fg_subtle).child(meta))
            })
            .child(div().flex_1())
            .children(status_badge(view.status, &t))
            .when(working, |el| {
                el.child(
                    Button::new(("card-stop", view.pos))
                        .icon(icon("square"))
                        .ghost()
                        .xsmall()
                        .tooltip("Stop this agent")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.controller.update(cx, |c, cx| c.interrupt_thread(&stop_id, cx));
                        })),
                )
            });

        let header = h_flex()
            .id(("card-head", view.pos))
            .items_start()
            .gap(px(10.))
            .px(px(10.))
            .py(px(8.))
            .rounded(RADIUS_LG)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_card(&toggle_id, open, cx)))
            .child(agent_avatar(&t))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(top_line)
                    // Open, the transcript shows the prompt in full.
                    .when(!open, |el| {
                        el.child(div().truncate().text_size(TEXT_BODY).text_color(t.fg).child(view.title))
                            .when_some(view.preview, |el, preview| {
                                el.child(div().truncate().text_size(TEXT_SMALL).text_color(t.fg_muted).child(preview))
                            })
                    }),
            )
            .child(
                div().flex_none().pt(px(2.)).child(
                    gpui_component::Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                        .size(px(14.))
                        .text_color(t.fg_subtle),
                ),
            );

        let mut card = v_flex()
            .id(ElementId::Name(view.thread_id.clone()))
            .flex_none()
            .rounded(RADIUS_LG)
            .border_1()
            .border_color(if open { t.border_strong } else { t.border })
            .when(open, |el| el.bg(t.surface))
            .child(header);
        if let Some(body) = body {
            card = card.child(self.render_open_body(&view.thread_id, body, cwd, window, cx));
        }
        self.column(card)
    }

    fn render_open_body(
        &self,
        thread_id: &str,
        body: OpenBody,
        cwd: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = tokens(cx).clone();
        let mut list = v_flex()
            .px(px(12.))
            .pt(px(10.))
            .pb(px(12.))
            .gap(px(12.))
            .border_t_1()
            .border_color(t.border)
            .text_size(TEXT_BODY)
            .when(self.full, |el| el.px(px(20.)).gap(px(16.)));
        if body.loading {
            list = list.child(
                h_flex()
                    .gap_2()
                    .text_color(t.fg_muted)
                    .child(Spinner::new().small())
                    .child("Loading the conversation…"),
            );
        }
        for entry in &body.entries {
            list = list.child(self.render_entry(thread_id, entry, cwd, window, cx));
        }
        if body.running {
            list = list.child(
                h_flex()
                    .gap(px(8.))
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_muted)
                    .child(Spinner::new().xsmall().color(t.accent_text))
                    .child("Working…"),
            );
        }
        list.into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx);
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .px(px(32.))
            .text_center()
            .text_size(TEXT_BODY)
            .text_color(t.fg_muted)
            .child(
                div()
                    .size(px(36.))
                    .mb(px(4.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(10.))
                    .bg(t.accent_soft)
                    .text_color(t.accent_text)
                    .child(icon("sparkles").size(px(18.))),
            )
            .child(
                div()
                    .text_size(TEXT_TITLE)
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(t.fg)
                    .child(format!("Ask {} about this workspace", self.controller.read(cx).agent_name())),
            )
            .child(div().max_w(px(if self.full { 420. } else { 300. })).child(match self.controller.read(cx).provider() {
                Provider::OpenAi => {
                    "It reads and edits files, runs commands in its sandbox and, with Browser on, looks at pages in the built-in browser."
                }
                Provider::Anthropic => {
                    "It reads and edits files, runs commands with your approval and, with Browser on, looks at pages in the built-in browser."
                }
            }))
            .child(
                div()
                    .max_w(px(if self.full { 420. } else { 300. }))
                    .text_color(t.fg_subtle)
                    .child(format!(
                        "Each @{} message starts its own agent, so several can work at once.",
                        mention::handle(self.controller.read(cx).provider())
                    )),
            )
    }

    fn render_entry(
        &self,
        thread_id: &str,
        entry: &Entry,
        cwd: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = tokens(cx).clone();
        let muted = t.fg_muted;
        let expanded = self.expanded.contains(&entry.id);
        let entry_id = entry.id.clone();
        match &entry.kind {
            EntryKind::User { text } => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative_px(0.88))
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(RADIUS_LG)
                        .bg(t.raised)
                        .text_size(TEXT_BODY)
                        .child(text.clone()),
                )
                .into_any_element(),
            EntryKind::Agent { text } => div()
                .text_size(TEXT_BODY)
                .child(TextView::markdown(SharedString::from(format!("md-{}", entry.id)), text.clone(), window, cx).selectable(true))
                .into_any_element(),
            EntryKind::Reasoning { text } => {
                let shown = if expanded || entry.streaming {
                    text.clone()
                } else {
                    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                    let first = first.trim_matches('*');
                    format!("{} …", cockpit_common::truncate_chars(first, 120))
                };
                div()
                    .id(SharedString::from(format!("reasoning-{}", entry.id)))
                    .pl(px(10.))
                    .border_l_2()
                    .border_color(t.border)
                    .text_size(TEXT_SMALL)
                    .italic()
                    .text_color(t.fg_subtle)
                    .cursor_pointer()
                    .hover(|s| s.text_color(muted))
                    .child(shown)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(&entry_id, cx)))
                    .into_any_element()
            }
            EntryKind::Plan { text } => v_flex()
                .gap(px(6.))
                .child(caps_label("Plan", cx))
                .child(div().text_size(TEXT_BODY).child(text.clone()))
                .into_any_element(),
            EntryKind::Command { command, cwd: dir, output, status, exit_code, duration_ms } => {
                let (badge, badge_color) = command_badge(*status, *exit_code, *duration_ms, cx);
                let lines: Vec<&str> = output.lines().collect();
                let shown = if expanded {
                    tail_chars(output, MAX_OUTPUT_CHARS).to_string()
                } else {
                    let start = lines.len().saturating_sub(COLLAPSED_OUTPUT_LINES);
                    lines[start..].join("\n")
                };
                let more = !expanded && lines.len() > COLLAPSED_OUTPUT_LINES;
                activity_card(&t)
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("cmd-{}", entry.id)))
                            .gap(px(8.))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(&entry_id, cx)))
                            .child(div().flex_none().text_color(t.fg_subtle).child("run"))
                            .child(div().flex_1().min_w_0().truncate().text_color(t.fg).child(command.clone()))
                            .when(*status == ItemStatus::InProgress, |el| {
                                el.child(Spinner::new().xsmall().color(t.accent_text))
                            })
                            .child(div().flex_none().text_color(badge_color).child(badge)),
                    )
                    .when(!dir.is_empty() && expanded, |el| {
                        el.child(div().text_color(t.fg_subtle).child(format!("in {dir}")))
                    })
                    .when(!shown.is_empty(), |el| {
                        el.child(
                            div()
                                .p(px(8.))
                                .rounded(px(5.))
                                .bg(t.surface)
                                .border_1()
                                .border_color(t.border)
                                .text_color(muted)
                                .whitespace_normal()
                                .child(shown),
                        )
                    })
                    .when(more, |el| {
                        el.child(div().text_color(t.fg_subtle).child(format!(
                            "{} more lines · click the command to expand",
                            lines.len() - COLLAPSED_OUTPUT_LINES
                        )))
                    })
                    .into_any_element()
            }
            EntryKind::FileChange { files, status } => {
                let verb = match status {
                    ItemStatus::InProgress => "editing",
                    ItemStatus::Declined => "declined",
                    ItemStatus::Failed => "failed",
                    _ => "edit",
                };
                let mut card = v_flex().gap(px(4.));
                for (ix, file) in files.iter().enumerate() {
                    card = card.child(self.render_changed_file(&entry.id, ix, verb, *status, file, cwd, cx));
                }
                card.into_any_element()
            }
            EntryKind::Tool { label, detail, success } => {
                let (mark, color) = match success {
                    Some(true) => ("done", t.success),
                    Some(false) => ("failed", t.danger),
                    None => ("…", t.fg_subtle),
                };
                activity_card(&t)
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(div().flex_none().text_color(t.fg).child(label.clone()))
                            .when_some(detail.clone(), |el, detail| {
                                el.child(div().flex_1().min_w_0().truncate().text_color(muted).child(detail))
                            })
                            .when(detail.is_none(), |el| el.child(div().flex_1()))
                            .child(div().flex_none().text_color(color).child(mark)),
                    )
                    .into_any_element()
            }
            EntryKind::Approval { request_id, kind, state } => {
                self.render_approval(thread_id, &entry.id, request_id, kind, *state, cx)
            }
            EntryKind::Error { message } => div()
                .px(px(12.))
                .py(px(8.))
                .rounded(RADIUS)
                .bg(t.danger.alpha(0.08))
                .border_1()
                .border_color(t.danger.alpha(0.3))
                .text_size(TEXT_BODY)
                .text_color(t.danger)
                .child(message.clone())
                .into_any_element(),
            EntryKind::Notice { text } => div()
                .w_full()
                .text_center()
                .text_size(TEXT_SMALL)
                .text_color(t.fg_subtle)
                .child(text.clone())
                .into_any_element(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_changed_file(
        &self,
        entry_id: &str,
        ix: usize,
        verb: &'static str,
        status: ItemStatus,
        file: &ChangedFile,
        cwd: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = tokens(cx).clone();
        let key = format!("{entry_id}/{ix}");
        let expanded = self.expanded.contains(&key);
        let path = cwd.join(&file.path);
        let (added, removed) = count_changes(&file.diff);
        let toggle_key = key.clone();
        let has_diff = !file.diff.is_empty();
        activity_card(&t)
            .child(
                h_flex()
                    .id(SharedString::from(format!("toggle-{key}")))
                    .gap(px(8.))
                    .when(has_diff, |el| {
                        el.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(&toggle_key, cx)))
                    })
                    .child(div().flex_none().text_color(t.fg_subtle).child(verb))
                    .child(
                        div()
                            .id(SharedString::from(format!("open-{key}")))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(t.fg)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(file.path.clone())
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(AgentPanelEvent::OpenFile(path.clone()));
                            })),
                    )
                    .when(status == ItemStatus::InProgress, |el| {
                        el.child(Spinner::new().xsmall().color(t.accent_text))
                    })
                    .when(added > 0, |el| el.child(div().flex_none().text_color(t.success).child(format!("+{added}"))))
                    .when(removed > 0, |el| el.child(div().flex_none().text_color(t.danger).child(format!("−{removed}"))))
                    .when(has_diff, |el| {
                        el.child(
                            gpui_component::Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                .size(px(12.))
                                .text_color(t.fg_subtle),
                        )
                    }),
            )
            .when(expanded, |el| el.child(render_diff(&file.diff, cx)))
            .into_any_element()
    }

    fn render_approval(
        &self,
        thread_id: &str,
        entry_id: &str,
        request_id: &RequestId,
        kind: &ApprovalKind,
        state: ApprovalState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = tokens(cx).clone();
        let (title, body, reason) = match kind {
            ApprovalKind::Command { command, cwd, reason } => (
                "Run this command?".to_string(),
                Some(format!(
                    "{}{}",
                    command.clone().unwrap_or_else(|| "(command not shown)".into()),
                    cwd.as_ref().map(|c| format!("\n  in {c}")).unwrap_or_default()
                )),
                reason.clone(),
            ),
            ApprovalKind::FileChange { reason, grant_root } => (
                "Apply these file changes?".to_string(),
                grant_root.as_ref().map(|root| format!("Also allow writes under {root} for this session")),
                reason.clone(),
            ),
            ApprovalKind::Tool { tool, detail, reason } => (format!("Allow {tool}?"), detail.clone(), reason.clone()),
        };
        let outcome = match state {
            ApprovalState::Pending => None,
            ApprovalState::Accepted => Some(("Approved", t.success)),
            ApprovalState::AcceptedForSession => Some(("Approved for this session", t.success)),
            ApprovalState::Declined => Some(("Declined", t.danger)),
            ApprovalState::Expired => Some(("No longer needed", t.fg_subtle)),
        };
        let pending = outcome.is_none();

        let decide = |label: &'static str, suffix: &str, decision: ApprovalDecision| {
            let thread_id = thread_id.to_string();
            let request_id = request_id.clone();
            Button::new(SharedString::from(format!("{entry_id}-{suffix}")))
                .label(label)
                .xsmall()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.controller
                        .update(cx, |c, cx| c.resolve_approval(&thread_id, &request_id, decision, cx));
                }))
        };

        attention_card(&t, pending)
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(icon("shield-check").size(px(14.)).text_color(if pending { t.warning } else { t.fg_subtle }))
                    .child(div().text_size(TEXT_BODY).font_semibold().child(title)),
            )
            .when_some(body, |el, body| {
                el.child(
                    div()
                        .p(px(8.))
                        .rounded(px(5.))
                        .bg(t.surface)
                        .border_1()
                        .border_color(t.border)
                        .font_family(t.mono.clone())
                        .text_size(TEXT_SMALL)
                        .child(body),
                )
            })
            .when_some(reason, |el, reason| el.child(div().text_size(TEXT_SMALL).text_color(t.fg_muted).child(reason)))
            .map(|el| match outcome {
                Some((label, color)) => el.child(div().text_size(TEXT_SMALL).text_color(color).child(label)),
                None => el.child(
                    h_flex()
                        .gap(px(6.))
                        .child(decide("Approve", "accept", ApprovalDecision::Accept).primary())
                        .child(decide("Approve for session", "session", ApprovalDecision::AcceptForSession))
                        .child(div().flex_1())
                        .child(decide("Decline", "decline", ApprovalDecision::Decline).ghost()),
                ),
            })
            .into_any_element()
    }

    // --- bottom ------------------------------------------------------------

    /// Browser actions waiting for the user, and the live plan.
    fn render_pending(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let controller = self.controller.read(cx);
        let agent_name = controller.agent_name();
        let t = tokens(cx).clone();
        let pending: Vec<(String, String)> = controller
            .pending_tools()
            .iter()
            .map(|p| (p.action.describe(), p.reason.clone()))
            .collect();
        let plan: Vec<(String, PlanStepStatus)> = controller
            .active_thread()
            .filter(|t| t.transcript.is_running())
            .map(|t| t.transcript.plan().0.iter().map(|s| (s.step.clone(), s.status)).collect())
            .unwrap_or_default();
        if pending.is_empty() && plan.is_empty() {
            return None;
        }
        let mut column = v_flex()
            .px(px(14.))
            .pt(px(12.))
            .pb(px(4.))
            .gap(px(10.))
            .when(!self.full, |el| el.border_t_1().border_color(t.border));
        if !plan.is_empty() {
            let mut steps = v_flex().gap(px(7.)).child(caps_label("Plan", cx));
            for (step, status) in plan {
                steps = steps.child(
                    h_flex()
                        .gap(px(8.))
                        .text_size(TEXT_BODY)
                        .child(plan_marker(status, &t))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(match status {
                                    PlanStepStatus::Completed => t.fg_muted,
                                    PlanStepStatus::InProgress => t.fg,
                                    _ => t.fg_muted,
                                })
                                .child(step),
                        ),
                );
            }
            column = column.child(steps);
        }
        for (ix, (action, reason)) in pending.into_iter().enumerate() {
            column = column.child(
                attention_card(&t, true)
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(gpui_component::Icon::new(IconName::Globe).size(px(14.)).text_color(t.warning))
                            .child(div().text_size(TEXT_BODY).font_semibold().child("Browser action needs approval")),
                    )
                    .child(div().text_size(TEXT_SMALL).child(format!("{agent_name} wants to {action}.")))
                    .child(div().text_size(TEXT_SMALL).text_color(t.fg_muted).child(reason))
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .child(
                                Button::new(("tool-allow", ix))
                                    .label("Allow")
                                    .primary()
                                    .xsmall()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.controller.update(cx, |c, cx| c.answer_tool(ix, true, cx));
                                    })),
                            )
                            .child(
                                Button::new(("tool-deny", ix))
                                    .label("Deny")
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.controller.update(cx, |c, cx| c.answer_tool(ix, false, cx));
                                    })),
                            ),
                    ),
            );
        }
        Some(self.column(column))
    }

    fn render_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let suggestions = self.mention_suggestions(window, cx);
        let controller = self.controller.read(cx);
        let connected = matches!(controller.state(), ConnectionState::Connected { .. }) && !controller.needs_login();
        let running = controller.active_is_running();
        let queued = controller.queued();
        let replying_to = controller.active_thread().map(|t| t.title.clone());
        let hint = self.hint.clone();
        let empty = self.composer.read(cx).value().trim().is_empty();
        let focused = self.composer.read(cx).focus_handle(cx).is_focused(window);
        let t = tokens(cx).clone();

        let model_label: SharedString = match (controller.model(), controller.effective_model()) {
            (_, Some(model)) => model.display_name.clone().into(),
            (Some(id), None) => id.to_string().into(),
            (None, None) => "Default model".into(),
        };
        let efforts: Vec<String> = controller
            .effective_model()
            .map(|m| m.supported_reasoning_efforts.iter().map(|e| e.reasoning_effort.clone()).collect())
            .unwrap_or_default();
        let effort_label: SharedString = controller
            .effort()
            .map(str::to_string)
            .or_else(|| controller.effective_model().map(|m| m.default_reasoning_effort.clone()))
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "effort".into())
            .into();
        let permissions = controller.permissions();
        let browser_tools = controller.browser_tools();
        let browser_tooltip: SharedString =
            format!("Let {} use the built-in browser (applies to new threads)", controller.agent_name()).into();
        let usage_today = controller.usage_today();
        let context = controller.active_thread().and_then(|t| {
            let usage = t.transcript.usage()?;
            let window = usage.model_context_window.filter(|w| *w > 0)?;
            Some((usage.last.total_tokens as f64 / window as f64 * 100.0).round() as i64)
        });

        let handle = self.controller.clone();
        let model_menu = Button::new("model-picker")
            .label(model_label)
            .ghost()
            .xsmall()
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, cx| {
                let controller = handle.read(cx);
                let current = controller.model().map(str::to_string);
                let default_handle = handle.clone();
                menu = menu.item(
                    PopupMenuItem::new(match controller.provider() {
                        Provider::OpenAi => "Server default",
                        Provider::Anthropic => "Default (Claude Opus 5.5)",
                    })
                        .checked(current.is_none())
                        .on_click(move |_, _, cx| default_handle.update(cx, |c, cx| c.set_model(None, cx))),
                );
                for model in controller.models().iter().filter(|m| !m.hidden) {
                    let id = model.id.clone();
                    let handle = handle.clone();
                    menu = menu.item(
                        PopupMenuItem::new(model.display_name.clone())
                            .checked(current.as_deref() == Some(id.as_str()))
                            .on_click(move |_, _, cx| handle.update(cx, |c, cx| c.set_model(Some(id.clone()), cx))),
                    );
                }
                menu.max_h(px(360.)).scrollable(true)
            });

        let handle = self.controller.clone();
        let effort_menu = Button::new("effort-picker")
            .label(effort_label)
            .ghost()
            .xsmall()
            .dropdown_caret(true)
            .disabled(efforts.is_empty())
            .dropdown_menu(move |mut menu, _, cx| {
                let current = handle.read(cx).effort().map(str::to_string);
                let default_handle = handle.clone();
                menu = menu.item(
                    PopupMenuItem::new("Model default")
                        .checked(current.is_none())
                        .on_click(move |_, _, cx| default_handle.update(cx, |c, cx| c.set_effort(None, cx))),
                );
                for effort in &efforts {
                    let value = effort.clone();
                    let handle = handle.clone();
                    menu = menu.item(
                        PopupMenuItem::new(effort.clone())
                            .checked(current.as_deref() == Some(effort.as_str()))
                            .on_click(move |_, _, cx| handle.update(cx, |c, cx| c.set_effort(Some(value.clone()), cx))),
                    );
                }
                menu
            });

        let handle = self.controller.clone();
        let permissions_menu = Button::new("permissions-picker")
            .icon(icon("shield-check"))
            .label(permissions.label())
            .ghost()
            .xsmall()
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, cx| {
                let current = handle.read(cx).permissions();
                let provider = handle.read(cx).provider();
                for preset in PermissionPreset::ALL {
                    let handle = handle.clone();
                    menu = menu.item(
                        PopupMenuItem::new(permission_description(preset, provider))
                            .checked(current == preset)
                            .on_click(move |_, _, cx| handle.update(cx, |c, cx| c.set_permissions(preset, cx))),
                    );
                }
                menu.min_w(px(300.))
            });

        let action_button = if running && empty {
            Button::new("agent-stop")
                .icon(icon("square"))
                .small()
                .tooltip("Stop (⌃C)")
                .on_click(cx.listener(|this, _, _, cx| this.controller.update(cx, |c, cx| c.interrupt(cx))))
        } else {
            Button::new("agent-send")
                .icon(icon("arrow-up"))
                .primary()
                .small()
                .tooltip(if running { "Queue after the running turn (↩)" } else { "Send (↩)" })
                .disabled(empty || !connected)
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };

        let composer = v_flex()
            .flex_none()
            .px(px(12.))
            .pt(px(8.))
            .pb(px(10.))
            .gap(px(6.))
            .when(self.full, |el| el.px(px(20.)).pb(px(16.)))
            .when_some(suggestions, |el, (_, handles)| el.child(self.render_mention_picker(&handles, cx)))
            .child(
                v_flex()
                    .rounded(RADIUS_LG)
                    .border_1()
                    .border_color(if focused { t.accent.alpha(0.55) } else { t.border_strong })
                    .bg(t.surface)
                    .when(!t.dark, |el| el.shadow_xs())
                    // Where a message without a mention goes.
                    .when_some(replying_to, |el, title| {
                        el.child(
                            h_flex()
                                .pl(px(12.))
                                .pr(px(6.))
                                .pt(px(5.))
                                .gap(px(5.))
                                .text_size(TEXT_SMALL)
                                .text_color(t.fg_subtle)
                                .child(div().flex_none().child("Replying to"))
                                .child(div().min_w_0().truncate().text_color(t.fg_muted).child(title))
                                .child(div().flex_1())
                                .child(
                                    Button::new("composer-close-thread")
                                        .icon(icon("x"))
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Close this thread")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.controller.update(cx, |c, cx| c.start_draft(cx));
                                        })),
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .items_start()
                            .pl(px(6.))
                            .pr(px(10.))
                            .pt(px(4.))
                            .child(
                                div().flex_1().text_size(TEXT_BODY).child(
                                    Input::new(&self.composer)
                                        .appearance(false)
                                        .disabled(!connected),
                                ),
                            )
                            // The shortcut that brings you here, while you are elsewhere.
                            .when(empty && !focused, |el| el.child(div().pt(px(7.)).child(key_chip("⌘L", cx)))),
                    )
                    .child(
                        h_flex()
                            .px(px(6.))
                            .pb(px(6.))
                            .gap(px(2.))
                            .child(div().flex_shrink().min_w_0().overflow_hidden().child(model_menu))
                            .child(effort_menu)
                            .child(permissions_menu)
                            .child(div().flex_1())
                            .child(
                                h_flex()
                                    .gap(px(5.))
                                    .text_color(if browser_tools { t.accent_text } else { t.fg_subtle })
                                    .child(gpui_component::Icon::new(IconName::Globe).size(px(13.)))
                                    .child(Switch::new("browser-tools")
                                    .checked(browser_tools)
                                    .xsmall()
                                    .tooltip(browser_tooltip)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        let checked = *checked;
                                        this.controller.update(cx, |c, cx| c.set_browser_tools(checked, cx));
                                    }))),
                            )
                            .child(div().w(px(6.)))
                            .child(action_button),
                    ),
            )
            .child(
                h_flex()
                    .px(px(4.))
                    .gap(px(6.))
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_subtle)
                    .child(format!("Today {} tokens", format_tokens(usage_today)))
                    .when_some(context, |el, pct| el.child(format!("· context {pct}%")))
                    .when(queued > 0, |el| el.child(format!("· {queued} queued")))
                    .child(div().flex_1())
                    .map(|el| match hint {
                        Some(hint) => el.child(div().min_w_0().truncate().text_color(t.warning).child(hint)),
                        None => el.child("↩ send · ⇧↩ newline"),
                    }),
            );
        self.column(composer)
    }

    /// The handles that can finish the `@…` being typed. Rows act on mouse
    /// down without taking focus from the composer, so a click lands.
    fn render_mention_picker(&self, handles: &[Handle], cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let selected = self.mention_selected.min(handles.len().saturating_sub(1));
        let blue = mention_blue(t.dark);
        let mut list = v_flex()
            .id("mention-picker")
            .p(px(4.))
            .gap(px(2.))
            .rounded(RADIUS_LG)
            .border_1()
            .border_color(t.border_strong)
            .bg(t.surface)
            .when(!t.dark, |el| el.shadow_xs());
        for (ix, handle) in handles.iter().copied().enumerate() {
            let is_selected = ix == selected;
            let hover_bg = t.raised;
            list = list.child(
                h_flex()
                    .id(("mention-option", ix))
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(RADIUS_LG)
                    .cursor_pointer()
                    .when(is_selected, |el| el.bg(t.raised))
                    .hover(move |s| s.bg(hover_bg))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            // Keep the composer focused, then finish the handle.
                            window.prevent_default();
                            cx.stop_propagation();
                            this.complete_mention(Some(handle), window, cx);
                        }),
                    )
                    .child(agent_avatar(&t))
                    .child(
                        div()
                            .flex_none()
                            .text_size(TEXT_BODY)
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(blue)
                            .child(format!("@{}", handle.name())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_muted)
                            .child(handle.description()),
                    )
                    .child(div().flex_1())
                    .when(is_selected, |el| el.child(key_chip("⇥", cx))),
            );
        }
        list
    }
}

/// The color of `@codex` / `@chatgpt` in the composer and suggestions.
fn mention_blue(dark: bool) -> Hsla {
    if dark {
        gpui::rgb(0x58a6ff).into()
    } else {
        gpui::rgb(0x0969da).into()
    }
}

impl Focusable for AgentPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.read(cx).focus_handle(cx)
    }
}

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_placeholder(window, cx);
        if self.full {
            self.refresh_diff_summary(cx);
        }
        let connection = self.render_connection(cx).map(|el| self.column(div().child(el)));
        let login = if connection.is_none() {
            self.render_login(cx).map(|el| self.column(div().child(el)))
        } else {
            None
        };
        let body = if connection.is_some() || login.is_some() {
            None
        } else {
            Some(self.render_feed(window, cx))
        };

        v_flex()
            .key_context("AgentComposer")
            .size_full()
            .text_color(tokens(cx).fg)
            .on_action(cx.listener(|this, _: &actions::InterruptAgent, _, cx| {
                this.controller.update(cx, |c, cx| c.interrupt(cx));
            }))
            // Enter finishes an @handle being typed, else sends; Shift-Enter
            // (bound to the secondary Enter) falls through to the input and
            // inserts a newline.
            .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                if !action.secondary && this.composer.read(cx).focus_handle(cx).is_focused(window) {
                    cx.stop_propagation();
                    if !this.complete_mention(None, window, cx) {
                        this.submit(window, cx);
                    }
                }
            }))
            // While the @ suggestions are open: Tab accepts, ↑↓ choose.
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this.complete_mention(None, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if this.move_mention_selection(false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if this.move_mention_selection(true, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(self.render_header(cx))
            .children(connection)
            .children(login)
            .map(|el| match body {
                Some(body) => el.child(body),
                // Keep the composer at the bottom under a status card.
                None => el.child(div().flex_1()),
            })
            .children(self.render_pending(cx))
            .child(self.render_composer(window, cx))
    }
}

// --- helpers -----------------------------------------------------------------

/// What the feed showed last frame.
#[derive(Default)]
struct FeedSeen {
    threads: usize,
    open: Option<String>,
    /// Sum of the shown transcripts' revisions: changes with any output.
    revision: u64,
}

/// One card's data, copied out of the controller before drawing.
struct CardView {
    /// Position in the feed, which is also the scroll child index.
    pos: usize,
    thread_id: SharedString,
    title: SharedString,
    status: CardStatus,
    updated_at: i64,
    preview: Option<SharedString>,
    meta: Option<SharedString>,
    open: bool,
}

/// The open card's transcript, cut to what is displayed.
struct OpenBody {
    entries: Vec<Entry>,
    loading: bool,
    running: bool,
}

/// The agent's picture beside its messages.
fn agent_avatar(t: &Tokens) -> gpui::Div {
    div()
        .flex_none()
        .size(px(24.))
        .rounded(px(7.))
        .flex()
        .items_center()
        .justify_center()
        .bg(t.accent_soft)
        .text_color(t.accent_text)
        .child(icon("sparkles").size(px(13.)))
}

/// A card's state at a glance; nothing for a thread that has not run.
fn status_badge(status: CardStatus, t: &Tokens) -> Option<AnyElement> {
    let (label, color) = match status {
        CardStatus::NeedsApproval => ("Needs approval", t.warning),
        CardStatus::Working => ("Working", t.accent_text),
        CardStatus::Queued => ("Queued", t.fg_subtle),
        CardStatus::Loading => ("Loading", t.fg_subtle),
        CardStatus::Done => ("Done", t.fg_subtle),
        CardStatus::Stopped => ("Stopped", t.fg_subtle),
        CardStatus::Failed => ("Failed", t.danger),
        CardStatus::Idle => return None,
    };
    let row = h_flex().flex_none().gap(px(5.)).text_size(TEXT_SMALL).text_color(color);
    let row = match status {
        CardStatus::Working | CardStatus::Loading => row.child(Spinner::new().xsmall().color(color)),
        CardStatus::NeedsApproval => row.child(icon("shield-check").size(px(12.))),
        CardStatus::Done => row.child(chrome::status_dot(t.success)),
        _ => row.child(chrome::status_dot(color)),
    };
    Some(row.child(label).into_any_element())
}

/// A quiet box for one step of agent activity: commands, edits, tools.
fn activity_card(t: &Tokens) -> gpui::Div {
    v_flex()
        .px(px(10.))
        .py(px(7.))
        .gap(px(6.))
        .rounded(RADIUS)
        .bg(t.raised)
        .font_family(t.mono.clone())
        .text_size(px(11.5))
}

/// A box that asks for a decision; it calms down once answered.
fn attention_card(t: &Tokens, pending: bool) -> gpui::Div {
    v_flex()
        .p(px(12.))
        .gap(px(8.))
        .rounded(RADIUS_LG)
        .border_1()
        .when(pending, |el| el.border_color(t.warning.alpha(0.45)).bg(t.warning.alpha(0.06)))
        .when(!pending, |el| el.border_color(t.border))
}

/// A card for connection and sign-in states.
fn notice_card(t: &Tokens) -> gpui::Div {
    v_flex()
        .m(px(14.))
        .p(px(14.))
        .gap(px(10.))
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(t.border)
        .text_size(TEXT_BODY)
}

/// The check circle in front of a plan step.
fn plan_marker(status: PlanStepStatus, t: &Tokens) -> impl IntoElement {
    let base = div().flex_none().size(px(14.)).rounded_full().flex().items_center().justify_center();
    match status {
        PlanStepStatus::Completed => base
            .bg(t.accent)
            .text_color(t.accent_fg)
            .child(gpui_component::Icon::new(IconName::Check).size(px(9.))),
        PlanStepStatus::InProgress => base
            .border_1()
            .border_color(t.accent)
            .child(div().size(px(6.)).rounded_full().bg(t.accent)),
        _ => base.border_1().border_color(t.border_strong),
    }
}

fn relative_px(fraction: f32) -> gpui::Length {
    gpui::relative(fraction).into()
}

fn command_badge(
    status: ItemStatus,
    exit_code: Option<i32>,
    duration_ms: Option<i64>,
    cx: &App,
) -> (String, Hsla) {
    let duration = duration_ms
        .map(|ms| if ms >= 1000 { format!(" · {:.1}s", ms as f64 / 1000.0) } else { format!(" · {ms}ms") })
        .unwrap_or_default();
    let t = tokens(cx);
    match (status, exit_code) {
        (ItemStatus::InProgress, _) => ("running".into(), t.fg_subtle),
        (ItemStatus::Declined, _) => ("declined".into(), t.danger),
        (_, Some(0)) => (format!("ok{duration}"), t.fg_subtle),
        (_, Some(code)) => (format!("exit {code}{duration}"), t.danger),
        (ItemStatus::Failed, None) => ("failed".into(), t.danger),
        _ => (format!("done{duration}"), t.fg_subtle),
    }
}

fn render_diff(diff: &str, cx: &App) -> AnyElement {
    let t = tokens(cx);
    let mut lines = v_flex()
        .py(px(4.))
        .rounded(px(5.))
        .bg(t.surface)
        .border_1()
        .border_color(t.border)
        .overflow_hidden();
    for line in tail_chars(diff, MAX_OUTPUT_CHARS).lines() {
        let (color, bg) = if line.starts_with("+++") || line.starts_with("---") {
            (t.fg_subtle, None)
        } else if line.starts_with('+') {
            (t.fg, Some(t.added_bg()))
        } else if line.starts_with('-') {
            (t.fg, Some(t.removed_bg()))
        } else if line.starts_with("@@") {
            (t.accent_text, None)
        } else {
            (t.fg_muted, None)
        };
        lines = lines.child(
            div()
                .px(px(8.))
                .text_color(color)
                .when_some(bg, |el, bg| el.bg(bg))
                .child(if line.is_empty() { " ".to_string() } else { line.to_string() }),
        );
    }
    lines.into_any_element()
}

/// An entry with long command output and diffs cut to what is displayed.
fn displayed_part(entry: &Entry) -> Entry {
    let kind = match &entry.kind {
        EntryKind::Command { command, cwd, output, status, exit_code, duration_ms } => EntryKind::Command {
            command: command.clone(),
            cwd: cwd.clone(),
            output: tail_chars(output, MAX_OUTPUT_CHARS).to_string(),
            status: *status,
            exit_code: *exit_code,
            duration_ms: *duration_ms,
        },
        EntryKind::FileChange { files, status } => EntryKind::FileChange {
            files: files
                .iter()
                .map(|file| ChangedFile {
                    path: file.path.clone(),
                    change: file.change.clone(),
                    diff: tail_chars(&file.diff, MAX_OUTPUT_CHARS).to_string(),
                })
                .collect(),
            status: *status,
        },
        other => other.clone(),
    };
    Entry {
        id: entry.id.clone(),
        turn_id: entry.turn_id.clone(),
        kind,
        streaming: entry.streaming,
    }
}

/// The last `max` bytes of `text`, cut at a character boundary.
fn tail_chars(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn permission_description(preset: PermissionPreset, provider: Provider) -> String {
    let detail = match (provider, preset) {
        (_, PermissionPreset::ReadOnly) => "reads files; asks before edits and commands",
        (Provider::OpenAi, PermissionPreset::Auto) => "edits the workspace and runs sandboxed commands",
        // Claude Code has no sandbox here, so commands still ask.
        (Provider::Anthropic, PermissionPreset::Auto) => "edits files without asking; asks before commands",
        (Provider::OpenAi, PermissionPreset::FullAccess) => "no sandbox, never asks",
        (Provider::Anthropic, PermissionPreset::FullAccess) => "never asks (bypasses permissions)",
    };
    format!("{} — {detail}", preset.label())
}

/// The OpenAI / Anthropic switch in the panel header.
fn provider_menu(handle: Entity<AgentController>, current: Provider) -> impl IntoElement {
    Button::new("provider-picker")
        .label(current.agent_name())
        .ghost()
        .xsmall()
        .dropdown_caret(true)
        .tooltip("Switch agents, or manage the sign-in")
        .dropdown_menu(move |mut menu, _, cx| {
            for provider in Provider::ALL {
                let handle = handle.clone();
                menu = menu.item(
                    PopupMenuItem::new(format!("{} · {}", provider.agent_name(), provider.company()))
                        .checked(provider == current)
                        .on_click(move |_, _, cx| handle.update(cx, |c, cx| c.set_provider(provider, cx))),
                );
            }
            // The current agent's account, and a way out of it.
            let controller = handle.read(cx);
            let hangar = controller.uses_hangar_sign_in();
            let account = controller.account_label();
            if account.is_some() || current == Provider::OpenAi {
                menu = menu.separator();
            }
            if let Some(account) = account {
                menu = menu.label(account);
                if controller.can_sign_out() {
                    let handle = handle.clone();
                    let name = if hangar { "ChatGPT" } else { current.agent_name() };
                    menu = menu.item(
                        PopupMenuItem::new(format!("Sign out of {name}"))
                            .on_click(move |_, _, cx| handle.update(cx, |c, cx| c.sign_out(cx))),
                    );
                }
            }
            // Codex can run on Hangar's ChatGPT sign-in (Keychain) or its own.
            if current == Provider::OpenAi {
                let handle = handle.clone();
                let (label, use_hangar) = if hangar {
                    ("Use Codex's own sign-in", false)
                } else {
                    ("Use Hangar's ChatGPT sign-in", true)
                };
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .on_click(move |_, _, cx| handle.update(cx, |c, cx| c.set_hangar_sign_in(use_hangar, cx))),
                );
            }
            menu
        })
}

fn format_tokens(tokens: i64) -> String {
    match tokens {
        t if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1_000_000.0),
        t if t >= 1_000 => format!("{:.1}k", t as f64 / 1_000.0),
        t => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_respects_char_boundaries() {
        assert_eq!(tail_chars("héllo", 4), "llo");
        assert_eq!(tail_chars("abc", 10), "abc");
    }

    #[test]
    fn permission_wording_follows_the_provider() {
        // Claude Code has no command sandbox, so "Auto" must not promise one.
        assert!(permission_description(PermissionPreset::Auto, Provider::OpenAi).contains("sandboxed"));
        let claude = permission_description(PermissionPreset::Auto, Provider::Anthropic);
        assert!(claude.contains("asks before commands") && !claude.contains("sandbox"));
        assert!(permission_description(PermissionPreset::ReadOnly, Provider::Anthropic).starts_with("Read only"));
    }

    #[test]
    fn token_formatting() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_345), "12.3k");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }
}

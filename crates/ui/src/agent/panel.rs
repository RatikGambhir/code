//! The agent panel: thread picker, transcript, approvals and the composer.

use std::{collections::HashSet, path::PathBuf};

use cockpit_agents::{
    protocol::{ApprovalDecision, ItemStatus, LoginResponse, PlanStepStatus},
    rpc::RequestId,
    transcript::ChangedFile,
    ApprovalKind, ApprovalState, Entry, EntryKind, Provider,
};
use cockpit_orchestrator::PermissionPreset;
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Hsla, ScrollHandle, SharedString, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Enter, Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    switch::Switch,
    text::TextView,
    v_flex, Disableable as _, IconName, Selectable as _, Sizable as _, StyledExt as _,
};

use super::{AgentController, ConnectionState};
use crate::{
    actions,
    assets::icon,
    chat::diff::{chat_changes, count_changes, DiffSummary},
    chrome::{self, caps_label, key_chip},
    theme::{tokens, Tokens, RADIUS, RADIUS_LG, TEXT_BODY, TEXT_MONO, TEXT_SMALL, TEXT_TITLE},
};

/// Output shown for a command before the card is expanded.
const COLLAPSED_OUTPUT_LINES: usize = 6;
/// Output kept on screen for an expanded command (the tail).
const MAX_OUTPUT_CHARS: usize = 20_000;
/// The reading width of the conversation when it fills the window.
const CHAT_COLUMN_WIDTH: f32 = 780.;

pub enum AgentPanelEvent {
    OpenFile(PathBuf),
    /// The diff button in chat mode was pressed.
    ToggleDiff,
}

pub struct AgentPanel {
    controller: Entity<AgentController>,
    composer: Entity<InputState>,
    api_key: Entity<InputState>,
    scroll: ScrollHandle,
    /// Entries the user expanded (commands, diffs, reasoning).
    expanded: HashSet<String>,
    /// (thread, revision) last rendered, to follow new output.
    seen: (Option<String>, u64),
    /// The provider the composer's placeholder names.
    placeholder_for: Option<Provider>,
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
        let subscriptions = vec![
            cx.observe(&controller, |_, _, cx| cx.notify()),
            cx.subscribe(&composer, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        Self {
            controller,
            composer,
            api_key,
            scroll: ScrollHandle::new(),
            expanded: HashSet::new(),
            seen: (None, 0),
            placeholder_for: None,
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

    /// Totals of what the active thread changed, recomputed only when the
    /// transcript changes.
    fn refresh_diff_summary(&mut self, cx: &App) {
        let controller = self.controller.read(cx);
        let key = controller
            .active_thread()
            .map(|t| (Some(t.id.clone()), t.transcript.revision()))
            .unwrap_or((None, 0));
        if key == self.diff_summary.0 {
            return;
        }
        let summary = controller
            .active_thread()
            .map(|t| DiffSummary::of(&chat_changes(t.transcript.entries())))
            .unwrap_or_default();
        self.diff_summary = (key, summary);
    }

    /// Centers `el` in a reading-width column when the chat fills the window.
    fn column<E: Styled>(&self, el: E) -> E {
        if self.full {
            el.w_full().max_w(px(CHAT_COLUMN_WIDTH)).mx_auto()
        } else {
            el
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

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.controller.update(cx, |controller, cx| controller.send(text, cx));
        self.composer.update(cx, |state, cx| state.set_value("", window, cx));
        self.scroll.scroll_to_bottom();
    }

    fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_string());
        }
        cx.notify();
    }

    /// Keeps the transcript pinned to the bottom while new output arrives,
    /// unless the user scrolled up to read.
    fn follow_output(&mut self, cx: &App) {
        let controller = self.controller.read(cx);
        let current = controller
            .active_thread()
            .map(|t| (Some(t.id.clone()), t.transcript.revision()))
            .unwrap_or((None, 0));
        if current == self.seen {
            return;
        }
        let switched = current.0 != self.seen.0;
        let offset = -f32::from(self.scroll.offset().y);
        let max = f32::from(self.scroll.max_offset().height);
        if switched || max - offset < 48.0 {
            self.scroll.scroll_to_bottom();
        }
        self.seen = current;
    }

    /// Keeps "Ask Codex anything" / "Ask Claude anything" in step with the
    /// provider.
    fn sync_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let provider = self.controller.read(cx).provider();
        if self.placeholder_for == Some(provider) {
            return;
        }
        self.placeholder_for = Some(provider);
        let text = format!("Ask {} anything", provider.agent_name());
        self.composer.update(cx, |state, cx| state.set_placeholder(text, window, cx));
    }

    // --- header ------------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.read(cx);
        let title: SharedString = controller
            .active_thread()
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "New conversation".into());
        let name = controller.agent_name();
        let status: SharedString = match controller.state() {
            ConnectionState::Connected { user_agent } => format!("Connected · {user_agent}").into(),
            ConnectionState::Connecting => format!("Starting {name}…").into(),
            ConnectionState::Idle => "Not running".into(),
            ConnectionState::NotInstalled => format!("{name} not installed").into(),
            ConnectionState::Failed(err) => format!("Disconnected: {err}").into(),
        };
        let provider = controller.provider();
        let handle = self.controller.clone();
        let t = tokens(cx).clone();
        let running = controller.is_running();
        let (label, label_color): (SharedString, Hsla) = match controller.state() {
            ConnectionState::Connected { .. } if running => ("Working".into(), t.accent_text),
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
                Button::new("thread-picker")
                    .label(title)
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
                        let new_handle = handle.clone();
                        menu = menu.item(PopupMenuItem::new("New thread").on_click(move |_, _, cx| {
                            new_handle.update(cx, |c, cx| c.new_thread(cx));
                        }));
                        if let Some(id) = active {
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
                    .tooltip("New thread (⇧⌘N)")
                    .disabled(!matches!(self.controller.read(cx).state(), ConnectionState::Connected { .. }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |c, cx| c.new_thread(cx));
                    })),
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
        if !controller.needs_login() {
            return None;
        }
        if controller.provider() == Provider::Anthropic {
            return Some(self.render_claude_login(cx));
        }
        let waiting = controller.login_state().cloned();
        let muted = tokens(cx).fg_muted;
        let mut card = notice_card(tokens(cx))
            .child(div().text_size(TEXT_TITLE).font_semibold().child("Sign in to Codex"))
            .child(
                div()
                    .text_color(muted)
                    .child("Use your ChatGPT plan or an OpenAI API key. Credentials stay with Codex; Hangar never stores them."),
            );
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
                    .child(
                        Button::new("login-chatgpt")
                            .label("Sign in with ChatGPT")
                            .primary()
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
        let waiting = self.controller.read(cx).terminal_login();
        let muted = tokens(cx).fg_muted;
        let mut card = notice_card(tokens(cx))
            .child(div().text_size(TEXT_TITLE).font_semibold().child("Sign in to Claude"))
            .child(div().text_color(muted).child(
                "Use your Claude plan, or an Anthropic Console account billed by API use. Claude Code signs in and keeps the credentials; Hangar never sees them.",
            ));
        if waiting {
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
                            Button::new("claude-cancel-login")
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
                    Button::new("claude-recheck")
                        .label("Check again")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.update(cx, |c, cx| c.check_login(cx));
                        })),
                );
        }
        card.into_any_element()
    }

    // --- transcript ----------------------------------------------------------

    fn render_transcript(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let controller = self.controller.read(cx);
        let Some(thread) = controller.active_thread() else {
            return self.render_empty(cx).into_any_element();
        };
        let thread_id = thread.id.clone();
        let loading = thread.loading;
        let running = thread.transcript.is_running();
        // Rendering runs on every frame that changes anything (a cursor
        // blink included), so copy only what can be shown.
        let entries: Vec<Entry> = thread.transcript.entries().iter().map(displayed_part).collect();
        let cwd = controller.cwd().to_path_buf();

        let mut list = self.column(v_flex().px(px(14.)).py(px(14.)).gap(px(12.)).text_size(TEXT_BODY))
            .when(self.full, |el| el.px(px(24.)).py(px(24.)).gap(px(16.)));
        if loading {
            list = list.child(
                h_flex()
                    .gap_2()
                    .text_color(tokens(cx).fg_muted)
                    .child(Spinner::new().small())
                    .child("Loading the conversation…"),
            );
        }
        for entry in &entries {
            list = list.child(self.render_entry(&thread_id, entry, &cwd, window, cx));
        }
        if running {
            list = list.child(
                h_flex()
                    .gap(px(8.))
                    .text_size(TEXT_SMALL)
                    .text_color(tokens(cx).fg_muted)
                    .child(Spinner::new().xsmall().color(tokens(cx).accent_text))
                    .child("Working…"),
            );
        }

        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("agent-transcript")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(list),
            )
            .vertical_scrollbar(&self.scroll)
            .into_any_element()
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
        let mut column = v_flex().px(px(14.)).pt(px(12.)).pb(px(4.)).gap(px(10.));
        column = if self.full { self.column(column) } else { column.border_t_1().border_color(t.border) };
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
        Some(column.into_any_element())
    }

    fn render_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.read(cx);
        let connected = matches!(controller.state(), ConnectionState::Connected { .. }) && !controller.needs_login();
        let running = controller.is_running();
        let queued = controller.queued();
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

        self.column(v_flex())
            .flex_none()
            .px(px(12.))
            .pt(px(8.))
            .pb(px(10.))
            .gap(px(6.))
            .when(self.full, |el| el.px(px(20.)).pb(px(16.)))
            .child(
                v_flex()
                    .rounded(RADIUS_LG)
                    .border_1()
                    .border_color(if focused { t.accent.alpha(0.55) } else { t.border_strong })
                    .bg(t.surface)
                    .when(!t.dark, |el| el.shadow_xs())
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
                    .child("↩ send · ⇧↩ newline"),
            )
    }
}

impl Focusable for AgentPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.read(cx).focus_handle(cx)
    }
}

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_output(cx);
        self.sync_placeholder(window, cx);
        if self.full {
            self.refresh_diff_summary(cx);
        }
        let connection = self.render_connection(cx).map(|el| self.column(div()).child(el).into_any_element());
        let login = if connection.is_none() {
            self.render_login(cx).map(|el| self.column(div()).child(el).into_any_element())
        } else {
            None
        };
        let body = if connection.is_some() || login.is_some() {
            None
        } else {
            Some(self.render_transcript(window, cx))
        };

        v_flex()
            .key_context("AgentComposer")
            .size_full()
            .text_color(tokens(cx).fg)
            .on_action(cx.listener(|this, _: &actions::InterruptAgent, _, cx| {
                this.controller.update(cx, |c, cx| c.interrupt(cx));
            }))
            // Enter sends; Shift-Enter (bound to the secondary Enter) falls
            // through to the input and inserts a newline.
            .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                if !action.secondary && this.composer.read(cx).focus_handle(cx).is_focused(window) {
                    cx.stop_propagation();
                    this.submit(window, cx);
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
        .tooltip("Switch between OpenAI Codex and Anthropic Claude")
        .dropdown_menu(move |mut menu, _, _| {
            for provider in Provider::ALL {
                let handle = handle.clone();
                menu = menu.item(
                    PopupMenuItem::new(format!("{} · {}", provider.agent_name(), provider.company()))
                        .checked(provider == current)
                        .on_click(move |_, _, cx| handle.update(cx, |c, cx| c.set_provider(provider, cx))),
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

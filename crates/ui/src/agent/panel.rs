//! The agent panel: thread picker, transcript, approvals and the composer.

use std::{collections::HashSet, path::PathBuf};

use cockpit_agents::{
    protocol::{ApprovalDecision, ItemStatus, LoginResponse, PlanStepStatus},
    rpc::RequestId,
    transcript::ChangedFile,
    ApprovalKind, ApprovalState, Entry, EntryKind,
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
    v_flex, ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _,
};

use super::{AgentController, ConnectionState};
use crate::{actions, assets::icon, theme};

/// Output shown for a command before the card is expanded.
const COLLAPSED_OUTPUT_LINES: usize = 6;
/// Output kept on screen for an expanded command (the tail).
const MAX_OUTPUT_CHARS: usize = 20_000;

pub enum AgentPanelEvent {
    OpenFile(PathBuf),
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
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AgentPanelEvent> for AgentPanel {}

impl AgentPanel {
    pub fn new(controller: Entity<AgentController>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(2, 12)
                .placeholder("Ask Codex to change code… (Enter to send, Shift-Enter for a newline)")
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
            _subscriptions: subscriptions,
        }
    }

    pub fn controller(&self) -> &Entity<AgentController> {
        &self.controller
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

    // --- header ------------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.read(cx);
        let title: SharedString = controller
            .active_thread()
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "New conversation".into());
        let (dot, status): (Hsla, SharedString) = match controller.state() {
            ConnectionState::Connected { user_agent } => {
                (theme::added(cx), format!("Connected · {user_agent}").into())
            }
            ConnectionState::Connecting => (cx.theme().warning, "Starting Codex…".into()),
            ConnectionState::Idle => (cx.theme().muted_foreground, "Not running".into()),
            ConnectionState::NotInstalled => (cx.theme().danger, "Codex not installed".into()),
            ConnectionState::Failed(err) => (cx.theme().danger, format!("Disconnected: {err}").into()),
        };
        let handle = self.controller.clone();

        h_flex()
            .id("agent-header")
            .h(px(36.))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .id("agent-status")
                    .size(px(8.))
                    .rounded_full()
                    .bg(dot)
                    .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(status.clone()).build(window, cx)),
            )
            .child(
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
            )
            .child(div().flex_1())
            .child(
                Button::new("new-thread")
                    .icon(icon("plus"))
                    .ghost()
                    .small()
                    .tooltip("New thread (⇧⌘N)")
                    .disabled(!matches!(self.controller.read(cx).state(), ConnectionState::Connected { .. }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |c, cx| c.new_thread(cx));
                    })),
            )
    }

    // --- connection states ---------------------------------------------------

    fn render_connection(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.controller.read(cx).state().clone();
        let muted = cx.theme().muted_foreground;
        let card = || {
            v_flex()
                .m_3()
                .p_3()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .text_sm()
        };
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
                .m_3()
                .gap_2()
                .text_sm()
                .text_color(muted)
                .child(Spinner::new().small())
                .child("Starting the Codex app server…")
                .into_any_element(),
            ConnectionState::Idle => card()
                .child("Codex is not running.")
                .child(retry("Start Codex"))
                .into_any_element(),
            ConnectionState::NotInstalled => card()
                .child(div().font_semibold().child("Codex CLI not found"))
                .child(
                    div()
                        .text_color(muted)
                        .child("Install it, then press Retry. Cockpit looks on your login shell's PATH and in the usual install locations, or you can set its path in settings."),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_xs()
                        .p_2()
                        .rounded_sm()
                        .bg(cx.theme().muted)
                        .child("npm install -g @openai/codex\n# or\nbrew install codex"),
                )
                .child(retry("Retry"))
                .into_any_element(),
            ConnectionState::Failed(err) => card()
                .child(div().font_semibold().child("Codex stopped"))
                .child(div().text_color(muted).child(err))
                .child(retry("Restart Codex"))
                .into_any_element(),
        };
        Some(element)
    }

    fn render_login(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let controller = self.controller.read(cx);
        if !controller.needs_login() {
            return None;
        }
        let waiting = controller.login_state().cloned();
        let muted = cx.theme().muted_foreground;
        let mut card = v_flex()
            .m_3()
            .p_3()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .text_sm()
            .child(div().font_semibold().child("Sign in to Codex"))
            .child(
                div()
                    .text_color(muted)
                    .child("Use your ChatGPT plan or an OpenAI API key. Credentials stay with Codex; Cockpit never stores them."),
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

    // --- transcript ----------------------------------------------------------

    fn render_transcript(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let controller = self.controller.read(cx);
        let Some(thread) = controller.active_thread() else {
            return self.render_empty(cx).into_any_element();
        };
        let thread_id = thread.id.clone();
        let loading = thread.loading;
        let running = thread.transcript.is_running();
        let entries: Vec<Entry> = thread.transcript.entries().to_vec();
        let cwd = controller.cwd().to_path_buf();

        let mut list = v_flex().p_3().gap_3();
        if loading {
            list = list.child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
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
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(Spinner::new().xsmall())
                    .child("Codex is working…"),
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
        let muted = cx.theme().muted_foreground;
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .p_6()
            .text_center()
            .text_color(muted)
            .child(icon("sparkles").size(px(28.)))
            .child(div().text_color(cx.theme().foreground).child("Ask Codex anything about this workspace"))
            .child(div().text_sm().child(
                "It can read and edit files, run commands in its sandbox and, with Browser on, look at pages in the built-in browser.",
            ))
    }

    fn render_entry(
        &self,
        thread_id: &str,
        entry: &Entry,
        cwd: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let border = theme.border;
        let expanded = self.expanded.contains(&entry.id);
        let entry_id = entry.id.clone();
        match &entry.kind {
            EntryKind::User { text } => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative_px(0.9))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(theme.muted)
                        .text_sm()
                        .child(text.clone()),
                )
                .into_any_element(),
            EntryKind::Agent { text } => div()
                .text_sm()
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
                    .text_xs()
                    .italic()
                    .text_color(muted)
                    .cursor_pointer()
                    .child(shown)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(&entry_id, cx)))
                    .into_any_element()
            }
            EntryKind::Plan { text } => card(border)
                .child(section_title("Plan", muted))
                .child(div().text_sm().child(text.clone()))
                .into_any_element(),
            EntryKind::Command { command, cwd: dir, output, status, exit_code, duration_ms } => {
                let mono = theme.mono_font_family.clone();
                let (badge, badge_color) = command_badge(*status, *exit_code, *duration_ms, cx);
                let lines: Vec<&str> = output.lines().collect();
                let shown = if expanded {
                    tail_chars(output, MAX_OUTPUT_CHARS).to_string()
                } else {
                    let start = lines.len().saturating_sub(COLLAPSED_OUTPUT_LINES);
                    lines[start..].join("\n")
                };
                let more = !expanded && lines.len() > COLLAPSED_OUTPUT_LINES;
                card(border)
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("cmd-{}", entry.id)))
                            .gap_2()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(&entry_id, cx)))
                            .child(icon("terminal").small().text_color(muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(mono.clone())
                                    .text_xs()
                                    .child(command.clone()),
                            )
                            .when(*status == ItemStatus::InProgress, |el| el.child(Spinner::new().xsmall()))
                            .child(div().text_xs().text_color(badge_color).child(badge)),
                    )
                    .when(!dir.is_empty() && expanded, |el| {
                        el.child(div().text_xs().text_color(muted).child(format!("in {dir}")))
                    })
                    .when(!shown.is_empty(), |el| {
                        el.child(
                            div()
                                .p_2()
                                .rounded_sm()
                                .bg(theme.background)
                                .font_family(mono.clone())
                                .text_xs()
                                .text_color(muted)
                                .whitespace_normal()
                                .child(shown),
                        )
                    })
                    .when(more, |el| {
                        el.child(div().text_xs().text_color(muted).child(format!(
                            "{} more lines — click the command to expand",
                            lines.len() - COLLAPSED_OUTPUT_LINES
                        )))
                    })
                    .into_any_element()
            }
            EntryKind::FileChange { files, status } => {
                let mut card = card(border).child(
                    h_flex()
                        .gap_2()
                        .child(icon("file-diff").small().text_color(muted))
                        .child(section_title(
                            match status {
                                ItemStatus::InProgress => "Editing files",
                                ItemStatus::Declined => "Edit declined",
                                ItemStatus::Failed => "Edit failed",
                                _ => "Edited files",
                            },
                            muted,
                        )),
                );
                for (ix, file) in files.iter().enumerate() {
                    card = card.child(self.render_changed_file(&entry.id, ix, file, cwd, cx));
                }
                card.into_any_element()
            }
            EntryKind::Tool { label, detail, success } => {
                let (mark, color) = match success {
                    Some(true) => ("✓", theme::added(cx)),
                    Some(false) => ("✗", cx.theme().danger),
                    None => ("…", muted),
                };
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(icon("wrench").xsmall())
                    .child(div().text_color(cx.theme().foreground).child(label.clone()))
                    .when_some(detail.clone(), |el, detail| {
                        el.child(div().flex_1().min_w_0().truncate().child(detail))
                    })
                    .child(div().text_color(color).child(mark))
                    .into_any_element()
            }
            EntryKind::Approval { request_id, kind, state } => {
                self.render_approval(thread_id, &entry.id, request_id, kind, *state, cx)
            }
            EntryKind::Error { message } => div()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(theme.danger)
                .text_sm()
                .text_color(theme.danger)
                .child(message.clone())
                .into_any_element(),
            EntryKind::Notice { text } => div()
                .w_full()
                .text_center()
                .text_xs()
                .text_color(muted)
                .child(text.clone())
                .into_any_element(),
        }
    }

    fn render_changed_file(
        &self,
        entry_id: &str,
        ix: usize,
        file: &ChangedFile,
        cwd: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("{entry_id}/{ix}");
        let expanded = self.expanded.contains(&key);
        let muted = cx.theme().muted_foreground;
        let path = cwd.join(&file.path);
        let (added, removed) = diff_stats(&file.diff);
        let toggle_key = key.clone();
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .child(
                        Button::new(SharedString::from(format!("toggle-{key}")))
                            .icon(if expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                            .ghost()
                            .xsmall()
                            .disabled(file.diff.is_empty())
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(&toggle_key, cx))),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("open-{key}")))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(file.path.clone())
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(AgentPanelEvent::OpenFile(path.clone()));
                            })),
                    )
                    .child(div().text_color(muted).child(file.change.clone()))
                    .when(added > 0, |el| el.child(div().text_color(theme::added(cx)).child(format!("+{added}"))))
                    .when(removed > 0, |el| el.child(div().text_color(theme::deleted(cx)).child(format!("-{removed}")))),
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
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (title, body, reason) = match kind {
            ApprovalKind::Command { command, cwd, reason } => (
                "Run this command?",
                Some(format!(
                    "{}{}",
                    command.clone().unwrap_or_else(|| "(command not shown)".into()),
                    cwd.as_ref().map(|c| format!("\n  in {c}")).unwrap_or_default()
                )),
                reason.clone(),
            ),
            ApprovalKind::FileChange { reason, grant_root } => (
                "Apply these file changes?",
                grant_root.as_ref().map(|root| format!("Also allow writes under {root} for this session")),
                reason.clone(),
            ),
        };
        let outcome = match state {
            ApprovalState::Pending => None,
            ApprovalState::Accepted => Some(("Approved", theme::added(cx))),
            ApprovalState::AcceptedForSession => Some(("Approved for this session", theme::added(cx))),
            ApprovalState::Declined => Some(("Declined", theme.danger)),
            ApprovalState::Expired => Some(("No longer needed", muted)),
        };
        let border = if state == ApprovalState::Pending { theme.warning } else { theme.border };
        let mono = theme.mono_font_family.clone();

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

        card(border)
            .child(
                h_flex()
                    .gap_2()
                    .child(icon("shield-check").small().text_color(if outcome.is_none() { theme.warning } else { muted }))
                    .child(div().text_sm().font_semibold().child(title)),
            )
            .when_some(body, |el, body| {
                el.child(
                    div()
                        .p_2()
                        .rounded_sm()
                        .bg(theme.background)
                        .font_family(mono)
                        .text_xs()
                        .child(body),
                )
            })
            .when_some(reason, |el, reason| el.child(div().text_xs().text_color(muted).child(reason)))
            .map(|el| match outcome {
                Some((label, color)) => el.child(div().text_xs().text_color(color).child(label)),
                None => el.child(
                    h_flex()
                        .gap_2()
                        .child(decide("Approve", "accept", ApprovalDecision::Accept).primary())
                        .child(decide("Approve for session", "session", ApprovalDecision::AcceptForSession))
                        .child(decide("Decline", "decline", ApprovalDecision::Decline).danger()),
                ),
            })
            .into_any_element()
    }

    // --- bottom ------------------------------------------------------------

    /// Browser actions waiting for the user, and the live plan.
    fn render_pending(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let controller = self.controller.read(cx);
        let theme = cx.theme();
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
        let muted = theme.muted_foreground;
        let mut column = v_flex().px_3().pt_2().gap_2();
        if !plan.is_empty() {
            let mut steps = card(theme.border).child(
                h_flex()
                    .gap_2()
                    .child(icon("list-checks").small().text_color(muted))
                    .child(section_title("Plan", muted)),
            );
            for (step, status) in plan {
                let (mark, color) = match status {
                    PlanStepStatus::Completed => ("●", theme::added(cx)),
                    PlanStepStatus::InProgress => ("◐", cx.theme().warning),
                    _ => ("○", muted),
                };
                steps = steps.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .child(div().text_color(color).child(mark))
                        .child(div().flex_1().child(step)),
                );
            }
            column = column.child(steps);
        }
        for (ix, (action, reason)) in pending.into_iter().enumerate() {
            column = column.child(
                card(cx.theme().warning)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(IconName::Globe)
                            .child(div().text_sm().font_semibold().child("Browser action needs approval")),
                    )
                    .child(div().text_xs().child(format!("Codex wants to {action}.")))
                    .child(div().text_xs().text_color(muted).child(reason))
                    .child(
                        h_flex()
                            .gap_2()
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
                                    .danger()
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

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.read(cx);
        let connected = matches!(controller.state(), ConnectionState::Connected { .. }) && !controller.needs_login();
        let running = controller.is_running();
        let queued = controller.queued();
        let empty = self.composer.read(cx).value().trim().is_empty();
        let theme = cx.theme();
        let muted = theme.muted_foreground;

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
                    PopupMenuItem::new("Server default")
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
                for preset in PermissionPreset::ALL {
                    let handle = handle.clone();
                    menu = menu.item(
                        PopupMenuItem::new(permission_description(preset))
                            .checked(current == preset)
                            .on_click(move |_, _, cx| handle.update(cx, |c, cx| c.set_permissions(preset, cx))),
                    );
                }
                menu.min_w(px(300.))
            });

        let action_button = if running && empty {
            Button::new("agent-stop")
                .icon(icon("circle-stop"))
                .danger()
                .small()
                .tooltip("Stop (⌃C)")
                .on_click(cx.listener(|this, _, _, cx| this.controller.update(cx, |c, cx| c.interrupt(cx))))
        } else {
            Button::new("agent-send")
                .icon(icon("arrow-up"))
                .primary()
                .small()
                .tooltip(if running { "Queue after the running turn" } else { "Send (Enter)" })
                .disabled(empty || !connected)
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };

        v_flex()
            .p_2()
            .gap_1()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .px_1()
                    .child(
                        Input::new(&self.composer)
                            .appearance(false)
                            .disabled(!connected),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(model_menu)
                    .child(effort_menu)
                    .child(permissions_menu)
                    .child(
                        Switch::new("browser-tools")
                            .checked(browser_tools)
                            .label("Browser")
                            .xsmall()
                            .tooltip("Let Codex use the built-in browser (applies to new threads)")
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.controller.update(cx, |c, cx| c.set_browser_tools(checked, cx));
                            })),
                    )
                    .child(div().flex_1())
                    .child(action_button),
            )
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Today: {} tokens", format_tokens(usage_today)))
                    .when_some(context, |el, pct| el.child(format!("· Context {pct}%")))
                    .when(queued > 0, |el| el.child(format!("· {queued} queued"))),
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
        let connection = self.render_connection(cx);
        let login = if connection.is_none() { self.render_login(cx) } else { None };
        let body = if connection.is_some() || login.is_some() {
            None
        } else {
            Some(self.render_transcript(window, cx))
        };

        v_flex()
            .key_context("AgentComposer")
            .size_full()
            .bg(cx.theme().background)
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
            .when_some(body, |el, body| el.child(body))
            .children(self.render_pending(cx))
            .child(self.render_composer(cx))
    }
}

// --- helpers -----------------------------------------------------------------

fn card(border: Hsla) -> gpui::Div {
    v_flex().p_2().gap_1p5().rounded_md().border_1().border_color(border)
}

fn section_title(text: &'static str, color: Hsla) -> impl IntoElement {
    div().text_xs().font_semibold().text_color(color).child(text)
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
    match (status, exit_code) {
        (ItemStatus::InProgress, _) => ("running".into(), cx.theme().muted_foreground),
        (ItemStatus::Declined, _) => ("declined".into(), cx.theme().danger),
        (_, Some(0)) => (format!("exit 0{duration}"), theme::added(cx)),
        (_, Some(code)) => (format!("exit {code}{duration}"), cx.theme().danger),
        (ItemStatus::Failed, None) => ("failed".into(), cx.theme().danger),
        _ => (format!("done{duration}"), cx.theme().muted_foreground),
    }
}

fn render_diff(diff: &str, cx: &App) -> AnyElement {
    let added = theme::added(cx);
    let deleted = theme::deleted(cx);
    let muted = cx.theme().muted_foreground;
    let mut lines = v_flex()
        .p_2()
        .rounded_sm()
        .bg(cx.theme().background)
        .font_family(cx.theme().mono_font_family.clone())
        .text_xs();
    for line in tail_chars(diff, MAX_OUTPUT_CHARS).lines() {
        let color = if line.starts_with("+++") || line.starts_with("---") {
            muted
        } else if line.starts_with('+') {
            added
        } else if line.starts_with('-') {
            deleted
        } else if line.starts_with("@@") {
            cx.theme().accent_foreground
        } else {
            cx.theme().foreground
        };
        lines = lines.child(div().text_color(color).child(if line.is_empty() { " ".to_string() } else { line.to_string() }));
    }
    lines.into_any_element()
}

fn diff_stats(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
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

fn permission_description(preset: PermissionPreset) -> String {
    let detail = match preset {
        PermissionPreset::ReadOnly => "reads files; asks before edits and commands",
        PermissionPreset::Auto => "edits the workspace and runs sandboxed commands",
        PermissionPreset::FullAccess => "no sandbox, never asks",
    };
    format!("{} — {detail}", preset.label())
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
    fn diff_stats_skip_headers() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n+more\n same\n";
        assert_eq!(diff_stats(diff), (2, 1));
    }

    #[test]
    fn tail_respects_char_boundaries() {
        assert_eq!(tail_chars("héllo", 4), "llo");
        assert_eq!(tail_chars("abc", 10), "abc");
    }

    #[test]
    fn token_formatting() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_345), "12.3k");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }
}

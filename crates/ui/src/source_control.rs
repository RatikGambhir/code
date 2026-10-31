//! The source control sidebar: branch, staged and unstaged changes, commit.

use std::path::{Path, PathBuf};

use cockpit_git::{FileState, Repository, Status, StatusEntry};
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, SharedString, Subscription, Task, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    v_flex, Disableable as _, IconName, Sizable as _,
};

use crate::{
    assets::icon,
    chrome::{self, caps_label, panel_header},
    theme::{self, tokens, RADIUS, TEXT_MONO, TEXT_SMALL},
};

pub enum SourceControlEvent {
    /// The repository status was refreshed.
    StatusChanged,
    /// Show a diff (computed already) in a read-only tab.
    OpenDiff { title: String, diff: String },
    Notify(SharedString),
}

pub struct SourceControl {
    workspace_root: PathBuf,
    repo: Option<Repository>,
    status: Option<Status>,
    message: Entity<InputState>,
    /// A discard waiting for a second click.
    confirm_discard: Option<String>,
    refreshing: Option<Task<()>>,
    /// Another refresh was asked for while one ran.
    refresh_again: bool,
    busy: bool,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SourceControlEvent> for SourceControl {}

impl Focusable for SourceControl {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SourceControl {
    pub fn new(workspace_root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let message = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(2, 8)
                .placeholder("Commit message (⌘Enter to commit)")
        });
        let subscriptions = vec![cx.subscribe_in(&message, window, |this, _, event: &InputEvent, window, cx| {
            match event {
                InputEvent::PressEnter { secondary: true } => this.commit(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        })];
        let mut this = Self {
            repo: None,
            status: None,
            message,
            confirm_discard: None,
            refreshing: None,
            refresh_again: false,
            busy: false,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
            workspace_root,
        };
        this.discover(cx);
        this
    }

    pub fn repository(&self) -> Option<&Repository> {
        self.repo.as_ref()
    }

    pub fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }

    pub fn branch(&self) -> Option<&str> {
        self.status.as_ref()?.branch.head.as_deref()
    }

    pub fn change_count(&self) -> usize {
        self.status.as_ref().map(|s| s.entries.len()).unwrap_or(0)
    }

    fn discover(&mut self, cx: &mut Context<Self>) {
        let root = self.workspace_root.clone();
        let task = cx.background_spawn(async move { Repository::discover(&root) });
        cx.spawn(async move |this, cx| {
            let repo = task.await;
            this.update(cx, |this, cx| {
                match repo {
                    Ok(repo) => this.repo = repo,
                    Err(err) => tracing::warn!("finding the git repository: {err:#}"),
                }
                this.refresh(cx);
            })
        })
        .detach();
    }

    /// Re-reads `git status` in the background. At most one runs at a time;
    /// requests meanwhile (a build touching hundreds of files) collapse into
    /// one more run.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing.is_some() {
            self.refresh_again = true;
            return;
        }
        let Some(repo) = self.repo.clone() else {
            self.status = None;
            cx.emit(SourceControlEvent::StatusChanged);
            cx.notify();
            return;
        };
        let task = cx.background_spawn(async move { repo.status() });
        self.refreshing = Some(cx.spawn(async move |this, cx| {
            let status = task.await;
            this.update(cx, |this, cx| {
                match status {
                    Ok(status) => this.status = Some(status),
                    Err(err) => tracing::warn!("git status: {err:#}"),
                }
                this.refreshing = None;
                cx.emit(SourceControlEvent::StatusChanged);
                cx.notify();
                if std::mem::take(&mut this.refresh_again) {
                    this.refresh(cx);
                }
            })
            .ok();
        }));
    }

    /// Runs a git operation in the background, then refreshes.
    fn run(
        &mut self,
        what: &'static str,
        op: impl FnOnce(&Repository) -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else { return };
        self.busy = true;
        cx.notify();
        let task = cx.background_spawn(async move { op(&repo) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                if let Err(err) = result {
                    cx.emit(SourceControlEvent::Notify(format!("{what} failed: {err:#}").into()));
                }
                this.refresh(cx);
            })
        })
        .detach();
    }

    fn init_repository(&mut self, cx: &mut Context<Self>) {
        let root = self.workspace_root.clone();
        let task = cx.background_spawn(async move { Repository::init(&root) });
        cx.spawn(async move |this, cx| {
            let repo = task.await;
            this.update(cx, |this, cx| {
                match repo {
                    Ok(repo) => this.repo = Some(repo),
                    Err(err) => cx.emit(SourceControlEvent::Notify(format!("git init failed: {err:#}").into())),
                }
                this.refresh(cx);
            })
        })
        .detach();
    }

    fn stage(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        self.run(
            "Staging",
            move |repo| {
                let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
                repo.stage(&paths)
            },
            cx,
        );
    }

    fn unstage(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        self.run(
            "Unstaging",
            move |repo| {
                let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
                repo.unstage(&paths)
            },
            cx,
        );
    }

    fn discard(&mut self, path: String, cx: &mut Context<Self>) {
        if self.confirm_discard.as_deref() != Some(path.as_str()) {
            self.confirm_discard = Some(path);
            cx.notify();
            return;
        }
        self.confirm_discard = None;
        self.run("Discarding", move |repo| repo.discard(&path), cx);
    }

    pub fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.message.read(cx).value().trim().to_string();
        if message.is_empty() || !self.has_staged() || self.busy {
            return;
        }
        let Some(repo) = self.repo.clone() else { return };
        self.busy = true;
        cx.notify();
        let task = cx.background_spawn(async move { repo.commit(&message) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(sha) => {
                        this.message.update(cx, |state, cx| state.set_value("", window, cx));
                        let short: String = sha.chars().take(7).collect();
                        cx.emit(SourceControlEvent::Notify(format!("Committed {short}").into()));
                    }
                    Err(err) => cx.emit(SourceControlEvent::Notify(format!("Commit failed: {err:#}").into())),
                }
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Focuses the commit message.
    pub fn focus_message(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.message.update(cx, |state, cx| state.focus(window, cx));
    }

    fn has_staged(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.staged().next().is_some())
    }

    fn open_diff(&mut self, path: String, staged: bool, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        let task = cx.background_spawn({
            let path = path.clone();
            async move { repo.diff_file(&path, staged) }
        });
        cx.spawn(async move |this, cx| {
            let diff = task.await;
            this.update(cx, |_, cx| match diff {
                Ok(diff) if diff.trim().is_empty() => {
                    cx.emit(SourceControlEvent::Notify(format!("No changes in {path}").into()))
                }
                Ok(diff) => {
                    let name = Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone());
                    let title = if staged { format!("{name} (staged)") } else { format!("{name} (changes)") };
                    cx.emit(SourceControlEvent::OpenDiff { title, diff });
                }
                Err(err) => cx.emit(SourceControlEvent::Notify(format!("git diff failed: {err:#}").into())),
            })
        })
        .detach();
    }

    fn render_section(
        &self,
        title: &'static str,
        staged: bool,
        entries: Vec<StatusEntry>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subtle = tokens(cx).fg_subtle;
        let all: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        let mut section = v_flex().px(px(6.)).pb(px(6.)).child(
            h_flex()
                .h(px(28.))
                .pl(px(8.))
                .gap_1()
                .child(caps_label(title, cx))
                .child(div().flex_1().text_size(TEXT_SMALL).text_color(subtle).child(entries.len().to_string()))
                .child(
                    Button::new(if staged { "unstage-all" } else { "stage-all" })
                        .icon(if staged { IconName::Minus } else { IconName::Plus })
                        .ghost()
                        .xsmall()
                        .tooltip(if staged { "Unstage all" } else { "Stage all" })
                        .disabled(entries.is_empty() || self.busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if staged {
                                this.unstage(all.clone(), cx);
                            } else {
                                this.stage(all.clone(), cx);
                            }
                        })),
                ),
        );
        for (ix, entry) in entries.into_iter().enumerate() {
            section = section.child(self.render_entry(ix, entry, staged, cx));
        }
        section.into_any_element()
    }

    fn render_entry(&self, ix: usize, entry: StatusEntry, staged: bool, cx: &mut Context<Self>) -> AnyElement {
        let state = if staged { entry.index } else { entry.display_state() };
        let color = state_color(state, cx);
        let t = tokens(cx);
        let (muted, fg, mono) = (t.fg_subtle, t.fg, t.mono.clone());
        let path = Path::new(&entry.path);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.path.clone());
        let dir = path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let confirming = !staged && self.confirm_discard.as_deref() == Some(entry.path.as_str());
        let prefix = if staged { "staged" } else { "unstaged" };
        let (open_path, action_path, discard_path) = (entry.path.clone(), entry.path.clone(), entry.path.clone());

        chrome::list_row((prefix, ix), false, cx)
            .group("scm-row")
            .pr(px(4.))
            .gap(px(4.))
            .on_click(cx.listener(move |this, _, _, cx| this.open_diff(open_path.clone(), staged, cx)))
            .child(
                div()
                    .flex_shrink()
                    .truncate()
                    .font_family(mono.clone())
                    .text_size(TEXT_MONO)
                    .text_color(if state == FileState::Deleted { muted } else { fg })
                    .when(state == FileState::Deleted, |el| el.line_through())
                    .child(name),
            )
            .child(div().flex_1().min_w_0().truncate().text_size(TEXT_SMALL).text_color(muted).child(dir))
            .when(!staged, |el| {
                el.child(
                    Button::new(("discard", ix))
                        .icon(icon("rotate-cw"))
                        .when(confirming, |b| b.label("Discard?").danger())
                        .when(!confirming, |b| b.ghost())
                        .xsmall()
                        .tooltip("Discard changes")
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.discard(discard_path.clone(), cx);
                        })),
                )
            })
            .child(
                Button::new((if staged { "unstage" } else { "stage" }, ix))
                    .icon(if staged { IconName::Minus } else { IconName::Plus })
                    .ghost()
                    .xsmall()
                    .tooltip(if staged { "Unstage" } else { "Stage" })
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if staged {
                            this.unstage(vec![action_path.clone()], cx);
                        } else {
                            this.stage(vec![action_path.clone()], cx);
                        }
                    })),
            )
            .child(chrome::status_badge(state.badge(), color, cx))
            .into_any_element()
    }
}

fn state_color(state: FileState, cx: &App) -> Hsla {
    match state {
        FileState::Added | FileState::Untracked => theme::added(cx),
        FileState::Deleted | FileState::Unmerged => theme::deleted(cx),
        _ => theme::modified(cx),
    }
}

impl Render for SourceControl {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let muted = t.fg_muted;
        let header = panel_header("Source control", cx)
            .child(
                Button::new("scm-refresh")
                    .icon(icon("refresh-cw"))
                    .ghost()
                    .xsmall()
                    .tooltip("Refresh")
                    .loading(self.refreshing.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );

        let root = v_flex().size_full().track_focus(&self.focus_handle).child(header);
        if self.repo.is_none() {
            return root.child(
                v_flex()
                    .px(px(14.))
                    .gap(px(10.))
                    .text_size(theme::TEXT_BODY)
                    .text_color(muted)
                    .child("This folder is not a git repository.")
                    .child(
                        Button::new("git-init")
                            .label("Initialize repository")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.init_repository(cx))),
                    ),
            );
        }

        let (staged, unstaged): (Vec<StatusEntry>, Vec<StatusEntry>) = match &self.status {
            Some(status) => (status.staged().cloned().collect(), status.unstaged().cloned().collect()),
            None => (Vec::new(), Vec::new()),
        };
        let branch = self.status.as_ref().map(|s| {
            let b = &s.branch;
            let mut text = b.head.clone().unwrap_or_else(|| "detached HEAD".into());
            if b.ahead > 0 {
                text.push_str(&format!(" ↑{}", b.ahead));
            }
            if b.behind > 0 {
                text.push_str(&format!(" ↓{}", b.behind));
            }
            text
        });
        let can_commit = !staged.is_empty() && !self.message.read(cx).value().trim().is_empty() && !self.busy;

        root.child(
            v_flex()
                .px(px(12.))
                .pb(px(10.))
                .gap(px(8.))
                .when_some(branch, |el, branch| {
                    el.child(
                        h_flex()
                            .px(px(2.))
                            .gap(px(6.))
                            .text_size(TEXT_SMALL)
                            .text_color(muted)
                            .child(icon("git-branch").size(px(12.)))
                            .child(div().font_family(t.mono.clone()).child(branch)),
                    )
                })
                .child(
                    div()
                        .px(px(2.))
                        .rounded(RADIUS)
                        .border_1()
                        .border_color(t.border_strong)
                        .bg(t.surface)
                        .child(Input::new(&self.message).appearance(false).small()),
                )
                .child(
                    Button::new("commit")
                        .icon(icon("git-commit-horizontal"))
                        .label(if staged.is_empty() { "Nothing staged" } else { "Commit" })
                        .primary()
                        .small()
                        .w_full()
                        .disabled(!can_commit)
                        .loading(self.busy)
                        .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                ),
        )
        .child(
            div().flex_1().min_h_0().child(
                v_flex()
                    .id("scm-lists")
                    .size_full()
                    .pt(px(4.))
                    .border_t_1()
                    .border_color(t.border)
                    .overflow_y_scrollbar()
                    .when(!staged.is_empty(), |el| {
                        el.child(self.render_section("Staged changes", true, staged, cx))
                    })
                    .child(self.render_section("Changes", false, unstaged, cx)),
            ),
        )
    }
}

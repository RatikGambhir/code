//! The diff panel beside the chat: what this conversation changed, or
//! everything uncommitted in the repository, one collapsible file at a time.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use cockpit_git::{StatusKind, Repository};
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Hsla, ScrollHandle, Subscription, Task, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    spinner::Spinner,
    v_flex, IconName, Sizable as _,
};

use super::diff::{chat_changes, parse_lines, split_git_diff, ChangeKind, DiffSummary, FileDiff, LineKind};
use crate::{
    agent::AgentController,
    assets::icon,
    chrome,
    source_control::{SourceControl, SourceControlEvent},
    theme::{self, tokens, Tokens, TEXT_BODY, TEXT_SMALL},
};

/// Lines drawn per file before the rest is left to a full tab.
const MAX_LINES_PER_FILE: usize = 400;
/// Lines drawn across all files; later files start collapsed.
const MAX_LINES_TOTAL: usize = 3_000;
/// Untracked files diffed one by one (each is a `git` run).
const MAX_UNTRACKED: usize = 40;

pub enum DiffPanelEvent {
    OpenFile(PathBuf),
    /// Show a whole diff in a read-only editor tab.
    OpenDiff { title: String, diff: String },
    Close,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum DiffSource {
    /// Files the active conversation edited.
    #[default]
    Chat,
    /// `git diff HEAD` plus untracked files.
    Uncommitted,
}

/// The working tree's changes, as last loaded.
#[derive(Default)]
struct GitDiffs {
    files: Vec<FileDiff>,
    root: Option<PathBuf>,
    loading: bool,
    /// Loaded at least once.
    loaded: bool,
    /// The status changed since the last load.
    stale: bool,
    no_repo: bool,
    error: Option<String>,
}

pub struct DiffPanel {
    controller: Entity<AgentController>,
    source_control: Entity<SourceControl>,
    source: DiffSource,
    /// The active thread's changes, keyed by (thread, revision).
    chat: ((Option<String>, u64), Vec<FileDiff>),
    git: GitDiffs,
    /// Files the user folded, as "source:path".
    collapsed: HashSet<String>,
    /// Files the user unfolded past the line budget.
    expanded: HashSet<String>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
    _load: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DiffPanelEvent> for DiffPanel {}

impl DiffPanel {
    pub fn new(controller: Entity<AgentController>, source_control: Entity<SourceControl>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&controller, |_, _, cx| cx.notify()),
            cx.subscribe(&source_control, |this, _, event: &SourceControlEvent, cx| {
                if matches!(event, SourceControlEvent::StatusChanged) {
                    this.git.stale = true;
                    if this.source == DiffSource::Uncommitted {
                        this.load_git(cx);
                    }
                }
            }),
        ];
        Self {
            controller,
            source_control,
            source: DiffSource::default(),
            chat: ((None, 0), Vec::new()),
            git: GitDiffs { stale: true, ..GitDiffs::default() },
            collapsed: HashSet::new(),
            expanded: HashSet::new(),
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _load: None,
            _subscriptions: subscriptions,
        }
    }

    /// Called when the panel is shown, so a stale working-tree diff reloads.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        if self.source == DiffSource::Uncommitted && self.git.stale {
            self.load_git(cx);
        }
    }

    fn set_source(&mut self, source: DiffSource, cx: &mut Context<Self>) {
        self.source = source;
        self.scroll.set_offset(gpui::point(px(0.), px(0.)));
        if source == DiffSource::Uncommitted && (self.git.stale || !self.git.loaded) {
            self.load_git(cx);
        }
        cx.notify();
    }

    /// Reads `git diff HEAD` and the untracked files in the background,
    /// keeping the previous result on screen meanwhile.
    fn load_git(&mut self, cx: &mut Context<Self>) {
        let scm = self.source_control.read(cx);
        let Some(repo) = scm.repository().cloned() else {
            self.git = GitDiffs { no_repo: true, loaded: true, ..GitDiffs::default() };
            cx.notify();
            return;
        };
        let untracked: Vec<String> = scm
            .status()
            .map(|s| {
                s.entries
                    .iter()
                    .filter(|e| e.kind == StatusKind::Untracked)
                    .take(MAX_UNTRACKED)
                    .map(|e| e.path.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.git.loading = true;
        self.git.stale = false;
        let task = cx.background_spawn(async move { working_tree_diff(&repo, &untracked) });
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.git.loading = false;
                this.git.loaded = true;
                this.git.no_repo = false;
                match result {
                    Ok((root, files)) => {
                        this.git.root = Some(root);
                        this.git.files = files;
                        this.git.error = None;
                    }
                    Err(err) => this.git.error = Some(format!("{err:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn refresh_chat(&mut self, cx: &App) {
        let controller = self.controller.read(cx);
        let key = controller
            .active_thread()
            .map(|t| (Some(t.id.clone()), t.transcript.revision()))
            .unwrap_or((None, 0));
        if key != self.chat.0 {
            let files = controller
                .active_thread()
                .map(|t| chat_changes(t.transcript.entries()))
                .unwrap_or_default();
            self.chat = (key, files);
        }
    }

    fn files(&self) -> &[FileDiff] {
        match self.source {
            DiffSource::Chat => &self.chat.1,
            DiffSource::Uncommitted => &self.git.files,
        }
    }

    /// Where a listed path lives on disk.
    fn absolute(&self, path: &str, cx: &App) -> PathBuf {
        let base = match self.source {
            DiffSource::Chat => self.controller.read(cx).cwd().to_path_buf(),
            DiffSource::Uncommitted => self.git.root.clone().unwrap_or_else(|| self.controller.read(cx).cwd().to_path_buf()),
        };
        base.join(path)
    }

    fn key(&self, path: &str) -> String {
        format!("{:?}:{path}", self.source)
    }

    fn toggle_file(&mut self, key: String, over_budget: bool, cx: &mut Context<Self>) {
        let set = if over_budget { &mut self.expanded } else { &mut self.collapsed };
        if !set.remove(&key) {
            set.insert(key);
        }
        cx.notify();
    }

    fn set_all_collapsed(&mut self, collapse: bool, cx: &mut Context<Self>) {
        let keys: Vec<String> = self.files().iter().map(|f| self.key(&f.path)).collect();
        if collapse {
            self.collapsed.extend(keys);
        } else {
            for key in keys {
                self.collapsed.remove(&key);
            }
        }
        cx.notify();
    }

    // --- rendering ---------------------------------------------------------

    fn render_header(&self, summary_chat: DiffSummary, summary_git: Option<DiffSummary>, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let tab = |id: &'static str, label: &'static str, count: Option<usize>, source: DiffSource, cx: &mut Context<Self>| {
            let active = self.source == source;
            chrome::underline_tab(id, active, cx)
                .text_size(TEXT_BODY)
                .when(active, |el| el.font_weight(FontWeight::MEDIUM))
                .child(label)
                .when_some(count.filter(|c| *c > 0), |el, count| {
                    el.child(
                        div()
                            .px(px(5.))
                            .rounded_full()
                            .bg(t.raised)
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_muted)
                            .child(count.to_string()),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| this.set_source(source, cx)))
        };
        let any_open = self.files().iter().any(|f| !self.collapsed.contains(&self.key(&f.path)));
        h_flex()
            .h(px(40.))
            .flex_none()
            .pl(px(4.))
            .pr(px(6.))
            .gap(px(2.))
            .border_b_1()
            .border_color(t.border)
            .child(tab("diff-tab-chat", "This chat", Some(summary_chat.files), DiffSource::Chat, cx))
            .child(tab("diff-tab-git", "Uncommitted", summary_git.map(|s| s.files), DiffSource::Uncommitted, cx))
            .child(div().flex_1())
            .when(self.source == DiffSource::Uncommitted, |el| {
                el.child(
                    Button::new("diff-refresh")
                        .icon(icon("refresh-cw"))
                        .ghost()
                        .xsmall()
                        .tooltip("Refresh")
                        .loading(self.git.loading)
                        .on_click(cx.listener(|this, _, _, cx| this.load_git(cx))),
                )
            })
            .child(
                Button::new("diff-fold-all")
                    .icon(IconName::ChevronsUpDown)
                    .ghost()
                    .xsmall()
                    .tooltip(if any_open { "Collapse all" } else { "Expand all" })
                    .on_click(cx.listener(move |this, _, _, cx| this.set_all_collapsed(any_open, cx))),
            )
            .child(
                Button::new("diff-close")
                    .icon(icon("x"))
                    .ghost()
                    .xsmall()
                    .tooltip("Close (⇧⌘D)")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffPanelEvent::Close))),
            )
    }

    fn render_summary(&self, summary: DiffSummary, cx: &App) -> impl IntoElement {
        let t = tokens(cx);
        h_flex()
            .flex_none()
            .px(px(12.))
            .py(px(8.))
            .gap(px(8.))
            .text_size(TEXT_SMALL)
            .text_color(t.fg_muted)
            .child(if summary.files == 1 { "1 file changed".to_string() } else { format!("{} files changed", summary.files) })
            .child(div().text_color(t.success).child(format!("+{}", summary.added)))
            .child(div().text_color(t.danger).child(format!("−{}", summary.removed)))
            .when(self.source == DiffSource::Uncommitted && self.git.loading, |el| {
                el.child(Spinner::new().xsmall())
            })
    }

    fn render_empty(&self, cx: &App) -> AnyElement {
        let t = tokens(cx);
        let (title, detail): (&str, Option<String>) = match self.source {
            DiffSource::Chat if self.controller.read(cx).active_thread().is_none() => {
                ("No conversation selected", Some("Changes the agent makes show up here.".into()))
            }
            DiffSource::Chat => ("No changes yet", Some("Files this conversation edits show up here as it works.".into())),
            DiffSource::Uncommitted if self.git.no_repo => ("Not a git repository", None),
            DiffSource::Uncommitted if self.git.error.is_some() => ("Could not read the diff", self.git.error.clone()),
            DiffSource::Uncommitted if self.git.loading || !self.git.loaded => {
                return h_flex()
                    .p(px(16.))
                    .gap(px(8.))
                    .text_size(TEXT_BODY)
                    .text_color(t.fg_muted)
                    .child(Spinner::new().small())
                    .child("Reading changes…")
                    .into_any_element();
            }
            DiffSource::Uncommitted => ("Working tree clean", Some("Nothing differs from the last commit.".into())),
        };
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .px(px(24.))
            .text_center()
            .child(icon("file-diff").size(px(20.)).text_color(t.fg_subtle))
            .child(div().text_size(TEXT_BODY).font_weight(FontWeight::MEDIUM).text_color(t.fg).child(title))
            .when_some(detail, |el, detail| {
                el.child(div().max_w(px(260.)).text_size(TEXT_SMALL).text_color(t.fg_muted).child(detail))
            })
            .into_any_element()
    }

    fn render_file(&self, ix: usize, file: &FileDiff, over_budget: bool, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let key = self.key(&file.path);
        let open = if over_budget { self.expanded.contains(&key) } else { !self.collapsed.contains(&key) };
        let path = Path::new(&file.path);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| file.path.clone());
        let dir = path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let badge_color = match file.change {
            ChangeKind::Added => theme::added(cx),
            ChangeKind::Deleted => theme::deleted(cx),
            _ => theme::modified(cx),
        };
        let absolute = self.absolute(&file.path, cx);
        let tab_title = format!("{name} (diff)");
        let full_diff = file.diff.clone();
        let deleted = file.change == ChangeKind::Deleted;

        let header = h_flex()
            .id(("diff-file", ix))
            .h(px(32.))
            .px(px(8.))
            .gap(px(6.))
            .cursor_pointer()
            .bg(t.raised)
            .when(open, |el| el.border_b_1().border_color(t.border))
            .hover(|s| s.bg(t.hover))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_file(key.clone(), over_budget, cx)))
            .child(
                gpui_component::Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                    .size(px(12.))
                    .text_color(t.fg_subtle),
            )
            .child(chrome::status_badge(file.change.badge(), badge_color, cx))
            .child(
                div()
                    .flex_shrink()
                    .truncate()
                    .text_size(TEXT_BODY)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if deleted { t.fg_muted } else { t.fg })
                    .when(deleted, |el| el.line_through())
                    .child(name),
            )
            .child(div().flex_1().min_w_0().truncate().text_size(TEXT_SMALL).text_color(t.fg_subtle).child(dir))
            .when(file.added > 0, |el| el.child(div().flex_none().text_size(TEXT_SMALL).text_color(t.success).child(format!("+{}", file.added))))
            .when(file.removed > 0, |el| el.child(div().flex_none().text_size(TEXT_SMALL).text_color(t.danger).child(format!("−{}", file.removed))))
            .when(!deleted, |el| {
                el.child(
                    Button::new(("diff-open-file", ix))
                        .icon(icon("external-link"))
                        .ghost()
                        .xsmall()
                        .tooltip("Open file")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.stop_propagation();
                            cx.emit(DiffPanelEvent::OpenFile(absolute.clone()));
                        })),
                )
            });

        let mut card = v_flex()
            .mx(px(10.))
            .mb(px(10.))
            .rounded(theme::RADIUS)
            .border_1()
            .border_color(t.border)
            .overflow_hidden()
            .child(header);
        if open {
            let lines = parse_lines(&file.diff);
            if lines.is_empty() {
                card = card.child(
                    div()
                        .px(px(12.))
                        .py(px(8.))
                        .text_size(TEXT_SMALL)
                        .text_color(t.fg_subtle)
                        .child(if file.diff.contains("Binary files") { "Binary file" } else { "No textual changes" }),
                );
            } else {
                let hidden = lines.len().saturating_sub(MAX_LINES_PER_FILE);
                let mut body = v_flex().py(px(2.)).font_family(t.mono.clone()).text_size(px(11.5));
                for line in lines.iter().take(MAX_LINES_PER_FILE) {
                    body = body.child(render_line(line.kind, line.old, line.new, line.text, &t));
                }
                card = card.child(body);
                if hidden > 0 {
                    card = card.child(
                        div()
                            .id(("diff-more", ix))
                            .px(px(12.))
                            .py(px(6.))
                            .border_t_1()
                            .border_color(t.border)
                            .text_size(TEXT_SMALL)
                            .text_color(t.accent_text)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(format!("{hidden} more lines · open the full diff in a tab"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(DiffPanelEvent::OpenDiff { title: tab_title.clone(), diff: full_diff.clone() });
                            })),
                    );
                }
            }
        }
        card.into_any_element()
    }
}

/// One diff line: old and new line numbers, a sign, the text.
fn render_line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str, t: &Tokens) -> AnyElement {
    if kind == LineKind::Hunk {
        return div()
            .px(px(10.))
            .py(px(2.))
            .my(px(2.))
            .bg(t.accent_soft.alpha(0.5))
            .text_color(t.accent_text)
            .truncate()
            .child(text.to_string())
            .into_any_element();
    }
    let (sign, bg, fg): (&str, Option<Hsla>, Hsla) = match kind {
        LineKind::Added => ("+", Some(t.added_bg()), t.fg),
        LineKind::Removed => ("−", Some(t.removed_bg()), t.fg),
        _ => (" ", None, t.fg_muted),
    };
    let number = |n: Option<u32>| {
        div()
            .flex_none()
            .w(px(34.))
            .pr(px(6.))
            .text_right()
            .text_color(t.fg_subtle.alpha(0.7))
            .child(n.map(|n| n.to_string()).unwrap_or_default())
    };
    h_flex()
        .items_start()
        .when_some(bg, |el, bg| el.bg(bg))
        .child(number(old))
        .child(number(new))
        .child(div().flex_none().w(px(14.)).text_color(fg).child(sign))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pr(px(8.))
                .text_color(fg)
                .child(if text.is_empty() { " ".to_string() } else { text.replace('\t', "    ") }),
        )
        .into_any_element()
}

/// `git diff HEAD` split per file, plus untracked files as all-added.
fn working_tree_diff(repo: &Repository, untracked: &[String]) -> anyhow::Result<(PathBuf, Vec<FileDiff>)> {
    let mut files = split_git_diff(&repo.diff_head()?);
    for path in untracked {
        match repo.diff_file(path, false) {
            Ok(diff) if !diff.trim().is_empty() => {
                let mut parsed = split_git_diff(&diff);
                // `--no-index` names the file by its path as given.
                for file in &mut parsed {
                    file.path = path.clone();
                    file.change = ChangeKind::Added;
                }
                files.extend(parsed);
            }
            Ok(_) => {}
            Err(err) => tracing::debug!("diffing untracked {path}: {err:#}"),
        }
    }
    Ok((repo.root().to_path_buf(), files))
}

impl Focusable for DiffPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_chat(cx);
        let summary_chat = DiffSummary::of(&self.chat.1);
        let summary_git = self.git.loaded.then(|| DiffSummary::of(&self.git.files));
        let summary = match self.source {
            DiffSource::Chat => summary_chat,
            DiffSource::Uncommitted => summary_git.unwrap_or_default(),
        };
        let header = self.render_header(summary_chat, summary_git, cx);

        let body = if summary.files == 0 {
            self.render_empty(cx)
        } else {
            // Draw files until the line budget runs out; the rest start folded.
            let mut budget = MAX_LINES_TOTAL;
            let mut list = v_flex().pb(px(12.));
            for (ix, file) in self.files().iter().enumerate() {
                let lines = file.diff.lines().count().min(MAX_LINES_PER_FILE);
                let over_budget = lines > budget;
                if !over_budget && !self.collapsed.contains(&self.key(&file.path)) {
                    budget -= lines;
                }
                list = list.child(self.render_file(ix, file, over_budget, cx));
            }
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .id("diff-panel-files")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .child(self.render_summary(summary, cx))
                        .child(list),
                )
                .vertical_scrollbar(&self.scroll)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .text_color(tokens(cx).fg)
            .child(header)
            .child(body)
    }
}

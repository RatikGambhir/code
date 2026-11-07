//! The diff panel beside the chat: what this conversation changed, or
//! everything uncommitted in the repository, one collapsible file at a time.
//!
//! Diffs are parsed once when they change and drawn as a virtualized list of
//! rows (file headers and lines), so long diffs scroll smoothly and a
//! streaming reply costs nothing: the panel only wakes up when an edit lands.

use std::{
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
};

use cockpit_git::{Repository, StatusKind};
use gpui::{
    div, list, prelude::*, px, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Hsla, ListAlignment, ListOffset, ListState, Subscription, Task, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    spinner::Spinner,
    v_flex, IconName, Sizable as _,
};

use super::diff::{
    applied_edits, chat_changes, shown_lines, split_git_diff, ChangeKind, DiffSummary, FileDiff, LineKind, ShownLine,
};
use crate::{
    agent::AgentController,
    assets::icon,
    chrome,
    source_control::{SourceControl, SourceControlEvent},
    theme::{self, tokens, Tokens, TEXT_BODY, TEXT_SMALL},
};

/// Lines drawn per file; the rest is left to a full diff tab.
const MAX_LINES_PER_FILE: usize = 5_000;
/// Untracked files diffed one by one (each is a `git` run).
const MAX_UNTRACKED: usize = 40;
/// Rows laid out beyond the viewport so fast scrolling doesn't pop in.
const OVERDRAW: f32 = 600.;

pub enum DiffPanelEvent {
    OpenFile(PathBuf),
    /// Show a whole diff in a read-only editor tab.
    OpenDiff { title: String, diff: String },
    Close,
    /// The active conversation just wrote to disk.
    ChangesApplied { thread_id: String },
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum DiffSource {
    /// Files the active conversation edited.
    #[default]
    Chat,
    /// `git diff HEAD` plus untracked files.
    Uncommitted,
}

/// A file's diff with its lines parsed for drawing.
struct ShownFile {
    diff: FileDiff,
    lines: Vec<ShownLine>,
    /// Lines past [`MAX_LINES_PER_FILE`].
    hidden: usize,
}

impl ShownFile {
    fn new(diff: FileDiff) -> Self {
        let (lines, hidden) = shown_lines(&diff.diff, MAX_LINES_PER_FILE);
        Self { diff, lines, hidden }
    }
}

/// One row of the virtualized list.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Row {
    Summary,
    File(usize),
    Line { file: usize, line: usize },
    /// "N more lines · open the full diff".
    More(usize),
    /// A file with nothing to draw (binary, mode change).
    Note(usize),
}

impl Row {
    fn file(self) -> Option<usize> {
        match self {
            Row::Summary => None,
            Row::File(file) | Row::More(file) | Row::Note(file) | Row::Line { file, .. } => Some(file),
        }
    }
}

/// The working tree's changes, as last loaded.
#[derive(Default)]
struct GitDiffs {
    files: Vec<ShownFile>,
    root: Option<PathBuf>,
    loading: bool,
    /// Loaded at least once.
    loaded: bool,
    /// The status changed since the last load.
    stale: bool,
    no_repo: bool,
    error: Option<String>,
}

/// The active conversation's changes.
#[derive(Default)]
struct ChatDiffs {
    thread_id: Option<String>,
    /// The transcript's `changes_revision` these files were built from.
    revision: u64,
    files: Vec<ShownFile>,
    /// Edits that reached the disk, to notice new ones.
    applied: usize,
    /// The file the latest applied edit touched.
    latest: Option<String>,
}

pub struct DiffPanel {
    controller: Entity<AgentController>,
    source_control: Entity<SourceControl>,
    source: DiffSource,
    chat: ChatDiffs,
    git: GitDiffs,
    /// Files the user folded, as "source:path".
    collapsed: HashSet<String>,
    rows: Vec<Row>,
    list: ListState,
    focus_handle: FocusHandle,
    _load: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DiffPanelEvent> for DiffPanel {}

impl DiffPanel {
    pub fn new(controller: Entity<AgentController>, source_control: Entity<SourceControl>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            // Fires on every streamed token; `sync_chat` returns early unless
            // a file change arrived or the conversation switched.
            cx.observe(&controller, |this, _, cx| this.sync_chat(cx)),
            cx.subscribe(&source_control, |this, _, event: &SourceControlEvent, cx| {
                if matches!(event, SourceControlEvent::StatusChanged) {
                    this.git.stale = true;
                    if this.source == DiffSource::Uncommitted {
                        this.load_git(cx);
                    }
                }
            }),
        ];
        let mut panel = Self {
            controller,
            source_control,
            source: DiffSource::default(),
            chat: ChatDiffs::default(),
            git: GitDiffs { stale: true, ..GitDiffs::default() },
            collapsed: HashSet::new(),
            rows: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(OVERDRAW)),
            focus_handle: cx.focus_handle(),
            _load: None,
            _subscriptions: subscriptions,
        };
        panel.sync_chat(cx);
        panel
    }

    /// Called when the panel is shown, so a stale working-tree diff reloads.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        if self.source == DiffSource::Uncommitted && self.git.stale {
            self.load_git(cx);
        }
    }

    /// Switches to this chat's changes and scrolls to the file the agent
    /// edited last, unfolding it.
    pub fn reveal_latest(&mut self, cx: &mut Context<Self>) {
        if self.source != DiffSource::Chat {
            self.source = DiffSource::Chat;
            self.rebuild_rows(false);
        }
        if let Some(path) = self.chat.latest.clone() {
            if self.collapsed.remove(&self.key(&path)) {
                self.rebuild_rows(true);
            }
            let row = self.chat.files.iter().position(|f| f.diff.path == path).and_then(|file| {
                self.rows.iter().position(|row| *row == Row::File(file))
            });
            if let Some(item_ix) = row {
                self.list.scroll_to(ListOffset { item_ix, offset_in_item: px(0.) });
            }
        }
        cx.notify();
    }

    fn set_source(&mut self, source: DiffSource, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        self.source = source;
        self.rebuild_rows(false);
        if source == DiffSource::Uncommitted && (self.git.stale || !self.git.loaded) {
            self.load_git(cx);
        }
        cx.notify();
    }

    /// Rebuilds this chat's diffs when its file changes moved on, and
    /// announces edits that just reached the disk.
    fn sync_chat(&mut self, cx: &mut Context<Self>) {
        let controller = self.controller.read(cx);
        let thread = controller.active_thread();
        let revision = thread.map_or(0, |t| t.transcript.changes_revision());
        if self.chat.thread_id.as_deref() == thread.map(|t| t.id.as_str()) && self.chat.revision == revision {
            return;
        }
        let thread_id = thread.map(|t| t.id.clone());
        let entries = thread.map_or(&[][..], |t| t.transcript.entries());
        let files: Vec<ShownFile> = chat_changes(entries).into_iter().map(ShownFile::new).collect();
        let applied = applied_edits(entries);

        // Opening an older conversation is not "the agent just edited".
        let grew = thread_id.is_some() && thread_id == self.chat.thread_id && applied.count > self.chat.applied;
        let same_thread = thread_id == self.chat.thread_id;
        self.chat = ChatDiffs { thread_id: thread_id.clone(), revision, files, applied: applied.count, latest: applied.latest };
        if self.source == DiffSource::Chat {
            self.rebuild_rows(same_thread);
        }
        cx.notify();
        if let (true, Some(thread_id)) = (grew, thread_id) {
            cx.emit(DiffPanelEvent::ChangesApplied { thread_id });
        }
    }

    /// Reads `git diff HEAD` and the untracked files in the background,
    /// keeping the previous result on screen meanwhile.
    fn load_git(&mut self, cx: &mut Context<Self>) {
        let scm = self.source_control.read(cx);
        let Some(repo) = scm.repository().cloned() else {
            self.git = GitDiffs { no_repo: true, loaded: true, ..GitDiffs::default() };
            if self.source == DiffSource::Uncommitted {
                self.rebuild_rows(false);
            }
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
        // Parsing happens off the main thread too: working-tree diffs can be huge.
        let task = cx.background_spawn(async move {
            working_tree_diff(&repo, &untracked)
                .map(|(root, files)| (root, files.into_iter().map(ShownFile::new).collect::<Vec<_>>()))
        });
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
                if this.source == DiffSource::Uncommitted {
                    this.rebuild_rows(true);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn files(&self) -> &[ShownFile] {
        match self.source {
            DiffSource::Chat => &self.chat.files,
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

    fn push_file_rows(rows: &mut Vec<Row>, ix: usize, file: &ShownFile, open: bool) {
        rows.push(Row::File(ix));
        if !open {
            return;
        }
        if file.lines.is_empty() {
            rows.push(Row::Note(ix));
            return;
        }
        rows.extend((0..file.lines.len()).map(|line| Row::Line { file: ix, line }));
        if file.hidden > 0 {
            rows.push(Row::More(ix));
        }
    }

    /// Lays the rows out again after the files changed. `keep_scroll` holds
    /// the reader's place (new edits arriving); otherwise back to the top.
    fn rebuild_rows(&mut self, keep_scroll: bool) {
        let top = self.list.logical_scroll_top();
        let mut rows = std::mem::take(&mut self.rows);
        rows.clear();
        if !self.files().is_empty() {
            rows.push(Row::Summary);
            for (ix, file) in self.files().iter().enumerate() {
                let open = !self.collapsed.contains(&self.key(&file.diff.path));
                Self::push_file_rows(&mut rows, ix, file, open);
            }
        }
        self.rows = rows;
        self.list.reset(self.rows.len());
        if keep_scroll {
            self.list.scroll_to(top);
        }
    }

    /// The rows file `ix` occupies.
    fn file_rows(&self, ix: usize) -> Option<Range<usize>> {
        let start = self.rows.iter().position(|row| *row == Row::File(ix))?;
        let len = self.rows[start..].iter().take_while(|row| row.file() == Some(ix)).count();
        Some(start..start + len)
    }

    /// Folds or unfolds one file, re-measuring only its rows.
    fn toggle_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (Some(old), Some(file)) = (self.file_rows(ix), self.files().get(ix)) else { return };
        let key = self.key(&file.diff.path);
        let open = !self.collapsed.remove(&key);
        if !open {
            self.collapsed.insert(key);
        }
        let mut replacement = Vec::new();
        Self::push_file_rows(&mut replacement, ix, &self.files()[ix], !open);
        let count = replacement.len();
        self.rows.splice(old.clone(), replacement);
        self.list.splice(old, count);
        cx.notify();
    }

    fn set_all_collapsed(&mut self, collapse: bool, cx: &mut Context<Self>) {
        let keys: Vec<String> = self.files().iter().map(|f| self.key(&f.diff.path)).collect();
        if collapse {
            self.collapsed.extend(keys);
        } else {
            for key in keys {
                self.collapsed.remove(&key);
            }
        }
        self.rebuild_rows(false);
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
        let any_open = self.files().iter().any(|f| !self.collapsed.contains(&self.key(&f.diff.path)));
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

    fn summary(&self) -> DiffSummary {
        self.files().iter().fold(DiffSummary::default(), |acc, f| DiffSummary {
            files: acc.files + 1,
            added: acc.added + f.diff.added,
            removed: acc.removed + f.diff.removed,
        })
    }

    fn render_summary(&self, cx: &App) -> AnyElement {
        let t = tokens(cx);
        let summary = self.summary();
        h_flex()
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
            .into_any_element()
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

    /// One list row. File rows are framed so each file reads as a card.
    fn render_row(&mut self, ix: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(&row) = self.rows.get(ix) else { return div().into_any_element() };
        if row == Row::Summary {
            return self.render_summary(cx);
        }
        let t = tokens(cx).clone();
        let header = matches!(row, Row::File(_));
        // A file's card ends where the next file (or the list) starts.
        let last = !matches!(self.rows.get(ix + 1), Some(Row::Line { .. } | Row::More(_) | Row::Note(_)));
        let content = match row {
            Row::File(file) => self.render_file_header(file, !last, cx),
            Row::Line { file, line } => render_line(&self.files()[file].lines[line], &t),
            Row::More(file) => self.render_more(file, cx),
            Row::Note(file) => {
                let binary = self.files()[file].diff.diff.contains("Binary files");
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .text_size(TEXT_SMALL)
                    .text_color(t.fg_subtle)
                    .child(if binary { "Binary file" } else { "No textual changes" })
                    .into_any_element()
            }
            Row::Summary => unreachable!("handled above"),
        };
        let radius = theme::RADIUS;
        div()
            .px(px(10.))
            .when(last, |el| el.pb(px(10.)))
            .child(
                div()
                    .overflow_hidden()
                    .border_color(t.border)
                    .border_l_1()
                    .border_r_1()
                    .when(header, |el| el.border_t_1().rounded_tl(radius).rounded_tr(radius))
                    .when(last, |el| el.border_b_1().rounded_bl(radius).rounded_br(radius))
                    .when(matches!(row, Row::Line { .. }), |el| el.font_family(t.mono.clone()).text_size(px(11.5)))
                    .child(content),
            )
            .into_any_element()
    }

    fn render_file_header(&self, ix: usize, open: bool, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx).clone();
        let file = &self.files()[ix].diff;
        let path = Path::new(&file.path);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| file.path.clone());
        let dir = path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let badge_color = match file.change {
            ChangeKind::Added => theme::added(cx),
            ChangeKind::Deleted => theme::deleted(cx),
            _ => theme::modified(cx),
        };
        let absolute = self.absolute(&file.path, cx);
        let deleted = file.change == ChangeKind::Deleted;

        h_flex()
            .id(("diff-file", ix))
            .h(px(32.))
            .px(px(8.))
            .gap(px(6.))
            .cursor_pointer()
            .bg(t.raised)
            .when(open, |el| el.border_b_1().border_color(t.border))
            .hover(|s| s.bg(t.hover))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_file(ix, cx)))
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
            })
            .into_any_element()
    }

    fn render_more(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = tokens(cx);
        let file = &self.files()[ix];
        let name = Path::new(&file.diff.path).file_name().map_or_else(|| file.diff.path.clone(), |n| n.to_string_lossy().into_owned());
        let title = format!("{name} (diff)");
        let full_diff = file.diff.diff.clone();
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
            .child(format!("{} more lines · open the full diff in a tab", file.hidden))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(DiffPanelEvent::OpenDiff { title: title.clone(), diff: full_diff.clone() });
            }))
            .into_any_element()
    }
}

/// One diff line: old and new line numbers, a sign, the text.
pub(crate) fn render_line(line: &ShownLine, t: &Tokens) -> AnyElement {
    if line.kind == LineKind::Hunk {
        return div()
            .px(px(10.))
            .py(px(2.))
            .bg(t.accent_soft.alpha(0.5))
            .text_color(t.accent_text)
            .truncate()
            .child(line.text.clone())
            .into_any_element();
    }
    let (sign, bg, fg): (&str, Option<Hsla>, Hsla) = match line.kind {
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
            .children(n.map(|n| n.to_string()))
    };
    h_flex()
        .items_start()
        .when_some(bg, |el, bg| el.bg(bg))
        .child(number(line.old))
        .child(number(line.new))
        .child(div().flex_none().w(px(14.)).text_color(fg).child(sign))
        .child(div().flex_1().min_w_0().pr(px(8.)).text_color(fg).child(line.text.clone()))
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
        let summary_chat = DiffSummary { files: self.chat.files.len(), ..DiffSummary::default() };
        let summary_git = self.git.loaded.then(|| DiffSummary { files: self.git.files.len(), ..DiffSummary::default() });
        let header = self.render_header(summary_chat, summary_git, cx);

        let body = if self.rows.is_empty() {
            self.render_empty(cx)
        } else {
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    list(self.list.clone(), cx.processor(|this, ix, window, cx| this.render_row(ix, window, cx)))
                        .size_full(),
                )
                .vertical_scrollbar(&self.list)
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

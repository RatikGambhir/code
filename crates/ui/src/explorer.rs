//! The file tree.
//!
//! Directories load lazily on expansion (in the background) and the visible
//! rows are a flat list rendered with `uniform_list`, so huge trees stay fast.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
};

use cockpit_editor::fs_tree::{self, DirEntry};
use cockpit_git::{FileState, Status};
use gpui::{
    div, prelude::*, px, uniform_list, App, Context, ElementId, EventEmitter, FocusHandle,
    Focusable, Hsla, MouseButton, SharedString, UniformListScrollHandle, Window,
};
use gpui_component::{v_flex, Icon, IconName};

use crate::{
    chrome::{self, panel_header},
    theme::{self, tokens, TEXT_BODY, TEXT_MONO},
};

pub enum ExplorerEvent {
    OpenFile(PathBuf),
}

#[derive(Clone)]
struct Row {
    path: PathBuf,
    name: SharedString,
    depth: usize,
    is_dir: bool,
    ignored: bool,
    expanded: bool,
}

pub struct Explorer {
    root: PathBuf,
    children: HashMap<PathBuf, Vec<DirEntry>>,
    expanded: HashSet<PathBuf>,
    rows: Vec<Row>,
    selected: Option<PathBuf>,
    /// Git state per absolute path; directories carry their children's.
    git: HashMap<PathBuf, FileState>,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
}

impl EventEmitter<ExplorerEvent> for Explorer {}

impl Focusable for Explorer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Explorer {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut explorer = Self {
            expanded: HashSet::from([root.clone()]),
            root: root.clone(),
            children: HashMap::new(),
            rows: Vec::new(),
            selected: None,
            git: HashMap::new(),
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        };
        explorer.load_dir(root, cx);
        explorer
    }

    fn load_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let listing = cx.background_spawn({
            let dir = dir.clone();
            async move { fs_tree::list_dir(&dir) }
        });
        cx.spawn(async move |this, cx| {
            let entries = listing.await;
            this.update(cx, |this, cx| {
                match entries {
                    Ok(entries) => {
                        this.children.insert(dir, entries);
                    }
                    Err(err) => {
                        tracing::debug!("listing {}: {err}", dir.display());
                        this.children.remove(&dir);
                        this.expanded.remove(&dir);
                    }
                }
                this.rebuild_rows();
                cx.notify();
            })
        })
        .detach();
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        self.push_rows(&self.root.clone(), 0, &mut rows);
        self.rows = rows;
    }

    fn push_rows(&self, dir: &Path, depth: usize, rows: &mut Vec<Row>) {
        let Some(entries) = self.children.get(dir) else { return };
        for entry in entries {
            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
            rows.push(Row {
                path: entry.path.clone(),
                name: entry.name.clone().into(),
                depth,
                is_dir: entry.is_dir,
                ignored: entry.ignored,
                expanded,
            });
            if expanded {
                self.push_rows(&entry.path, depth + 1, rows);
            }
        }
    }

    fn toggle(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.expanded.remove(path) {
            self.rebuild_rows();
        } else {
            self.expanded.insert(path.to_path_buf());
            if self.children.contains_key(path) {
                self.rebuild_rows();
            } else {
                self.load_dir(path.to_path_buf(), cx);
            }
        }
        cx.notify();
    }

    /// Re-lists the loaded directories that contain `paths`.
    pub fn refresh_paths<'a>(&mut self, paths: impl IntoIterator<Item = &'a PathBuf>, cx: &mut Context<Self>) {
        let mut dirs: HashSet<PathBuf> = HashSet::new();
        for path in paths {
            if let Some(parent) = path.parent() {
                if self.children.contains_key(parent) {
                    dirs.insert(parent.to_path_buf());
                }
            }
            if self.children.contains_key(path.as_path()) {
                dirs.insert(path.clone());
            }
        }
        for dir in dirs {
            self.load_dir(dir, cx);
        }
    }

    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        let dirs: Vec<PathBuf> = self.children.keys().cloned().collect();
        for dir in dirs {
            self.load_dir(dir, cx);
        }
    }

    pub fn set_git_status(&mut self, status: Option<&Status>, repo_root: Option<&Path>, cx: &mut Context<Self>) {
        self.git.clear();
        if let (Some(status), Some(repo_root)) = (status, repo_root) {
            for entry in &status.entries {
                let state = entry.display_state();
                let path = repo_root.join(&entry.path);
                // Directories show that something inside them changed.
                for ancestor in path.ancestors().skip(1) {
                    if !ancestor.starts_with(&self.root) || ancestor == self.root {
                        break;
                    }
                    self.git.entry(ancestor.to_path_buf()).or_insert(FileState::Modified);
                }
                self.git.insert(path, state);
            }
        }
        cx.notify();
    }

    /// Selects `path`, expanding its parents, and scrolls it into view.
    pub fn reveal(&mut self, path: &Path, cx: &mut Context<Self>) {
        let mut needs_load = Vec::new();
        for ancestor in path.ancestors().skip(1) {
            if !ancestor.starts_with(&self.root) {
                break;
            }
            if self.expanded.insert(ancestor.to_path_buf()) && !self.children.contains_key(ancestor) {
                needs_load.push(ancestor.to_path_buf());
            }
        }
        for dir in needs_load {
            self.load_dir(dir, cx);
        }
        self.selected = Some(path.to_path_buf());
        self.rebuild_rows();
        if let Some(index) = self.rows.iter().position(|row| row.path == path) {
            self.scroll.scroll_to_item(index, gpui::ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn git_color(&self, path: &Path, cx: &App) -> Option<Hsla> {
        self.git.get(path).map(|state| match state {
            FileState::Added | FileState::Untracked => theme::added(cx),
            FileState::Deleted => theme::deleted(cx),
            FileState::Unmerged => theme::deleted(cx),
            _ => theme::modified(cx),
        })
    }

    fn render_rows(&mut self, range: Range<usize>, _window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let t = tokens(cx).clone();
        range
            .filter_map(|ix| self.rows.get(ix).cloned().map(|row| (ix, row)))
            .map(|(ix, row)| {
                let selected = self.selected.as_ref() == Some(&row.path);
                let git = self.git_color(&row.path, cx);
                let badge = self.git.get(&row.path).filter(|_| !row.is_dir).map(|s| s.badge());
                let deleted = matches!(self.git.get(&row.path), Some(FileState::Deleted));
                let color = if row.ignored || deleted {
                    t.fg_subtle
                } else if row.is_dir {
                    t.fg_muted
                } else if selected {
                    t.accent_text
                } else {
                    t.fg
                };
                let path = row.path.clone();
                // Files line up with their folder's name, past the chevron.
                let indent = 6. + row.depth as f32 * 12.;
                div()
                    .h(px(26.))
                    .px(px(6.))
                    .child(
                        chrome::list_row(ElementId::NamedInteger("explorer-row".into(), ix as u64), selected, cx)
                            .h_full()
                            .pl(px(indent))
                            .pr(px(8.))
                            .gap(px(4.))
                            .child(
                                div().w(px(14.)).flex_none().flex().justify_center().when(row.is_dir, |el| {
                                    el.child(
                                        Icon::new(if row.expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                            .size(px(12.))
                                            .text_color(t.fg_subtle),
                                    )
                                }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_color(color)
                                    .map(|el| {
                                        if row.is_dir {
                                            el.text_size(TEXT_BODY)
                                        } else {
                                            el.font_family(t.mono.clone()).text_size(TEXT_MONO)
                                        }
                                    })
                                    .when(selected && !row.is_dir, |el| el.font_weight(gpui::FontWeight::MEDIUM))
                                    .child(row.name.clone()),
                            )
                            .when(row.is_dir && git.is_some(), |el| {
                                el.child(chrome::status_dot(git.unwrap_or(t.fg_subtle).alpha(0.7)))
                            })
                            .when_some(badge.zip(git), |el, (badge, color)| el.child(chrome::status_badge(badge, color, cx)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &gpui::MouseDownEvent, _window, cx| {
                                    this.selected = Some(path.clone());
                                    if row.is_dir {
                                        this.toggle(&path, cx);
                                    } else if event.click_count >= 1 {
                                        cx.emit(ExplorerEvent::OpenFile(path.clone()));
                                    }
                                    cx.notify();
                                }),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for Explorer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root: SharedString = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(
                panel_header("Explorer", cx).child(
                    div()
                        .max_w(px(140.))
                        .truncate()
                        .text_size(theme::TEXT_SMALL)
                        .text_color(tokens(cx).fg_subtle)
                        .child(root),
                ),
            )
            .child(
                uniform_list("explorer-rows", self.rows.len(), cx.processor(Self::render_rows))
                    .track_scroll(self.scroll.clone())
                    .pb(px(8.))
                    .flex_1(),
            )
    }
}

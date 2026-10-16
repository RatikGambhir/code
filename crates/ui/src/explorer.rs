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
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Icon, IconName, Sizable as _};

use crate::theme;

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
        let theme = cx.theme();
        let (muted, foreground, selection) = (theme.muted_foreground, theme.foreground, theme.list_active);
        range
            .filter_map(|ix| self.rows.get(ix).cloned().map(|row| (ix, row)))
            .map(|(ix, row)| {
                let selected = self.selected.as_ref() == Some(&row.path);
                let color = self
                    .git_color(&row.path, cx)
                    .unwrap_or(if row.ignored { muted } else { foreground });
                let badge = self.git.get(&row.path).filter(|_| !row.is_dir).map(|s| s.badge());
                let path = row.path.clone();
                let icon = if row.is_dir {
                    Icon::new(if row.expanded { IconName::FolderOpen } else { IconName::Folder })
                } else {
                    Icon::new(IconName::File)
                };
                h_flex()
                    .id(ElementId::NamedInteger("explorer-row".into(), ix as u64))
                    .w_full()
                    .h(px(24.))
                    .pl(px(8. + row.depth as f32 * 14.))
                    .pr_2()
                    .gap_1()
                    .text_sm()
                    .text_color(color)
                    .when(selected, |el| el.bg(selection))
                    .hover(|el| el.bg(theme::hover_overlay(cx)))
                    .child(
                        div().w(px(12.)).flex_none().when(row.is_dir, |el| {
                            el.child(
                                Icon::new(if row.expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                    .xsmall()
                                    .text_color(muted),
                            )
                        }),
                    )
                    .child(icon.small().text_color(if row.is_dir { muted } else { color }))
                    .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().child(row.name.clone()))
                    .when_some(badge, |el, badge| el.child(div().text_xs().text_color(color).child(badge)))
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
                    )
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for Explorer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title: SharedString = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().to_uppercase())
            .unwrap_or_default()
            .into();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .h(px(30.))
                    .px_3()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(title),
            )
            .child(
                uniform_list("explorer-rows", self.rows.len(), cx.processor(Self::render_rows))
                    .track_scroll(self.scroll.clone())
                    .flex_1(),
            )
    }
}

//! The command palette (⇧⌘P) and quick open (⌘P): one fuzzy-filtered list
//! over either commands or workspace files.

use std::path::PathBuf;

use cockpit_editor::{fs_tree, fuzzy};
use gpui::{
    div, prelude::*, px, uniform_list, Action, AnyElement, App, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Subscription, UniformListScrollHandle, Window,
};
use gpui_component::{
    h_flex,
    input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
    kbd::Kbd,
    v_flex, ActiveTheme as _, Icon, IconName, Sizable as _,
};

use crate::actions;

const MAX_FILES: usize = 50_000;
const MAX_SHOWN: usize = 200;

pub enum PaletteEvent {
    Dismissed,
    Run(Box<dyn Action>),
    OpenFile(PathBuf),
}

struct Command {
    label: &'static str,
    action: Box<dyn Action>,
    keys: Option<Kbd>,
}

enum Source {
    Commands(Vec<Command>),
    /// Paths relative to the root.
    Files { root: PathBuf, paths: Vec<String>, loading: bool },
}

pub struct Palette {
    source: Source,
    query: Entity<InputState>,
    /// Indices into the source with their score, best first.
    matches: Vec<usize>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Focusable for Palette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

/// Commands listed in the palette, in display order.
fn commands() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("Go to File", Box::new(actions::QuickOpen)),
        ("Open Folder…", Box::new(actions::OpenFolder)),
        ("Save", Box::new(actions::Save)),
        ("Close Tab", Box::new(actions::CloseTab)),
        ("Next Tab", Box::new(actions::NextTab)),
        ("Previous Tab", Box::new(actions::PreviousTab)),
        ("Split Editor", Box::new(actions::SplitPane)),
        ("New Browser Tab", Box::new(actions::NewBrowserTab)),
        ("New Terminal", Box::new(actions::NewTerminal)),
        ("Toggle Sidebar", Box::new(actions::ToggleSidebar)),
        ("Toggle Terminal", Box::new(actions::ToggleTerminal)),
        ("Toggle Agent Panel", Box::new(actions::ToggleAgentPanel)),
        ("Show Explorer", Box::new(actions::ShowExplorer)),
        ("Show Source Control", Box::new(actions::ShowSourceControl)),
        ("Find in Files", Box::new(actions::ShowSearch)),
        ("Codex: Focus Composer", Box::new(actions::FocusAgent)),
        ("Codex: New Thread", Box::new(actions::NewAgentThread)),
        ("Codex: Stop Turn", Box::new(actions::InterruptAgent)),
        ("Toggle Light/Dark Theme", Box::new(actions::ToggleTheme)),
        ("About Cockpit", Box::new(actions::About)),
        ("Quit", Box::new(actions::Quit)),
    ]
}

impl Palette {
    pub fn commands(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let commands = commands()
            .into_iter()
            .map(|(label, action)| {
                let keys = Kbd::binding_for_action(action.as_ref(), None, window);
                Command { label, action, keys }
            })
            .collect();
        Self::new(Source::Commands(commands), "Type a command", window, cx)
    }

    pub fn files(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let listing = cx.background_spawn({
            let root = root.clone();
            async move { workspace_files(&root) }
        });
        cx.spawn(async move |this, cx| {
            let paths = listing.await;
            this.update(cx, |this, cx| {
                if let Source::Files { paths: slot, loading, .. } = &mut this.source {
                    *slot = paths;
                    *loading = false;
                }
                this.filter(cx);
            })
            .ok();
        })
        .detach();
        Self::new(
            Source::Files { root, paths: Vec::new(), loading: true },
            "Go to file",
            window,
            cx,
        )
    }

    fn new(source: Source, placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscriptions = vec![cx.subscribe(&query, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => this.filter(cx),
            InputEvent::PressEnter { .. } => this.confirm(this.selected, cx),
            _ => {}
        })];
        let mut palette = Self {
            source,
            query,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        palette.filter(cx);
        palette
    }

    fn len(&self) -> usize {
        match &self.source {
            Source::Commands(commands) => commands.len(),
            Source::Files { paths, .. } => paths.len(),
        }
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).value().to_string();
        let query = query.trim();
        self.matches = if query.is_empty() {
            (0..self.len().min(MAX_SHOWN)).collect()
        } else {
            match &self.source {
                Source::Commands(commands) => fuzzy::rank(
                    query,
                    commands.iter().enumerate().map(|(ix, c)| (c.label, ix)),
                    MAX_SHOWN,
                ),
                Source::Files { paths, .. } => fuzzy::rank(
                    query,
                    paths.iter().enumerate().map(|(ix, p)| (p.as_str(), ix)),
                    MAX_SHOWN,
                ),
            }
            .into_iter()
            .map(|(ix, _)| ix)
            .collect()
        };
        self.selected = 0;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(len)) as usize;
        self.scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Center);
        cx.notify();
    }

    fn confirm(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(&index) = self.matches.get(row) else { return };
        match &self.source {
            Source::Commands(commands) => cx.emit(PaletteEvent::Run(commands[index].action.boxed_clone())),
            Source::Files { root, paths, .. } => cx.emit(PaletteEvent::OpenFile(root.join(&paths[index]))),
        }
    }

    fn render_rows(&mut self, range: std::ops::Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (muted, selection) = (theme.muted_foreground, theme.list_active);
        range
            .filter_map(|row| self.matches.get(row).map(|&ix| (row, ix)))
            .map(|(row, ix)| {
                let selected = row == self.selected;
                let content = match &self.source {
                    Source::Commands(commands) => {
                        let command = &commands[ix];
                        h_flex()
                            .flex_1()
                            .gap_4()
                            .child(div().flex_1().child(command.label))
                            .when_some(command.keys.clone(), |el, keys| {
                                el.child(keys.appearance(false).text_xs().text_color(muted))
                            })
                    }
                    Source::Files { paths, .. } => {
                        let path = std::path::Path::new(&paths[ix]);
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let dir = path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                        h_flex()
                            .flex_1()
                            .gap_2()
                            .child(Icon::new(IconName::File).small().text_color(muted))
                            .child(div().flex_none().child(name))
                            .child(div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(dir))
                    }
                };
                h_flex()
                    .id(("palette-row", row))
                    .w_full()
                    .h(px(28.))
                    .px_3()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(selection))
                    .on_click(cx.listener(move |this, _, _, cx| this.confirm(row, cx)))
                    .child(content)
                    .into_any_element()
            })
            .collect()
    }
}

/// Files for quick open, relative to `root`.
fn workspace_files(root: &std::path::Path) -> Vec<String> {
    fs_tree::list_files(root, MAX_FILES)
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

impl Render for Palette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let loading = matches!(self.source, Source::Files { loading: true, .. });
        let empty = self.matches.is_empty();
        v_flex()
            .key_context("Palette")
            .w(px(560.))
            .max_h(px(440.))
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .shadow_lg()
            .overflow_hidden()
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                cx.stop_propagation();
                this.move_selection(-1, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                cx.stop_propagation();
                this.move_selection(1, cx);
            }))
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.stop_propagation();
                cx.emit(PaletteEvent::Dismissed);
            }))
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Input::new(&self.query).appearance(false)),
            )
            .map(|el| {
                if empty {
                    el.child(
                        div()
                            .p_3()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(if loading { "Indexing files…" } else { "No matches" }),
                    )
                } else {
                    el.child(
                        div().py_1().child(
                            uniform_list("palette-rows", self.matches.len(), cx.processor(Self::render_rows))
                                .track_scroll(self.scroll.clone())
                                .w_full()
                                .h(px(28. * self.matches.len().min(12) as f32)),
                        ),
                    )
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_open_lists_relative_paths_and_finds_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();

        let mut files = workspace_files(dir.path());
        files.sort();
        assert_eq!(files, vec!["README.md".to_string(), "src/main.rs".to_string()]);

        let best = fuzzy::rank("readme", files.iter().map(|f| (f.as_str(), f.clone())), 5);
        assert_eq!(best.first().map(|(f, _)| f.as_str()), Some("README.md"));
    }
}

//! A window onto one folder: sidebar, editor panes, terminal, agent panel,
//! title and status bars. It owns the wiring between them.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use cockpit_agents::{
    protocol::{DynamicToolCallResponse, ToolOutputItem},
    ToolCall,
};
use cockpit_browser::{
    automation::{click_script, format_console, truncate_for_agent, untrusted_block},
    BrowserAction,
};
use cockpit_editor::watcher::{FsChange, WorkspaceWatcher};
use cockpit_persistence::{TabKind, TabRecord, WorkspaceRecord};
use futures::channel::oneshot;
use cockpit_agents::protocol::PlanStepStatus;
use gpui::{
    deferred, div, prelude::*, px, AnyElement, App, AsyncWindowContext, Context, Entity,
    FocusHandle, Focusable, FontWeight, Hsla, PathPromptOptions, PromptLevel, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::Notification,
    resizable::{h_resizable, resizable_panel, v_resizable, ResizableState},
    v_flex, ActiveTheme as _, IconName, Root, Sizable as _, TitleBar, WindowExt as _,
};
use serde::{Deserialize, Serialize};

use crate::{
    actions,
    agent::{AgentController, AgentControllerEvent, AgentPanel, AgentPanelEvent, ConnectionState},
    appearance::{AppearanceEvent, AppearancePanel},
    assets::icon,
    chrome,
    browser_view::{BrowserEvent, BrowserView},
    editor_view::{EditorEvent, EditorView, LocationKind},
    explorer::{Explorer, ExplorerEvent},
    lsp::{LspEvent, LspManager, Target},
    palette::{LocationItem, Palette, PaletteEvent},
    pane::{Pane, PaneEvent, PaneItem},
    search_panel::{SearchPanel, SearchPanelEvent},
    source_control::{SourceControl, SourceControlEvent},
    state::{AppState, ThemePreference},
    terminal_view::{TerminalPanel, TerminalPanelEvent},
    theme::{self, RADIUS_LG, TEXT_BODY, TEXT_CAPTION, TEXT_SMALL},
};

/// How long a page may take to load before an agent action returns anyway.
const AGENT_LOAD_TIMEOUT: Duration = Duration::from_secs(20);
const PAGE_TEXT_LIMIT: usize = 20_000;
const CONSOLE_LIMIT: usize = 50;
/// How many places Go Back remembers.
const NAV_HISTORY: usize = 100;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SidebarView {
    #[default]
    Explorer,
    SourceControl,
    Search,
}

/// What survives a restart, besides tabs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Layout {
    sidebar: SidebarView,
    sidebar_visible: bool,
    terminal_visible: bool,
    agent_visible: bool,
    split: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            sidebar: SidebarView::Explorer,
            sidebar_visible: true,
            terminal_visible: false,
            agent_visible: true,
            split: false,
        }
    }
}

pub struct Workspace {
    root: PathBuf,
    name: SharedString,
    workspace_id: i64,
    layout: Layout,
    focus_handle: FocusHandle,

    explorer: Entity<Explorer>,
    source_control: Entity<SourceControl>,
    search: Entity<SearchPanel>,
    lsp: Entity<LspManager>,
    panes: [Entity<Pane>; 2],
    active_pane: usize,
    terminal: Option<Entity<TerminalPanel>>,
    agent: Entity<AgentController>,
    agent_panel: Entity<AgentPanel>,
    /// The browser tab Codex drives.
    agent_browser: Option<WeakEntity<BrowserView>>,

    panel_sizes: HashMap<&'static str, Entity<ResizableState>>,
    palette: Option<Entity<Palette>>,
    /// Where focus was before the palette opened.
    palette_return: Option<FocusHandle>,
    appearance: Option<Entity<AppearancePanel>>,
    /// Where go-to-definition and friends jumped from, for Go Back (⌃-).
    nav_back: Vec<Target>,
    nav_forward: Vec<Target>,

    save_tabs: Option<Task<()>>,
    restoring: bool,
    _watcher: Option<WorkspaceWatcher>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Workspace {
    pub fn new(record: WorkspaceRecord, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let root = PathBuf::from(&record.path);
        let layout: Layout = record
            .layout_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_default();

        let explorer = cx.new(|cx| Explorer::new(root.clone(), cx));
        let source_control = cx.new(|cx| SourceControl::new(root.clone(), window, cx));
        let search = cx.new(|cx| SearchPanel::new(root.clone(), window, cx));
        let lsp = cx.new(|_| LspManager::new(root.clone()));
        let panes = [cx.new(Pane::new), cx.new(Pane::new)];
        let agent = cx.new(|cx| AgentController::new(record.id, root.clone(), cx));
        let agent_panel = cx.new(|cx| AgentPanel::new(agent.clone(), window, cx));

        let mut subscriptions = vec![
            cx.subscribe_in(&explorer, window, |this, _, event: &ExplorerEvent, window, cx| match event {
                ExplorerEvent::OpenFile(path) => {
                    this.open_file(path.clone(), true, window, cx);
                }
            }),
            cx.subscribe_in(&source_control, window, |this, _, event: &SourceControlEvent, window, cx| {
                match event {
                    SourceControlEvent::StatusChanged => {
                        let scm = this.source_control.read(cx);
                        let status = scm.status().cloned();
                        let repo_root = scm.repository().map(|r| r.root().to_path_buf());
                        this.explorer
                            .update(cx, |e, cx| e.set_git_status(status.as_ref(), repo_root.as_deref(), cx));
                        cx.notify();
                    }
                    SourceControlEvent::OpenDiff { title, diff } => {
                        this.open_read_only(title, "diff", diff.clone(), window, cx);
                    }
                    SourceControlEvent::Notify(message) => {
                        window.push_notification(Notification::info(message.clone()), cx);
                    }
                }
            }),
            cx.subscribe_in(&search, window, |this, _, event: &SearchPanelEvent, window, cx| match event {
                SearchPanelEvent::OpenMatch { path, line, column } => {
                    if let Some(editor) = this.open_file(path.clone(), true, window, cx) {
                        let (line, column) = (*line, *column);
                        editor.update(cx, |editor, cx| editor.go_to(line, column, window, cx));
                    }
                }
            }),
            cx.subscribe_in(&lsp, window, |this, lsp, event: &LspEvent, _, cx| {
                if let LspEvent::Diagnostics(path) = event {
                    let diagnostics = lsp.read(cx).diagnostics(path).to_vec();
                    for editor in this.editors_for(path, cx) {
                        editor.update(cx, |editor, cx| editor.set_diagnostics(&diagnostics, cx));
                    }
                }
                cx.notify();
            }),
            cx.subscribe_in(&agent, window, Self::on_agent_event),
            cx.subscribe_in(&agent_panel, window, |this, _, event: &AgentPanelEvent, window, cx| match event {
                AgentPanelEvent::OpenFile(path) => {
                    this.open_file(path.clone(), true, window, cx);
                }
            }),
            cx.observe(&agent, |_, _, cx| cx.notify()),
        ];
        for (ix, pane) in panes.iter().enumerate() {
            subscriptions.push(cx.subscribe_in(pane, window, move |this, _, event: &PaneEvent, window, cx| {
                match event {
                    PaneEvent::Focused => this.active_pane = ix,
                    PaneEvent::Changed => this.schedule_save_tabs(cx),
                    PaneEvent::Empty if ix == 1 => {
                        this.layout.split = false;
                        this.active_pane = 0;
                        this.save_layout(cx);
                        this.panes[0].update(cx, |pane, cx| {
                            let active = pane.active_index();
                            pane.activate(active, true, window, cx);
                        });
                    }
                    PaneEvent::Empty => {}
                }
                cx.notify();
            }));
        }

        subscriptions.push(theme::follow_system_appearance(window, cx));

        let mut tasks = Vec::new();
        let watcher = match WorkspaceWatcher::watch(&root) {
            Ok((watcher, changes)) => {
                tasks.push(cx.spawn_in(window, async move |this, cx| {
                    while let Ok(change) = changes.recv().await {
                        if this
                            .update_in(cx, |this, window, cx| this.on_fs_change(change, window, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
                Some(watcher)
            }
            Err(err) => {
                tracing::warn!("watching {}: {err:#}", root.display());
                None
            }
        };

        let name = record.name.clone().into();
        let mut workspace = Self {
            root,
            name,
            workspace_id: record.id,
            layout,
            focus_handle: cx.focus_handle(),
            explorer,
            source_control,
            search,
            lsp,
            panes,
            active_pane: 0,
            terminal: None,
            agent,
            agent_panel,
            agent_browser: None,
            panel_sizes: HashMap::new(),
            palette: None,
            palette_return: None,
            appearance: None,
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
            save_tabs: None,
            restoring: false,
            _watcher: watcher,
            _tasks: tasks,
            _subscriptions: subscriptions,
        };
        if workspace.layout.terminal_visible {
            workspace.ensure_terminal(window, cx);
        }
        cx.on_release(|this: &mut Self, cx| {
            this.agent.update(cx, |agent, _| agent.shutdown());
            this.lsp.update(cx, |lsp, cx| lsp.shutdown(cx));
        })
        .detach();
        workspace.restore_tabs(window, cx);
        workspace
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // --- persistence -------------------------------------------------------

    fn save_layout(&self, cx: &mut App) {
        let Ok(json) = serde_json::to_string(&self.layout) else { return };
        let id = self.workspace_id;
        AppState::global(cx)
            .db
            .write_detached("layout", move |w| w.save_layout(id, &json));
    }

    /// Saves open tabs shortly after the last change.
    fn schedule_save_tabs(&mut self, cx: &mut Context<Self>) {
        if self.restoring {
            return;
        }
        self.save_tabs = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            this.update(cx, |this, cx| this.save_tabs_now(cx)).ok();
        }));
    }

    fn save_tabs_now(&mut self, cx: &mut Context<Self>) {
        let mut tabs = Vec::new();
        for (pane_ix, pane) in self.panes.iter().enumerate() {
            let pane = pane.read(cx);
            for (ix, item) in pane.items().iter().enumerate() {
                let (kind, resource) = match item {
                    PaneItem::Editor(editor) => {
                        let editor = editor.read(cx);
                        if editor.is_read_only() {
                            continue;
                        }
                        (TabKind::File, editor.path().to_string_lossy().into_owned())
                    }
                    PaneItem::Browser(browser) => {
                        let url = browser.read(cx).url().to_string();
                        if url.is_empty() || url == "about:blank" {
                            continue;
                        }
                        (TabKind::Browser, url)
                    }
                };
                tabs.push(TabRecord {
                    pane: pane_ix as i64,
                    kind,
                    resource,
                    active: ix == pane.active_index(),
                });
            }
        }
        let id = self.workspace_id;
        AppState::global(cx)
            .db
            .write_detached("tabs", move |w| w.save_tabs(id, &tabs));
    }

    fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = AppState::global(cx);
        if !state.settings.restore_tabs {
            return;
        }
        let id = self.workspace_id;
        let tabs = state.db.read(move |r| r.load_tabs(id));
        self.restoring = true;
        cx.spawn_in(window, async move |this, cx| {
            let tabs = tabs.await.unwrap_or_else(|err| {
                tracing::warn!("loading tabs: {err:#}");
                Vec::new()
            });
            this.update_in(cx, |this, window, cx| {
                let mut active: [Option<usize>; 2] = [None, None];
                for tab in tabs {
                    let pane = (tab.pane as usize).min(1);
                    if pane == 1 && !this.layout.split {
                        this.layout.split = true;
                    }
                    this.active_pane = pane;
                    let opened = match tab.kind {
                        TabKind::File => {
                            let path = PathBuf::from(&tab.resource);
                            path.is_file() && this.open_file(path, false, window, cx).is_some()
                        }
                        TabKind::Browser => {
                            this.open_browser(&tab.resource, false, false, window, cx);
                            true
                        }
                        TabKind::Diff => false,
                    };
                    if opened && tab.active {
                        active[pane] = Some(this.panes[pane].read(cx).active_index());
                    }
                }
                for (pane, index) in active.into_iter().enumerate() {
                    if let Some(index) = index {
                        this.panes[pane].update(cx, |p, cx| p.activate(index, false, window, cx));
                    }
                }
                if this.panes[1].read(cx).items().is_empty() {
                    this.layout.split = false;
                }
                this.active_pane = 0;
                this.restoring = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // --- opening things ----------------------------------------------------

    fn active_pane(&self) -> &Entity<Pane> {
        &self.panes[if self.layout.split { self.active_pane } else { 0 }]
    }

    fn all_editors(&self, cx: &App) -> Vec<Entity<EditorView>> {
        self.panes
            .iter()
            .flat_map(|pane| pane.read(cx).editors().cloned().collect::<Vec<_>>())
            .collect()
    }

    fn editors_for(&self, path: &Path, cx: &App) -> Vec<Entity<EditorView>> {
        self.all_editors(cx)
            .into_iter()
            .filter(|e| {
                let editor = e.read(cx);
                !editor.is_read_only() && editor.path() == path
            })
            .collect()
    }

    /// Opens `path` in an editor (or switches to it if already open).
    pub fn open_file(
        &mut self,
        path: PathBuf,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<EditorView>> {
        if path.is_dir() {
            return None;
        }
        // Already open: prefer the active pane, then the other.
        let order = if self.active_pane == 1 { [1, 0] } else { [0, 1] };
        for pane_ix in order {
            let pane = self.panes[pane_ix].clone();
            if let Some(index) = pane.read(cx).find_editor(&path, cx) {
                if pane_ix == 1 && !self.layout.split {
                    continue;
                }
                self.active_pane = pane_ix;
                pane.update(cx, |pane, cx| pane.activate(index, focus, window, cx));
                let editor = pane.read(cx).active_editor().cloned();
                self.explorer.update(cx, |e, cx| e.reveal(&path, cx));
                return editor;
            }
        }

        let lsp = self.lsp.clone();
        let mut error = None;
        let editor = cx.new(|cx| match EditorView::open(path.clone(), Some(&lsp), window, cx) {
            Ok(editor) => editor,
            Err(err) => {
                error = Some(err);
                EditorView::read_only("error", "txt", String::new(), window, cx)
            }
        });
        if let Some(err) = error {
            window.push_notification(
                Notification::error(format!("Could not open {}: {err:#}", path.display())),
                cx,
            );
            return None;
        }
        let diagnostics = self.lsp.read(cx).diagnostics(&path).to_vec();
        if !diagnostics.is_empty() {
            editor.update(cx, |editor, cx| editor.set_diagnostics(&diagnostics, cx));
        }
        self._subscriptions.push(cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, window, cx| {
            match event {
                EditorEvent::Saved(_) => this.source_control.update(cx, |scm, cx| scm.refresh(cx)),
                EditorEvent::Locations { kind, symbol, origin, targets } => {
                    this.show_locations(*kind, symbol, origin.clone(), targets.clone(), window, cx);
                }
                EditorEvent::Notify(message) => {
                    window.push_notification(Notification::info(message.clone()), cx);
                }
                EditorEvent::Changed => {}
            }
        }));
        let pane = self.active_pane().clone();
        pane.update(cx, |pane, cx| pane.add_item(PaneItem::Editor(editor.clone()), focus, window, cx));
        self.explorer.update(cx, |e, cx| e.reveal(&path, cx));
        Some(editor)
    }

    // --- code navigation ---------------------------------------------------

    /// Opens `target` and puts the cursor there.
    fn jump_to(&mut self, target: &Target, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(editor) = self.open_file(target.path.clone(), true, window, cx) else {
            return false;
        };
        let (line, column) = (target.line, target.column);
        editor.update(cx, |editor, cx| editor.go_to(line, column, window, cx));
        true
    }

    /// Jumps to `target`, remembering `origin` for Go Back.
    fn navigate(&mut self, origin: Target, target: &Target, window: &mut Window, cx: &mut Context<Self>) {
        if self.jump_to(target, window, cx) && origin != *target {
            push_bounded(&mut self.nav_back, origin);
            self.nav_forward.clear();
        }
    }

    /// The cursor of the active editor, if it shows a file.
    fn current_location(&self, cx: &App) -> Option<Target> {
        let editor = self.active_pane().read(cx).active_editor()?.read(cx);
        if editor.is_read_only() {
            return None;
        }
        let cursor = editor.cursor(cx);
        Some(Target {
            path: editor.path().to_path_buf(),
            line: cursor.line,
            column: cursor.character,
        })
    }

    fn navigate_history(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_location(cx);
        let (from, to) = if back {
            (&mut self.nav_back, &mut self.nav_forward)
        } else {
            (&mut self.nav_forward, &mut self.nav_back)
        };
        let Some(target) = from.pop() else { return };
        if let Some(current) = current {
            push_bounded(to, current);
        }
        self.jump_to(&target, window, cx);
    }

    /// The language server's answer to go-to-definition or find-references:
    /// jump straight to a single definition, list anything else.
    fn show_locations(
        &mut self,
        kind: LocationKind,
        symbol: &str,
        origin: Target,
        mut targets: Vec<Target>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        targets.dedup();
        let symbol = if symbol.is_empty() { "this symbol".to_string() } else { format!("`{symbol}`") };
        if targets.is_empty() {
            let mut message = match kind {
                LocationKind::Definition => format!("No definition found for {symbol}."),
                LocationKind::References => format!("No references found for {symbol}."),
            };
            if self.lsp.read(cx).status().is_some_and(|s| s.contains('…')) {
                message.push_str(" The language server is still loading; try again in a moment.");
            }
            window.push_notification(Notification::info(message), cx);
            return;
        }
        if kind == LocationKind::Definition && targets.len() == 1 {
            self.navigate(origin, &targets[0], window, cx);
            return;
        }
        let title = match kind {
            LocationKind::Definition => format!("{} definitions of {symbol}", targets.len()),
            LocationKind::References if targets.len() == 1 => format!("1 reference to {symbol}"),
            LocationKind::References => format!("{} references to {symbol}", targets.len()),
        };
        let root = self.root.clone();
        let items = cx.background_spawn(async move { location_items(&root, targets) });
        cx.spawn_in(window, async move |this, cx| {
            let items = items.await;
            this.update_in(cx, |this, window, cx| {
                let palette = cx.new(|cx| Palette::locations(title, items, window, cx));
                this.show_palette(palette, Some(origin), window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Stops every language server and reconnects the open editors, which
    /// starts them again.
    fn restart_language_servers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editors = self.all_editors(cx);
        for editor in &editors {
            editor.update(cx, |editor, cx| editor.detach_lsp(cx));
        }
        self.lsp.update(cx, |lsp, cx| lsp.restart(cx));
        let lsp = self.lsp.clone();
        for editor in &editors {
            editor.update(cx, |editor, cx| editor.connect_lsp(&lsp, cx));
        }
        window.push_notification(Notification::info("Restarting language servers…"), cx);
        cx.notify();
    }

    fn open_read_only(&mut self, title: &str, extension: &str, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| EditorView::read_only(title, extension, text, window, cx));
        let pane = self.active_pane().clone();
        pane.update(cx, |pane, cx| pane.add_item(PaneItem::Editor(editor), true, window, cx));
    }

    pub fn open_browser(
        &mut self,
        url: &str,
        focus: bool,
        agent_driven: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<BrowserView> {
        let browser = cx.new(|cx| BrowserView::new(url, agent_driven, window, cx));
        self._subscriptions.push(cx.subscribe_in(&browser, window, |this, browser, event: &BrowserEvent, window, cx| {
            match event {
                BrowserEvent::OpenInNewTab(url) => {
                    this.open_browser(url, true, false, window, cx);
                }
                BrowserEvent::Navigated { url, title } => {
                    if browser.read(cx).agent_driven {
                        let url = url.clone();
                        this.agent.update(cx, |agent, _| agent.set_agent_browser_url(Some(url)));
                    }
                    if url.starts_with("http") {
                        let state = AppState::global(cx);
                        let profile = state.browser_profile_id;
                        let (url, title) = (url.clone(), title.clone());
                        state.db.write_detached("history", move |w| {
                            w.record_visit(profile, &url, (!title.is_empty()).then_some(title.as_str()))
                        });
                    }
                    this.schedule_save_tabs(cx);
                }
                BrowserEvent::Changed => {}
            }
        }));
        // Codex's tab goes to the other pane when split, so it does not
        // cover what the user is looking at.
        let pane = if agent_driven && self.layout.split {
            self.panes[1 - self.active_pane].clone()
        } else {
            self.active_pane().clone()
        };
        pane.update(cx, |pane, cx| pane.add_item(PaneItem::Browser(browser.clone()), focus, window, cx));
        browser
    }

    fn new_browser_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let home = AppState::global(cx).settings.home_page.clone();
        let browser = self.open_browser(&home, true, false, window, cx);
        browser.update(cx, |browser, cx| browser.focus_address(window, cx));
    }

    // --- filesystem --------------------------------------------------------

    fn on_fs_change(&mut self, change: FsChange, window: &mut Window, cx: &mut Context<Self>) {
        if !change.paths.is_empty() {
            self.explorer.update(cx, |e, cx| e.refresh_paths(change.paths.iter(), cx));
            for editor in self.all_editors(cx) {
                let path = editor.read(cx).path().to_path_buf();
                if change.paths.contains(&path) {
                    editor.update(cx, |editor, cx| editor.on_disk_change(window, cx));
                }
            }
        }
        self.source_control.update(cx, |scm, cx| scm.refresh(cx));
    }

    // --- agent -------------------------------------------------------------

    fn on_agent_event(
        &mut self,
        _: &Entity<AgentController>,
        event: &AgentControllerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            AgentControllerEvent::Notify(message) => {
                window.push_notification(Notification::info(message.clone()), cx);
            }
            AgentControllerEvent::FilesChanged(paths) => {
                for editor in self.all_editors(cx) {
                    let path = editor.read(cx).path().to_path_buf();
                    if paths.contains(&path) {
                        editor.update(cx, |editor, cx| editor.on_disk_change(window, cx));
                    }
                }
                self.explorer.update(cx, |e, cx| e.refresh_paths(paths.iter(), cx));
                self.source_control.update(cx, |scm, cx| scm.refresh(cx));
            }
            AgentControllerEvent::RunBrowserAction { call, action } => {
                self.run_browser_action((**call).clone(), action.clone(), window, cx);
            }
            AgentControllerEvent::RunInTerminal { program, args } => {
                let terminal = self.ensure_terminal(window, cx);
                let program = Some((program.clone(), args.clone()));
                terminal.update(cx, |t, cx| {
                    t.add_terminal(program, cx);
                    t.focus_active(window, cx);
                });
                self.layout.terminal_visible = true;
                self.save_layout(cx);
                cx.notify();
            }
        }
    }

    /// The tab Codex drives, opened on first use.
    fn agent_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<BrowserView> {
        if let Some(browser) = self.agent_browser.as_ref().and_then(WeakEntity::upgrade) {
            let open = self
                .panes
                .iter()
                .any(|pane| pane.read(cx).position_of(browser.entity_id()).is_some());
            if open {
                return browser;
            }
        }
        let browser = self.open_browser("about:blank", false, true, window, cx);
        self.agent_browser = Some(browser.downgrade());
        browser
    }

    fn run_browser_action(
        &mut self,
        call: ToolCall,
        action: BrowserAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let agent = self.agent.clone();
        let respond = move |response: DynamicToolCallResponse, cx: &mut App| {
            agent.read(cx).respond_tool_call(&call, response);
        };

        if let BrowserAction::ListTabs = action {
            let agent_tab = self.agent_browser.as_ref().map(|b| b.entity_id());
            let mut listing = String::new();
            for (ix, browser) in self.panes.iter().flat_map(|p| p.read(cx).browsers().cloned().collect::<Vec<_>>()).enumerate() {
                let view = browser.read(cx);
                let marker = if Some(browser.entity_id()) == agent_tab { " (your tab)" } else { "" };
                listing.push_str(&format!("{}. {} — {}{marker}\n", ix + 1, view.title(), view.url()));
            }
            if listing.is_empty() {
                listing = "No browser tabs are open.".into();
            }
            respond(
                DynamicToolCallResponse::text(untrusted_block("Open tabs", "the IDE browser", &listing), true),
                cx,
            );
            return;
        }

        let browser = self.agent_browser(window, cx);
        let Some(tab) = browser.read(cx).tab().cloned() else {
            respond(
                DynamicToolCallResponse::text("The embedded browser is not available in this build.", false),
                cx,
            );
            return;
        };
        let url = browser.read(cx).url().to_string();

        match action {
            BrowserAction::Navigate { url } => {
                browser.update(cx, |b, cx| b.navigate(&url, cx));
                self.respond_after_load(browser, respond, window, cx);
            }
            BrowserAction::Reload => {
                browser.update(cx, |b, cx| b.reload(cx));
                self.respond_after_load(browser, respond, window, cx);
            }
            BrowserAction::GoBack => {
                browser.update(cx, |b, _| b.go_back());
                self.respond_after_load(browser, respond, window, cx);
            }
            BrowserAction::Screenshot => {
                let response = match browser.read(cx).screenshot_data_url() {
                    Ok(image_url) => DynamicToolCallResponse {
                        content_items: vec![
                            ToolOutputItem::InputText {
                                text: format!("Screenshot of {url} (page content is untrusted data)."),
                            },
                            ToolOutputItem::InputImage { image_url },
                        ],
                        success: true,
                    },
                    Err(err) => DynamicToolCallResponse::text(format!("No screenshot: {err:#}"), false),
                };
                respond(response, cx);
            }
            BrowserAction::ReadConsole { max_entries } => {
                let text = format_console(&tab.console(), max_entries.unwrap_or(CONSOLE_LIMIT));
                respond(
                    DynamicToolCallResponse::text(untrusted_block("Console output", &url, &text), true),
                    cx,
                );
            }
            BrowserAction::ReadPageText { max_chars } => {
                let (tx, rx) = oneshot::channel();
                tab.page_text(Box::new(move |text| {
                    let _ = tx.send(text);
                }));
                cx.spawn(async move |_, cx| {
                    let response = match rx.await {
                        Ok(text) => DynamicToolCallResponse::text(
                            untrusted_block(
                                "Visible text",
                                &url,
                                &truncate_for_agent(&text, max_chars.unwrap_or(PAGE_TEXT_LIMIT)),
                            ),
                            true,
                        ),
                        Err(_) => DynamicToolCallResponse::text("The page closed before its text was read.", false),
                    };
                    cx.update(|cx| respond(response, cx)).ok();
                })
                .detach();
            }
            BrowserAction::Click { selector } => {
                let (tx, rx) = oneshot::channel();
                tab.eval(
                    &click_script(&selector),
                    Box::new(move |result| {
                        let _ = tx.send(result);
                    }),
                );
                cx.spawn(async move |_, cx| {
                    let response = match rx.await {
                        Ok(Ok(value)) if value.get("found").and_then(|f| f.as_bool()) == Some(true) => {
                            let detail = format!(
                                "<{}> {}",
                                value.get("tag").and_then(|t| t.as_str()).unwrap_or("element"),
                                value.get("text").and_then(|t| t.as_str()).unwrap_or("")
                            );
                            DynamicToolCallResponse::text(
                                format!(
                                    "Clicked the first match for `{selector}`.\n{}",
                                    untrusted_block("Clicked element", &url, &detail)
                                ),
                                true,
                            )
                        }
                        Ok(Ok(_)) => DynamicToolCallResponse::text(format!("No element matches `{selector}`."), false),
                        Ok(Err(err)) => DynamicToolCallResponse::text(format!("The click script failed: {err}"), false),
                        Err(_) => DynamicToolCallResponse::text("The page closed before the click ran.", false),
                    };
                    cx.update(|cx| respond(response, cx)).ok();
                })
                .detach();
            }
            BrowserAction::ListTabs => unreachable!("handled above"),
        }
    }

    /// Answers a navigation-type action once the page settles.
    fn respond_after_load(
        &mut self,
        browser: Entity<BrowserView>,
        respond: impl FnOnce(DynamicToolCallResponse, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let browser = browser.downgrade();
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            let started = std::time::Instant::now();
            // Give the navigation a moment to begin before checking for idle.
            cx.background_executor().timer(Duration::from_millis(300)).await;
            loop {
                let state = browser.read_with(cx, |b, _| b.state().clone());
                let Ok(state) = state else {
                    cx.update(|_, cx| {
                        respond(DynamicToolCallResponse::text("The browser tab was closed.", false), cx)
                    })
                    .ok();
                    return;
                };
                let timed_out = started.elapsed() > AGENT_LOAD_TIMEOUT;
                if !state.loading || timed_out {
                    let response = match (&state.load_error, timed_out) {
                        (Some(err), _) => DynamicToolCallResponse::text(format!("Failed to load {}: {err}", state.url), false),
                        (None, true) => DynamicToolCallResponse::text(
                            format!("{} is still loading after {}s.", state.url, AGENT_LOAD_TIMEOUT.as_secs()),
                            true,
                        ),
                        (None, false) => DynamicToolCallResponse::text(
                            format!(
                                "Loaded {}\n{}",
                                state.url,
                                untrusted_block("Title", &state.url, &state.title)
                            ),
                            true,
                        ),
                    };
                    cx.update(|_, cx| respond(response, cx)).ok();
                    return;
                }
                cx.background_executor().timer(Duration::from_millis(200)).await;
            }
        })
        .detach();
    }

    // --- layout toggles ----------------------------------------------------

    fn ensure_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let root = self.root.clone();
        let terminal = cx.new(|cx| TerminalPanel::new(root, cx));
        self._subscriptions.push(cx.subscribe_in(
            &terminal,
            window,
            |this, _, event: &TerminalPanelEvent, window, cx| match event {
                TerminalPanelEvent::OpenUrl(url) => {
                    this.open_browser(url, true, false, window, cx);
                }
            },
        ));
        self.terminal = Some(terminal.clone());
        terminal
    }

    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.layout.terminal_visible = !self.layout.terminal_visible;
        if self.layout.terminal_visible {
            let terminal = self.ensure_terminal(window, cx);
            terminal.update(cx, |t, cx| t.focus_active(window, cx));
        } else {
            self.focus_center(window, cx);
        }
        self.save_layout(cx);
        cx.notify();
    }

    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existed = self.terminal.is_some();
        let terminal = self.ensure_terminal(window, cx);
        terminal.update(cx, |t, cx| {
            if existed {
                t.add_terminal(None, cx);
            }
            t.focus_active(window, cx);
        });
        self.layout.terminal_visible = true;
        self.save_layout(cx);
        cx.notify();
    }

    fn show_sidebar(&mut self, view: SidebarView, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.sidebar_visible && self.layout.sidebar == view && view != SidebarView::Search {
            self.layout.sidebar_visible = false;
        } else {
            self.layout.sidebar = view;
            self.layout.sidebar_visible = true;
        }
        match (self.layout.sidebar_visible, view) {
            (true, SidebarView::Search) => self.search.update(cx, |s, cx| s.focus_query(None, window, cx)),
            (true, SidebarView::Explorer) => window.focus(&self.explorer.focus_handle(cx)),
            (true, SidebarView::SourceControl) => window.focus(&self.source_control.focus_handle(cx)),
            (false, _) => self.focus_center(window, cx),
        }
        self.save_layout(cx);
        cx.notify();
    }

    fn toggle_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.layout.agent_visible = !self.layout.agent_visible;
        if self.layout.agent_visible {
            window.focus(&self.agent_panel.focus_handle(cx));
        } else {
            self.focus_center(window, cx);
        }
        self.save_layout(cx);
        cx.notify();
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.layout.sidebar_visible = !self.layout.sidebar_visible;
        if !self.layout.sidebar_visible {
            self.focus_center(window, cx);
        }
        self.save_layout(cx);
        cx.notify();
    }

    /// Shows source control with the commit message focused.
    fn start_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.layout.sidebar = SidebarView::SourceControl;
        self.layout.sidebar_visible = true;
        self.source_control.update(cx, |scm, cx| scm.focus_message(window, cx));
        self.save_layout(cx);
        cx.notify();
    }

    fn toggle_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.appearance.take().is_some() {
            self.focus_center(window, cx);
            cx.notify();
            return;
        }
        let panel = cx.new(|cx| AppearancePanel::new(window, cx));
        self._subscriptions.push(cx.subscribe_in(&panel, window, |this, _, event: &AppearanceEvent, window, cx| {
            match event {
                AppearanceEvent::Dismissed => {
                    this.appearance = None;
                    this.focus_center(window, cx);
                    cx.notify();
                }
            }
        }));
        window.focus(&panel.focus_handle(cx));
        self.appearance = Some(panel);
        cx.notify();
    }

    fn focus_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.layout.agent_visible {
            self.layout.agent_visible = true;
            self.save_layout(cx);
        }
        window.focus(&self.agent_panel.focus_handle(cx));
        cx.notify();
    }

    fn toggle_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.split {
            // Fold the second pane's tabs into the first.
            let items: Vec<PaneItem> = self.panes[1].read(cx).items().to_vec();
            for item in items {
                let id = item.entity_id();
                self.panes[1].update(cx, |pane, cx| pane.take_item(id, cx));
                self.panes[0].update(cx, |pane, cx| pane.add_item(item, false, window, cx));
            }
            self.layout.split = false;
            self.active_pane = 0;
            self.panes[0].update(cx, |pane, cx| {
                let active = pane.active_index();
                pane.activate(active, true, window, cx);
            });
        } else {
            // The new pane starts empty; files opened while it is active go there.
            self.layout.split = true;
            self.active_pane = 1;
            window.focus(&self.panes[1].focus_handle(cx));
        }
        self.save_layout(cx);
        self.schedule_save_tabs(cx);
        cx.notify();
    }

    fn focus_center(&self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane().clone();
        window.focus(&pane.focus_handle(cx));
    }

    // --- commands ----------------------------------------------------------

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_pane().read(cx).active_editor().cloned() else { return };
        if let Err(err) = editor.update(cx, |editor, cx| editor.save(cx)) {
            window.push_notification(Notification::error(format!("Save failed: {err:#}")), cx);
        }
    }

    fn open_folder(&mut self, _: &actions::OpenFolder, _: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(path) = paths.into_iter().next() {
                    cx.update(|cx| crate::open_folder(path, cx)).ok();
                }
            }
        })
        .detach();
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = if cx.theme().is_dark() { ThemePreference::Light } else { ThemePreference::Dark };
        AppState::save_settings(cx, |settings| settings.theme = next);
        theme::apply(Some(window), cx);
    }

    fn about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let detail = format!(
            "Version {}\nA native IDE with an embedded Chromium browser and the Codex agent.",
            cockpit_common::APP_VERSION
        );
        // Nothing to do with the answer; dropping the receiver keeps the alert.
        drop(window.prompt(PromptLevel::Info, cockpit_common::APP_NAME, Some(&detail), &["OK"], cx));
    }

    fn open_palette(&mut self, files: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
        }
        let palette = if files {
            let root = self.root.clone();
            cx.new(|cx| Palette::files(root, window, cx))
        } else {
            cx.new(|cx| Palette::commands(window, cx))
        };
        self.show_palette(palette, None, window, cx);
    }

    /// Shows `palette`; `origin` is where a chosen location is jumped from.
    fn show_palette(&mut self, palette: Entity<Palette>, origin: Option<Target>, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
        }
        self.palette_return = window.focused(cx);
        self._subscriptions.push(cx.subscribe_in(&palette, window, move |this, _, event: &PaletteEvent, window, cx| {
            match event {
                PaletteEvent::Dismissed => this.close_palette(window, cx),
                PaletteEvent::OpenFile(path) => {
                    this.close_palette(window, cx);
                    this.open_file(path.clone(), true, window, cx);
                }
                PaletteEvent::OpenLocation(target) => {
                    this.close_palette(window, cx);
                    match origin.clone() {
                        Some(origin) => this.navigate(origin, target, window, cx),
                        None => {
                            this.jump_to(target, window, cx);
                        }
                    }
                }
                PaletteEvent::Run(action) => {
                    let action = action.boxed_clone();
                    this.close_palette(window, cx);
                    // Runs once this update ends, from wherever focus went back to.
                    window.dispatch_action(action, cx);
                }
            }
        }));
        window.focus(&palette.focus_handle(cx));
        self.palette = Some(palette);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_none() {
            return;
        }
        match self.palette_return.take() {
            Some(handle) => window.focus(&handle),
            None => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    // --- rendering ---------------------------------------------------------

    /// Workspace name, then the active file's path inside it.
    fn breadcrumb(&self, cx: &App) -> Vec<SharedString> {
        let mut crumbs: Vec<SharedString> = vec![self.name.clone()];
        match self.active_pane().read(cx).active_item() {
            Some(PaneItem::Editor(editor)) => {
                let editor = editor.read(cx);
                if editor.is_read_only() {
                    crumbs.push(editor.title());
                } else {
                    let path = editor.path();
                    let relative = path.strip_prefix(&self.root).unwrap_or(path);
                    crumbs.extend(
                        relative
                            .components()
                            .map(|c| SharedString::from(c.as_os_str().to_string_lossy().into_owned())),
                    );
                }
            }
            Some(item) => crumbs.push(item.title(cx)),
            None => {}
        }
        // Long paths keep the root and the last two parts.
        if crumbs.len() > 4 {
            let tail = crumbs.split_off(crumbs.len() - 2);
            crumbs.truncate(1);
            crumbs.push("…".into());
            crumbs.extend(tail);
        }
        crumbs
    }

    /// What Codex is doing, for the pill in the title bar.
    fn agent_pill(&self, cx: &App) -> Option<(SharedString, Hsla, Hsla)> {
        let t = theme::tokens(cx);
        let agent = self.agent.read(cx);
        if !agent.pending_tools().is_empty() {
            return Some(("Needs approval".into(), t.warning, t.warning.alpha(0.12)));
        }
        if !agent.is_running() {
            return None;
        }
        let progress = agent.active_thread().and_then(|thread| {
            let steps = thread.transcript.plan().0;
            if steps.is_empty() {
                return None;
            }
            let done = steps.iter().filter(|s| s.status == PlanStepStatus::Completed).count();
            let current = steps
                .iter()
                .position(|s| s.status == PlanStepStatus::InProgress)
                .map(|ix| ix + 1)
                .unwrap_or((done + 1).min(steps.len()));
            Some((current, steps.len()))
        });
        let label = match progress {
            Some((current, total)) => format!("Working · step {current} of {total}"),
            None => "Working".to_string(),
        };
        Some((label.into(), t.accent_text, t.accent_soft))
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme::tokens(cx).clone();
        let crumbs = self.breadcrumb(cx);
        let last = crumbs.len() - 1;
        let mut breadcrumb = h_flex().min_w_0().gap(px(6.)).text_size(TEXT_BODY).overflow_hidden();
        for (ix, crumb) in crumbs.into_iter().enumerate() {
            if ix > 0 {
                breadcrumb = breadcrumb.child(div().flex_none().text_color(t.fg_subtle).child("/"));
            }
            breadcrumb = breadcrumb.child(
                div()
                    .when(ix < last, |el| el.flex_none().text_color(t.frame_muted))
                    .when(ix == last, |el| el.min_w_0().truncate().text_color(t.fg).font_weight(FontWeight::MEDIUM))
                    .child(crumb),
            );
        }

        let pill = self.agent_pill(cx).map(|(label, color, bg)| {
            h_flex()
                .id("agent-pill")
                .flex_none()
                .h(px(20.))
                .px(px(8.))
                .gap(px(6.))
                .rounded_full()
                .bg(bg)
                .text_color(color)
                .text_size(TEXT_CAPTION)
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .child(chrome::status_dot(color))
                .child(SharedString::from(label.to_uppercase()))
                .on_click(cx.listener(|this, _, window, cx| this.focus_agent(window, cx)))
        });

        let running = self.agent.read(cx).is_running();
        let changes = self.source_control.read(cx).change_count();
        let (hover, surface, border) = (t.raised, t.surface, t.border_strong);

        TitleBar::new().child(
            h_flex()
                .w_full()
                .h_full()
                .pr(px(10.))
                .gap(px(10.))
                .child(chrome::logo_mark(16., cx))
                .child(breadcrumb)
                .children(pill)
                .child(div().flex_1())
                .child(
                    h_flex()
                        .flex_none()
                        .gap(px(2.))
                        .child(
                            chrome::frame_button("customize-theme", IconName::Palette, self.appearance.is_some(), cx)
                                .tooltip(chrome::tooltip("Appearance"))
                                .on_click(cx.listener(|this, _, window, cx| this.toggle_appearance(window, cx))),
                        )
                        .child(
                            chrome::frame_button("toggle-sidebar", IconName::PanelLeft, false, cx)
                                .tooltip(chrome::tooltip("Toggle sidebar (⌘B)"))
                                .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx))),
                        )
                        .child(
                            chrome::frame_button("toggle-terminal", IconName::PanelBottom, false, cx)
                                .tooltip(chrome::tooltip("Toggle terminal (⌘J)"))
                                .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx))),
                        )
                        .child(
                            chrome::frame_button("toggle-agent", IconName::PanelRight, false, cx)
                                .tooltip(chrome::tooltip("Toggle Codex (⌥⌘B)"))
                                .on_click(cx.listener(|this, _, window, cx| this.toggle_agent(window, cx))),
                        ),
                )
                .when(running, |el| {
                    el.child(
                        div()
                            .id("title-stop")
                            .flex_none()
                            .h(px(26.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .rounded(px(7.))
                            .border_1()
                            .border_color(border)
                            .bg(surface)
                            .text_size(TEXT_BODY)
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .tooltip(chrome::tooltip("Stop the running turn (⌃C in the composer)"))
                            .child("Stop")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.agent.update(cx, |agent, cx| agent.interrupt(cx));
                            })),
                    )
                })
                .when(changes > 0, |el| {
                    el.child(
                        Button::new("title-commit")
                            .label(if changes == 1 { "Commit 1 file".to_string() } else { format!("Commit {changes} files") })
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| this.start_commit(window, cx))),
                    )
                }),
        )
    }

    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme::tokens(cx).clone();
        let current = self.layout.sidebar_visible.then_some(self.layout.sidebar);
        let changes = self.source_control.read(cx).change_count();
        let item = |id: &'static str, view: SidebarView, icon: gpui_component::Icon, tooltip: &'static str, cx: &mut Context<Self>| {
            chrome::frame_button(id, icon, current == Some(view), cx)
                .tooltip(chrome::tooltip(tooltip))
                .on_click(cx.listener(move |this, _, window, cx| this.show_sidebar(view, window, cx)))
        };
        v_flex()
            .w(px(44.))
            .h_full()
            .flex_none()
            .items_center()
            .pt(px(2.))
            .pb(px(8.))
            .gap(px(4.))
            .child(item("activity-explorer", SidebarView::Explorer, icon("files"), "Explorer (⇧⌘E)", cx))
            .child(item("activity-search", SidebarView::Search, gpui_component::Icon::new(IconName::Search), "Search (⇧⌘F)", cx))
            .child(
                div()
                    .relative()
                    .child(item("activity-scm", SidebarView::SourceControl, icon("git-branch"), "Source control (⇧⌘G)", cx))
                    .when(changes > 0, |el| {
                        el.child(
                            div()
                                .absolute()
                                .top(px(-3.))
                                .right(px(-5.))
                                .min_w(px(15.))
                                .h(px(15.))
                                .px(px(4.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .border_2()
                                .border_color(t.frame)
                                .bg(t.accent)
                                .text_color(t.accent_fg)
                                .text_size(px(9.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(if changes > 99 { "99+".to_string() } else { changes.to_string() }),
                        )
                    }),
            )
            .child(div().flex_1())
            .child(
                chrome::frame_button("activity-browser", IconName::Globe, false, cx)
                    .tooltip(chrome::tooltip("New browser tab (⌘T)"))
                    .on_click(cx.listener(|this, _, window, cx| this.new_browser_tab(window, cx))),
            )
            .child(
                chrome::frame_button("activity-codex", icon("sparkles"), self.layout.agent_visible, cx)
                    .tooltip(chrome::tooltip("Codex (⌥⌘B)"))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_agent(window, cx))),
            )
    }

    fn render_sidebar(&self) -> AnyElement {
        match self.layout.sidebar {
            SidebarView::Explorer => self.explorer.clone().into_any_element(),
            SidebarView::SourceControl => self.source_control.clone().into_any_element(),
            SidebarView::Search => self.search.clone().into_any_element(),
        }
    }

    /// Panel sizes for one arrangement of panels. Each arrangement keeps its
    /// own, so hiding the sidebar and showing it again restores its width.
    /// (Panels are left out rather than hidden: a group sizes a panel that
    /// starts hidden badly once it appears.)
    fn panel_sizes(&mut self, key: &'static str, cx: &mut Context<Self>) -> Entity<ResizableState> {
        self.panel_sizes
            .entry(key)
            .or_insert_with(|| cx.new(|_| ResizableState::default()))
            .clone()
    }

    fn render_center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if !self.layout.split {
            return self.panes[0].clone().into_any_element();
        }
        let sizes = self.panel_sizes("panes", cx);
        let marker = theme::tokens(cx).accent.alpha(0.6);
        let pane = |ix: usize| {
            div()
                .relative()
                .size_full()
                .child(self.panes[ix].clone())
                // The pane that takes new tabs.
                .when(ix == self.active_pane, |el| {
                    el.child(div().absolute().top_0().left_0().right_0().h(px(2.)).bg(marker))
                })
        };
        h_resizable("workspace-panes")
            .with_state(&sizes)
            .child(resizable_panel().child(pane(0)))
            .child(resizable_panel().child(pane(1)))
            .into_any_element()
    }

    /// The editor area, with the terminal under it when shown.
    fn render_middle(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let center = self.render_center(cx);
        let Some(terminal) = self.terminal.clone().filter(|_| self.layout.terminal_visible) else {
            return center;
        };
        let sizes = self.panel_sizes("rows", cx);
        v_resizable("workspace-rows")
            .with_state(&sizes)
            .child(resizable_panel().child(center))
            .child(
                resizable_panel()
                    .size(px(260.))
                    .size_range(px(80.)..px(1600.))
                    .child(div().size_full().child(terminal)),
            )
            .into_any_element()
    }

    fn render_columns(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let middle = self.render_middle(cx);
        let (sidebar, agent) = (self.layout.sidebar_visible, self.layout.agent_visible);
        if !sidebar && !agent {
            return middle;
        }
        let key = match (sidebar, agent) {
            (true, true) => "columns",
            (true, false) => "columns-sidebar",
            _ => "columns-agent",
        };
        let sizes = self.panel_sizes(key, cx);
        h_resizable(key)
            .with_state(&sizes)
            .when(sidebar, |group| {
                group.child(
                    resizable_panel()
                        .size(px(248.))
                        .size_range(px(160.)..px(640.))
                        .child(div().size_full().child(self.render_sidebar())),
                )
            })
            .child(resizable_panel().child(middle))
            .when(agent, |group| {
                group.child(
                    resizable_panel()
                        .size(px(380.))
                        .size_range(px(300.)..px(1000.))
                        .child(div().size_full().child(self.agent_panel.clone())),
                )
            })
            .into_any_element()
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme::tokens(cx).clone();
        let muted = t.frame_muted;
        let scm = self.source_control.read(cx);
        let branch = scm.branch().map(str::to_string);
        let changes = scm.change_count();
        let lsp = self.lsp.read(cx);
        let (errors, warnings) = lsp.diagnostic_counts();
        let lsp_status = lsp.status();
        let editor = self.active_pane().read(cx).active_editor().cloned();
        let editor_info = editor.map(|editor| {
            let editor = editor.read(cx);
            let cursor = editor.cursor(cx);
            (
                format!("Ln {}, Col {}", cursor.line + 1, cursor.character + 1),
                editor.language(),
                editor.line_ending(),
            )
        });
        let agent = self.agent.read(cx);
        let name = agent.agent_name();
        let agent_status: Option<(SharedString, Hsla)> = match agent.state() {
            ConnectionState::Connected { .. } if agent.is_running() => Some((format!("{name} working").into(), t.accent_text)),
            ConnectionState::Connected { .. } => Some((format!("{name} ready").into(), muted)),
            ConnectionState::Connecting => Some((format!("{name} starting…").into(), muted)),
            ConnectionState::NotInstalled => Some((format!("{name} not installed").into(), t.danger)),
            ConnectionState::Failed(_) => Some((format!("{name} stopped").into(), t.danger)),
            ConnectionState::Idle => None,
        };
        let hover = t.frame_hover;
        let clickable = move |id: &'static str| {
            h_flex()
                .id(id)
                .h(px(20.))
                .px(px(6.))
                .gap(px(5.))
                .rounded(px(5.))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
        };

        h_flex()
            .h(px(26.))
            .flex_none()
            .pl(px(8.))
            .pr(px(12.))
            .gap(px(6.))
            .text_size(TEXT_SMALL)
            .text_color(muted)
            .when_some(branch, |el, branch| {
                el.child(
                    clickable("status-branch")
                        .child(icon("git-branch").size(px(12.)))
                        .child(branch)
                        .when(changes > 0, |el| el.child(div().text_color(t.fg_subtle).child(format!("· {changes}"))))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.layout.sidebar_visible = false;
                            this.show_sidebar(SidebarView::SourceControl, window, cx);
                        })),
                )
            })
            .child(
                h_flex()
                    .px(px(6.))
                    .gap(px(10.))
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .text_color(if errors > 0 { t.danger } else { muted })
                            .child(gpui_component::Icon::new(IconName::CircleX).size(px(12.)))
                            .child(errors.to_string()),
                    )
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .text_color(if warnings > 0 { t.warning } else { muted })
                            .child(gpui_component::Icon::new(IconName::TriangleAlert).size(px(12.)))
                            .child(warnings.to_string()),
                    ),
            )
            .when_some(lsp_status, |el, status| el.child(div().truncate().max_w(px(320.)).child(status)))
            .child(div().flex_1())
            .when_some(editor_info, |el, (position, language, ending)| {
                el.child(div().px(px(6.)).child(position))
                    .child(div().px(px(6.)).child(language))
                    .child(div().px(px(6.)).child(ending))
            })
            .when_some(agent_status, |el, (text, color)| {
                el.child(
                    clickable("status-codex")
                        .text_color(color)
                        .child(chrome::status_dot(color))
                        .child(text)
                        .on_click(cx.listener(|this, _, window, cx| this.focus_agent(window, cx))),
                )
            })
    }

    /// The content card: every panel lives inside it.
    fn render_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let columns = self.render_columns(cx);
        let t = theme::tokens(cx);
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .mr(px(8.))
            .rounded(RADIUS_LG)
            .border_1()
            .border_color(t.border)
            .bg(t.surface)
            .overflow_hidden()
            .when(!t.dark, |el| el.shadow_xs())
            .child(columns)
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialogs = Root::render_dialog_layer(window, cx);
        let notifications = Root::render_notification_layer(window, cx);
        let card = self.render_card(cx);
        let t = theme::tokens(cx).clone();

        v_flex()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(t.frame)
            .text_color(t.fg)
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(|this, _: &actions::Save, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &actions::CloseTab, window, cx| {
                let pane = this.active_pane().clone();
                pane.update(cx, |pane, cx| pane.close_active(window, cx));
            }))
            .on_action(cx.listener(|this, _: &actions::NextTab, window, cx| {
                let pane = this.active_pane().clone();
                pane.update(cx, |pane, cx| pane.activate_next(window, cx));
            }))
            .on_action(cx.listener(|this, _: &actions::PreviousTab, window, cx| {
                let pane = this.active_pane().clone();
                pane.update(cx, |pane, cx| pane.activate_previous(window, cx));
            }))
            .on_action(cx.listener(|this, _: &actions::SplitPane, window, cx| this.toggle_split(window, cx)))
            .on_action(cx.listener(|this, _: &actions::NewBrowserTab, window, cx| this.new_browser_tab(window, cx)))
            .on_action(cx.listener(|this, _: &actions::NewTerminal, window, cx| this.new_terminal(window, cx)))
            .on_action(cx.listener(|this, _: &actions::ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)))
            .on_action(cx.listener(|this, _: &actions::ToggleTerminal, window, cx| this.toggle_terminal(window, cx)))
            .on_action(cx.listener(|this, _: &actions::ToggleAgentPanel, window, cx| this.toggle_agent(window, cx)))
            .on_action(cx.listener(|this, _: &actions::ShowExplorer, window, cx| {
                this.show_sidebar(SidebarView::Explorer, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ShowSourceControl, window, cx| {
                this.show_sidebar(SidebarView::SourceControl, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ShowSearch, window, cx| {
                this.show_sidebar(SidebarView::Search, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::CommandPalette, window, cx| this.open_palette(false, window, cx)))
            .on_action(cx.listener(|this, _: &actions::QuickOpen, window, cx| this.open_palette(true, window, cx)))
            .on_action(cx.listener(|this, _: &actions::FocusAgent, window, cx| this.focus_agent(window, cx)))
            .on_action(cx.listener(|this, _: &actions::NewAgentThread, window, cx| {
                this.focus_agent(window, cx);
                this.agent.update(cx, |agent, cx| agent.new_thread(cx));
            }))
            .on_action(cx.listener(|this, _: &actions::InterruptAgent, _, cx| {
                this.agent.update(cx, |agent, cx| agent.interrupt(cx));
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleTheme, window, cx| this.toggle_theme(window, cx)))
            .on_action(cx.listener(|this, _: &actions::CustomizeTheme, window, cx| this.toggle_appearance(window, cx)))
            .on_action(cx.listener(|this, _: &actions::About, window, cx| this.about(window, cx)))
            .on_action(cx.listener(|this, _: &actions::NavigateBack, window, cx| this.navigate_history(true, window, cx)))
            .on_action(cx.listener(|this, _: &actions::NavigateForward, window, cx| {
                this.navigate_history(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::RestartLanguageServers, window, cx| {
                this.restart_language_servers(window, cx)
            }))
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_activity_bar(cx))
                    .child(card),
            )
            .child(self.render_status_bar(cx))
            .when_some(self.appearance.clone(), |el, panel| {
                el.child(
                    deferred(
                        div()
                            .id("appearance-backdrop")
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .occlude()
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_appearance(window, cx)))
                            .child(
                                div()
                                    .id("appearance-frame")
                                    .absolute()
                                    .top(px(40.))
                                    .right(px(12.))
                                    .on_click(|_, _, cx| cx.stop_propagation())
                                    .child(panel),
                            ),
                    )
                    .with_priority(1),
                )
            })
            .when_some(self.palette.clone(), |el, palette| {
                el.child(
                    deferred(
                        div()
                            .id("palette-backdrop")
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .flex()
                            .justify_center()
                            .pt(px(56.))
                            .occlude()
                            .on_click(cx.listener(|this, _, window, cx| this.close_palette(window, cx)))
                            .child(
                                div()
                                    .id("palette-frame")
                                    .h_auto()
                                    .on_click(|_, _, cx| cx.stop_propagation())
                                    .child(palette),
                            ),
                    )
                    .with_priority(1),
                )
            })
            .children(dialogs)
            .children(notifications)
    }
}

fn push_bounded(stack: &mut Vec<Target>, target: Target) {
    if stack.last() != Some(&target) {
        stack.push(target);
    }
    if stack.len() > NAV_HISTORY {
        stack.remove(0);
    }
}

/// Palette rows for `targets`, with each line's text as its preview. Each
/// file is read once.
fn location_items(root: &Path, targets: Vec<Target>) -> Vec<LocationItem> {
    let mut files: HashMap<PathBuf, Vec<String>> = HashMap::new();
    targets
        .into_iter()
        .map(|target| {
            let lines = files.entry(target.path.clone()).or_insert_with(|| {
                std::fs::read_to_string(&target.path)
                    .map(|text| text.lines().map(str::to_string).collect())
                    .unwrap_or_default()
            });
            let preview = lines
                .get(target.line as usize)
                .map(|line| cockpit_common::truncate_chars(line.trim(), 200).to_string())
                .unwrap_or_default();
            LocationItem::new(target, root, preview)
        })
        .collect()
}

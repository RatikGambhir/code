//! A window onto one folder: sidebar, editor panes, terminal, agent panel,
//! title and status bars. It owns the wiring between them.

use std::{
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
use gpui::{
    deferred, div, prelude::*, px, AnyElement, App, AsyncWindowContext, Context, Entity,
    FocusHandle, Focusable, PathPromptOptions, PromptLevel, SharedString, Subscription, Task,
    WeakEntity, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::Notification,
    resizable::{h_resizable, resizable_panel, v_resizable},
    v_flex, ActiveTheme as _, IconName, Root, Selectable as _, Sizable as _, TitleBar, WindowExt as _,
};
use serde::{Deserialize, Serialize};

use crate::{
    actions,
    agent::{AgentController, AgentControllerEvent, AgentPanel, AgentPanelEvent, ConnectionState},
    assets::icon,
    browser_view::{BrowserEvent, BrowserView},
    editor_view::{EditorEvent, EditorView},
    explorer::{Explorer, ExplorerEvent},
    lsp::{LspEvent, LspManager},
    palette::{Palette, PaletteEvent},
    pane::{Pane, PaneEvent, PaneItem},
    search_panel::{SearchPanel, SearchPanelEvent},
    source_control::{SourceControl, SourceControlEvent},
    state::{AppState, ThemePreference},
    terminal_view::{TerminalPanel, TerminalPanelEvent},
    theme,
};

/// How long a page may take to load before an agent action returns anyway.
const AGENT_LOAD_TIMEOUT: Duration = Duration::from_secs(20);
const PAGE_TEXT_LIMIT: usize = 20_000;
const CONSOLE_LIMIT: usize = 50;

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

    palette: Option<Entity<Palette>>,
    /// Where focus was before the palette opened.
    palette_return: Option<FocusHandle>,

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
            palette: None,
            palette_return: None,
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
        self._subscriptions.push(cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, _, cx| {
            if let EditorEvent::Saved(_) = event {
                this.source_control.update(cx, |scm, cx| scm.refresh(cx));
            }
        }));
        let pane = self.active_pane().clone();
        pane.update(cx, |pane, cx| pane.add_item(PaneItem::Editor(editor.clone()), focus, window, cx));
        self.explorer.update(cx, |e, cx| e.reveal(&path, cx));
        Some(editor)
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
                self.run_browser_action(call.clone(), action.clone(), window, cx);
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
        let _ = window.prompt(PromptLevel::Info, cockpit_common::APP_NAME, Some(&detail), &["OK"], cx);
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
        self.palette_return = window.focused(cx);
        self._subscriptions.push(cx.subscribe_in(&palette, window, |this, _, event: &PaletteEvent, window, cx| {
            match event {
                PaletteEvent::Dismissed => this.close_palette(window, cx),
                PaletteEvent::OpenFile(path) => {
                    this.close_palette(window, cx);
                    this.open_file(path.clone(), true, window, cx);
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

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_title = self
            .active_pane()
            .read(cx)
            .active_item()
            .map(|item| item.title(cx));
        let title: SharedString = match active_title {
            Some(item) => format!("{} — {item}", self.name).into(),
            None => self.name.clone(),
        };
        let toggle = |id: &'static str, name: IconName, tooltip: &'static str, on: bool| {
            Button::new(id)
                .icon(name)
                .ghost()
                .xsmall()
                .tooltip(tooltip)
                .when(on, |b| b.selected(true))
        };
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .gap_1()
                .child(div().flex_1())
                .child(div().text_sm().text_color(cx.theme().muted_foreground).child(title))
                .child(div().flex_1())
                .child(
                    toggle("toggle-sidebar", IconName::PanelLeft, "Toggle Sidebar (⌘B)", self.layout.sidebar_visible)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.layout.sidebar_visible = !this.layout.sidebar_visible;
                            if !this.layout.sidebar_visible {
                                this.focus_center(window, cx);
                            }
                            this.save_layout(cx);
                            cx.notify();
                        })),
                )
                .child(
                    toggle("toggle-terminal", IconName::PanelBottom, "Toggle Terminal (⌘J)", self.layout.terminal_visible)
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx))),
                )
                .child(
                    toggle("toggle-agent", IconName::PanelRight, "Toggle Codex (⌥⌘B)", self.layout.agent_visible)
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_agent(window, cx))),
                ),
        )
    }

    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.layout.sidebar_visible.then_some(self.layout.sidebar);
        let changes = self.source_control.read(cx).change_count();
        let item = |id: &'static str, view: SidebarView, element: gpui_component::Icon, tooltip: &'static str| {
            Button::new(id)
                .icon(element)
                .ghost()
                .selected(current == Some(view))
                .tooltip(tooltip)
                .on_click(cx.listener(move |this, _, window, cx| this.show_sidebar(view, window, cx)))
        };
        v_flex()
            .w(px(44.))
            .h_full()
            .flex_none()
            .items_center()
            .py_2()
            .gap_1()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(item("activity-explorer", SidebarView::Explorer, icon("files"), "Explorer (⇧⌘E)"))
            .child(
                div()
                    .relative()
                    .child(item(
                        "activity-scm",
                        SidebarView::SourceControl,
                        icon("git-branch"),
                        "Source Control (⇧⌘G)",
                    ))
                    .when(changes > 0, |el| {
                        el.child(
                            div()
                                .absolute()
                                .top(px(-2.))
                                .right(px(-4.))
                                .px_1()
                                .rounded_full()
                                .bg(cx.theme().primary)
                                .text_color(cx.theme().primary_foreground)
                                .text_size(px(9.))
                                .child(if changes > 99 { "99+".to_string() } else { changes.to_string() }),
                        )
                    }),
            )
            .child(item(
                "activity-search",
                SidebarView::Search,
                gpui_component::Icon::new(IconName::Search),
                "Search (⇧⌘F)",
            ))
            .child(div().flex_1())
            .child(
                Button::new("activity-browser")
                    .icon(IconName::Globe)
                    .ghost()
                    .tooltip("New Browser Tab (⌘T)")
                    .on_click(cx.listener(|this, _, window, cx| this.new_browser_tab(window, cx))),
            )
            .child(
                Button::new("activity-codex")
                    .icon(icon("sparkles"))
                    .ghost()
                    .selected(self.layout.agent_visible)
                    .tooltip("Codex (⌥⌘B)")
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

    fn render_center(&self, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().border;
        let pane = |ix: usize| {
            div()
                .size_full()
                .when(self.layout.split && ix == self.active_pane, |el| el.border_t_1().border_color(border))
                .child(self.panes[ix].clone())
        };
        h_resizable("workspace-panes")
            .child(resizable_panel().child(pane(0)))
            .child(resizable_panel().visible(self.layout.split).child(pane(1)))
            .into_any_element()
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
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
        let agent_status: Option<(SharedString, gpui::Hsla)> = match agent.state() {
            ConnectionState::Connected { .. } if agent.is_running() => Some(("Codex working…".into(), theme.warning)),
            ConnectionState::Connected { .. } => Some(("Codex ready".into(), muted)),
            ConnectionState::Connecting => Some(("Codex starting…".into(), muted)),
            ConnectionState::NotInstalled => Some(("Codex not installed".into(), theme.danger)),
            ConnectionState::Failed(_) => Some(("Codex stopped".into(), theme.danger)),
            ConnectionState::Idle => None,
        };

        h_flex()
            .h(px(24.))
            .flex_none()
            .px_2()
            .gap_3()
            .text_xs()
            .text_color(muted)
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .when_some(branch, |el, branch| {
                el.child(
                    h_flex()
                        .id("status-branch")
                        .gap_1()
                        .cursor_pointer()
                        .child(icon("git-branch").xsmall())
                        .child(branch)
                        .when(changes > 0, |el| el.child(format!("· {changes}")))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.layout.sidebar_visible = false;
                            this.show_sidebar(SidebarView::SourceControl, window, cx);
                        })),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_color(if errors > 0 { theme.danger } else { muted }).child(format!("✕ {errors}")))
                    .child(div().text_color(if warnings > 0 { theme.warning } else { muted }).child(format!("⚠ {warnings}"))),
            )
            .when_some(lsp_status, |el, status| el.child(div().truncate().max_w(px(320.)).child(status)))
            .child(div().flex_1())
            .when_some(editor_info, |el, (position, language, ending)| {
                el.child(position).child(language).child(ending)
            })
            .when_some(agent_status, |el, (text, color)| {
                el.child(
                    div()
                        .id("status-codex")
                        .cursor_pointer()
                        .text_color(color)
                        .child(text)
                        .on_click(cx.listener(|this, _, window, cx| this.focus_agent(window, cx))),
                )
            })
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialogs = Root::render_dialog_layer(window, cx);
        let notifications = Root::render_notification_layer(window, cx);
        let terminal = self.terminal.clone();
        let sidebar = self.render_sidebar();
        let center = self.render_center(cx);

        v_flex()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
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
            .on_action(cx.listener(|this, _: &actions::ToggleSidebar, window, cx| {
                this.layout.sidebar_visible = !this.layout.sidebar_visible;
                if !this.layout.sidebar_visible {
                    this.focus_center(window, cx);
                }
                this.save_layout(cx);
                cx.notify();
            }))
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
            .on_action(cx.listener(|this, _: &actions::About, window, cx| this.about(window, cx)))
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_activity_bar(cx))
                    .child(
                        div().flex_1().min_w_0().h_full().child(
                            h_resizable("workspace-columns")
                                .child(
                                    resizable_panel()
                                        .size(px(260.))
                                        .size_range(px(160.)..px(640.))
                                        .visible(self.layout.sidebar_visible)
                                        .child(div().size_full().bg(cx.theme().sidebar).child(sidebar)),
                                )
                                .child(
                                    resizable_panel().child(
                                        v_resizable("workspace-rows")
                                            .child(resizable_panel().child(center))
                                            .child(
                                                resizable_panel()
                                                    .size(px(260.))
                                                    .size_range(px(80.)..px(1200.))
                                                    .visible(self.layout.terminal_visible && terminal.is_some())
                                                    .child(
                                                        div()
                                                            .size_full()
                                                            .border_t_1()
                                                            .border_color(cx.theme().border)
                                                            .children(terminal),
                                                    ),
                                            ),
                                    ),
                                )
                                .child(
                                    resizable_panel()
                                        .size(px(420.))
                                        .size_range(px(300.)..px(1000.))
                                        .visible(self.layout.agent_visible)
                                        .child(
                                            div()
                                                .size_full()
                                                .border_l_1()
                                                .border_color(cx.theme().border)
                                                .child(self.agent_panel.clone()),
                                        ),
                                ),
                        ),
                    ),
            )
            .child(self.render_status_bar(cx))
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

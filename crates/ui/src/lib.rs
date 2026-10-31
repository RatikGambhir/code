//! Cockpit's user interface, built with GPUI and gpui-component.

pub mod actions;
pub mod agent;
mod appearance;
mod assets;
pub mod browser_view;
mod chrome;
pub mod editor_view;
pub mod explorer;
pub mod lsp;
pub mod palette;
pub mod pane;
pub mod search_panel;
pub mod source_control;
pub mod state;
pub mod terminal_view;
mod theme;
pub mod welcome;
pub mod workspace;

use std::path::PathBuf;

use gpui::{
    px, size, App, AppContext as _, Focusable as _, Menu, MenuItem, OsAction, PathPromptOptions,
    SystemMenuType, WindowBounds, WindowOptions,
};
use gpui_component::{Root, TitleBar};

pub use assets::Assets;
pub use state::{AppState, Settings};

use crate::{welcome::Welcome, workspace::Workspace};

/// Sets up the UI once `AppState` is installed as a global.
pub fn init(cx: &mut App) {
    gpui_component::init(cx);
    theme::apply(None, cx);
    actions::bind_keys(cx);
    cx.on_action(|_: &actions::Quit, cx| cx.quit());
    // When no window handles it (the welcome window, or none open).
    cx.on_action(|_: &actions::OpenFolder, cx| prompt_open_folder(cx));
    cx.set_menus(menus());
}

/// Clicking the Dock icon with no windows open.
pub fn reopen(cx: &mut App) {
    if cx.windows().is_empty() {
        open_welcome(cx);
    }
}

fn menus() -> Vec<Menu> {
    use gpui_component::input;
    vec![
        Menu {
            name: cockpit_common::APP_NAME.into(),
            items: vec![
                MenuItem::action("About Cockpit", actions::About),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Quit Cockpit", actions::Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Open Folder…", actions::OpenFolder),
                MenuItem::action("Go to File…", actions::QuickOpen),
                MenuItem::separator(),
                MenuItem::action("Save", actions::Save),
                MenuItem::action("Close Tab", actions::CloseTab),
                MenuItem::separator(),
                MenuItem::action("New Browser Tab", actions::NewBrowserTab),
                MenuItem::action("New Terminal", actions::NewTerminal),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", input::Undo, OsAction::Undo),
                MenuItem::os_action("Redo", input::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
                MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
                MenuItem::separator(),
                MenuItem::action("Find in Files", actions::ShowSearch),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Command Palette…", actions::CommandPalette),
                MenuItem::separator(),
                MenuItem::action("Explorer", actions::ShowExplorer),
                MenuItem::action("Source Control", actions::ShowSourceControl),
                MenuItem::action("Search", actions::ShowSearch),
                MenuItem::separator(),
                MenuItem::action("Toggle Sidebar", actions::ToggleSidebar),
                MenuItem::action("Toggle Terminal", actions::ToggleTerminal),
                MenuItem::action("Toggle Codex", actions::ToggleAgentPanel),
                MenuItem::action("Split Editor", actions::SplitPane),
                MenuItem::separator(),
                MenuItem::action("Customize Theme…", actions::CustomizeTheme),
                MenuItem::action("Toggle Light/Dark", actions::ToggleTheme),
            ],
        },
        Menu {
            name: "Codex".into(),
            items: vec![
                MenuItem::action("Ask Codex", actions::FocusAgent),
                MenuItem::action("New Thread", actions::NewAgentThread),
                MenuItem::action("Stop", actions::InterruptAgent),
            ],
        },
    ]
}

/// Opens the folders named on the command line, else the last workspace,
/// else the welcome window.
pub fn launch(paths: Vec<PathBuf>, cx: &mut App) {
    let folders: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_dir()).collect();
    if !folders.is_empty() {
        for folder in folders {
            open_folder(folder, cx);
        }
        return;
    }
    let last = AppState::global(cx)
        .db
        .read_blocking(|r| r.recent_workspaces(1))
        .ok()
        .and_then(|recent| recent.into_iter().next())
        .map(|w| PathBuf::from(w.path))
        .filter(|p| p.is_dir());
    match last {
        Some(path) => open_folder(path, cx),
        None => open_welcome(cx),
    }
}

pub fn prompt_open_folder(cx: &mut App) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Open".into()),
    });
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(paths))) = paths.await {
            if let Some(path) = paths.into_iter().next() {
                cx.update(|cx| open_folder(path, cx)).ok();
            }
        }
    })
    .detach();
}

fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitleBar::title_bar_options()),
        window_bounds: Some(WindowBounds::centered(size(px(1480.), px(920.)), cx)),
        window_min_size: Some(size(px(760.), px(480.))),
        app_id: Some(cockpit_common::APP_IDENTIFIER.into()),
        ..Default::default()
    }
}

/// Opens `path` in a window, or brings forward the window already showing it.
pub fn open_folder(path: PathBuf, cx: &mut App) {
    let path = path.canonicalize().unwrap_or(path);
    for handle in cx.windows() {
        let Some(root) = handle.downcast::<Root>() else { continue };
        let shows_path = root
            .read_with(cx, |root, cx| {
                root.view()
                    .clone()
                    .downcast::<Workspace>()
                    .is_ok_and(|w| w.read(cx).root() == path)
            })
            .unwrap_or(false);
        if shows_path {
            root.update(cx, |_, window, _| window.activate_window()).ok();
            return;
        }
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let path_text = path.to_string_lossy().into_owned();
    let record = match AppState::global(cx)
        .db
        .write_blocking(move |w| w.touch_workspace(&path_text, &name))
    {
        Ok(record) => record,
        Err(err) => {
            tracing::error!("opening {}: {err:#}", path.display());
            return;
        }
    };
    let opened = cx.open_window(window_options(cx), |window, cx| {
        let workspace = cx.new(|cx| Workspace::new(record, window, cx));
        window.focus(&workspace.focus_handle(cx));
        cx.new(|cx| Root::new(workspace, window, cx))
    });
    if let Err(err) = opened {
        tracing::error!("opening a window: {err:#}");
    }
    cx.activate(true);
}

pub fn open_welcome(cx: &mut App) {
    let mut options = window_options(cx);
    options.window_bounds = Some(WindowBounds::centered(size(px(760.), px(520.)), cx));
    let opened = cx.open_window(options, |window, cx| {
        let welcome = cx.new(|cx| Welcome::new(window, cx));
        window.focus(&welcome.focus_handle(cx));
        cx.new(|cx| Root::new(welcome, window, cx))
    });
    if let Err(err) = opened {
        tracing::error!("opening the welcome window: {err:#}");
    }
    cx.activate(true);
}

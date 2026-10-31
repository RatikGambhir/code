//! Hangar: a native IDE (GPUI) with an embedded Chromium browser and the
//! Codex agent.
//!
//! Start-up order matters (see `cockpit_browser::cef`): the login shell's
//! `PATH` is adopted before any thread exists, Chromium is loaded before GPUI
//! creates `NSApp`, and Chromium is initialized inside GPUI's run callback.

use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

use anyhow::{Context as _, Result};
use cockpit_agents::AgentRuntime;
use cockpit_browser::{BrowserRuntime, RuntimeOptions};
use cockpit_common::{shell_env, AppPaths};
use cockpit_persistence::{Database, DbHandle};
use cockpit_ui::{state::SETTINGS_KEY, AppState, Assets, Settings};
use gpui::Application;
use tracing_subscriber::{fmt::writer::MakeWriterExt as _, EnvFilter};

const BROWSER_PROFILE: &str = "default";

fn main() {
    if let Err(err) = run() {
        eprintln!("hangar: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // Apps launched from Finder get launchd's minimal PATH, but codex, git
    // and language servers live on the login shell's. `set_var` is only
    // sound before other threads exist, so this goes first.
    shell_env::adopt_login_shell_path();
    let paths = AppPaths::resolve().context("creating the data directory")?;
    init_logging(&paths);
    tracing::info!("{} {} starting", cockpit_common::APP_NAME, cockpit_common::APP_VERSION);

    let profile_dir = paths.browser_profile(BROWSER_PROFILE);
    let browser = BrowserRuntime::load(RuntimeOptions {
        profile_dir: Some(profile_dir.clone()),
        log_file: Some(paths.logs_dir().join("chromium.log")),
        remote_debugging_port: std::env::var("HANGAR_REMOTE_DEBUGGING_PORT")
            .ok()
            .and_then(|port| port.parse().ok()),
    });

    let db = DbHandle::spawn(Database::open(&paths.database()).context("opening the database")?)?;
    let settings: Settings = db
        .read_blocking(|r| r.setting_json(SETTINGS_KEY))
        .unwrap_or_else(|err| {
            tracing::warn!("reading settings: {err:#}");
            None
        })
        .unwrap_or_default();
    let profile_id = {
        let dir = profile_dir.to_string_lossy().into_owned();
        db.write_blocking(move |w| w.ensure_browser_profile(BROWSER_PROFILE, &dir))
            .map(|profile| profile.id)
            .map_err(|err| tracing::warn!("registering the browser profile: {err:#}"))
            .ok()
    };
    let agents = Arc::new(AgentRuntime::new().context("starting the agent runtime")?);

    // Folders to open; macOS may add `-psn_…` and similar flags.
    let folders: Vec<PathBuf> = std::env::args_os()
        .skip(1)
        .filter(|arg| !arg.to_string_lossy().starts_with('-'))
        .map(PathBuf::from)
        .collect();

    // Folders dropped on the Dock icon or opened with `open -a Hangar <dir>`
    // arrive as file URLs, possibly before the UI is up; queue them.
    let (open_tx, open_rx) = async_channel::unbounded::<PathBuf>();

    let browser = Rc::new(RefCell::new(Some(browser)));
    let app = Application::new().with_assets(Assets);
    app.on_reopen(cockpit_ui::reopen);
    app.on_open_urls(move |urls| {
        for path in urls.iter().filter_map(|url| folder_for_url(url)) {
            let _ = open_tx.try_send(path);
        }
    });
    app.run(move |cx| {
        let engine = browser
            .borrow()
            .as_ref()
            .map(BrowserRuntime::start)
            .expect("the browser runtime is set until quit");
        cx.set_global(AppState {
            paths,
            db,
            browser: engine,
            agents,
            settings,
            browser_profile: profile_dir,
            browser_profile_id: profile_id,
        });
        cockpit_ui::init(cx);

        // Windows are gone by the time this future runs, so pages are closing;
        // let them finish (cookies flush) and shut Chromium down.
        let runtime = browser.clone();
        cx.on_app_quit(move |_| {
            let runtime = runtime.borrow_mut().take();
            async move {
                if let Some(runtime) = runtime {
                    runtime.shutdown();
                }
            }
        })
        .detach();

        cockpit_ui::launch(folders, cx);
        cx.spawn(async move |cx| {
            while let Ok(path) = open_rx.recv().await {
                if cx.update(|cx| cockpit_ui::open_folder(path, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    });
    Ok(())
}

/// The folder to open for a `file://` URL: the folder itself, or a file's
/// parent.
fn folder_for_url(url: &str) -> Option<PathBuf> {
    let path = url::Url::parse(url).ok()?.to_file_path().ok()?;
    if path.is_dir() {
        Some(path)
    } else {
        path.parent().map(PathBuf::from)
    }
}

/// Logs to stderr and `<data dir>/logs/hangar.log`. `HANGAR_LOG` takes an
/// `EnvFilter` directive (`debug`, `cockpit_agents=trace`, …).
fn init_logging(paths: &AppPaths) {
    let filter = EnvFilter::try_from_env("HANGAR_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,wgpu=warn,naga=warn,blade_graphics=warn"));
    let file = std::fs::create_dir_all(paths.logs_dir())
        .and_then(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(paths.logs_dir().join("hangar.log"))
        })
        .ok();
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false);
    let result = match file {
        Some(file) => builder
            .with_writer(std::io::stderr.and(std::sync::Mutex::new(file)))
            .try_init(),
        None => builder.with_writer(std::io::stderr).try_init(),
    };
    if let Err(err) = result {
        eprintln!("logging is unavailable: {err}");
    }
}

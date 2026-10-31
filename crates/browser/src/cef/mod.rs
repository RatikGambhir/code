//! Chromium (CEF) in off-screen mode, for macOS.
//!
//! # Bring-up order
//!
//! GPUI and Chromium both assume they own the application object, so the
//! order matters:
//!
//! 1. [`CefRuntime::load`] at the top of `main` loads the framework and runs
//!    `cef_execute_process` (child processes normally use the separate helper
//!    binary and never get here).
//! 2. GPUI starts and creates `NSApp` as its own `GPUIApplication` class.
//!    Initializing Chromium first would create a plain `NSApplication` and
//!    GPUI would crash looking for its ivars.
//! 3. [`CefRuntime::start`], inside GPUI's run callback, retrofits Chromium's
//!    protocols onto NSApp ([`nsapp`]), calls `cef_initialize` with an
//!    external message pump, and starts the run-loop timer ([`pump`]).
//! 4. [`CefRuntime::shutdown`] after GPUI's run loop returns.
//!
//! # Packaging
//!
//! Chromium finds `Chromium Embedded Framework.framework` and the
//! `<App> Helper*.app` bundles through the `.app` layout, so the app must run
//! from its bundle (`cargo xtask bundle`). A bare `cargo run` gets an
//! [`crate::UnavailableEngine`] explaining that.

mod handlers;
mod nsapp;
mod pump;
mod shared;
mod tab;

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context as _, Result};
use cef::{args::Args, library_loader::LibraryLoader};

use crate::{
    engine::{BrowserEngine, BrowserTab, TabOptions},
    RuntimeOptions,
};

/// Where the framework must be, relative to `Contents/MacOS/<exe>`.
const FRAMEWORK_BINARY: &str =
    "../Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework";

/// Browsers created and not yet closed (they close asynchronously).
pub(crate) static LIVE_BROWSERS: AtomicUsize = AtomicUsize::new(0);
/// Set once `cef_shutdown` ran; the pump must not call into Chromium after.
pub(crate) static SHUT_DOWN: AtomicBool = AtomicBool::new(false);

/// How long quitting waits for pages to close (and flush cookies).
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct CefRuntime {
    inner: Rc<Inner>,
}

struct Inner {
    // Unloads the framework when dropped, so it must outlive everything else.
    _loader: LibraryLoader,
    app: RefCell<cef::App>,
    options: RuntimeOptions,
    started: Cell<bool>,
    /// Per-process secret that marks script results in console messages.
    secret: String,
}

impl CefRuntime {
    pub fn load(options: RuntimeOptions) -> Result<Self> {
        let exe = std::env::current_exe().context("locating the executable")?;
        let framework = exe
            .parent()
            .map(|dir| dir.join(FRAMEWORK_BINARY))
            .filter(|path| path.exists());
        if framework.is_none() {
            bail!(
                "Chromium Embedded Framework not found next to {}. Build and launch the app \
                 bundle with `cargo xtask bundle --open` instead of `cargo run`.",
                exe.display()
            );
        }

        static LOADED: AtomicBool = AtomicBool::new(false);
        if LOADED.swap(true, Ordering::SeqCst) {
            bail!("the browser runtime was loaded twice");
        }

        let loader = LibraryLoader::new(&exe, false);
        if !loader.load() {
            bail!("loading the Chromium Embedded Framework failed");
        }
        // Pins the API version; must precede any other CEF call.
        let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);

        let args = Args::new();
        let mut app = handlers::HangarApp::new(handlers::AppConfig {
            remote_debugging_port: options.remote_debugging_port,
        });
        let code = cef::execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());
        if code >= 0 {
            // This process was launched as a Chromium child after all.
            std::process::exit(code);
        }

        Ok(Self {
            inner: Rc::new(Inner {
                _loader: loader,
                app: RefCell::new(app),
                options,
                started: Cell::new(false),
                secret: random_secret(),
            }),
        })
    }

    /// Initializes Chromium. Call once GPUI has created NSApp.
    pub fn start(&self) -> Result<Rc<dyn BrowserEngine>> {
        if !self.inner.started.get() {
            nsapp::adopt_cef_app_protocol().map_err(|err| anyhow!(err))?;

            let mut settings = cef::Settings {
                windowless_rendering_enabled: 1,
                // GPUI drives [NSApp run]; Chromium is pumped from a timer.
                multi_threaded_message_loop: 0,
                external_message_pump: 1,
                persist_session_cookies: 1,
                log_severity: cef::LogSeverity::WARNING,
                ..Default::default()
            };
            if let Some(profile) = &self.inner.options.profile_dir {
                std::fs::create_dir_all(profile).ok();
                let path = profile.to_string_lossy();
                settings.root_cache_path = path.as_ref().into();
                settings.cache_path = path.as_ref().into();
            }
            if let Some(log) = &self.inner.options.log_file {
                settings.log_file = log.to_string_lossy().as_ref().into();
            }

            let args = Args::new();
            let mut app = self.inner.app.borrow_mut();
            if cef::initialize(Some(args.as_main_args()), Some(&settings), Some(&mut *app), std::ptr::null_mut()) != 1 {
                bail!("cef_initialize failed (see the Chromium log for details)");
            }
            self.inner.started.set(true);
            pump::start();
        }
        Ok(Rc::new(CefEngine {
            runtime: self.inner.clone(),
        }))
    }

    /// Shuts Chromium down at quit. Engines may still be referenced (the UI
    /// is being torn down around us), so this does not wait for them to drop;
    /// it waits for the open pages to finish closing, then shuts down.
    pub fn shutdown(self) {
        if !self.inner.started.get() {
            return;
        }
        let deadline = Instant::now() + CLOSE_TIMEOUT;
        while LIVE_BROWSERS.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            cef::do_message_loop_work();
            std::thread::sleep(Duration::from_millis(5));
        }
        let open = LIVE_BROWSERS.load(Ordering::SeqCst);
        if open > 0 {
            tracing::warn!("shutting Chromium down with {open} page(s) still open");
        }
        SHUT_DOWN.store(true, Ordering::SeqCst);
        self.inner.started.set(false);
        cef::shutdown();
    }
}

/// The body of the `<App> Helper*.app` executables: Chromium launches them
/// for its renderer, GPU and utility processes. Returns the exit code.
pub fn run_helper() -> i32 {
    let args = Args::new();
    // Helpers enter the sandbox before loading anything else. With
    // `--no-sandbox` (HANGAR_CEF_NO_SANDBOX) there is nothing to enter.
    let _sandbox = (!std::env::args().any(|arg| arg == "--no-sandbox")).then(|| {
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        sandbox
    });
    let Ok(exe) = std::env::current_exe() else { return 1 };
    let loader = LibraryLoader::new(&exe, true);
    if !loader.load() {
        eprintln!("{}: loading the Chromium Embedded Framework failed", exe.display());
        return 1;
    }
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let mut app = handlers::HangarApp::new(handlers::AppConfig {
        remote_debugging_port: None,
    });
    cef::execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut())
}

impl Drop for Inner {
    fn drop(&mut self) {
        if self.started.get() {
            cef::shutdown();
        }
    }
}

struct CefEngine {
    runtime: Rc<Inner>,
}

impl BrowserEngine for CefEngine {
    fn name(&self) -> &str {
        "Chromium"
    }

    fn availability(&self) -> Result<(), String> {
        Ok(())
    }

    fn create_tab(&self, options: TabOptions) -> Result<Rc<dyn BrowserTab>> {
        let tab = tab::CefTab::create(&options, &self.runtime.secret)?;
        Ok(Rc::new(tab))
    }
}

fn random_secret() -> String {
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded from the OS; two of them give 128 random bits.
    let mut parts = [0u64; 2];
    for part in &mut parts {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        );
        *part = hasher.finish();
    }
    format!("{:016x}{:016x}", parts[0], parts[1])
}

//! The embedded browser.
//!
//! On macOS pages are rendered by Chromium through the Chromium Embedded
//! Framework in off-screen mode: CEF paints into a pixel buffer that the UI
//! draws as an ordinary GPUI element, so the page clips, layers and animates
//! with the rest of the window (a native child view could do none of that).
//!
//! The rest of the IDE only sees [`BrowserEngine`] / [`BrowserTab`], so the
//! engine can be swapped (WKWebView, a remote browser) without touching the
//! views. Everything outside [`cef`] is engine-independent and unit tested on
//! any host.

pub mod address;
pub mod automation;
pub mod engine;
pub mod frame;
pub mod input;

#[cfg(target_os = "macos")]
pub mod cef;

pub use address::{is_local_url, origin_of, parse_address, resolve_address, AddressInput};
pub use automation::{BrowserAction, ConsoleEntry, ConsoleLevel};
pub use engine::{
    BrowserEngine, BrowserTab, CursorKind, EditCommand, PageRect, TabOptions, TabState, UnavailableEngine,
    ViewGeometry,
};
pub use frame::Frame;
pub use input::{KeyInput, Modifiers, MouseButton, MouseInput};

use std::{path::PathBuf, rc::Rc};

/// Entry point of Chromium's helper processes (the `Hangar Helper*.app`
/// executables). Only meaningful inside the macOS app bundle.
pub fn run_helper() -> i32 {
    #[cfg(target_os = "macos")]
    {
        cef::run_helper()
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("the Chromium helper only runs on macOS");
        1
    }
}

/// Settings for the browser runtime.
#[derive(Clone, Debug, Default)]
pub struct RuntimeOptions {
    /// Profile directory: cookies, local storage and cache live here.
    pub profile_dir: Option<PathBuf>,
    /// Chromium's own log file.
    pub log_file: Option<PathBuf>,
    /// Exposes the DevTools protocol on this localhost port (for debugging
    /// the IDE itself; off by default).
    pub remote_debugging_port: Option<u16>,
}

/// The process-wide browser runtime: either Chromium or an explanation of
/// why it is not available.
pub enum BrowserRuntime {
    #[cfg(target_os = "macos")]
    Chromium(cef::CefRuntime),
    Unavailable(String),
}

impl BrowserRuntime {
    /// Step one, at the very top of `main`: load Chromium into the process.
    /// Must run before the UI framework creates `NSApp` (see [`cef`]).
    pub fn load(options: RuntimeOptions) -> Self {
        #[cfg(target_os = "macos")]
        {
            match cef::CefRuntime::load(options) {
                Ok(runtime) => BrowserRuntime::Chromium(runtime),
                Err(err) => {
                    tracing::warn!("Chromium is unavailable: {err:#}");
                    BrowserRuntime::Unavailable(format!("{err:#}"))
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = options;
            BrowserRuntime::Unavailable(
                "The embedded Chromium browser is only built for macOS.".into(),
            )
        }
    }

    /// Step two, once the UI framework owns `NSApp`: initialize Chromium and
    /// return the engine to create tabs with.
    pub fn start(&self) -> Rc<dyn BrowserEngine> {
        match self {
            #[cfg(target_os = "macos")]
            BrowserRuntime::Chromium(runtime) => match runtime.start() {
                Ok(engine) => engine,
                Err(err) => {
                    tracing::error!("Chromium failed to start: {err:#}");
                    Rc::new(UnavailableEngine::new(format!("Chromium failed to start: {err:#}")))
                }
            },
            BrowserRuntime::Unavailable(reason) => Rc::new(UnavailableEngine::new(reason.clone())),
        }
    }

    /// Step three, after the UI's run loop returned.
    pub fn shutdown(self) {
        #[cfg(target_os = "macos")]
        if let BrowserRuntime::Chromium(runtime) = self {
            runtime.shutdown();
        }
    }
}

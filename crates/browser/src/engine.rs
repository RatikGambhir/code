//! The browser engine abstraction.
//!
//! The UI and the agent bridge program against [`BrowserEngine`] and
//! [`BrowserTab`]. The Chromium backend (`cef`, macOS) is the real
//! implementation; [`UnavailableEngine`] stands in when Chromium cannot run
//! (an unbundled `cargo run`, a missing framework, another OS), so the rest of
//! the IDE keeps working and the browser pane explains why it is empty.

use std::{ops::Range, rc::Rc, sync::Arc};

use anyhow::Result;
use cockpit_common::BrowserTabId;

use crate::{
    automation::ConsoleEntry,
    frame::Frame,
    input::{KeyInput, MouseInput},
};

/// The pointer shape the page asks for.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CursorKind {
    #[default]
    Arrow,
    Pointer,
    Text,
    VerticalText,
    Crosshair,
    Grab,
    Grabbing,
    NotAllowed,
    ContextMenu,
    Copy,
    Alias,
    ResizeLeftRight,
    ResizeUpDown,
    ResizeNeSw,
    ResizeNwSe,
    ResizeColumn,
    ResizeRow,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabState {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    /// The last main-frame load failure, cleared by the next navigation.
    pub load_error: Option<String>,
    pub cursor: CursorKind,
}

/// Where and how large the page is drawn, in logical pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ViewGeometry {
    pub width: f32,
    pub height: f32,
    pub scale_factor: f32,
}

/// A rectangle in the page's coordinate space.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct PageRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Editing commands, sent to the focused frame. Off-screen Chromium does not
/// turn ⌘C/⌘V into these by itself, so the UI binds them explicitly.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EditCommand {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

#[derive(Clone, Debug)]
pub struct TabOptions {
    pub url: String,
    /// Frames per second while the page animates.
    pub frame_rate: u32,
}

impl TabOptions {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            frame_rate: 60,
        }
    }
}

pub type EvalCallback = Box<dyn FnOnce(Result<serde_json::Value, String>)>;
pub type TextCallback = Box<dyn FnOnce(String)>;

/// One browser tab. All methods must be called on the main thread.
pub trait BrowserTab {
    fn id(&self) -> BrowserTabId;
    fn state(&self) -> TabState;

    fn navigate(&self, url: &str);
    fn reload(&self);
    fn stop(&self);
    fn go_back(&self);
    fn go_forward(&self);
    fn set_zoom_level(&self, level: f64);
    fn edit(&self, command: EditCommand);
    fn show_devtools(&self);
    fn close(&self);

    /// Called when the view is laid out; triggers a resize when it changed.
    fn set_geometry(&self, geometry: ViewGeometry);
    fn set_focus(&self, focused: bool);
    /// Called whenever something visible changed (new frame, title, …).
    fn set_listener(&self, listener: Box<dyn Fn()>);

    /// The newest painted frame and its generation number, which increases
    /// with every paint.
    fn latest_frame(&self) -> Option<(u64, Arc<Frame>)>;
    /// The popup layer (`<select>` dropdowns), when one is showing.
    fn popup(&self) -> Option<(u64, Arc<Frame>, PageRect)>;
    /// URLs the page tried to open in a new window.
    fn take_popup_requests(&self) -> Vec<String>;

    fn send_mouse(&self, input: MouseInput);
    fn send_key_down(&self, input: &KeyInput);
    fn send_key_up(&self, input: &KeyInput);
    /// Text committed by the platform input method, plus the key press that
    /// produced it (held back until the input method decided).
    fn insert_text(&self, text: &str, key: Option<&KeyInput>);
    fn set_composition(&self, text: &str, selection: Option<Range<usize>>);
    fn commit_composition(&self, text: &str);
    fn cancel_composition(&self);
    fn finish_composition(&self);
    /// Where the composed text is, for placing the candidate window.
    fn composition_bounds(&self, range: Range<usize>) -> Option<PageRect>;

    /// Runs JavaScript in the main frame and reports its (JSON) result.
    fn eval(&self, script: &str, done: EvalCallback);
    /// The page's visible text.
    fn page_text(&self, done: TextCallback);
    /// Recent console messages, oldest first.
    fn console(&self) -> Vec<ConsoleEntry>;
}

pub trait BrowserEngine {
    fn name(&self) -> &str;
    /// `Err` explains why tabs cannot be created.
    fn availability(&self) -> Result<(), String>;
    fn create_tab(&self, options: TabOptions) -> Result<Rc<dyn BrowserTab>>;
}

/// The engine used when Chromium is not available.
pub struct UnavailableEngine {
    reason: String,
}

impl UnavailableEngine {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl BrowserEngine for UnavailableEngine {
    fn name(&self) -> &str {
        "unavailable"
    }

    fn availability(&self) -> Result<(), String> {
        Err(self.reason.clone())
    }

    fn create_tab(&self, _options: TabOptions) -> Result<Rc<dyn BrowserTab>> {
        anyhow::bail!("{}", self.reason)
    }
}

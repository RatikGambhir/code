//! State shared between Chromium's callbacks and the UI for one tab.
//!
//! Chromium runs single-threaded on the main thread here (external message
//! pump), so every callback happens on the same thread as the UI: `Cell` and
//! `RefCell` suffice.

use std::{
    cell::{Cell, RefCell},
    sync::Arc,
};

use cef::{Browser, BrowserHost, ImplBrowser};
use cockpit_common::BrowserTabId;

use crate::{
    automation::{ConsoleEntry, ConsoleLog, ScriptResults},
    engine::{CursorKind, EvalCallback, PageRect, TabState},
    frame::Frame,
};

pub(crate) struct FrameSlot {
    frame: RefCell<Option<Arc<Frame>>>,
    generation: Cell<u64>,
}

impl FrameSlot {
    fn new() -> Self {
        Self {
            frame: RefCell::new(None),
            generation: Cell::new(0),
        }
    }

    fn put(&self, frame: Frame) {
        *self.frame.borrow_mut() = Some(Arc::new(frame));
        self.generation.set(self.generation.get() + 1);
    }

    fn get(&self) -> Option<(u64, Arc<Frame>)> {
        self.frame
            .borrow()
            .as_ref()
            .map(|frame| (self.generation.get(), frame.clone()))
    }

    fn clear(&self) {
        *self.frame.borrow_mut() = None;
        self.generation.set(self.generation.get() + 1);
    }
}

pub(crate) struct TabShared {
    pub(crate) id: BrowserTabId,
    browser: RefCell<Option<Browser>>,
    /// Logical size of the view and the display's scale factor.
    view_size: Cell<(f32, f32)>,
    scale_factor: Cell<f32>,
    resized: Cell<bool>,
    screen_changed: Cell<bool>,
    page: FrameSlot,
    popup: FrameSlot,
    popup_visible: Cell<bool>,
    popup_rect: Cell<PageRect>,
    state: RefCell<TabState>,
    dirty: Cell<bool>,
    listener: RefCell<Option<Box<dyn Fn()>>>,
    console: RefCell<ConsoleLog>,
    pub(crate) results: RefCell<ScriptResults<EvalCallback>>,
    popup_requests: RefCell<Vec<String>>,
    composition_bounds: RefCell<Vec<PageRect>>,
}

impl TabShared {
    pub(crate) fn new(url: &str, secret: &str) -> Self {
        Self {
            id: BrowserTabId::next(),
            browser: RefCell::new(None),
            // Chromium refuses a zero-sized view; start with something sane.
            view_size: Cell::new((800.0, 600.0)),
            scale_factor: Cell::new(2.0),
            resized: Cell::new(false),
            screen_changed: Cell::new(false),
            page: FrameSlot::new(),
            popup: FrameSlot::new(),
            popup_visible: Cell::new(false),
            popup_rect: Cell::new(PageRect::default()),
            state: RefCell::new(TabState {
                url: url.to_string(),
                ..Default::default()
            }),
            dirty: Cell::new(false),
            listener: RefCell::new(None),
            console: RefCell::new(ConsoleLog::default()),
            results: RefCell::new(ScriptResults::new(secret)),
            popup_requests: RefCell::new(Vec::new()),
            composition_bounds: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn set_browser(&self, browser: Browser) {
        *self.browser.borrow_mut() = Some(browser);
    }

    pub(crate) fn browser(&self) -> Option<Browser> {
        self.browser.borrow().clone()
    }

    pub(crate) fn host(&self) -> Option<BrowserHost> {
        self.browser.borrow().as_ref().and_then(|b| b.host())
    }

    pub(crate) fn take_browser(&self) -> Option<Browser> {
        self.browser.borrow_mut().take()
    }

    fn mark_dirty(&self) {
        self.dirty.set(true);
    }

    // --- geometry ----------------------------------------------------------

    pub(crate) fn set_geometry(&self, width: f32, height: f32, scale_factor: f32) {
        let (width, height) = (width.max(1.0), height.max(1.0));
        let (old_w, old_h) = self.view_size.get();
        // Ignore sub-pixel jitter so layout rounding does not resize every frame.
        if (old_w - width).abs() >= 1.0 || (old_h - height).abs() >= 1.0 {
            self.view_size.set((width, height));
            self.resized.set(true);
        }
        if (self.scale_factor.get() - scale_factor).abs() > f32::EPSILON {
            self.scale_factor.set(scale_factor);
            self.screen_changed.set(true);
        }
    }

    pub(crate) fn view_size(&self) -> (f32, f32) {
        self.view_size.get()
    }

    pub(crate) fn scale_factor(&self) -> f32 {
        self.scale_factor.get()
    }

    // --- frames ------------------------------------------------------------

    pub(crate) fn put_page_frame(&self, frame: Frame) {
        self.page.put(frame);
        self.mark_dirty();
    }

    pub(crate) fn put_popup_frame(&self, frame: Frame) {
        self.popup.put(frame);
        self.mark_dirty();
    }

    pub(crate) fn page_frame(&self) -> Option<(u64, Arc<Frame>)> {
        self.page.get()
    }

    pub(crate) fn popup(&self) -> Option<(u64, Arc<Frame>, PageRect)> {
        if !self.popup_visible.get() {
            return None;
        }
        self.popup
            .get()
            .map(|(generation, frame)| (generation, frame, self.popup_rect.get()))
    }

    pub(crate) fn set_popup_visible(&self, visible: bool) {
        self.popup_visible.set(visible);
        if !visible {
            // Keeping the last frame would leave a ghost dropdown behind.
            self.popup.clear();
        }
        self.mark_dirty();
    }

    pub(crate) fn set_popup_rect(&self, rect: PageRect) {
        self.popup_rect.set(rect);
        self.mark_dirty();
    }

    // --- page state ----------------------------------------------------------

    pub(crate) fn state(&self) -> TabState {
        self.state.borrow().clone()
    }

    pub(crate) fn update_state(&self, update: impl FnOnce(&mut TabState)) {
        let mut state = self.state.borrow_mut();
        let before = state.clone();
        update(&mut state);
        if *state != before {
            drop(state);
            self.mark_dirty();
        }
    }

    pub(crate) fn set_cursor(&self, cursor: CursorKind) {
        self.update_state(|state| state.cursor = cursor);
    }

    pub(crate) fn push_console(&self, entry: ConsoleEntry) {
        self.console.borrow_mut().push(entry);
        self.mark_dirty();
    }

    pub(crate) fn console(&self) -> Vec<ConsoleEntry> {
        self.console.borrow().entries()
    }

    pub(crate) fn clear_console(&self) {
        self.console.borrow_mut().clear();
    }

    pub(crate) fn push_popup_request(&self, url: String) {
        self.popup_requests.borrow_mut().push(url);
        self.mark_dirty();
    }

    pub(crate) fn take_popup_requests(&self) -> Vec<String> {
        std::mem::take(&mut self.popup_requests.borrow_mut())
    }

    pub(crate) fn set_composition_bounds(&self, bounds: Vec<PageRect>) {
        *self.composition_bounds.borrow_mut() = bounds;
    }

    pub(crate) fn composition_bounds(&self, range: std::ops::Range<usize>) -> Option<PageRect> {
        let bounds = self.composition_bounds.borrow();
        let slice = match bounds.get(range) {
            Some(slice) if !slice.is_empty() => slice,
            _ if !bounds.is_empty() => &bounds[..],
            _ => return None,
        };
        slice.iter().copied().reduce(|a, b| {
            let x = a.x.min(b.x);
            let y = a.y.min(b.y);
            PageRect {
                x,
                y,
                width: (a.x + a.width).max(b.x + b.width) - x,
                height: (a.y + a.height).max(b.y + b.height) - y,
            }
        })
    }

    pub(crate) fn set_listener(&self, listener: Box<dyn Fn()>) {
        *self.listener.borrow_mut() = Some(listener);
    }

    /// Called by the pump after each round of Chromium work.
    pub(crate) fn after_pump(&self) {
        if self.resized.replace(false) {
            if let Some(host) = self.host() {
                use cef::ImplBrowserHost as _;
                host.was_resized();
            }
        }
        if self.screen_changed.replace(false) {
            if let Some(host) = self.host() {
                use cef::ImplBrowserHost as _;
                host.notify_screen_info_changed();
                host.was_resized();
            }
        }
        if self.dirty.replace(false) {
            if let Ok(listener) = self.listener.try_borrow() {
                if let Some(listener) = listener.as_ref() {
                    listener();
                }
            }
        }
    }

    /// Fails pending script calls (navigation or close discards them).
    pub(crate) fn fail_pending_scripts(&self, reason: &str) {
        let pending = self.results.borrow_mut().drain();
        for done in pending {
            done(Err(reason.to_string()));
        }
    }
}

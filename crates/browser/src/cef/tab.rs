//! One Chromium browser rendered off-screen.

use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

use cef::*;
use cockpit_common::BrowserTabId;

use super::{
    handlers::{CockpitClient, TextVisitor},
    pump,
    shared::TabShared,
};
use crate::{
    automation::{ConsoleEntry, PAGE_TEXT_SCRIPT},
    engine::{
        BrowserTab, EditCommand, EvalCallback, PageRect, TabOptions, TabState, TextCallback,
        ViewGeometry,
    },
    frame::Frame as PageFrame,
    input::{self as keys, KeyInput, MouseButton, MouseInput},
};

pub(crate) struct CefTab {
    shared: Rc<TabShared>,
}

impl CefTab {
    pub(crate) fn create(options: &TabOptions, secret: &str) -> anyhow::Result<Self> {
        let shared = Rc::new(TabShared::new(&options.url, secret));
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            ..Default::default()
        };
        let settings = BrowserSettings {
            windowless_frame_rate: options.frame_rate.clamp(1, 120) as i32,
            // Opaque white, like a normal browser window.
            background_color: 0xFF_FF_FF_FF,
            ..Default::default()
        };
        let mut client = CockpitClient::build(shared.clone());
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&options.url.as_str().into()),
            Some(&settings),
            None,
            None,
        )
        .ok_or_else(|| anyhow::anyhow!("Chromium could not create a browser"))?;
        shared.set_browser(browser);
        pump::register(&shared);
        Ok(Self { shared })
    }

    fn host(&self) -> Option<BrowserHost> {
        self.shared.host()
    }

    fn main_frame(&self) -> Option<Frame> {
        self.shared.browser().and_then(|b| b.main_frame())
    }

    fn send_char_events(host: &BrowserHost, text: &str) {
        for unit in keys::text_char_codes(text) {
            let event = KeyEvent {
                type_: KeyEventType::CHAR,
                windows_key_code: unit as i32,
                character: unit,
                unmodified_character: unit,
                ..Default::default()
            };
            host.send_key_event(Some(&event));
        }
    }

    fn key_event(input: &KeyInput, type_: KeyEventType) -> KeyEvent {
        let vk = keys::windows_key_code(&input.key);
        KeyEvent {
            type_,
            modifiers: keys::modifier_flags(&input.modifiers),
            windows_key_code: vk,
            native_key_code: keys::mac_key_code(&input.key),
            character: keys::first_utf16(input.key_char.as_deref().unwrap_or_default()),
            unmodified_character: keys::first_utf16(&input.key),
            ..Default::default()
        }
    }
}

fn cef_button(button: MouseButton) -> MouseButtonType {
    match button {
        MouseButton::Left => MouseButtonType::LEFT,
        MouseButton::Middle => MouseButtonType::MIDDLE,
        MouseButton::Right => MouseButtonType::RIGHT,
    }
}

fn mouse_event(x: f32, y: f32, modifiers: u32) -> MouseEvent {
    MouseEvent {
        x: x.round() as i32,
        y: y.round() as i32,
        modifiers,
    }
}

/// CEF ranges on macOS use `u32::MAX` for "no range" (NSNotFound).
fn no_range() -> cef::Range {
    cef::Range {
        from: u32::MAX,
        to: u32::MAX,
    }
}

impl BrowserTab for CefTab {
    fn id(&self) -> BrowserTabId {
        self.shared.id
    }

    fn state(&self) -> TabState {
        self.shared.state()
    }

    fn navigate(&self, url: &str) {
        if let Some(frame) = self.main_frame() {
            frame.load_url(Some(&url.into()));
        }
    }

    fn reload(&self) {
        if let Some(browser) = self.shared.browser() {
            browser.reload();
        }
    }

    fn stop(&self) {
        if let Some(browser) = self.shared.browser() {
            browser.stop_load();
        }
    }

    fn go_back(&self) {
        if let Some(browser) = self.shared.browser() {
            browser.go_back();
        }
    }

    fn go_forward(&self) {
        if let Some(browser) = self.shared.browser() {
            browser.go_forward();
        }
    }

    fn set_zoom_level(&self, level: f64) {
        if let Some(host) = self.host() {
            host.set_zoom_level(level);
        }
    }

    fn edit(&self, command: EditCommand) {
        let Some(browser) = self.shared.browser() else { return };
        let Some(frame) = browser.focused_frame().or_else(|| browser.main_frame()) else {
            return;
        };
        match command {
            EditCommand::Undo => frame.undo(),
            EditCommand::Redo => frame.redo(),
            EditCommand::Cut => frame.cut(),
            EditCommand::Copy => frame.copy(),
            EditCommand::Paste => frame.paste(),
            EditCommand::SelectAll => frame.select_all(),
        }
    }

    fn show_devtools(&self) {
        if let Some(host) = self.host() {
            // DevTools opens as its own native window, which is the normal
            // Chromium DevTools experience.
            let window_info = WindowInfo::default();
            let settings = BrowserSettings::default();
            host.show_dev_tools(Some(&window_info), None, Some(&settings), None);
        }
    }

    fn close(&self) {
        self.shared.fail_pending_scripts("the tab was closed");
        if super::SHUT_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if let Some(browser) = self.shared.take_browser() {
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
        }
    }

    fn set_geometry(&self, geometry: ViewGeometry) {
        self.shared
            .set_geometry(geometry.width, geometry.height, geometry.scale_factor);
    }

    fn set_focus(&self, focused: bool) {
        if let Some(host) = self.host() {
            host.set_focus(focused as i32);
        }
    }

    fn set_listener(&self, listener: Box<dyn Fn()>) {
        self.shared.set_listener(listener);
    }

    fn latest_frame(&self) -> Option<(u64, Arc<PageFrame>)> {
        self.shared.page_frame()
    }

    fn popup(&self) -> Option<(u64, Arc<PageFrame>, PageRect)> {
        self.shared.popup()
    }

    fn take_popup_requests(&self) -> Vec<String> {
        self.shared.take_popup_requests()
    }

    fn send_mouse(&self, input: MouseInput) {
        let Some(host) = self.host() else { return };
        match input {
            MouseInput::Move { x, y, pressed, modifiers } => {
                let flags = keys::modifier_flags(&modifiers) | keys::button_flag(pressed);
                host.send_mouse_move_event(Some(&mouse_event(x, y, flags)), 0);
            }
            MouseInput::Down { x, y, button, click_count, modifiers } => {
                let flags = keys::modifier_flags(&modifiers) | keys::button_flag(Some(button));
                host.send_mouse_click_event(
                    Some(&mouse_event(x, y, flags)),
                    cef_button(button),
                    0,
                    click_count.max(1) as i32,
                );
            }
            MouseInput::Up { x, y, button, click_count, modifiers } => {
                let flags = keys::modifier_flags(&modifiers);
                host.send_mouse_click_event(
                    Some(&mouse_event(x, y, flags)),
                    cef_button(button),
                    1,
                    click_count.max(1) as i32,
                );
            }
            MouseInput::Wheel { x, y, delta_x, delta_y, modifiers } => {
                let flags = keys::modifier_flags(&modifiers);
                host.send_mouse_wheel_event(
                    Some(&mouse_event(x, y, flags)),
                    delta_x.round() as i32,
                    delta_y.round() as i32,
                );
            }
            MouseInput::Leave => {
                host.send_mouse_move_event(Some(&mouse_event(-1.0, -1.0, 0)), 1);
            }
        }
    }

    fn send_key_down(&self, input: &KeyInput) {
        let Some(host) = self.host() else { return };
        host.send_key_event(Some(&Self::key_event(input, KeyEventType::RAWKEYDOWN)));
    }

    fn send_key_up(&self, input: &KeyInput) {
        let Some(host) = self.host() else { return };
        host.send_key_event(Some(&Self::key_event(input, KeyEventType::KEYUP)));
    }

    fn insert_text(&self, text: &str, key: Option<&KeyInput>) {
        let Some(host) = self.host() else { return };
        // The physical key press was held back until the input method decided
        // it was text; deliver it first so keydown handlers still fire.
        if let Some(key) = key {
            host.send_key_event(Some(&Self::key_event(key, KeyEventType::RAWKEYDOWN)));
        }
        Self::send_char_events(&host, text);
    }

    fn set_composition(&self, text: &str, selection: Option<Range<usize>>) {
        let Some(host) = self.host() else { return };
        let length = text.encode_utf16().count() as u32;
        let underlines = [CompositionUnderline {
            range: cef::Range { from: 0, to: length },
            color: 0xFF00_0000,
            ..Default::default()
        }];
        let selection = selection.map(|range| cef::Range {
            from: range.start as u32,
            to: range.end as u32,
        });
        host.ime_set_composition(
            Some(&text.into()),
            Some(&underlines),
            Some(&no_range()),
            selection.as_ref(),
        );
    }

    fn commit_composition(&self, text: &str) {
        if let Some(host) = self.host() {
            host.ime_commit_text(Some(&text.into()), Some(&no_range()), 0);
        }
    }

    fn cancel_composition(&self) {
        if let Some(host) = self.host() {
            host.ime_cancel_composition();
        }
    }

    fn finish_composition(&self) {
        if let Some(host) = self.host() {
            host.ime_finish_composing_text(0);
        }
    }

    fn composition_bounds(&self, range: Range<usize>) -> Option<PageRect> {
        self.shared.composition_bounds(range)
    }

    fn eval(&self, script: &str, done: EvalCallback) {
        let Some(frame) = self.main_frame() else {
            done(Err("the tab has no page".into()));
            return;
        };
        let wrapped = self.shared.results.borrow_mut().wrap(script, done);
        frame.execute_java_script(Some(&wrapped.as_str().into()), Some(&"cockpit://automation".into()), 1);
    }

    fn page_text(&self, done: TextCallback) {
        // innerText reflects what is rendered (no hidden elements, no
        // scripts); fall back to Chromium's own text dump if scripts fail.
        let frame = self.main_frame();
        let fallback = Rc::new(RefCell::new(Some(done)));
        let on_result = {
            let fallback = fallback.clone();
            Box::new(move |result: Result<serde_json::Value, String>| match result {
                Ok(value) => {
                    if let Some(done) = fallback.borrow_mut().take() {
                        done(value["text"].as_str().unwrap_or_default().to_string());
                    }
                }
                Err(_) => {
                    if let Some(frame) = &frame {
                        let mut visitor = TextVisitor::new(fallback.clone());
                        frame.text(Some(&mut visitor));
                    } else if let Some(done) = fallback.borrow_mut().take() {
                        done(String::new());
                    }
                }
            })
        };
        self.eval(PAGE_TEXT_SCRIPT, on_result);
    }

    fn console(&self) -> Vec<ConsoleEntry> {
        self.shared.console()
    }
}

impl Drop for CefTab {
    fn drop(&mut self) {
        self.close();
    }
}

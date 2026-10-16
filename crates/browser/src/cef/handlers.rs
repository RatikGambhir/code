//! Chromium's callbacks, which only ever update [`TabShared`]; the UI reads
//! from there.
//!
//! cef-rs's `wrap_*!` macros define a handler as a struct of `Clone` fields
//! plus overrides of the trait's default methods.

use std::rc::Rc;

use cef::{rc::Rc as _, *};

use super::shared::TabShared;
use crate::{
    automation::{ConsoleEntry, ConsoleLevel},
    engine::{CursorKind, PageRect},
    frame::Frame as PageFrame,
};

/// Settings carried by the application object into every process.
#[derive(Clone)]
pub(crate) struct AppConfig {
    pub(crate) remote_debugging_port: Option<u16>,
}

wrap_app! {
    pub(crate) struct CockpitApp {
        config: AppConfig,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else { return };
            // Off-screen rendering: Chromium must not open windows of its own.
            command_line.append_switch(Some(&"no-startup-window".into()));
            command_line.append_switch(Some(&"noerrdialogs".into()));
            command_line.append_switch(Some(&"hide-crash-restore-bubble".into()));
            // Unsigned development builds change identity on every rebuild,
            // which would make the Keychain prompt for the cookie key each
            // launch. Release builds use the real Keychain.
            if cfg!(debug_assertions) || std::env::var_os("COCKPIT_CEF_MOCK_KEYCHAIN").is_some() {
                command_line.append_switch(Some(&"use-mock-keychain".into()));
            }
            // Escape hatch for broken helper setups. Disables Chromium's
            // process sandbox, so never on by default.
            if std::env::var_os("COCKPIT_CEF_NO_SANDBOX").is_some() {
                command_line.append_switch(Some(&"no-sandbox".into()));
            }
            if let Some(port) = self.config.remote_debugging_port {
                command_line.append_switch_with_value(
                    Some(&"remote-debugging-port".into()),
                    Some(&port.to_string().as_str().into()),
                );
                command_line.append_switch_with_value(
                    Some(&"remote-allow-origins".into()),
                    Some(&format!("http://127.0.0.1:{port}").as_str().into()),
                );
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(CockpitBrowserProcessHandler::new())
        }
    }
}

wrap_browser_process_handler! {
    pub(crate) struct CockpitBrowserProcessHandler;

    impl BrowserProcessHandler {
        /// The pump runs at 60 Hz regardless, so there is nothing to schedule.
        fn on_schedule_message_pump_work(&self, _delay_ms: i64) {}
    }
}

/// `Rc<TabShared>` wrapped so it can be a handler field.
#[derive(Clone)]
pub(crate) struct TabRef(pub(crate) Rc<TabShared>);

wrap_render_handler! {
    pub(crate) struct CockpitRenderHandler {
        tab: TabRef,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let (width, height) = self.tab.0.view_size();
            rect.x = 0;
            rect.y = 0;
            rect.width = (width.round() as i32).max(1);
            rect.height = (height.round() as i32).max(1);
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(screen_info) = screen_info else { return 0 };
            screen_info.device_scale_factor = self.tab.0.scale_factor();
            1
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            self.tab.0.set_popup_visible(show != 0);
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(rect) = rect else { return };
            self.tab.0.set_popup_rect(page_rect(rect));
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let len = width as usize * height as usize * 4;
            // The buffer is only valid during this call.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, len) }.to_vec();
            let Some(frame) = PageFrame::new(width as u32, height as u32, pixels) else {
                return;
            };
            if type_ == PaintElementType::POPUP {
                self.tab.0.put_popup_frame(frame);
            } else {
                self.tab.0.put_page_frame(frame);
            }
        }

        fn on_ime_composition_range_changed(
            &self,
            _browser: Option<&mut Browser>,
            _selected_range: Option<&Range>,
            character_bounds: Option<&[Rect]>,
        ) {
            let bounds = character_bounds.unwrap_or_default().iter().map(page_rect).collect();
            self.tab.0.set_composition_bounds(bounds);
        }
    }
}

fn page_rect(rect: &Rect) -> PageRect {
    PageRect {
        x: rect.x as f32,
        y: rect.y as f32,
        width: rect.width as f32,
        height: rect.height as f32,
    }
}

fn is_main_frame(frame: &Option<&mut Frame>) -> bool {
    frame.as_ref().is_none_or(|frame| frame.is_main() != 0)
}

wrap_display_handler! {
    pub(crate) struct CockpitDisplayHandler {
        tab: TabRef,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            let title = title.map(|t| t.to_string()).unwrap_or_default();
            self.tab.0.update_state(|state| state.title = title);
        }

        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if !is_main_frame(&frame) {
                return;
            }
            if let Some(url) = url {
                let url = url.to_string();
                self.tab.0.update_state(|state| state.url = url);
            }
        }

        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(message) = message.map(|m| m.to_string()) else { return 0 };
            // Results of scripts the IDE ran are routed back, not logged.
            let result = self.tab.0.results.borrow_mut().take(&message);
            if let Some((done, outcome)) = result {
                done(outcome);
                return 1;
            }
            if self.tab.0.results.borrow().is_result_message(&message) {
                return 1;
            }
            let level = if level == LogSeverity::ERROR || level == LogSeverity::FATAL {
                ConsoleLevel::Error
            } else if level == LogSeverity::WARNING {
                ConsoleLevel::Warning
            } else if level == LogSeverity::VERBOSE {
                ConsoleLevel::Debug
            } else {
                ConsoleLevel::Info
            };
            self.tab.0.push_console(ConsoleEntry {
                level,
                message,
                source: source.map(|s| s.to_string()).unwrap_or_default(),
                line,
            });
            0
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: *mut u8,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            // Off-screen, Chromium never sets the real cursor; the UI does.
            self.tab.0.set_cursor(cursor_kind(type_));
            1
        }
    }
}

pub(crate) fn cursor_kind(cursor: CursorType) -> CursorKind {
    match cursor {
        CursorType::HAND => CursorKind::Pointer,
        CursorType::IBEAM => CursorKind::Text,
        CursorType::VERTICALTEXT => CursorKind::VerticalText,
        CursorType::CROSS | CursorType::CELL => CursorKind::Crosshair,
        CursorType::GRAB => CursorKind::Grab,
        CursorType::GRABBING | CursorType::MOVE | CursorType::MIDDLEPANNING => CursorKind::Grabbing,
        CursorType::CONTEXTMENU => CursorKind::ContextMenu,
        CursorType::ALIAS | CursorType::DND_LINK => CursorKind::Alias,
        CursorType::COPY | CursorType::DND_COPY => CursorKind::Copy,
        CursorType::NOTALLOWED | CursorType::NODROP | CursorType::DND_NONE => CursorKind::NotAllowed,
        CursorType::EASTRESIZE | CursorType::WESTRESIZE | CursorType::EASTWESTRESIZE => {
            CursorKind::ResizeLeftRight
        }
        CursorType::NORTHRESIZE | CursorType::SOUTHRESIZE | CursorType::NORTHSOUTHRESIZE => {
            CursorKind::ResizeUpDown
        }
        CursorType::NORTHEASTRESIZE | CursorType::SOUTHWESTRESIZE | CursorType::NORTHEASTSOUTHWESTRESIZE => {
            CursorKind::ResizeNeSw
        }
        CursorType::NORTHWESTRESIZE | CursorType::SOUTHEASTRESIZE | CursorType::NORTHWESTSOUTHEASTRESIZE => {
            CursorKind::ResizeNwSe
        }
        CursorType::COLUMNRESIZE => CursorKind::ResizeColumn,
        CursorType::ROWRESIZE => CursorKind::ResizeRow,
        _ => CursorKind::Arrow,
    }
}

wrap_load_handler! {
    pub(crate) struct CockpitLoadHandler {
        tab: TabRef,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: ::std::os::raw::c_int,
            can_go_back: ::std::os::raw::c_int,
            can_go_forward: ::std::os::raw::c_int,
        ) {
            self.tab.0.update_state(|state| {
                state.loading = is_loading != 0;
                state.can_go_back = can_go_back != 0;
                state.can_go_forward = can_go_forward != 0;
            });
        }

        fn on_load_start(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            if is_main_frame(&frame) {
                // A new document: old console output and scripts no longer apply.
                self.tab.0.clear_console();
                self.tab.0.fail_pending_scripts("the page navigated away");
                self.tab.0.update_state(|state| state.load_error = None);
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            // ERR_ABORTED only means another navigation replaced this one.
            if error_code == Errorcode::from(sys::cef_errorcode_t::ERR_ABORTED) || !is_main_frame(&frame) {
                return;
            }
            let text = error_text.map(|t| t.to_string()).unwrap_or_default();
            let url = failed_url.map(|u| u.to_string()).unwrap_or_default();
            self.tab.0.update_state(|state| state.load_error = Some(format!("{text} ({url})")));
        }
    }
}

wrap_life_span_handler! {
    pub(crate) struct CockpitLifeSpanHandler {
        tab: TabRef,
    }

    impl LifeSpanHandler {
        /// `target=_blank` and `window.open` become IDE tabs instead of
        /// native windows.
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            if let Some(url) = target_url.map(|u| u.to_string()).filter(|u| !u.is_empty()) {
                self.tab.0.push_popup_request(url);
            }
            1
        }
    }
}

wrap_context_menu_handler! {
    pub(crate) struct CockpitContextMenuHandler;

    impl ContextMenuHandler {
        /// Chromium's native context menu cannot attach to an off-screen
        /// view; suppress it rather than show it in the wrong place.
        fn on_before_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
        ) {
            if let Some(model) = model {
                model.clear();
            }
        }
    }
}

wrap_client! {
    pub(crate) struct CockpitClient {
        render_handler: RenderHandler,
        display_handler: DisplayHandler,
        load_handler: LoadHandler,
        life_span_handler: LifeSpanHandler,
        context_menu_handler: ContextMenuHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display_handler.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.context_menu_handler.clone())
        }
    }
}

impl CockpitClient {
    pub(crate) fn build(tab: Rc<TabShared>) -> Client {
        Self::new(
            CockpitRenderHandler::new(TabRef(tab.clone())),
            CockpitDisplayHandler::new(TabRef(tab.clone())),
            CockpitLoadHandler::new(TabRef(tab.clone())),
            CockpitLifeSpanHandler::new(TabRef(tab)),
            CockpitContextMenuHandler::new(),
        )
    }
}

wrap_string_visitor! {
    pub(crate) struct TextVisitor {
        done: Rc<std::cell::RefCell<Option<crate::engine::TextCallback>>>,
    }

    impl CefStringVisitor {
        fn visit(&self, string: Option<&CefString>) {
            if let Some(done) = self.done.borrow_mut().take() {
                done(string.map(|s| s.to_string()).unwrap_or_default());
            }
        }
    }
}

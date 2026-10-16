//! A browser tab: toolbar plus the page, drawn from Chromium's off-screen
//! frames as an ordinary GPUI element.
//!
//! Because the page is just an image in GPUI's scene it clips, layers and
//! resizes with everything else. Input goes the other way: GPUI mouse and
//! key events are translated and sent to Chromium, and text input (including
//! IME composition) arrives through [`EntityInputHandler`].

use std::{
    cell::Cell,
    ops::Range,
    rc::Rc,
    sync::Arc,
};

use cockpit_browser::{
    resolve_address, BrowserTab, CursorKind, EditCommand, Frame, KeyInput, Modifiers as PageModifiers,
    MouseButton as PageButton, MouseInput, PageRect, TabOptions, TabState, ViewGeometry,
};
use gpui::{
    div, img, prelude::*, px, relative, AnyElement, App, Bounds, Context, CursorStyle, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, Hitbox, HitboxBehavior, ImageSource, InspectorElementId, KeyDownEvent,
    KeyUpEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    Pixels, Point, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, Style, Subscription,
    UTF16Selection, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
};
use smallvec::smallvec;

use crate::{actions, assets::icon, state::AppState};

pub enum BrowserEvent {
    /// The page asked to open a URL in a new window.
    OpenInNewTab(String),
    /// Title, URL or loading state changed.
    Changed,
    /// A page finished navigating to a URL (for history).
    Navigated { url: String, title: String },
}

pub struct BrowserView {
    tab: Option<Rc<dyn BrowserTab>>,
    unavailable: Option<SharedString>,
    address: Entity<InputState>,
    address_stale: bool,
    focus_handle: FocusHandle,
    page_image: Option<(u64, Arc<RenderImage>)>,
    popup_image: Option<(u64, Arc<RenderImage>, PageRect)>,
    /// Images no longer shown, to evict from the GPU atlas on next render.
    stale_images: Vec<Arc<RenderImage>>,
    /// Where the page was last laid out, in window coordinates.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    state: TabState,
    /// A printable key held back until the input method decides it is text.
    pending_key: Option<KeyInput>,
    /// Text the input method is composing (mirrored for the OS's queries).
    composition: String,
    zoom_level: f64,
    /// This tab is the one Codex drives through its browser tools.
    pub agent_driven: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BrowserEvent> for BrowserView {}

impl Drop for BrowserView {
    fn drop(&mut self) {
        self.close();
    }
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BrowserView {
    pub fn new(url: &str, agent_driven: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let engine = AppState::global(cx).browser.clone();
        let (tab, unavailable) = match engine.create_tab(TabOptions::new(url)) {
            Ok(tab) => (Some(tab), None),
            Err(err) => (None, Some(SharedString::from(format!("{err:#}")))),
        };
        if let Some(tab) = &tab {
            let view = cx.entity().downgrade();
            let async_cx = cx.to_async();
            tab.set_listener(Box::new(move || {
                let mut cx = async_cx.clone();
                let _ = view.update(&mut cx, |view, cx| view.on_tab_changed(cx));
            }));
        }

        let address = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search or enter address")
                .default_value(url.to_string())
        });
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.subscribe_in(&address, window, |this, address, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = address.read(cx).value().to_string();
                    this.load(&text, cx);
                    window.focus(&this.focus_handle);
                }
            }),
            cx.on_focus_in(&focus_handle, window, |this, _, _| {
                if let Some(tab) = &this.tab {
                    tab.set_focus(true);
                }
            }),
            cx.on_focus_out(&focus_handle, window, |this, _, _, _| {
                if let Some(tab) = &this.tab {
                    tab.set_focus(false);
                }
            }),
        ];

        Self {
            state: TabState {
                url: url.to_string(),
                ..Default::default()
            },
            tab,
            unavailable,
            address,
            address_stale: false,
            focus_handle,
            page_image: None,
            popup_image: None,
            stale_images: Vec::new(),
            bounds: Rc::new(Cell::new(Bounds::default())),
            pending_key: None,
            composition: String::new(),
            zoom_level: 0.0,
            agent_driven,
            _subscriptions: subscriptions,
        }
    }

    pub fn tab(&self) -> Option<&Rc<dyn BrowserTab>> {
        self.tab.as_ref()
    }

    /// Closes the page. The view stays empty afterwards.
    pub fn close(&mut self) {
        if let Some(tab) = self.tab.take() {
            tab.close();
        }
    }

    pub fn title(&self) -> SharedString {
        if !self.state.title.is_empty() {
            self.state.title.clone().into()
        } else if self.state.url.is_empty() || self.state.url == "about:blank" {
            "New Tab".into()
        } else {
            self.state.url.clone().into()
        }
    }

    pub fn url(&self) -> &str {
        &self.state.url
    }

    pub fn state(&self) -> &TabState {
        &self.state
    }

    /// Loads address-bar style input (URL or search).
    pub fn load(&mut self, input: &str, cx: &mut Context<Self>) {
        let search_url = AppState::global(cx).settings.search_url.clone();
        let url = resolve_address(input, &search_url);
        self.navigate(&url, cx);
    }

    pub fn navigate(&mut self, url: &str, cx: &mut Context<Self>) {
        if let Some(tab) = &self.tab {
            tab.navigate(url);
        }
        self.state.url = url.to_string();
        self.address_stale = true;
        cx.notify();
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = &self.tab {
            if self.state.loading {
                tab.stop();
            } else {
                tab.reload();
            }
        }
        cx.notify();
    }

    pub fn go_back(&mut self) {
        if let Some(tab) = &self.tab {
            tab.go_back();
        }
    }

    pub fn go_forward(&mut self) {
        if let Some(tab) = &self.tab {
            tab.go_forward();
        }
    }

    /// The newest frame as a PNG data URL, for the agent's screenshot tool.
    pub fn screenshot_data_url(&self) -> anyhow::Result<String> {
        let tab = self.tab.as_ref().ok_or_else(|| anyhow::anyhow!("the browser is unavailable"))?;
        let (_, frame) = tab
            .latest_frame()
            .ok_or_else(|| anyhow::anyhow!("the page has not painted yet"))?;
        frame.to_png_data_url(1600)
    }

    pub fn focus_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.address.update(cx, |input, cx| input.focus(window, cx));
        // Select the whole address, as browsers do.
        window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
    }

    fn on_tab_changed(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tab.clone() else { return };

        if let Some((generation, frame)) = tab.latest_frame() {
            if self.page_image.as_ref().map(|(g, _)| *g) != Some(generation) {
                if let Some(image) = render_image(&frame) {
                    if let Some((_, old)) = self.page_image.replace((generation, image)) {
                        self.stale_images.push(old);
                    }
                }
            }
        }
        match tab.popup() {
            Some((generation, frame, rect)) => {
                if self.popup_image.as_ref().map(|(g, ..)| *g) != Some(generation) {
                    if let Some(image) = render_image(&frame) {
                        if let Some((_, old, _)) = self.popup_image.replace((generation, image, rect)) {
                            self.stale_images.push(old);
                        }
                    }
                } else if let Some((_, _, r)) = self.popup_image.as_mut() {
                    *r = rect;
                }
            }
            None => {
                if let Some((_, old, _)) = self.popup_image.take() {
                    self.stale_images.push(old);
                }
            }
        }

        for url in tab.take_popup_requests() {
            cx.emit(BrowserEvent::OpenInNewTab(url));
        }

        let state = tab.state();
        let finished_loading = self.state.loading && !state.loading;
        if state.url != self.state.url {
            self.address_stale = true;
        }
        let changed = state.url != self.state.url
            || state.title != self.state.title
            || state.loading != self.state.loading
            || state.load_error != self.state.load_error;
        self.state = state;
        if finished_loading && self.state.load_error.is_none() {
            cx.emit(BrowserEvent::Navigated {
                url: self.state.url.clone(),
                title: self.state.title.clone(),
            });
        }
        if changed {
            cx.emit(BrowserEvent::Changed);
        }
        cx.notify();
    }

    fn page_point(&self, position: Point<Pixels>) -> (f32, f32) {
        let origin = self.bounds.get().origin;
        (f32::from(position.x - origin.x), f32::from(position.y - origin.y))
    }

    fn send_mouse(&self, input: MouseInput) {
        if let Some(tab) = &self.tab {
            tab.send_mouse(input);
        }
    }

    fn send_tab(&self, backwards: bool) {
        let Some(tab) = &self.tab else { return };
        let input = KeyInput {
            key: "tab".into(),
            key_char: Some("\t".into()),
            modifiers: PageModifiers {
                shift: backwards,
                ..Default::default()
            },
        };
        tab.send_key_down(&input);
        tab.send_key_up(&input);
    }

    fn edit(&mut self, command: EditCommand) {
        if let Some(tab) = &self.tab {
            tab.edit(command);
        }
    }

    fn set_zoom(&mut self, level: f64, cx: &mut Context<Self>) {
        self.zoom_level = level.clamp(-5.0, 9.0);
        if let Some(tab) = &self.tab {
            tab.set_zoom_level(self.zoom_level);
        }
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let loading = self.state.loading;
        h_flex()
            .h(px(36.))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().tab_bar)
            .child(
                Button::new("back")
                    .icon(IconName::ArrowLeft)
                    .ghost()
                    .small()
                    .disabled(!self.state.can_go_back)
                    .tooltip("Back")
                    .on_click(cx.listener(|this, _, _, _| this.go_back())),
            )
            .child(
                Button::new("forward")
                    .icon(IconName::ArrowRight)
                    .ghost()
                    .small()
                    .disabled(!self.state.can_go_forward)
                    .tooltip("Forward")
                    .on_click(cx.listener(|this, _, _, _| this.go_forward())),
            )
            .child(
                Button::new("reload")
                    .icon(if loading { icon("x") } else { icon("rotate-cw") })
                    .ghost()
                    .small()
                    .tooltip(if loading { "Stop" } else { "Reload" })
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.address).small().cleanable(false)),
            )
            .when(self.zoom_level != 0.0, |el| {
                el.child(
                    Button::new("zoom-reset")
                        .label(format!("{:+.0}", self.zoom_level))
                        .ghost()
                        .xsmall()
                        .tooltip("Reset zoom")
                        .on_click(cx.listener(|this, _, _, cx| this.set_zoom(0.0, cx))),
                )
            })
            .when(self.agent_driven, |el| {
                el.child(
                    h_flex()
                        .gap_1()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .text_xs()
                        .bg(cx.theme().accent)
                        .text_color(cx.theme().accent_foreground)
                        .child(Icon::new(IconName::Bot).xsmall())
                        .child("Agent"),
                )
            })
            .child(
                Button::new("devtools")
                    .icon(icon("wrench"))
                    .ghost()
                    .small()
                    .tooltip("Developer Tools")
                    .disabled(self.tab.is_none())
                    .on_click(cx.listener(|this, _, _, _| {
                        if let Some(tab) = &this.tab {
                            tab.show_devtools();
                        }
                    })),
            )
            .child(
                Button::new("open-external")
                    .icon(IconName::ExternalLink)
                    .ghost()
                    .small()
                    .tooltip("Open in default browser")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.state.url.starts_with("http") {
                            cx.open_url(&this.state.url);
                        }
                    })),
            )
    }

    fn render_unavailable(&self, reason: &SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .p_8()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Globe).large())
            .child(div().text_lg().text_color(cx.theme().foreground).child("The embedded browser is not available"))
            .child(div().max_w(px(520.)).text_sm().text_center().child(reason.clone()))
    }
}

fn render_image(frame: &Frame) -> Option<Arc<RenderImage>> {
    // GPUI's atlas takes BGRA, which is what Chromium paints; the image
    // crate's "RGBA" type is only a container here.
    let buffer = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra.clone())?;
    Some(Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)])))
}

fn page_modifiers(modifiers: &gpui::Modifiers) -> PageModifiers {
    PageModifiers {
        shift: modifiers.shift,
        control: modifiers.control,
        alt: modifiers.alt,
        command: modifiers.platform,
        function: modifiers.function,
    }
}

fn page_button(button: MouseButton) -> Option<PageButton> {
    match button {
        MouseButton::Left => Some(PageButton::Left),
        MouseButton::Middle => Some(PageButton::Middle),
        MouseButton::Right => Some(PageButton::Right),
        MouseButton::Navigate(_) => None,
    }
}

fn key_input(keystroke: &gpui::Keystroke) -> KeyInput {
    KeyInput {
        key: keystroke.key.clone(),
        key_char: keystroke.key_char.clone(),
        modifiers: page_modifiers(&keystroke.modifiers),
    }
}

fn cursor_style(kind: CursorKind) -> CursorStyle {
    match kind {
        CursorKind::Arrow | CursorKind::ContextMenu => CursorStyle::Arrow,
        CursorKind::Pointer => CursorStyle::PointingHand,
        CursorKind::Text => CursorStyle::IBeam,
        CursorKind::VerticalText => CursorStyle::IBeamCursorForVerticalLayout,
        CursorKind::Crosshair => CursorStyle::Crosshair,
        CursorKind::Grab => CursorStyle::OpenHand,
        CursorKind::Grabbing => CursorStyle::ClosedHand,
        CursorKind::NotAllowed => CursorStyle::OperationNotAllowed,
        CursorKind::Copy => CursorStyle::DragCopy,
        CursorKind::Alias => CursorStyle::DragLink,
        CursorKind::ResizeLeftRight => CursorStyle::ResizeLeftRight,
        CursorKind::ResizeUpDown => CursorStyle::ResizeUpDown,
        CursorKind::ResizeNeSw => CursorStyle::ResizeUpRightDownLeft,
        CursorKind::ResizeNwSe => CursorStyle::ResizeUpLeftDownRight,
        CursorKind::ResizeColumn => CursorStyle::ResizeColumn,
        CursorKind::ResizeRow => CursorStyle::ResizeRow,
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.stale_images.drain(..) {
            cx.drop_image(image, Some(window));
        }
        if self.address_stale && !self.address.focus_handle(cx).is_focused(window) {
            self.address_stale = false;
            let url = self.state.url.clone();
            self.address.update(cx, |input, cx| input.set_value(url, window, cx));
        }

        let page: AnyElement = match &self.unavailable {
            Some(reason) => self.render_unavailable(reason, cx).into_any_element(),
            None => div()
                .id("page")
                .relative()
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .bg(gpui::white())
                .track_focus(&self.focus_handle)
                .key_context(actions::BROWSER_CONTEXT)
                .on_action(cx.listener(|this, _: &actions::FocusAddressBar, window, cx| this.focus_address(window, cx)))
                .on_action(cx.listener(|this, _: &actions::Reload, _, cx| this.reload(cx)))
                .on_action(cx.listener(|this, _: &actions::GoBack, _, _| this.go_back()))
                .on_action(cx.listener(|this, _: &actions::GoForward, _, _| this.go_forward()))
                .on_action(cx.listener(|this, _: &actions::ShowDevTools, _, _| {
                    if let Some(tab) = &this.tab {
                        tab.show_devtools();
                    }
                }))
                .on_action(cx.listener(|this, _: &actions::ZoomIn, _, cx| this.set_zoom(this.zoom_level + 0.5, cx)))
                .on_action(cx.listener(|this, _: &actions::ZoomOut, _, cx| this.set_zoom(this.zoom_level - 0.5, cx)))
                .on_action(cx.listener(|this, _: &actions::ResetZoom, _, cx| this.set_zoom(0.0, cx)))
                .on_action(cx.listener(|this, _: &actions::Copy, _, _| this.edit(EditCommand::Copy)))
                .on_action(cx.listener(|this, _: &actions::Cut, _, _| this.edit(EditCommand::Cut)))
                .on_action(cx.listener(|this, _: &actions::Paste, _, _| this.edit(EditCommand::Paste)))
                .on_action(cx.listener(|this, _: &actions::SelectAll, _, _| this.edit(EditCommand::SelectAll)))
                .on_action(cx.listener(|this, _: &actions::Undo, _, _| this.edit(EditCommand::Undo)))
                .on_action(cx.listener(|this, _: &actions::Redo, _, _| this.edit(EditCommand::Redo)))
                .on_action(cx.listener(|this, _: &actions::PageTab, _, _| this.send_tab(false)))
                .on_action(cx.listener(|this, _: &actions::PageBackTab, _, _| this.send_tab(true)))
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, _| {
                    let (x, y) = this.page_point(event.position);
                    this.send_mouse(MouseInput::Move {
                        x,
                        y,
                        pressed: event.pressed_button.and_then(page_button),
                        modifiers: page_modifiers(&event.modifiers),
                    });
                }))
                .on_hover(cx.listener(|this, hovered: &bool, _, _| {
                    if !*hovered {
                        this.send_mouse(MouseInput::Leave);
                    }
                }))
                .on_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, _| {
                    window.focus(&this.focus_handle);
                    let Some(button) = page_button(event.button) else {
                        match event.button {
                            MouseButton::Navigate(gpui::NavigationDirection::Back) => this.go_back(),
                            MouseButton::Navigate(gpui::NavigationDirection::Forward) => this.go_forward(),
                            _ => {}
                        }
                        return;
                    };
                    let (x, y) = this.page_point(event.position);
                    this.send_mouse(MouseInput::Down {
                        x,
                        y,
                        button,
                        click_count: event.click_count as u32,
                        modifiers: page_modifiers(&event.modifiers),
                    });
                }))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
                .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, _| {
                    let (x, y) = this.page_point(event.position);
                    let (delta_x, delta_y) = match event.delta {
                        ScrollDelta::Pixels(delta) => (f32::from(delta.x), f32::from(delta.y)),
                        ScrollDelta::Lines(delta) => (delta.x * 40.0, delta.y * 40.0),
                    };
                    this.send_mouse(MouseInput::Wheel {
                        x,
                        y,
                        delta_x,
                        delta_y,
                        modifiers: page_modifiers(&event.modifiers),
                    });
                }))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, _| {
                    let input = key_input(&event.keystroke);
                    if cockpit_browser::input::waits_for_text_input(&input) {
                        // The input method decides next whether this is text
                        // (replace_text_in_range) or composition.
                        this.pending_key = Some(input);
                    } else if let Some(tab) = &this.tab {
                        tab.send_key_down(&input);
                    }
                }))
                .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, _| {
                    if let Some(tab) = &this.tab {
                        tab.send_key_up(&key_input(&event.keystroke));
                    }
                }))
                .child(BrowserSurface {
                    view: cx.entity(),
                    page: self.page_image.as_ref().map(|(_, image)| image.clone()),
                    popup: self.popup_image.as_ref().map(|(_, image, rect)| (image.clone(), *rect)),
                    tab: self.tab.clone(),
                    bounds: self.bounds.clone(),
                    cursor: cursor_style(self.state.cursor),
                    focus_handle: self.focus_handle.clone(),
                })
                .when_some(self.state.load_error.clone(), |el, error| {
                    el.child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .px_3()
                            .py_2()
                            .text_sm()
                            .bg(cx.theme().danger)
                            .text_color(cx.theme().danger_foreground)
                            .child(SharedString::from(format!("Could not load the page: {error}"))),
                    )
                })
                .into_any_element(),
        };

        v_flex().size_full().child(self.render_toolbar(cx)).child(page)
    }
}

impl BrowserView {
    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        let Some(button) = page_button(event.button) else { return };
        let (x, y) = self.page_point(event.position);
        self.send_mouse(MouseInput::Up {
            x,
            y,
            button,
            click_count: event.click_count as u32,
            modifiers: page_modifiers(&event.modifiers),
        });
    }
}

/// Draws the latest frame and reports the laid-out rect back to Chromium.
struct BrowserSurface {
    view: Entity<BrowserView>,
    page: Option<Arc<RenderImage>>,
    popup: Option<(Arc<RenderImage>, PageRect)>,
    tab: Option<Rc<dyn BrowserTab>>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    cursor: CursorStyle,
    focus_handle: FocusHandle,
}

impl IntoElement for BrowserSurface {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for BrowserSurface {
    type RequestLayoutState = AnyElement;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();

        let mut child = div()
            .relative()
            .size_full()
            .when_some(self.page.clone(), |el, image| {
                el.child(img(ImageSource::Render(image)).object_fit(ObjectFit::Fill).size_full())
            })
            .when_some(self.popup.clone(), |el, (image, rect)| {
                el.child(
                    div()
                        .absolute()
                        .left(px(rect.x))
                        .top(px(rect.y))
                        .w(px(rect.width))
                        .h(px(rect.height))
                        .child(img(ImageSource::Render(image)).object_fit(ObjectFit::Fill).size_full()),
                )
            })
            .into_any_element();
        let child_layout = child.request_layout(window, cx);
        (window.request_layout(style, [child_layout], cx), child)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.bounds.set(bounds);
        if let Some(tab) = &self.tab {
            tab.set_geometry(ViewGeometry {
                width: f32::from(bounds.size.width),
                height: f32::from(bounds.size.height),
                scale_factor: window.scale_factor(),
            });
        }
        child.prepaint(window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut AnyElement,
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        child.paint(window, cx);
        // Off-screen Chromium never sets the real cursor; apply its choice.
        window.set_cursor_style(self.cursor, hitbox);
        // Makes the page the OS text-input client while focused, which is
        // what lets input methods compose into it.
        window.handle_input(
            &self.focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
    }
}

// ---------------------------------------------------------------------------
// Text input and IME
//
// The OS input method only ever asks about the text it is composing, so a
// mirror of that composition is enough; the page owns the real text. All
// ranges are UTF-16 code units, in GPUI and in CEF alike.
// ---------------------------------------------------------------------------

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_slice(text: &str, range: Range<usize>) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = range.end.min(units.len());
    let start = range.start.min(end);
    String::from_utf16_lossy(&units[start..end])
}

impl EntityInputHandler for BrowserView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let length = utf16_len(&self.composition);
        let clamped = range.start.min(length)..range.end.min(length);
        if clamped != range {
            *adjusted_range = Some(clamped.clone());
        }
        Some(utf16_slice(&self.composition, clamped))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // An empty selection at the end of the composition; returning None
        // would make macOS treat the page as not accepting text.
        let caret = utf16_len(&self.composition);
        Some(UTF16Selection {
            range: caret..caret,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        let length = utf16_len(&self.composition);
        (length > 0).then_some(0..length)
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composition.clear();
        self.pending_key = None;
        cx.defer_in(window, |this, _, _| {
            if let Some(tab) = &this.tab {
                tab.finish_composition();
            }
        });
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_composing = !self.composition.is_empty();
        self.composition.clear();
        let pending_key = if was_composing { None } else { self.pending_key.take() };
        self.pending_key = None;
        let text = text.to_string();
        // Defer: AppKit is mid-callback, and Chromium may call back into us.
        cx.defer_in(window, move |this, _, _| {
            let Some(tab) = &this.tab else { return };
            if was_composing {
                tab.commit_composition(&text);
            } else {
                tab.insert_text(&text, pending_key.as_ref());
            }
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_key = None;
        if new_text.is_empty() {
            // An empty marked string cancels the composition.
            self.composition.clear();
            cx.defer_in(window, |this, _, _| {
                if let Some(tab) = &this.tab {
                    tab.cancel_composition();
                }
            });
            return;
        }
        self.composition = new_text.to_string();
        let text = new_text.to_string();
        cx.defer_in(window, move |this, _, _| {
            if let Some(tab) = &this.tab {
                tab.set_composition(&text, new_selected_range);
            }
        });
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let rect = self.tab.as_ref()?.composition_bounds(range_utf16)?;
        Some(Bounds {
            origin: element_bounds.origin + gpui::point(px(rect.x), px(rect.y)),
            size: gpui::size(px(rect.width), px(rect.height)),
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_helpers_match_what_the_os_counts() {
        assert_eq!(utf16_len("にほんご"), 4);
        assert_eq!(utf16_len("\u{1F600}"), 2);
        assert_eq!(utf16_slice("にほんご", 1..3), "ほん");
        assert_eq!(utf16_slice("abc", 1..99), "bc");
    }
}

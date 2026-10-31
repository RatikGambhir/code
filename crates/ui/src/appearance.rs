//! The appearance popover: light or dark, the accent colour and how much
//! the window frame takes it. Changes apply live and are saved.

use gpui::{
    div, hsla, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Hsla, Subscription, Task, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Escape, Input, InputEvent, InputState},
    slider::{Slider, SliderEvent, SliderState},
    v_flex, Sizable as _,
};

use crate::{
    chrome::{caps_label, tooltip},
    state::{AppState, Settings, ThemePreference},
    theme::{self, tokens, PRESETS, RADIUS, RADIUS_LG, TEXT_BODY, TEXT_SMALL, TEXT_TITLE},
};

pub enum AppearanceEvent {
    Dismissed,
}

pub struct AppearancePanel {
    hue: Entity<SliderState>,
    tint: Entity<SliderState>,
    hex: Entity<InputState>,
    focus_handle: FocusHandle,
    save: Option<Task<()>>,
    /// Set while the panel updates its own controls, so their change events
    /// are not taken as the user's.
    syncing: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AppearanceEvent> for AppearancePanel {}

impl Focusable for AppearancePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AppearancePanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let look = theme::current_look(Some(window), cx);
        let hue = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(359.)
                .step(1.)
                .default_value((look.accent.h * 360.).round())
        });
        let tint = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(1.)
                .default_value((look.tint * 100.).round())
        });
        let hex = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("#2f7a45")
                .default_value(theme::to_hex(look.accent))
        });

        let subscriptions = vec![
            cx.subscribe_in(&hue, window, |this, _, event: &SliderEvent, window, cx| {
                let SliderEvent::Change(value) = event;
                if this.syncing {
                    return;
                }
                let current = theme::current_look(Some(window), cx).accent;
                // A grey accent has no hue to turn; give it some colour.
                let accent = hsla(value.start() / 360., current.s.max(0.5), current.l.clamp(0.32, 0.5), 1.);
                this.set_accent(accent, false, true, window, cx);
            }),
            cx.subscribe_in(&tint, window, |this, _, event: &SliderEvent, window, cx| {
                let SliderEvent::Change(value) = event;
                if this.syncing {
                    return;
                }
                let tint = value.start() / 100.;
                this.update(window, cx, |s| s.tint = tint);
            }),
            cx.subscribe_in(&hex, window, |this, input, event: &InputEvent, window, cx| {
                if this.syncing || !matches!(event, InputEvent::Change) {
                    return;
                }
                let text = input.read(cx).value().trim().to_string();
                let text = if text.starts_with('#') { text } else { format!("#{text}") };
                if text.len() == 7 {
                    if let Some(color) = theme::parse_hex(&text) {
                        this.set_accent(color, true, false, window, cx);
                    }
                }
            }),
        ];

        // Sliders learn their width when first painted; draw once more so the
        // thumbs start where the values are.
        cx.on_next_frame(window, |_, _, cx| cx.notify());

        Self {
            hue,
            tint,
            hex,
            focus_handle: cx.focus_handle(),
            save: None,
            syncing: false,
            _subscriptions: subscriptions,
        }
    }

    /// Changes the settings in memory, repaints, and saves a moment later.
    fn update(&mut self, window: &mut Window, cx: &mut Context<Self>, change: impl FnOnce(&mut Settings)) {
        change(&mut cx.global_mut::<AppState>().settings);
        theme::apply(Some(window), cx);
        theme::save_settings_debounced(&mut self.save, cx);
        cx.notify();
    }

    fn set_accent(&mut self, color: Hsla, sync_hue: bool, sync_hex: bool, window: &mut Window, cx: &mut Context<Self>) {
        let hex = theme::to_hex(color);
        self.update(window, cx, |s| s.accent = hex.clone());
        self.syncing = true;
        if sync_hue && color.s > 0.05 {
            let hue = (color.h * 360.).round();
            self.hue.update(cx, |state, cx| state.set_value(hue, window, cx));
        }
        if sync_hex {
            self.hex.update(cx, |state, cx| state.set_value(hex, window, cx));
        }
        self.syncing = false;
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update(window, cx, |s| {
            s.theme = ThemePreference::System;
            s.tint = theme::DEFAULT_TINT;
        });
        let accent = theme::parse_hex(theme::DEFAULT_ACCENT).unwrap_or_default();
        self.set_accent(accent, true, true, window, cx);
        self.syncing = true;
        self.tint
            .update(cx, |state, cx| state.set_value(theme::DEFAULT_TINT * 100., window, cx));
        self.syncing = false;
    }

    fn render_mode(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx);
        let current = AppState::global(cx).settings.theme;
        let selected_bg = if t.dark { t.border_strong } else { t.surface };
        let options = [
            (ThemePreference::System, "System"),
            (ThemePreference::Light, "Light"),
            (ThemePreference::Dark, "Dark"),
        ];
        let mut row = h_flex().p(px(2.)).gap(px(2.)).rounded(px(8.)).bg(t.raised);
        for (pref, label) in options {
            let selected = current == pref;
            let fg = t.fg;
            row = row.child(
                div()
                    .id(label)
                    .flex_1()
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(RADIUS)
                    .text_size(TEXT_BODY)
                    .cursor_pointer()
                    .text_color(if selected { t.fg } else { t.fg_muted })
                    .when(selected, |el| el.bg(selected_bg).shadow_xs().font_weight(FontWeight::MEDIUM))
                    .when(!selected, |el| el.hover(move |s| s.text_color(fg)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.update(window, cx, |s| s.theme = pref);
                    })),
            );
        }
        row
    }

    fn render_swatches(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx);
        let current = theme::to_hex(theme::current_look(None, cx).accent);
        let ring = t.fg.alpha(0.85);
        let mut row = h_flex().justify_between();
        for &(name, hex) in PRESETS {
            let color = theme::preset_color(hex);
            let selected = theme::to_hex(color) == current;
            row = row.child(
                div()
                    .id(name)
                    .size(px(26.))
                    .p(px(2.))
                    .rounded_full()
                    .border_2()
                    .border_color(if selected { ring } else { hsla(0., 0., 0., 0.) })
                    .cursor_pointer()
                    .tooltip(tooltip(name))
                    .child(
                        div()
                            .size_full()
                            .rounded_full()
                            .bg(color)
                            .border_1()
                            .border_color(hsla(0., 0., 0., 0.08)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_accent(color, true, true, window, cx);
                    })),
            );
        }
        row
    }
}

impl Render for AppearancePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx).clone();
        let tint = AppState::global(cx).settings.tint;
        let section = |label: &'static str, cx: &App| v_flex().gap(px(8.)).child(caps_label(label, cx));
        let accent = theme::current_look(None, cx).accent;

        v_flex()
            .id("appearance")
            .key_context("Appearance")
            .track_focus(&self.focus_handle)
            .w(px(296.))
            .p(px(14.))
            .gap(px(16.))
            .rounded(RADIUS_LG)
            .border_1()
            .border_color(t.border)
            .bg(if t.dark { t.raised } else { t.surface })
            .text_color(t.fg)
            .shadow_lg()
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.stop_propagation();
                cx.emit(AppearanceEvent::Dismissed);
            }))
            .child(
                h_flex()
                    .child(
                        div()
                            .flex_1()
                            .text_size(TEXT_TITLE)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Appearance"),
                    )
                    .child(
                        Button::new("appearance-reset")
                            .label("Reset")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, window, cx| this.reset(window, cx))),
                    ),
            )
            .child(section("Mode", cx).child(self.render_mode(cx)))
            .child(
                section("Accent", cx)
                    .child(self.render_swatches(cx))
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .child(div().w(px(36.)).text_size(TEXT_SMALL).text_color(t.fg_muted).child("Hue"))
                            .child(div().flex_1().child(Slider::new(&self.hue))),
                    )
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .child(div().w(px(36.)).text_size(TEXT_SMALL).text_color(t.fg_muted).child("Hex"))
                            .child(
                                h_flex()
                                    .flex_1()
                                    .h(px(28.))
                                    .pl(px(8.))
                                    .gap(px(6.))
                                    .rounded(RADIUS)
                                    .border_1()
                                    .border_color(t.border)
                                    .child(div().size(px(12.)).rounded(px(3.)).bg(accent))
                                    .child(
                                        div()
                                            .flex_1()
                                            .font_family(t.mono.clone())
                                            .child(Input::new(&self.hex).appearance(false).xsmall()),
                                    ),
                            ),
                    ),
            )
            .child(
                section("Frame tint", cx)
                    .child(Slider::new(&self.tint))
                    .child(
                        h_flex()
                            .justify_between()
                            .text_size(TEXT_SMALL)
                            .text_color(t.fg_subtle)
                            .child("Neutral")
                            .child(format!("{}%", (tint * 100.).round()))
                            .child("Vivid"),
                    ),
            )
    }
}

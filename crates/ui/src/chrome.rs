//! Small pieces of interface shared by every view, so labels, chips and tabs
//! look the same wherever they appear.

use gpui::{div, prelude::*, px, App, Div, ElementId, FontWeight, Hsla, SharedString, Stateful};
use gpui_component::{h_flex, Icon};

use crate::theme::{tokens, RADIUS, TEXT_CAPTION, TEXT_SMALL};

/// An uppercase section label: "EXPLORER", "PLAN", "STAGED CHANGES".
pub fn caps_label(text: impl Into<SharedString>, cx: &App) -> Div {
    let text: SharedString = text.into();
    div()
        .text_size(TEXT_CAPTION)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(tokens(cx).fg_subtle)
        .child(SharedString::from(text.to_uppercase()))
}

/// The header row of a sidebar view: a label and room for actions.
pub fn panel_header(label: &'static str, cx: &App) -> Div {
    h_flex()
        .h(px(36.))
        .flex_none()
        .pl(px(14.))
        .pr(px(8.))
        .gap_1()
        .child(caps_label(label, cx).flex_1())
}

/// A keyboard shortcut drawn as a small key cap.
pub fn key_chip(keys: impl Into<SharedString>, cx: &App) -> Div {
    let t = tokens(cx);
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(4.))
        .border_1()
        .border_color(t.border)
        .bg(t.raised)
        .font_family(t.mono.clone())
        .text_size(px(10.5))
        .text_color(t.fg_muted)
        .child(keys.into())
}

/// Hangar's mark: a 2×2 grid of rounded tiles in the accent.
pub fn logo_mark(size: f32, cx: &App) -> Div {
    let accent = tokens(cx).accent;
    let tile = size * 0.42;
    let gap = size - tile * 2.;
    let cell = |strong: bool| {
        div()
            .size(px(tile))
            .rounded(px(size * 0.12))
            .bg(if strong { accent } else { accent.alpha(0.5) })
    };
    div()
        .flex_none()
        .size(px(size))
        .flex()
        .flex_col()
        .gap(px(gap))
        .child(h_flex().gap(px(gap)).child(cell(true)).child(cell(false)))
        .child(h_flex().gap(px(gap)).child(cell(false)).child(cell(true)))
}

/// A small filled circle, for live status.
pub fn status_dot(color: Hsla) -> Div {
    div().flex_none().size(px(6.)).rounded_full().bg(color)
}

/// A tab drawn as text with an accent underline when active (editor tabs,
/// terminal tabs).
pub fn underline_tab(id: impl Into<ElementId>, active: bool, cx: &App) -> Stateful<Div> {
    let t = tokens(cx);
    let (fg, muted, accent) = (t.fg, t.fg_muted, t.accent);
    h_flex()
        .id(id)
        .relative()
        .h_full()
        .flex_none()
        .px(px(10.))
        .gap(px(6.))
        .cursor_pointer()
        .text_color(if active { fg } else { muted })
        .hover(move |s| s.text_color(fg))
        .when(active, |el| {
            el.child(
                div()
                    .absolute()
                    .bottom_0()
                    .left(px(8.))
                    .right(px(8.))
                    .h(px(2.))
                    .rounded_full()
                    .bg(accent),
            )
        })
}

/// A square icon button for the activity rail and the title bar, which sit
/// on the tinted frame where gpui-component's ghost buttons read poorly.
pub fn frame_button(id: impl Into<ElementId>, icon: impl Into<Icon>, selected: bool, cx: &App) -> Stateful<Div> {
    let t = tokens(cx);
    let (hover, pressed) = (t.frame_hover, t.pressed);
    div()
        .id(id)
        .flex_none()
        .size(px(30.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .cursor_pointer()
        .text_color(if selected { t.accent_text } else { t.frame_muted })
        .when(selected, |el| el.bg(t.accent_soft))
        .when(!selected, |el| el.hover(move |s| s.bg(hover)))
        .active(move |s| s.bg(pressed))
        .child(icon.into().size(px(16.)))
}

/// A tooltip builder for plain elements.
pub fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut gpui::Window, &mut App) -> gpui::AnyView + 'static {
    let text: SharedString = text.into();
    move |window, cx| gpui_component::tooltip::Tooltip::new(text.clone()).build(window, cx)
}

/// A row in a sidebar list: inset, rounded, with hover and selected states.
pub fn list_row(id: impl Into<ElementId>, selected: bool, cx: &App) -> Stateful<Div> {
    let t = tokens(cx);
    let hover = t.hover;
    h_flex()
        .id(id)
        .w_full()
        .h(px(26.))
        .px(px(8.))
        .gap(px(6.))
        .rounded(RADIUS)
        .cursor_pointer()
        .when(selected, |el| el.bg(t.accent_soft))
        .when(!selected, |el| el.hover(move |s| s.bg(hover)))
}

/// A git status letter in its colour.
pub fn status_badge(letter: impl Into<SharedString>, color: Hsla, cx: &App) -> Div {
    div()
        .flex_none()
        .w(px(12.))
        .text_size(TEXT_SMALL)
        .font_family(tokens(cx).mono.clone())
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(letter.into())
}

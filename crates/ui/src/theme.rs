//! Cockpit's look.
//!
//! Everything is derived from three choices the user makes: an accent colour,
//! how strongly the window frame is tinted by it, and light or dark. The
//! result is a small set of tokens (`Tokens`) the views read, which is also
//! written into gpui-component's theme so its buttons, inputs, menus and
//! the code editor follow the same palette.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use cockpit_terminal::TermColor;
use gpui::{hsla, px, rgb, App, Global, Hsla, Pixels, Rgba, SharedString, Task, Window, WindowAppearance};
use gpui_component::{
    highlighter::{HighlightTheme, HighlightThemeStyle},
    ActiveTheme as _, Theme, ThemeMode,
};
use serde_json::json;

use crate::state::{AppState, ThemePreference};

// --- type scale ---------------------------------------------------------------

/// Uppercase section labels ("EXPLORER", "PLAN").
pub const TEXT_CAPTION: Pixels = px(10.5);
/// Secondary text: badges, metadata, status line.
pub const TEXT_SMALL: Pixels = px(11.5);
/// Most interface text.
pub const TEXT_BODY: Pixels = px(12.5);
/// Headings inside panels.
pub const TEXT_TITLE: Pixels = px(13.);
/// File names, commands and paths.
pub const TEXT_MONO: Pixels = px(12.);

/// Rounding of rows, chips and small controls.
pub const RADIUS: Pixels = px(6.);
/// Rounding of the content card, popovers and the palette.
pub const RADIUS_LG: Pixels = px(10.);

// --- presets -----------------------------------------------------------------

/// Accent colours offered in the appearance panel.
pub const PRESETS: &[(&str, u32)] = &[
    ("Forest", 0x2f7a45),
    ("Ocean", 0x2563eb),
    ("Iris", 0x6d5bd0),
    ("Rose", 0xc8416a),
    ("Ember", 0xd0532b),
    ("Amber", 0xb7791f),
    ("Lagoon", 0x0f8a8a),
    ("Graphite", 0x55585e),
];

pub const DEFAULT_ACCENT: &str = "#2f7a45";
pub const DEFAULT_TINT: f32 = 0.2;

// --- tokens ------------------------------------------------------------------

/// The palette every view draws with.
#[derive(Clone, Debug)]
pub struct Tokens {
    pub dark: bool,
    /// The window frame: title bar, activity rail and status line.
    pub frame: Hsla,
    pub frame_muted: Hsla,
    /// Hover wash for controls sitting on the frame.
    pub frame_hover: Hsla,
    /// The content card.
    pub surface: Hsla,
    /// Chips, message bubbles and code blocks inside the card.
    pub raised: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub fg: Hsla,
    pub fg_muted: Hsla,
    pub fg_subtle: Hsla,
    /// Fill of primary controls.
    pub accent: Hsla,
    /// Text and icons on `accent`.
    pub accent_fg: Hsla,
    /// Selected rows and active chips.
    pub accent_soft: Hsla,
    /// The accent where it is used as text: readable on `surface`.
    pub accent_text: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub mono: SharedString,
}

impl Global for Tokens {}

pub fn tokens(cx: &App) -> &Tokens {
    cx.global::<Tokens>()
}

/// What the user chose.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Look {
    pub accent: Hsla,
    /// 0 keeps the frame neutral, 1 floods it with the accent.
    pub tint: f32,
    pub dark: bool,
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0., 1.)
}

/// Relative luminance (sRGB), for picking text on a fill.
fn luminance(color: Hsla) -> f32 {
    let c: Rgba = color.into();
    let lin = |v: f32| if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG contrast ratio between two colours.
fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (luminance(a) + 0.05, luminance(b) + 0.05);
    la.max(lb) / la.min(lb)
}

/// White or near-black, whichever reads better on `fill`.
fn ink_on(fill: Hsla, dark_ink: Hsla) -> Hsla {
    let white = gpui::white();
    if contrast(white, fill) >= contrast(dark_ink, fill) {
        white
    } else {
        dark_ink
    }
}

impl Tokens {
    pub fn derive(look: Look, mono: SharedString) -> Self {
        let Hsla { h, s, l, .. } = look.accent;
        let t = look.tint.clamp(0., 1.);
        // Hue-tinted neutrals: a grey accent gives pure greys.
        let tone = |sat: f32, light: f32| hsla(h, (sat * s).clamp(0., 1.), light, 1.);
        let success_hue = 142. / 360.;

        if look.dark {
            let accent = hsla(h, s, l.clamp(0.5, 0.7), 1.);
            let accent_fg = ink_on(accent, tone(0.3, 0.08));
            Self {
                dark: true,
                frame: tone(lerp(0.12, 0.7, t), lerp(0.07, 0.18, t)),
                frame_muted: tone(0.08, 0.6),
                frame_hover: hsla(0., 0., 1., 0.07),
                surface: tone(lerp(0.06, 0.14, t), 0.105),
                raised: tone(lerp(0.07, 0.16, t), 0.15),
                border: tone(lerp(0.07, 0.16, t), 0.19),
                border_strong: tone(0.08, 0.27),
                fg: tone(0.06, 0.93),
                fg_muted: tone(0.06, 0.64),
                fg_subtle: tone(0.06, 0.47),
                accent,
                accent_fg,
                accent_soft: accent.alpha(0.16),
                accent_text: hsla(h, s, l.max(0.7), 1.),
                hover: hsla(0., 0., 1., 0.06),
                pressed: hsla(0., 0., 1., 0.1),
                success: hsla(success_hue, 0.45, 0.62, 1.),
                warning: hsla(38. / 360., 0.8, 0.62, 1.),
                danger: hsla(0., 0.72, 0.67, 1.),
                mono,
            }
        } else {
            let accent = hsla(h, s, l.clamp(0.28, 0.52), 1.);
            let accent_fg = ink_on(accent, tone(0.3, 0.1));
            Self {
                dark: false,
                frame: tone(lerp(0.05, 0.6, t), lerp(0.955, 0.87, t)),
                frame_muted: tone(0.08, 0.4),
                frame_hover: hsla(0., 0., 0., 0.055),
                surface: tone(lerp(0., 0.25, t), 0.997),
                raised: tone(lerp(0.08, 0.25, t), 0.962),
                border: tone(lerp(0.06, 0.2, t), 0.91),
                border_strong: tone(0.08, 0.84),
                fg: tone(0.12, 0.13),
                fg_muted: tone(0.06, 0.42),
                fg_subtle: tone(0.05, 0.6),
                accent,
                accent_fg,
                accent_soft: accent.alpha(0.11),
                accent_text: hsla(h, s, l.min(0.36), 1.),
                hover: hsla(0., 0., 0., 0.045),
                pressed: hsla(0., 0., 0., 0.08),
                success: hsla(success_hue, 0.52, 0.34, 1.),
                warning: hsla(36. / 360., 0.85, 0.4, 1.),
                danger: hsla(0., 0.65, 0.48, 1.),
                mono,
            }
        }
    }

    /// Background of a removed line in a diff.
    pub fn removed_bg(&self) -> Hsla {
        self.danger.alpha(if self.dark { 0.16 } else { 0.09 })
    }

    /// Background of an added line in a diff.
    pub fn added_bg(&self) -> Hsla {
        self.success.alpha(if self.dark { 0.16 } else { 0.1 })
    }
}

// --- parsing -------------------------------------------------------------------

/// Parses `#rrggbb` (or any form `Rgba` accepts).
pub fn parse_hex(text: &str) -> Option<Hsla> {
    Rgba::try_from(text.trim()).ok().map(Into::into)
}

pub fn to_hex(color: Hsla) -> String {
    let c: Rgba = color.into();
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

fn to_hex_alpha(color: Hsla) -> String {
    let c: Rgba = color.into();
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b), byte(c.a))
}

pub fn preset_color(hex: u32) -> Hsla {
    rgb(hex).into()
}

// --- applying -------------------------------------------------------------------

/// The first installed monospace face from a short list of good ones.
fn default_mono(cx: &App) -> &'static str {
    static MONO: OnceLock<&'static str> = OnceLock::new();
    MONO.get_or_init(|| {
        let installed = cx.text_system().all_font_names();
        ["JetBrains Mono", "Geist Mono", "SF Mono", "Menlo", "DejaVu Sans Mono", "Consolas"]
            .into_iter()
            .find(|name| installed.iter().any(|font| font == name))
            .unwrap_or("Menlo")
    })
}

fn is_dark(window: Option<&Window>, cx: &App) -> bool {
    match AppState::global(cx).settings.theme {
        ThemePreference::Light => false,
        ThemePreference::Dark => true,
        ThemePreference::System => {
            let appearance = window.map(|w| w.appearance()).unwrap_or_else(|| cx.window_appearance());
            matches!(appearance, WindowAppearance::Dark | WindowAppearance::VibrantDark)
        }
    }
}

/// The look the settings describe.
pub fn current_look(window: Option<&Window>, cx: &App) -> Look {
    let settings = &AppState::global(cx).settings;
    Look {
        accent: parse_hex(&settings.accent).unwrap_or_else(|| parse_hex(DEFAULT_ACCENT).unwrap()),
        tint: settings.tint,
        dark: is_dark(window, cx),
    }
}

/// Re-derives the palette from the settings and repaints every window.
pub fn apply(window: Option<&mut Window>, cx: &mut App) {
    let look = current_look(window.as_deref(), cx);
    let settings = &AppState::global(cx).settings;
    let mono: SharedString = settings
        .mono_font
        .clone()
        .filter(|f| !f.trim().is_empty())
        .unwrap_or_else(|| default_mono(cx).to_string())
        .into();
    let editor_size = settings.editor_font_size;

    Theme::change(if look.dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    let tokens = Tokens::derive(look, mono);
    install(&tokens, editor_size, cx);
    cx.set_global(tokens);
    cx.refresh_windows();
}

/// Writes the tokens into gpui-component's theme.
fn install(t: &Tokens, editor_font_size: f32, cx: &mut App) {
    let highlight = highlight_theme(t);
    let theme = Theme::global_mut(cx);
    let clear = hsla(0., 0., 0., 0.);

    theme.background = t.surface;
    theme.foreground = t.fg;
    theme.border = t.border;
    theme.muted = t.raised;
    // gpui-component uses this for placeholders and the editor gutter, which
    // want to sit further back than our own secondary text.
    theme.muted_foreground = hsla(t.fg_muted.h, t.fg_muted.s, 0.55, 1.);

    theme.primary = t.accent;
    theme.primary_hover = t.accent.blend(t.accent_fg.alpha(0.1));
    theme.primary_active = t.accent.blend(hsla(0., 0., 0., 0.12));
    theme.primary_foreground = t.accent_fg;

    theme.secondary = t.raised;
    theme.secondary_hover = t.hover;
    theme.secondary_active = t.pressed;
    theme.secondary_foreground = t.fg;

    theme.accent = t.raised;
    theme.accent_foreground = t.fg;

    theme.input = t.border_strong;
    theme.ring = t.accent.alpha(0.55);
    theme.caret = t.accent;
    theme.selection = t.accent.alpha(if t.dark { 0.3 } else { 0.2 });

    theme.list = clear;
    theme.list_even = clear;
    theme.list_head = t.surface;
    theme.list_hover = t.hover;
    theme.list_active = t.accent_soft;
    theme.list_active_border = t.accent.alpha(0.4);
    theme.table = clear;
    theme.table_even = clear;
    theme.table_hover = t.hover;
    theme.table_active = t.accent_soft;
    theme.table_active_border = t.accent.alpha(0.4);
    theme.table_head = t.surface;
    theme.table_head_foreground = t.fg_muted;
    theme.table_row_border = t.border;

    theme.popover = if t.dark { t.raised } else { t.surface };
    theme.popover_foreground = t.fg;
    theme.overlay = hsla(0., 0., 0., if t.dark { 0.45 } else { 0.18 });

    theme.sidebar = t.surface;
    theme.sidebar_foreground = t.fg;
    theme.sidebar_border = t.border;
    theme.sidebar_accent = t.accent_soft;
    theme.sidebar_accent_foreground = t.fg;
    theme.sidebar_primary = t.accent;
    theme.sidebar_primary_foreground = t.accent_fg;

    theme.tab_bar = clear;
    theme.tab = clear;
    theme.tab_active = clear;
    theme.tab_active_foreground = t.fg;
    theme.tab_foreground = t.fg_muted;
    theme.tab_bar_segmented = t.raised;

    theme.title_bar = t.frame;
    theme.title_bar_border = t.frame;
    theme.window_border = t.border;

    theme.link = t.accent_text;
    theme.link_hover = t.accent_text;
    theme.link_active = t.accent_text;

    theme.switch = t.border_strong;
    theme.switch_thumb = gpui::white();
    theme.slider_bar = t.accent;
    theme.slider_thumb = gpui::white();
    theme.progress_bar = t.accent;
    theme.skeleton = t.raised;

    theme.scrollbar = clear;
    theme.scrollbar_thumb = t.fg.alpha(0.16);
    theme.scrollbar_thumb_hover = t.fg.alpha(0.28);

    theme.drag_border = t.accent;
    theme.drop_target = t.accent.alpha(0.1);

    theme.success = t.success;
    theme.success_hover = t.success.blend(hsla(0., 0., 1., 0.1));
    theme.success_active = t.success.blend(hsla(0., 0., 0., 0.1));
    theme.success_foreground = gpui::white();
    theme.warning = t.warning;
    theme.warning_hover = t.warning.blend(hsla(0., 0., 1., 0.1));
    theme.warning_active = t.warning.blend(hsla(0., 0., 0., 0.1));
    theme.warning_foreground = gpui::white();
    theme.danger = t.danger;
    theme.danger_hover = t.danger.blend(hsla(0., 0., 1., 0.1));
    theme.danger_active = t.danger.blend(hsla(0., 0., 0., 0.1));
    theme.danger_foreground = gpui::white();
    theme.info = t.accent;
    theme.info_hover = theme.primary_hover;
    theme.info_active = theme.primary_active;
    theme.info_foreground = t.accent_fg;

    theme.radius = RADIUS;
    theme.radius_lg = RADIUS_LG;
    theme.shadow = true;
    theme.mono_font_family = t.mono.clone();
    theme.mono_font_size = px(editor_font_size);
    if let Some(highlight) = highlight {
        theme.highlight_theme = highlight;
    }
}

/// Syntax colours tuned to sit with any accent: they do not move with it.
fn highlight_theme(t: &Tokens) -> Option<Arc<HighlightTheme>> {
    let (keyword, function, ty, string, number, comment, tag, punctuation) = if t.dark {
        ("#c4a5ff", "#82aaff", "#6cd2df", "#b5d67a", "#f2a66b", "#6c736d", "#7fd3df", "#9aa19b")
    } else {
        ("#7b3fc4", "#2558c9", "#0b7285", "#557a1f", "#c2501a", "#8b918b", "#0b7285", "#5f6660")
    };
    let fg = to_hex(t.fg);
    let plain: &[(&str, &str)] = &[
        ("keyword", keyword),
        ("function", function),
        ("constructor", ty),
        ("type", ty),
        ("enum", ty),
        ("variant", ty),
        ("tag", tag),
        ("attribute", keyword),
        ("string", string),
        ("string.special", string),
        ("string.special.symbol", number),
        ("string.escape", number),
        ("string.regex", number),
        ("number", number),
        ("boolean", number),
        ("constant", number),
        ("punctuation", punctuation),
        ("punctuation.bracket", punctuation),
        ("punctuation.delimiter", punctuation),
        ("punctuation.special", keyword),
        ("punctuation.list_marker", number),
        ("operator", punctuation),
        ("property", &fg),
        ("variable", &fg),
        ("variable.special", keyword),
        ("label", function),
        ("link_text", function),
        ("link_uri", ty),
        ("text.literal", string),
        ("preproc", keyword),
        ("embedded", &fg),
    ];
    let mut syntax = serde_json::Map::new();
    for (name, color) in plain {
        syntax.insert((*name).into(), json!({ "color": color }));
    }
    syntax.insert("comment".into(), json!({ "color": comment, "font_style": "italic" }));
    syntax.insert("comment_doc".into(), json!({ "color": comment, "font_style": "italic" }));
    syntax.insert("title".into(), json!({ "color": fg, "font_weight": 600 }));
    syntax.insert("emphasis".into(), json!({ "font_style": "italic" }));
    syntax.insert("emphasis.strong".into(), json!({ "font_weight": 700 }));

    let mut style = serde_json::Map::new();
    let mut set = |key: &str, color: Hsla| {
        style.insert(key.into(), json!(to_hex_alpha(color)));
    };
    set("editor.background", t.surface);
    set("editor.foreground", t.fg);
    set("editor.active_line.background", t.fg.alpha(if t.dark { 0.04 } else { 0.03 }));
    set("editor.line_number", t.fg_subtle.alpha(0.8));
    set("editor.active_line_number", t.fg_muted);
    set("error", t.danger);
    set("warning", t.warning);
    set("info", t.accent_text);
    set("success", t.success);
    set("hint", t.fg_muted);
    style.insert("syntax".into(), serde_json::Value::Object(syntax));
    let style = serde_json::Value::Object(style);
    match serde_json::from_value::<HighlightThemeStyle>(style) {
        Ok(style) => Some(Arc::new(HighlightTheme {
            name: "Cockpit".into(),
            appearance: if t.dark { ThemeMode::Dark } else { ThemeMode::Light },
            style,
        })),
        Err(err) => {
            tracing::warn!("building the syntax theme: {err}");
            None
        }
    }
}

/// Re-applies the look when macOS switches between light and dark, if the
/// user follows the system.
pub fn follow_system_appearance<V: 'static>(window: &mut Window, cx: &mut gpui::Context<V>) -> gpui::Subscription {
    cx.observe_window_appearance(window, |_, window, cx| {
        if AppState::global(cx).settings.theme == ThemePreference::System {
            apply(Some(window), cx);
        }
    })
}

/// Saves settings shortly after the last change, so dragging a slider does
/// not write on every frame.
pub fn save_settings_debounced(slot: &mut Option<Task<()>>, cx: &mut App) {
    let settings = AppState::global(cx).settings.clone();
    *slot = Some(cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_millis(400)).await;
        cx.update(|cx| AppState::save_settings(cx, |s| *s = settings)).ok();
    }));
}

// --- status colours ---------------------------------------------------------------

pub fn added(cx: &App) -> Hsla {
    tokens(cx).success
}

/// Modified files carry the accent, like the rest of "what changed".
pub fn modified(cx: &App) -> Hsla {
    tokens(cx).accent_text
}

pub fn deleted(cx: &App) -> Hsla {
    tokens(cx).danger
}

// --- terminal -------------------------------------------------------------------

/// The 16 ANSI colors, tuned per mode for contrast.
fn ansi(index: u8, dark: bool) -> Hsla {
    const DARK: [u32; 16] = [
        0x2a2d2b, 0xf07a7f, 0x9fd27f, 0xe9c37a, 0x7fb0f5, 0xc79bf0, 0x6fcfd9, 0xd9dcd9,
        0x6c736d, 0xff8f94, 0xb8e79a, 0xffd690, 0x9cc4ff, 0xdab3ff, 0x8fe0e8, 0xffffff,
    ];
    const LIGHT: [u32; 16] = [
        0x1f2320, 0xc4262e, 0x2f7a3a, 0x8a6100, 0x2558c9, 0x7b3fc4, 0x0b7285, 0x6b716c,
        0x5a605b, 0xe0393f, 0x3c9a4a, 0xae7c00, 0x3a73e0, 0x9356db, 0x0f8fa6, 0x111311,
    ];
    let table = if dark { DARK } else { LIGHT };
    rgb(table[index as usize & 15]).into()
}

/// Maps a terminal color onto the theme. `foreground` selects which default
/// applies to `TermColor::Default`.
pub fn terminal_color(color: TermColor, foreground: bool, cx: &App) -> Option<Hsla> {
    let dark = cx.theme().is_dark();
    match color {
        TermColor::Default => foreground.then(|| cx.theme().foreground),
        TermColor::Indexed(i) if i < 16 => Some(ansi(i, dark)),
        TermColor::Indexed(i) if i < 232 => {
            // 6×6×6 color cube.
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v as u32 * 40 };
            let (r, g, b) = (level(i / 36), level((i / 6) % 6), level(i % 6));
            Some(rgb((r << 16) | (g << 8) | b).into())
        }
        TermColor::Indexed(i) => {
            let v = 8 + (i as u32 - 232) * 10;
            Some(rgb((v << 16) | (v << 8) | v).into())
        }
        TermColor::Rgb(r, g, b) => Some(rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(hex: u32, tint: f32, dark: bool) -> Look {
        Look { accent: preset_color(hex), tint, dark }
    }

    #[test]
    fn hex_round_trips() {
        let color = parse_hex("#2f7a45").unwrap();
        assert_eq!(to_hex(color), "#2f7a45");
        assert!(parse_hex("not a color").is_none());
    }

    #[test]
    fn text_stays_readable_for_every_preset_and_tint() {
        for &(name, hex) in PRESETS {
            for tint in [0.0, 0.5, 1.0] {
                for dark in [false, true] {
                    let t = Tokens::derive(look(hex, tint, dark), "Menlo".into());
                    assert!(contrast(t.fg, t.surface) > 12.0, "{name} fg/surface");
                    // The frame holds the breadcrumb and status line: WCAG AAA.
                    assert!(contrast(t.fg, t.frame) > 7.0, "{name} fg/frame tint {tint}");
                    assert!(contrast(t.fg_muted, t.surface) > 4.5, "{name} muted");
                    assert!(contrast(t.accent_fg, t.accent) > 3.0, "{name} on accent dark={dark}");
                    assert!(contrast(t.accent_text, t.surface) > 4.0, "{name} accent text dark={dark}");
                }
            }
        }
    }

    #[test]
    fn syntax_theme_builds() {
        for dark in [false, true] {
            let t = Tokens::derive(look(0x2f7a45, 0.2, dark), "Menlo".into());
            assert!(highlight_theme(&t).is_some());
        }
    }
}

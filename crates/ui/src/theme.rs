//! Theme setup and colors that gpui-component's theme does not cover.

use cockpit_terminal::TermColor;
use gpui::{hsla, rgb, App, Hsla, Window};
use gpui_component::{ActiveTheme as _, Theme, ThemeMode};

use crate::state::{AppState, ThemePreference};

pub fn apply(window: Option<&mut Window>, cx: &mut App) {
    match AppState::global(cx).settings.theme {
        ThemePreference::System => Theme::sync_system_appearance(window, cx),
        ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
}

/// Status colors that read on both light and dark backgrounds.
pub fn added(cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        rgb(0x7ec27e).into()
    } else {
        rgb(0x2f7d32).into()
    }
}

pub fn modified(cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        rgb(0xe2b464).into()
    } else {
        rgb(0x9a6700).into()
    }
}

pub fn deleted(cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        rgb(0xe57373).into()
    } else {
        rgb(0xc62828).into()
    }
}

/// The 16 ANSI colors, tuned per mode for contrast.
fn ansi(index: u8, dark: bool) -> Hsla {
    const DARK: [u32; 16] = [
        0x1e1e1e, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xd4d4d4,
        0x5c6370, 0xff7b85, 0xb5e890, 0xffd68a, 0x82c4ff, 0xdc95ee, 0x7fd3df, 0xffffff,
    ];
    const LIGHT: [u32; 16] = [
        0x000000, 0xc4262e, 0x2e7d32, 0x8a6100, 0x1565c0, 0x8e24aa, 0x00838f, 0x6b6b6b,
        0x555555, 0xe53935, 0x43a047, 0xb38600, 0x1e88e5, 0xab47bc, 0x00acc1, 0x1a1a1a,
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

/// A translucent overlay for hover states independent of the theme.
pub fn hover_overlay(cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        hsla(0., 0., 1., 0.06)
    } else {
        hsla(0., 0., 0., 0.05)
    }
}

//! Small pieces drawn the way GitHub draws them: pull request and run state
//! marks, label pills, counts and relative times.

use cockpit_github::{PullStatus, RunState};
use gpui::{div, prelude::*, px, AnyElement, Div, Hsla, Pixels};
use gpui_component::{h_flex, Icon, IconName};

use crate::{
    assets::icon,
    theme::{parse_hex, Tokens, RADIUS, TEXT_CAPTION, TEXT_SMALL},
};

/// GitHub's merged purple; the theme has no purple of its own.
fn merged(t: &Tokens) -> Hsla {
    parse_hex(if t.dark { "#a371f7" } else { "#8250df" }).unwrap_or(t.accent)
}

pub fn pull_color(status: PullStatus, t: &Tokens) -> Hsla {
    match status {
        PullStatus::Open => t.success,
        PullStatus::Draft => t.fg_muted,
        PullStatus::Merged => merged(t),
        PullStatus::Closed => t.danger,
    }
}

pub fn pull_icon(status: PullStatus, size: Pixels, t: &Tokens) -> Icon {
    icon(match status {
        PullStatus::Open => "git-pull-request",
        PullStatus::Draft => "git-pull-request-draft",
        PullStatus::Merged => "git-merge",
        PullStatus::Closed => "git-pull-request-closed",
    })
    .size(size)
    .text_color(pull_color(status, t))
}

pub fn issue_color(open: bool, t: &Tokens) -> Hsla {
    if open {
        t.success
    } else {
        merged(t)
    }
}

pub fn issue_icon(open: bool, size: Pixels, t: &Tokens) -> Icon {
    let mark = if open {
        icon("circle-dot")
    } else {
        Icon::new(IconName::CircleCheck)
    };
    mark.size(size).text_color(issue_color(open, t))
}

pub fn run_color(state: RunState, t: &Tokens) -> Hsla {
    match state {
        RunState::Success => t.success,
        RunState::Failure => t.danger,
        RunState::InProgress | RunState::Queued => t.warning,
        RunState::Cancelled | RunState::Skipped | RunState::Neutral => t.fg_subtle,
    }
}

pub fn run_icon(state: RunState, size: Pixels, t: &Tokens) -> Icon {
    let mark = match state {
        RunState::Success => Icon::new(IconName::CircleCheck),
        RunState::Failure => Icon::new(IconName::CircleX),
        RunState::InProgress => Icon::new(IconName::LoaderCircle),
        RunState::Queued => icon("circle-dot"),
        RunState::Cancelled => icon("circle-stop"),
        RunState::Skipped | RunState::Neutral => icon("circle-slash"),
    };
    mark.size(size).text_color(run_color(state, t))
}

/// A rounded pill with a coloured mark and text: a state ("Open") or a label.
pub fn pill(text: impl Into<gpui::SharedString>, color: Hsla, t: &Tokens) -> Div {
    h_flex()
        .flex_none()
        .h(px(20.))
        .px(px(8.))
        .gap(px(5.))
        .rounded_full()
        .border_1()
        .border_color(color.alpha(0.45))
        .bg(color.alpha(0.12))
        .text_size(TEXT_CAPTION)
        .text_color(t.fg)
        .child(text.into())
}

/// A GitHub label in its colour (GitHub sends hex without the `#`).
pub fn label(name: &str, color: Option<&str>, t: &Tokens) -> AnyElement {
    let color = color
        .and_then(|c| parse_hex(&format!("#{}", c.trim_start_matches('#'))))
        .unwrap_or(t.fg_muted);
    pill(name.to_string(), color, t).into_any_element()
}

/// A small count chip, as on GitHub's tabs.
pub fn count(n: usize, t: &Tokens) -> Div {
    div()
        .flex_none()
        .px(px(6.))
        .rounded_full()
        .bg(t.hover)
        .text_size(TEXT_CAPTION)
        .text_color(t.fg_muted)
        .child(if n >= 50 { "50+".to_string() } else { n.to_string() })
}

/// A plain bordered chip, for "Private", "Archived" and branch names.
pub fn chip(text: impl Into<gpui::SharedString>, t: &Tokens) -> Div {
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded(RADIUS)
        .border_1()
        .border_color(t.border)
        .text_size(TEXT_CAPTION)
        .text_color(t.fg_muted)
        .child(text.into())
}

/// A branch name in monospace on a soft accent.
pub fn branch(name: &str, t: &Tokens) -> Div {
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded(RADIUS)
        .bg(t.accent_soft)
        .font_family(t.mono.clone())
        .text_size(TEXT_SMALL)
        .text_color(t.accent_text)
        .child(name.to_string())
}

/// "just now", "5m ago", "3h ago", "4d ago", then a date.
pub fn ago(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let now = chrono::Utc::now().timestamp();
    let secs = (now - ts).max(0);
    match secs {
        0..60 => "just now".into(),
        60..3_600 => format!("{}m ago", secs / 60),
        3_600..86_400 => format!("{}h ago", secs / 3_600),
        86_400..2_592_000 => format!("{}d ago", secs / 86_400),
        _ => crate::tasks::short_date(ts),
    }
}

/// `1.2 KB`, `3.4 MB`.
pub fn size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{:.1} MB", b / KB / KB)
    }
}

/// `12k`, `1.2k`, `999`.
pub fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1000.),
        _ => format!("{}k", n / 1000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_short() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(2048), "2.0 KB");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1234), "1.2k");
        assert_eq!(compact(45_000), "45k");
        assert_eq!(ago(0), "");
        assert_eq!(ago(chrono::Utc::now().timestamp() - 7200), "2h ago");
    }
}

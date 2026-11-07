//! Small pieces drawn the way Linear draws them: status circles, priority
//! bars, initials avatars, project marks and short dates.

use chrono::{Datelike as _, Local, TimeZone as _};
use cockpit_tasks::{Priority, ProjectState, Status, StatusKind};
use gpui::{div, hsla, prelude::*, px, Div, Hsla, Pixels};
use gpui_component::Icon;

use crate::{
    assets::icon,
    theme::{parse_hex, Tokens},
};

/// Linear's default status colours, used when a status has none.
fn default_status_color(kind: StatusKind, t: &Tokens) -> Hsla {
    match kind {
        StatusKind::Triage => parse_hex("#fc7840").unwrap_or(t.warning),
        StatusKind::Backlog | StatusKind::Canceled => t.fg_subtle,
        StatusKind::Unstarted => t.fg_muted,
        StatusKind::Started => parse_hex("#f0bf00").unwrap_or(t.warning),
        StatusKind::Completed => parse_hex("#5e6ad2").unwrap_or(t.accent),
    }
}

pub fn status_color(status: &Status, t: &Tokens) -> Hsla {
    status
        .color
        .as_deref()
        .and_then(parse_hex)
        .unwrap_or_else(|| default_status_color(status.kind, t))
}

pub fn status_icon(status: &Status, size: Pixels, t: &Tokens) -> Icon {
    kind_icon(status.kind)
        .size(size)
        .text_color(status_color(status, t))
}

/// The mark for a status category, uncoloured.
pub fn kind_icon(kind: StatusKind) -> Icon {
    icon(match kind {
        StatusKind::Triage => "status-triage",
        StatusKind::Backlog => "status-backlog",
        StatusKind::Unstarted => "status-todo",
        StatusKind::Started => "status-started",
        StatusKind::Completed => "status-done",
        StatusKind::Canceled => "status-canceled",
    })
}

pub fn kind_color(kind: StatusKind, t: &Tokens) -> Hsla {
    default_status_color(kind, t)
}

pub fn priority_icon(priority: Priority, size: Pixels, t: &Tokens) -> Icon {
    let (name, color) = match priority {
        Priority::None => ("priority-none", t.fg_subtle),
        Priority::Low => ("priority-low", t.fg_muted),
        Priority::Medium => ("priority-medium", t.fg_muted),
        Priority::High => ("priority-high", t.fg_muted),
        Priority::Urgent => ("priority-urgent", parse_hex("#f2994a").unwrap_or(t.warning)),
    };
    icon(name).size(size).text_color(color)
}

pub fn project_color(color: Option<&str>, t: &Tokens) -> Hsla {
    color.and_then(parse_hex).unwrap_or(t.fg_muted)
}

pub fn project_icon(color: Option<&str>, size: Pixels, t: &Tokens) -> Icon {
    icon("project")
        .size(size)
        .text_color(project_color(color, t))
}

pub fn project_state_color(state: ProjectState, t: &Tokens) -> Hsla {
    match state {
        ProjectState::Started => parse_hex("#f0bf00").unwrap_or(t.warning),
        ProjectState::Completed => parse_hex("#5e6ad2").unwrap_or(t.accent),
        ProjectState::Paused => t.fg_muted,
        ProjectState::Canceled | ProjectState::Backlog => t.fg_subtle,
        ProjectState::Planned => t.fg_muted,
    }
}

/// A round avatar with initials, coloured by the name so each person keeps
/// one colour. Unassigned is a dashed outline, as in Linear.
pub fn avatar(name: Option<&str>, size: f32, t: &Tokens) -> Div {
    let base = div()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center();
    let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
        return base.border_1().border_dashed().border_color(t.fg_subtle);
    };
    let initials: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    let hue = name
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)))
        % 360;
    base.bg(hsla(
        hue as f32 / 360.,
        0.45,
        if t.dark { 0.42 } else { 0.55 },
        1.,
    ))
    .text_color(gpui::white())
    .text_size(px(size * 0.42))
    .font_weight(gpui::FontWeight::SEMIBOLD)
    .child(initials)
}

/// `Nov 3`, or `Nov 3, 2025` outside the current year.
pub fn short_date(ts: i64) -> String {
    let Some(date) = Local.timestamp_opt(ts, 0).single() else {
        return String::new();
    };
    if ts <= 0 {
        String::new()
    } else if date.year() == Local::now().year() {
        date.format("%b %-d").to_string()
    } else {
        date.format("%b %-d, %Y").to_string()
    }
}

/// `2026-12-01` → `Dec 1`.
pub fn target_date(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| {
            if d.year() == Local::now().year() {
                d.format("%b %-d").to_string()
            } else {
                d.format("%b %-d, %Y").to_string()
            }
        })
        .unwrap_or_else(|_| date.to_string())
}

/// A thin progress bar with its percentage.
pub fn progress(value: f32, color: Hsla, t: &Tokens) -> Div {
    let value = value.clamp(0.0, 1.0);
    gpui_component::h_flex()
        .gap(px(6.))
        .child(
            div()
                .w(px(48.))
                .h(px(4.))
                .rounded_full()
                .bg(t.border)
                .child(div().h_full().w(px(48. * value)).rounded_full().bg(color)),
        )
        .child(
            div()
                .w(px(30.))
                .text_size(crate::theme::TEXT_SMALL)
                .text_color(t.fg_muted)
                .child(format!("{}%", (value * 100.).round() as u32)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_dates_read_short() {
        let year = Local::now().year();
        assert_eq!(target_date(&format!("{year}-12-01")), "Dec 1");
        assert_eq!(target_date("2001-02-03"), "Feb 3, 2001");
        assert_eq!(target_date("soon"), "soon");
        assert_eq!(short_date(0), "");
    }
}

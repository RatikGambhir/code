//! cockpit patch: decorations an embedder draws in a code editor's gutter and
//! lines (breakpoints, run icons, the paused line, inline values), and the
//! clicks it receives in the gutter.
//!
//! Rows are zero-based buffer lines. Everything here is opt-in: an editor
//! without decorations lays out and paints exactly as upstream.

use std::collections::BTreeMap;

use gpui::{
    App, Bounds, Context, EventEmitter, Hsla, Modifiers, MouseButton, PathBuilder, Pixels, Point,
    SharedString, TextRun, Window, fill, point, px, size,
};

use super::{InputState, LastLayout, element::LINE_NUMBER_RIGHT_MARGIN};

/// Width of the icon lane right of the line numbers, when enabled.
pub(super) const LANE_WIDTH: Pixels = px(16.);

/// What to draw over a line's number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GutterMarker {
    pub color: Hsla,
    pub shape: MarkerShape,
    /// Only the outline (a disabled breakpoint).
    pub hollow: bool,
    /// A small dot in the middle in the editor background colour (a
    /// conditional breakpoint).
    pub inner_dot: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerShape {
    Circle,
    Diamond,
}

/// An icon in the lane right of the line numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LaneIcon {
    /// A right-pointing triangle (run this).
    Run(Hsla),
    /// An arrow (execution is paused here).
    Arrow(Hsla),
}

/// Text drawn after the end of a line.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineHint {
    pub text: SharedString,
    pub color: Hsla,
    pub background: Option<Hsla>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorDecorations {
    /// Reserve the icon lane right of the line numbers.
    pub lane: bool,
    pub markers: BTreeMap<usize, GutterMarker>,
    pub lane_icons: BTreeMap<usize, LaneIcon>,
    /// Full-width backgrounds, painted under the text.
    pub line_backgrounds: BTreeMap<usize, Hsla>,
    pub inline_hints: BTreeMap<usize, InlineHint>,
    /// Drawn faintly on the hovered gutter row when it has no marker.
    pub hover_marker: Option<GutterMarker>,
}

/// A press in the gutter. The editor does not move its cursor for it.
#[derive(Clone, Debug)]
pub struct GutterEvent {
    pub row: usize,
    pub button: MouseButton,
    pub modifiers: Modifiers,
    /// Whether the press landed in the icon lane rather than on the number.
    pub in_lane: bool,
    /// Window coordinates of the press, for anchoring a menu.
    pub position: Point<Pixels>,
}

impl EventEmitter<GutterEvent> for InputState {}

impl InputState {
    pub fn decorations(&self) -> &EditorDecorations {
        &self.decorations
    }

    pub fn set_decorations(&mut self, decorations: EditorDecorations, cx: &mut Context<Self>) {
        if self.decorations != decorations {
            self.decorations = decorations;
            cx.notify();
        }
    }

    /// The first visible row and how many rows are visible, from the last
    /// paint.
    pub fn visible_rows(&self) -> Option<std::ops::Range<usize>> {
        self.last_layout.as_ref().map(|l| l.visible_range.clone())
    }

    /// The gutter row under `position`, and whether it is in the lane.
    pub(super) fn gutter_hit(&self, position: Point<Pixels>) -> Option<(usize, bool)> {
        if !self.mode.is_code_editor() || !self.mode.line_number() {
            return None;
        }
        let layout = self.last_layout.as_ref()?;
        let bounds = self.last_bounds.as_ref()?;
        let x = position.x - self.input_bounds.origin.x;
        let gutter_width = layout.line_number_width - LINE_NUMBER_RIGHT_MARGIN;
        if x < px(0.) || x > gutter_width {
            return None;
        }
        let in_lane = self.decorations.lane && x > gutter_width - LANE_WIDTH - px(2.);
        let mut y = bounds.origin.y + layout.visible_top;
        for (ix, line) in layout.lines.iter().enumerate() {
            let height = layout.line_height * line.wrapped_lines.len().max(1) as f32;
            if position.y >= y && position.y < y + height {
                return Some((layout.visible_range.start + ix, in_lane));
            }
            y += height;
        }
        None
    }

    /// Emits a [`GutterEvent`] for a press in the gutter. Returns whether it
    /// was one.
    pub(super) fn handle_gutter_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((row, in_lane)) = self.gutter_hit(event.position) else {
            return false;
        };
        cx.emit(GutterEvent {
            row,
            button: event.button,
            modifiers: event.modifiers,
            in_lane,
            position: event.position,
        });
        true
    }

    pub(super) fn update_gutter_hover(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let row = self.gutter_hit(position).map(|(row, _)| row);
        if row != self.gutter_hover_row {
            self.gutter_hover_row = row;
            cx.notify();
        }
    }
}

/// Painting, called from the text element.
pub(super) struct DecorationPainter<'a> {
    pub decorations: &'a EditorDecorations,
    pub layout: &'a LastLayout,
    /// The element's bounds (not scrolled).
    pub input_bounds: Bounds<Pixels>,
    /// Text origin (scrolled).
    pub origin: Point<Pixels>,
    pub hover_row: Option<usize>,
    /// Width of the right-aligned number text, and how many characters it
    /// is padded to.
    pub number_width: Pixels,
    pub number_chars: usize,
}

impl DecorationPainter<'_> {
    /// The marker drawn on `row`, if any (a set one, else the hover one).
    fn marker(&self, row: usize) -> Option<GutterMarker> {
        self.decorations
            .markers
            .get(&row)
            .copied()
            .or_else(|| (self.hover_row == Some(row)).then_some(self.decorations.hover_marker).flatten())
    }

    /// Whether `row`'s number is hidden under a marker.
    pub fn covers_number(&self, row: usize) -> bool {
        self.marker(row).is_some()
    }

    /// (row, top, height) of each visible row.
    fn rows(&self) -> impl Iterator<Item = (usize, Pixels, Pixels)> + '_ {
        let mut y = self.origin.y + self.layout.visible_top;
        self.layout.lines.iter().enumerate().map(move |(ix, line)| {
            let height = self.layout.line_height * line.wrapped_lines.len().max(1) as f32;
            let top = y;
            y += height;
            (self.layout.visible_range.start + ix, top, height)
        })
    }

    pub fn paint_backgrounds(&self, width: Pixels, window: &mut Window) {
        if self.decorations.line_backgrounds.is_empty() {
            return;
        }
        for (row, top, height) in self.rows() {
            if let Some(color) = self.decorations.line_backgrounds.get(&row) {
                window.paint_quad(fill(
                    Bounds::new(point(self.input_bounds.origin.x, top), size(width, height)),
                    *color,
                ));
            }
        }
    }

    /// Hints after the end of each line; `font_size` and `run` style them.
    pub fn paint_hints(&self, font_size: Pixels, run: &TextRun, window: &mut Window, cx: &mut App) {
        if self.decorations.inline_hints.is_empty() {
            return;
        }
        let line_height = self.layout.line_height;
        let rows: Vec<_> = self.rows().collect();
        for (ix, (row, top, height)) in rows.into_iter().enumerate() {
            let Some(hint) = self.decorations.inline_hints.get(&row) else { continue };
            let Some(line) = self.layout.lines.get(ix) else { continue };
            let last_width = line.wrapped_lines.last().map(|l| l.width).unwrap_or_default();
            let text = hint.text.clone();
            let shaped = window.text_system().shape_line(
                text.clone(),
                font_size,
                &[TextRun { len: text.len(), color: hint.color, ..run.clone() }],
                None,
            );
            let x = self.origin.x + self.layout.line_number_width + last_width + font_size * 2.;
            let y = top + height - line_height;
            if let Some(bg) = hint.background {
                window.paint_quad(
                    fill(
                        Bounds::new(
                            point(x - px(4.), y + px(2.)),
                            size(shaped.width + px(8.), line_height - px(4.)),
                        ),
                        bg,
                    )
                    .corner_radii(px(3.)),
                );
            }
            _ = shaped.paint(point(x, y), line_height, window, cx);
        }
    }

    /// Markers over the numbers and icons in the lane. Painted after the
    /// numbers, so markers cover them.
    pub fn paint_gutter(&self, background: Hsla, window: &mut Window) {
        let d = self.decorations;
        let line_height = self.layout.line_height;
        let diameter = (line_height * 0.72).min(px(14.));
        for (row, top, _) in self.rows() {
            let center_y = top + line_height / 2.;
            if let Some(marker) = self.marker(row) {
                // Centered on where the row's digits are (numbers are
                // right-aligned), so it takes the number's place.
                let char_width = self.number_width / self.number_chars.max(1) as f32;
                let digits = (row + 1).to_string().len() as f32;
                let center = point(
                    self.input_bounds.origin.x + self.number_width - char_width * digits / 2.,
                    center_y,
                );
                paint_marker(marker, center, diameter, background, window);
            }
            if let Some(icon) = d.lane_icons.get(&row) {
                let lane_x = self.input_bounds.origin.x
                    + self.layout.line_number_width
                    - LINE_NUMBER_RIGHT_MARGIN
                    - LANE_WIDTH;
                let center = point(lane_x + LANE_WIDTH / 2., center_y);
                paint_lane_icon(*icon, center, diameter, window);
            }
        }
    }
}

fn paint_marker(
    marker: GutterMarker,
    center: Point<Pixels>,
    diameter: Pixels,
    background: Hsla,
    window: &mut Window,
) {
    let half = diameter / 2.;
    match marker.shape {
        MarkerShape::Circle => {
            let bounds = Bounds::new(point(center.x - half, center.y - half), size(diameter, diameter));
            if marker.hollow {
                window.paint_quad(
                    fill(bounds, background)
                        .corner_radii(half)
                        .border_widths(px(1.5))
                        .border_color(marker.color),
                );
            } else {
                window.paint_quad(fill(bounds, marker.color).corner_radii(half));
            }
        }
        MarkerShape::Diamond => {
            let mut builder = if marker.hollow { PathBuilder::stroke(px(1.5)) } else { PathBuilder::fill() };
            builder.move_to(point(center.x, center.y - half));
            builder.line_to(point(center.x + half, center.y));
            builder.line_to(point(center.x, center.y + half));
            builder.line_to(point(center.x - half, center.y));
            builder.close();
            if let Ok(path) = builder.build() {
                window.paint_path(path, marker.color);
            }
        }
    }
    if marker.inner_dot {
        let dot = diameter * 0.32;
        window.paint_quad(
            fill(
                Bounds::new(point(center.x - dot / 2., center.y - dot / 2.), size(dot, dot)),
                background,
            )
            .corner_radii(dot / 2.),
        );
    }
}

fn paint_lane_icon(icon: LaneIcon, center: Point<Pixels>, diameter: Pixels, window: &mut Window) {
    let half = diameter / 2.;
    match icon {
        LaneIcon::Run(color) => {
            let mut builder = PathBuilder::fill();
            builder.move_to(point(center.x - half * 0.7, center.y - half * 0.85));
            builder.line_to(point(center.x + half * 0.85, center.y));
            builder.line_to(point(center.x - half * 0.7, center.y + half * 0.85));
            builder.close();
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        }
        LaneIcon::Arrow(color) => {
            // A block arrow: shaft and head.
            let shaft = half * 0.45;
            let mut builder = PathBuilder::fill();
            builder.move_to(point(center.x - half, center.y - shaft));
            builder.line_to(point(center.x, center.y - shaft));
            builder.line_to(point(center.x, center.y - half));
            builder.line_to(point(center.x + half, center.y));
            builder.line_to(point(center.x, center.y + half));
            builder.line_to(point(center.x, center.y + shaft));
            builder.line_to(point(center.x - half, center.y + shaft));
            builder.close();
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        }
    }
}

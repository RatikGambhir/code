//! What the debugger draws in a code editor: breakpoints over the line
//! numbers, run icons beside `fn main` and tests, the paused line, values of
//! locals after the lines that mention them; and values on hover while
//! paused.

use std::{path::Path, rc::Rc};

use anyhow::Result;
use cockpit_debugger::{values, Runnable};
use gpui::{App, Hsla, Task, WeakEntity, Window};
use gpui_component::{
    input::{EditorDecorations, GutterMarker, HoverProvider, InlineHint, LaneIcon, MarkerShape, Rope, RopeExt as _},
    ActiveTheme as _,
};

use super::Debugger;
use crate::theme::tokens;

/// Decorations for the editor showing `path` with `text`, and the lines it
/// can run.
pub fn decorations(debugger: &Debugger, path: &Path, text: &str, cx: &App) -> (EditorDecorations, Vec<Runnable>) {
    let t = tokens(cx);
    let breakpoint = t.danger;
    let mut d = EditorDecorations::default();

    for bp in debugger.breakpoints().for_file(path) {
        let marker = GutterMarker {
            color: if bp.log_message.is_some() { t.warning } else { breakpoint },
            shape: if bp.log_message.is_some() { MarkerShape::Diamond } else { MarkerShape::Circle },
            hollow: !bp.enabled,
            inner_dot: bp.enabled && bp.is_conditional(),
        };
        d.markers.insert(bp.line, marker);
        if bp.enabled && bp.log_message.is_none() {
            d.line_backgrounds.insert(bp.line, breakpoint.alpha(if t.dark { 0.10 } else { 0.07 }));
        }
    }
    d.hover_marker = Some(GutterMarker {
        color: breakpoint.alpha(0.35),
        shape: MarkerShape::Circle,
        hollow: false,
        inner_dot: false,
    });

    let runnables = debugger.runnables(path, text);
    if path.extension().is_some_and(|e| e == "rs") && debugger.has_cargo_project() {
        d.lane = true;
    }
    for runnable in &runnables {
        d.lane_icons.insert(runnable.line, LaneIcon::Run(t.success));
    }

    if let Some((frame_path, line, top)) = debugger.paused_location() {
        if frame_path == path {
            let paused: Hsla = cx.theme().info;
            d.line_backgrounds.insert(line, paused.alpha(if top { 0.28 } else { 0.16 }));
            d.lane = true;
            d.lane_icons.insert(line, LaneIcon::Arrow(if top { paused } else { paused.alpha(0.6) }));
            if let Some(session) = debugger.session() {
                let hint_color = paused.alpha(if t.dark { 0.95 } else { 0.85 });
                let background = Some(paused.alpha(0.10));
                for (row, texts) in values::inline_value_lines(text, line, &session.locals()) {
                    d.inline_hints.insert(
                        row,
                        InlineHint { text: texts.join("   ").into(), color: hint_color, background },
                    );
                }
            }
        }
    }
    (d, runnables)
}

/// Hover: the value under the mouse while paused, else the language
/// server's hover.
pub struct DebugHover {
    pub debugger: WeakEntity<Debugger>,
    pub inner: Option<Rc<dyn HoverProvider>>,
}

impl HoverProvider for DebugHover {
    fn hover(&self, text: &Rope, offset: usize, window: &mut Window, cx: &mut App) -> Task<Result<Option<lsp_types::Hover>>> {
        let value = self.debugger.upgrade().and_then(|debugger| {
            let position = text.offset_to_position(offset);
            let row = position.line as usize;
            let line = text.slice_line(row).to_string();
            let column = offset.saturating_sub(text.line_start_offset(row));
            let expression = values::expression_at(&line, column)?;
            debugger.read(cx).evaluate_for_hover(&expression)
        });
        let Some(value) = value else {
            return match &self.inner {
                Some(inner) => inner.hover(text, offset, window, cx),
                None => Task::ready(Ok(None)),
            };
        };
        let fallback = self.inner.as_ref().map(|inner| inner.hover(text, offset, window, cx));
        cx.spawn(async move |_| {
            if let Some(text) = value.await {
                return Ok(Some(lsp_types::Hover {
                    contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
                        kind: lsp_types::MarkupKind::Markdown,
                        value: format!("```rust\n{text}\n```"),
                    }),
                    range: None,
                }));
            }
            match fallback {
                Some(task) => task.await,
                None => Ok(None),
            }
        })
    }
}

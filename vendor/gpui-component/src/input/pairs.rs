//! cockpit patch: bracket and quote pairs in code editors.
//!
//! - Typing `(`, `[`, `{` or `"` also inserts its closer, with the cursor
//!   between them; with text selected, the selection is wrapped instead.
//! - Typing a closer when the same character is next steps over it.
//! - Backspace between an empty pair deletes both.
//! - Enter between a pair opens an indented line between them.

use std::ops::Range;

use gpui::{Context, EntityInputHandler as _, Window};

use super::InputState;

fn closer_for(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        _ => None,
    }
}

fn is_closer(c: char) -> bool {
    matches!(c, ')' | ']' | '}' | '"')
}

impl InputState {
    fn pairs_enabled(&self) -> bool {
        self.mode.is_code_editor() && !self.disabled && self.ime_marked_range.is_none()
    }

    fn char_before(&self, offset: usize) -> Option<char> {
        if offset == 0 {
            return None;
        }
        let start = self.previous_boundary(offset);
        self.text.slice(start..offset).chars().last()
    }

    fn char_after(&self, offset: usize) -> Option<char> {
        self.text.slice(offset..).chars().next()
    }

    /// Handles one typed character; `true` when it was handled here.
    pub(super) fn type_paired(&mut self, new_text: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.pairs_enabled() || self.silent_replace_text {
            return false;
        }
        let mut chars = new_text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else { return false };
        let selection: Range<usize> = self.selected_range.into();
        let next = self.char_after(selection.end);

        // Step over a closer that is already there.
        if selection.is_empty() && is_closer(c) && next == Some(c) {
            let offset = selection.end + c.len_utf8();
            self.selected_range = (offset..offset).into();
            self.pause_blink_cursor(cx);
            cx.notify();
            return true;
        }

        let Some(close) = closer_for(c) else { return false };
        if c == '"' && !self.can_open_quote(selection.start) {
            return false;
        }
        if !selection.is_empty() {
            let inner = self.text.slice(selection.clone()).to_string();
            self.replace_text_in_range(None, &format!("{c}{inner}{close}"), window, cx);
            let start = selection.start + c.len_utf8();
            self.selected_range = (start..start + inner.len()).into();
            cx.notify();
            return true;
        }
        // Only before whitespace, the end, or punctuation that ends an
        // expression; `(` before `foo` should stay a single `(`.
        let next_allows = next.is_none_or(|n| n.is_whitespace() || matches!(n, ')' | ']' | '}' | ',' | ';' | ':'));
        if !next_allows {
            return false;
        }
        self.replace_text_in_range(None, &format!("{c}{close}"), window, cx);
        let cursor = selection.start + c.len_utf8();
        self.selected_range = (cursor..cursor).into();
        cx.notify();
        true
    }

    /// A `"` opens a string unless it follows a word or a backslash, or the
    /// line already has an unclosed string before the cursor.
    fn can_open_quote(&self, offset: usize) -> bool {
        if self.char_before(offset).is_some_and(|p| p.is_alphanumeric() || p == '\\' || p == '_') {
            return false;
        }
        let line_start = self.text.slice(..offset).to_string().rfind('\n').map_or(0, |n| n + 1);
        let before = self.text.slice(line_start..offset).to_string();
        let mut quotes = 0;
        let mut escaped = false;
        for ch in before.chars() {
            match ch {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => quotes += 1,
                _ => escaped = false,
            }
            if ch != '\\' {
                escaped = false;
            }
        }
        quotes % 2 == 0
    }

    /// Backspace between an empty pair: delete both. `true` when handled.
    pub(super) fn backspace_pair(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.pairs_enabled() || !self.selected_range.is_empty() {
            return false;
        }
        let cursor = self.cursor();
        let (Some(prev), Some(next)) = (self.char_before(cursor), self.char_after(cursor)) else { return false };
        if closer_for(prev) != Some(next) {
            return false;
        }
        self.selected_range = (cursor - prev.len_utf8()..cursor + next.len_utf8()).into();
        self.replace_text_in_range(None, "", window, cx);
        true
    }

    /// Enter between `{}`, `()` or `[]`: the closer goes to its own line and
    /// the cursor to an indented line between. `true` when handled.
    pub(super) fn enter_between_pair(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.pairs_enabled() || !self.selected_range.is_empty() {
            return false;
        }
        let cursor = self.cursor();
        let (Some(prev), Some(next)) = (self.char_before(cursor), self.char_after(cursor)) else { return false };
        if prev == '"' || closer_for(prev) != Some(next) {
            return false;
        }
        let line_start = self.text.slice(..cursor).to_string().rfind('\n').map_or(0, |n| n + 1);
        let indent: String = self
            .text
            .slice(line_start..cursor)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let step = self.mode.tab_size().to_string();
        let inner = format!("\n{indent}{step}");
        self.replace_text_in_range_silent(None, &format!("{inner}\n{indent}"), window, cx);
        let offset = cursor + inner.len();
        self.selected_range = (offset..offset).into();
        cx.notify();
        true
    }
}

#[cfg(test)]
mod tests {
    use gpui::{EntityInputHandler as _, TestAppContext};

    use crate::input::{Backspace, Enter, InputState};

    /// Types `keys` one character at a time ("⌫" is Backspace, "⏎" Enter)
    /// into a Rust code editor and returns the text with `|` at the cursor.
    fn typed(cx: &mut TestAppContext, keys: &str) -> String {
        cx.update(crate::init);
        let (state, cx) = cx.add_window_view(|window, cx| InputState::new(window, cx).code_editor("rust"));
        state.update_in(cx, |state, window, cx| {
            for key in keys.chars() {
                match key {
                    '⌫' => state.backspace(&Backspace, window, cx),
                    '⏎' => state.enter(&Enter { secondary: false }, window, cx),
                    c => state.replace_text_in_range(None, &c.to_string(), window, cx),
                }
            }
            let mut text = state.text.to_string();
            text.insert(state.cursor(), '|');
            text
        })
    }

    #[gpui::test]
    fn closes_brackets_and_steps_over_closers(cx: &mut TestAppContext) {
        assert_eq!(typed(cx, "f("), "f(|)");
        assert_eq!(typed(cx, "f(x)"), "f(x)|");
        assert_eq!(typed(cx, "v[{"), "v[{|}]");
        assert_eq!(typed(cx, "s(\"hi\")"), "s(\"hi\")|");
    }

    #[gpui::test]
    fn leaves_brackets_alone_before_words(cx: &mut TestAppContext) {
        assert_eq!(typed(cx, "x(⌫a"), "xa|");
        assert_eq!(typed(cx, "a\"b"), "a\"b|");
    }

    #[gpui::test]
    fn backspace_removes_an_empty_pair(cx: &mut TestAppContext) {
        assert_eq!(typed(cx, "f(⌫"), "f|");
        assert_eq!(typed(cx, "\"⌫"), "|");
    }

    #[gpui::test]
    fn enter_opens_a_block(cx: &mut TestAppContext) {
        assert_eq!(typed(cx, "fn f() {⏎"), "fn f() {\n  |\n}");
    }
}

use anyhow::Result;
use gpui::{Context, EntityInputHandler, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, CompletionTextEdit,
    InlineCompletionContext, InlineCompletionItem, InlineCompletionResponse,
    InlineCompletionTriggerKind, request::Completion,
};
use ropey::Rope;
use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use crate::input::{
    InputState, RopeExt as _,
    popovers::{CompletionMenu, ContextMenu},
};

/// Default debounce duration for inline completions.
const DEFAULT_INLINE_COMPLETION_DEBOUNCE: Duration = Duration::from_millis(300);

/// A trait for providing code completions based on the current input state and context.
pub trait CompletionProvider {
    /// Fetches completions based on the given byte offset.
    ///
    /// - The `offset` is in bytes of current cursor.
    ///
    /// textDocument/completion
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_completion
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionResponse>>;

    /// Fetches an inline completion suggestion for the given position.
    ///
    /// This is called after a debounce period when the user stops typing.
    /// The provider can analyze the text and cursor position to determine
    /// what inline completion suggestion to show.
    ///
    ///
    /// # Arguments
    /// * `rope` - The current text content
    /// * `offset` - The cursor position in bytes
    ///
    /// textDocument/inlineCompletion
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_inlineCompletion
    fn inline_completion(
        &self,
        _rope: &Rope,
        _offset: usize,
        _trigger: InlineCompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<Result<InlineCompletionResponse>> {
        Task::ready(Ok(InlineCompletionResponse::Array(vec![])))
    }

    /// Returns the debounce duration for inline completions.
    ///
    /// Default: 300ms
    #[inline]
    fn inline_completion_debounce(&self) -> Duration {
        DEFAULT_INLINE_COMPLETION_DEBOUNCE
    }

    fn resolve_completions(
        &self,
        _completion_indices: Vec<usize>,
        _completions: Rc<RefCell<Box<[Completion]>>>,
        _: &mut Context<InputState>,
    ) -> Task<Result<bool>> {
        Task::ready(Ok(false))
    }

    /// cockpit patch: which of `items` (the last answer) to show for `word`,
    /// the identifier typed before the cursor, best first, as indices into
    /// `items`. Called on every keystroke while the menu is open, so the
    /// list keeps up with typing while a fresh answer is on its way.
    ///
    /// The default keeps items whose filter text starts with the word,
    /// ignoring case, in their original order.
    fn filter_completions(&self, items: &[&CompletionItem], word: &str) -> Vec<usize> {
        let word = word.to_lowercase();
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.filter_text
                    .as_deref()
                    .unwrap_or(&item.label)
                    .to_lowercase()
                    .starts_with(&word)
            })
            .map(|(ix, _)| ix)
            .collect()
    }

    /// Determines if the completion should be triggered based on the given byte offset.
    ///
    /// This is called on the main thread.
    fn is_completion_trigger(
        &self,
        offset: usize,
        new_text: &str,
        cx: &mut Context<InputState>,
    ) -> bool;
}

pub(crate) struct InlineCompletion {
    /// Completion item to display as an inline completion suggestion
    pub(crate) item: Option<InlineCompletionItem>,
    /// Task for debouncing inline completion requests
    pub(crate) task: Task<Result<InlineCompletionResponse>>,
}

impl Default for InlineCompletion {
    fn default() -> Self {
        Self {
            item: None,
            task: Task::ready(Ok(InlineCompletionResponse::Array(vec![]))),
        }
    }
}

impl InputState {
    pub(crate) fn handle_completion_trigger(
        &mut self,
        range: &Range<usize>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.completion_inserting {
            return;
        }

        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };

        // Always schedule inline completion (debounced).
        // It will check if menu is open before showing the suggestion.
        self.schedule_inline_completion(window, cx);

        let start = range.end;
        let new_offset = self.cursor();

        let open_menu = match self.context_menu.as_ref() {
            Some(ContextMenu::Completion(menu)) if menu.read(cx).is_open() => Some(menu.clone()),
            _ => None,
        };

        if !provider.is_completion_trigger(start, new_text, cx) {
            // cockpit patch: Backspace narrows the open list back out (or
            // closes it once the word is gone), as typing narrows it.
            if let (Some(menu), true) = (open_menu, new_text.is_empty()) {
                let word = word_before(&self.text, new_offset);
                menu.update(cx, |menu, cx| {
                    if word.is_empty() {
                        menu.dismiss(cx);
                    } else {
                        menu.refilter(provider.as_ref(), &word, new_offset, window, cx);
                    }
                });
            }
            return;
        }

        let menu = match self.context_menu.as_ref() {
            Some(ContextMenu::Completion(menu)) => Some(menu),
            _ => None,
        };

        // To create or get the existing completion menu.
        let menu = match menu {
            Some(menu) => menu.clone(),
            None => {
                let menu = CompletionMenu::new(cx.entity(), window, cx);
                self.context_menu = Some(ContextMenu::Completion(menu.clone()));
                menu
            }
        };

        let start_offset = menu.read(cx).trigger_start_offset.unwrap_or(start);
        if new_offset < start_offset {
            return;
        }

        let query = self
            .text_for_range(
                self.range_to_utf16(&(start_offset..new_offset)),
                &mut None,
                window,
                cx,
            )
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        _ = menu.update(cx, |menu, _| {
            menu.update_query(start_offset, query.clone());
        });

        // cockpit patch: narrow what is on screen right away; the server's
        // answer to this keystroke replaces it when it comes.
        // After a character that ends the word (`.`, `;`, a space) there is
        // nothing to narrow to: the list closes until the answer comes.
        if open_menu.is_some() {
            let word = word_before(&self.text, new_offset);
            menu.update(cx, |menu, cx| {
                if word.is_empty() {
                    menu.hide(cx);
                } else {
                    menu.refilter(provider.as_ref(), &word, new_offset, window, cx);
                }
            });
        }

        let completion_context = CompletionContext {
            trigger_kind: lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(query),
        };

        let provider_responses =
            provider.completions(&self.text, new_offset, completion_context, window, cx);
        let request = menu.update(cx, |menu, _| menu.next_request());
        // cockpit patch: every answer may land, not just the latest one's:
        // dropping the earlier requests on each keystroke left a fast typist
        // without a menu until they paused for a whole server round trip.
        // An answer is shown only if nothing newer is, the menu was not
        // dismissed meanwhile, and the cursor is still in the word it was
        // asked about (filtered for what has been typed since).
        cx.spawn_in(window, async move |editor, cx| {
            let mut completions: Vec<CompletionItem> = vec![];
            if let Some(provider_responses) = provider_responses.await.ok() {
                match provider_responses {
                    CompletionResponse::Array(items) => completions.extend(items),
                    CompletionResponse::List(list) => completions.extend(list.items),
                }
            }

            editor
                .update_in(cx, |editor, window, cx| {
                    let current = matches!(
                        editor.context_menu.as_ref(),
                        Some(ContextMenu::Completion(current)) if *current == menu
                    );
                    if !current || !menu.read(cx).wants_answer(request) {
                        return;
                    }
                    if !editor.focus_handle.is_focused(window) {
                        return;
                    }
                    let cursor = editor.cursor();
                    if cursor < new_offset || !is_word(&editor.text.slice(new_offset..cursor).to_string()) {
                        return;
                    }
                    let word = word_before(&editor.text, cursor);
                    let Some(provider) = editor.lsp.completion_provider.clone() else {
                        return;
                    };
                    _ = menu.update(cx, |menu, cx| {
                        menu.set_items(request, cursor, completions, provider.as_ref(), &word, window, cx);
                    });

                    cx.notify();
                })
                .ok();

            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// cockpit patch: accepts `item` the way IntelliJ does: Enter inserts it
    /// before the rest of the word, Tab (`replace`) replaces the whole word.
    /// Snippets are expanded with the cursor at their first stop, a
    /// multi-line insertion follows the line's indentation, and the item's
    /// additional edits (such as the import an unimported name needs) are
    /// applied too.
    pub(crate) fn apply_completion(
        &mut self,
        item: &CompletionItem,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let len = self.text.len();
        let cursor = self.cursor().min(len);
        let word_start = cursor - word_before(&self.text, cursor).len();
        let word_end = cursor + word_after(&self.text, cursor).len();
        let offset = |position: &lsp_types::Position| self.text.position_to_offset(position).min(len);

        let (mut range, new_text) = match &item.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => {
                (offset(&edit.range.start)..offset(&edit.range.end), edit.new_text.clone())
            }
            Some(CompletionTextEdit::InsertAndReplace(edit)) => {
                let range = if replace { &edit.replace } else { &edit.insert };
                (offset(&range.start)..offset(&range.end), edit.new_text.clone())
            }
            None => (
                word_start..cursor,
                item.insert_text.clone().unwrap_or_else(|| item.label.clone()),
            ),
        };
        if range.start > range.end {
            range = range.end..range.start;
        }
        // The answer may predate the last few keystrokes: what was typed of
        // the word since then is replaced too.
        if range.end < cursor && is_word(&self.text.slice(range.end..cursor).to_string()) {
            range.end = cursor;
        }
        if replace && range.end >= cursor && range.end < word_end {
            range.end = word_end;
        }

        let new_text = if new_text.contains('\n')
            && item.insert_text_mode != Some(lsp_types::InsertTextMode::AS_IS)
        {
            let line_start = self.text.line_start_offset(self.text.offset_to_point(range.start).row);
            let indent: String = self
                .text
                .slice(line_start..range.start)
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            new_text.replace('\n', &format!("\n{indent}"))
        } else {
            new_text
        };
        let (new_text, selection) =
            if item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET) {
                expand_snippet(&new_text)
            } else {
                let end = new_text.len();
                (new_text, end..end)
            };

        // Additional edits must not touch the main one; applied from the
        // end of the text backwards, each range is still valid when its turn
        // comes.
        let mut edits: Vec<(Range<usize>, String, bool)> = item
            .additional_text_edits
            .iter()
            .flatten()
            .map(|edit| {
                let (start, end) = (offset(&edit.range.start), offset(&edit.range.end));
                (start.min(end)..start.max(end), edit.new_text.clone(), false)
            })
            .filter(|(edit, _, _)| edit.end <= range.start || edit.start >= range.end)
            .collect();
        edits.push((range.clone(), new_text, true));
        edits.sort_by(|a, b| b.0.start.cmp(&a.0.start).then(b.2.cmp(&a.2)));

        // Where the main edit starts once the edits before it are made.
        let shift: isize = edits
            .iter()
            .filter(|(edit, _, main)| !main && edit.start < range.start)
            .map(|(edit, text, _)| text.len() as isize - edit.len() as isize)
            .sum();
        for (edit, text, _) in &edits {
            let range_utf16 = self.range_to_utf16(edit);
            self.replace_text_in_range_silent(Some(range_utf16), text, window, cx);
        }

        let start = (range.start as isize + shift).max(0) as usize;
        self.move_to(start + selection.start, None, cx);
        if !selection.is_empty() {
            self.select_to(start + selection.end, cx);
        }
    }

    /// Schedule an inline completion request after debouncing.
    pub(crate) fn schedule_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Clear any existing inline completion on text change
        self.clear_inline_completion(cx);

        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };

        let offset = self.cursor();
        let text = self.text.clone();
        let debounce = provider.inline_completion_debounce();

        self.inline_completion.task = cx.spawn_in(window, async move |editor, cx| {
            // Debounce: wait before fetching to avoid unnecessary requests while typing
            smol::Timer::after(debounce).await;

            // Now fetch the inline completion after the debounce period
            let task = editor.update_in(cx, |editor, window, cx| {
                // Check if cursor has moved during debounce
                if editor.cursor() != offset {
                    return None;
                }

                // Don't fetch if completion menu is open
                if editor.is_context_menu_open(cx) {
                    return None;
                }

                let trigger = InlineCompletionContext {
                    trigger_kind: InlineCompletionTriggerKind::Automatic,
                    selected_completion_info: None,
                };

                Some(provider.inline_completion(&text, offset, trigger, window, cx))
            })?;

            let Some(task) = task else {
                return Ok(InlineCompletionResponse::Array(vec![]));
            };

            let response = task.await?;

            editor.update_in(cx, |editor, _window, cx| {
                // Only apply if cursor still hasn't moved
                if editor.cursor() != offset {
                    return;
                }

                // Don't show if completion menu opened while we were fetching
                if editor.is_context_menu_open(cx) {
                    return;
                }

                if let Some(item) = match response.clone() {
                    InlineCompletionResponse::Array(items) => items.into_iter().next(),
                    InlineCompletionResponse::List(comp_list) => comp_list.items.into_iter().next(),
                } {
                    editor.inline_completion.item = Some(item);
                    cx.notify();
                }
            })?;

            Ok(response)
        });
    }

    /// Check if an inline completion suggestion is currently displayed.
    #[inline]
    pub(crate) fn has_inline_completion(&self) -> bool {
        self.inline_completion.item.is_some()
    }

    /// Clear the inline completion suggestion.
    pub(crate) fn clear_inline_completion(&mut self, cx: &mut Context<Self>) {
        self.inline_completion = InlineCompletion::default();
        cx.notify();
    }

    /// Accept the inline completion, inserting it at the cursor position.
    /// Returns true if a completion was accepted, false if there was none.
    pub(crate) fn accept_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(completion_item) = self.inline_completion.item.take() else {
            return false;
        };

        let cursor = self.cursor();
        let range_utf16 = self.range_to_utf16(&(cursor..cursor));
        let completion_text = completion_item.insert_text;
        self.replace_text_in_range_silent(Some(range_utf16), &completion_text, window, cx);
        true
    }
}

/// cockpit patch: identifier characters, as language servers complete them.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_word(text: &str) -> bool {
    text.chars().all(is_word_char)
}

/// cockpit patch: the identifier ending at `offset`.
pub(crate) fn word_before(text: &Rope, offset: usize) -> String {
    let line_start = text.line_start_offset(text.offset_to_point(offset).row);
    let before = text.slice(line_start..offset).to_string();
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map_or(before.len(), |(ix, _)| ix);
    before[start..].to_string()
}

/// cockpit patch: the rest of the identifier from `offset` on.
fn word_after(text: &Rope, offset: usize) -> String {
    let line_end = text.line_end_offset(text.offset_to_point(offset).row).max(offset);
    text.slice(offset..line_end)
        .chars()
        .take_while(|c| is_word_char(*c))
        .collect()
}

/// cockpit patch: an LSP snippet as plain text, and where the cursor goes:
/// the first tab stop's placeholder (selected, so typing replaces it), else
/// the final stop `$0`, else the end. Choices become their first option and
/// variables their default.
pub(crate) fn expand_snippet(snippet: &str) -> (String, Range<usize>) {
    let mut parser = SnippetParser {
        chars: snippet.chars().collect(),
        pos: 0,
        out: String::new(),
        stops: Vec::new(),
    };
    parser.parse(false);
    let end = parser.out.len();
    let cursor = parser
        .stops
        .iter()
        .filter(|(n, _)| *n > 0)
        .min_by_key(|(n, _)| *n)
        .or_else(|| parser.stops.iter().find(|(n, _)| *n == 0))
        .map(|(_, range)| range.clone())
        .unwrap_or(end..end);
    (parser.out, cursor)
}

struct SnippetParser {
    chars: Vec<char>,
    pos: usize,
    out: String,
    /// Tab stops: number and the range of their text in `out`.
    stops: Vec<(u32, Range<usize>)>,
}

impl SnippetParser {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    /// Copies text up to the end, or (`nested`) up to and past the `}`
    /// closing the placeholder being read.
    fn parse(&mut self, nested: bool) {
        while let Some(c) = self.peek(0) {
            match c {
                '\\' if matches!(self.peek(1), Some('$' | '}' | '\\' | ',' | '|')) => {
                    self.out.push(self.peek(1).unwrap_or_default());
                    self.pos += 2;
                }
                '}' if nested => {
                    self.pos += 1;
                    return;
                }
                '$' => self.dollar(),
                _ => {
                    self.out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        (self.pos > start)
            .then(|| self.chars[start..self.pos].iter().collect::<String>().parse().ok())
            .flatten()
    }

    fn name(&mut self) -> bool {
        let start = self.pos;
        while self.peek(0).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        self.pos > start
    }

    fn dollar(&mut self) {
        self.pos += 1; // `$`
        if let Some(n) = self.number() {
            let at = self.out.len();
            self.stops.push((n, at..at));
            return;
        }
        if self.peek(0) != Some('{') {
            // `$NAME` (a variable we do not know) or a lone `$`.
            if !self.name() {
                self.out.push('$');
            }
            return;
        }
        self.pos += 1; // `{`
        let stop = self.number();
        if stop.is_none() && !self.name() {
            // Not a snippet construct after all.
            self.out.push_str("${");
            return;
        }
        let start = self.out.len();
        match self.peek(0) {
            Some(':') => {
                self.pos += 1;
                self.parse(true);
            }
            Some('|') => {
                self.pos += 1;
                let mut first = true;
                while let Some(c) = self.peek(0) {
                    self.pos += 1;
                    match c {
                        '|' if self.peek(0) == Some('}') => {
                            self.pos += 1;
                            break;
                        }
                        ',' => first = false,
                        '\\' => {
                            if let (true, Some(next)) = (first, self.peek(0)) {
                                self.out.push(next);
                            }
                            self.pos += 1;
                        }
                        _ if first => self.out.push(c),
                        _ => {}
                    }
                }
            }
            Some('}') => self.pos += 1,
            // A transform or something else unusual: skip to its end.
            _ => {
                let mut depth = 1;
                while let Some(c) = self.peek(0) {
                    self.pos += 1;
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(n) = stop {
            self.stops.push((n, start..self.out.len()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_expand_with_the_cursor_at_the_first_stop() {
        assert_eq!(expand_snippet("println!($0)"), ("println!()".into(), 9..9));
        assert_eq!(expand_snippet("foo(${1:a}, ${2:b})$0"), ("foo(a, b)".into(), 4..5));
        assert_eq!(expand_snippet("plain"), ("plain".into(), 5..5));
        assert_eq!(expand_snippet("x$1y$0"), ("xy".into(), 1..1));
        assert_eq!(expand_snippet("${1:outer ${2:inner}}"), ("outer inner".into(), 0..11));
        assert_eq!(expand_snippet("${1|one,two|}"), ("one".into(), 0..3));
        assert_eq!(expand_snippet("cost: \\$5 \\} $TM_SELECTED_TEXT${VAR:def}"), ("cost: $5 } def".into(), 14..14));
        assert_eq!(expand_snippet("a $ b ${ c"), ("a $ b ${ c".into(), 10..10));
        assert_eq!(expand_snippet("fn ${1:name}() {\n    $0\n}"), ("fn name() {\n    \n}".into(), 3..7));
    }

    #[test]
    fn words_around_the_cursor() {
        let text = Rope::from("let foo_bar = baz.qux;\nnext");
        assert_eq!(word_before(&text, 11), "foo_bar");
        assert_eq!(word_before(&text, 7), "foo");
        assert_eq!(word_after(&text, 7), "_bar");
        assert_eq!(word_before(&text, 18), "");
        assert_eq!(word_after(&text, 18), "qux");
        assert_eq!(word_before(&text, 23), "");
        assert_eq!(word_after(&text, 23), "next");
    }
}

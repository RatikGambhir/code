# Cockpit patches to gpui-component 0.5.1

This is gpui-component 0.5.1 from crates.io with a few additions, used by
Cockpit's debugger, notebooks and code editors. Every change is marked `cockpit patch`.

- `src/input/gutter.rs` (new): `EditorDecorations` (markers over line numbers,
  an icon lane right of them, full-width line backgrounds, text hints after
  line ends, a hover marker) set with `InputState::set_decorations`, and
  `GutterEvent`, emitted by `InputState` for presses in the gutter.
- `src/input/state.rs`: two fields; gutter presses are emitted instead of
  moving the cursor; mouse moves track the hovered gutter row.
- `src/input/element.rs`: the lane widens the gutter; decorations are painted.
- `src/input/mod.rs`: the module and its re-export.
- `src/input/state.rs`, `src/input/popovers/context_menu.rs`:
  `InputState::set_context_menu_extras` puts the embedder's items (label and
  action) at the top of a code editor's right-click menu ("Open Rust
  Notebook").
- `src/input/pairs.rs` (new), hooked into `state.rs` (typing, Backspace,
  Enter): code editors close `(`, `[`, `{` and `"` as you type them, step
  over a typed closer that is already there, delete an empty pair with one
  Backspace, and open an indented line on Enter between a pair.

- `src/input/state.rs`, `src/input/element.rs`:
  `InputState::set_text_highlights` colors byte ranges in inputs that are
  not code editors (the agent composer's `@codex` / `@chatgpt` mentions).
- `src/highlighter/highlighter.rs`: tree-sitter query matches are cached
  per byte range until the text changes. The editor asks for every visible
  line on every frame (cursor blinks, hovers, other views redrawing).
- `src/input/element.rs`: visible lines' highlights are combined line by
  line and appended (lines never overlap) instead of re-combining all
  earlier lines once per line.
- `src/input/lsp/completions.rs`, `src/input/popovers/completion_menu.rs`:
  completion that behaves like IntelliJ's. The open list is narrowed on
  every keystroke (`CompletionProvider::filter_completions`) while the
  server's answer for that keystroke is on its way; answers that arrive
  out of date still show (filtered for what was typed since) instead of
  being dropped, but never reopen a dismissed menu. Tab accepts, replacing
  the rest of the word; Enter accepts, inserting. Accepting applies the
  item's `additionalTextEdits` (auto-imports), expands snippets with the
  cursor at the first stop, and indents multi-line insertions like their
  line. Backspace narrows the list back out. The menu highlights the typed
  prefix from the label itself.

To move to a newer gpui-component, re-apply these and keep the version in
step with `gpui` in the workspace `Cargo.toml`.

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

To move to a newer gpui-component, re-apply these and keep the version in
step with `gpui` in the workspace `Cargo.toml`.

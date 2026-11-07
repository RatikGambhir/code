# Hangar

A native macOS IDE written in Rust: a [GPUI](https://www.gpui.rs) shell with
[gpui-component](https://github.com/longbridge/gpui-component), a real
Chromium browser (CEF, off-screen rendering) inside the window, a terminal,
git, and two coding agents you can switch between: OpenAI's Codex, driven
through the Codex App Server, and Anthropic's Claude, driven through Claude
Code — with an orchestration layer of its own for permissions, budgets,
approvals and the browser tools the agents can use.

v0.1 covers: file explorer and editor (tree-sitter highlighting, language
servers, auto-closed brackets and quotes), browser tabs, terminals, source control, find in files, a command
palette, running and debugging Rust, Rust notebooks for trying out a single
function, and an agent panel (Codex or Claude) where you prompt, watch the
turn stream, approve commands and edits, and interrupt.

## Requirements

- macOS 12 or later (Apple Silicon or Intel)
- Rust 1.88+ (`rustup` recommended)
- Xcode Command Line Tools (`xcode-select --install`)
- CMake and Ninja, used to build the Chromium wrapper: `brew install cmake ninja`
- At least one agent CLI:
  - Codex: `npm install -g @openai/codex` or `brew install codex` (or the
    copy inside the ChatGPT desktop app, which Hangar finds on its own), then
    sign in from the agent panel. *Continue with ChatGPT* is Hangar's own
    Sign in with ChatGPT: Hangar keeps the tokens in the macOS Keychain and
    Codex asks Hangar for an access token when it needs one
    (`Hangar --chatgpt-access-token`). Codex's own sign-in (ChatGPT or an
    OpenAI API key, kept by Codex in `~/.codex`) is still offered; switch
    between them from the Codex menu in the panel header. Codex ties each
    thread to the sign-in that started it.
  - `@chatgpt` in the Codex panel asks ChatGPT itself (no Codex, no tools)
    on your ChatGPT plan, with Hangar's sign-in: answers stream from
    `api.openai.com/v1/responses`, and conversations are kept in Hangar's
    data directory (`chatgpt-threads/`). `@codex` starts a Codex agent.
  - Claude Code: `curl -fsSL https://claude.ai/install.sh | bash` or
    `npm install -g @anthropic-ai/claude-code`, then sign in from the agent
    panel (a Claude plan, an Anthropic Console account, or
    `ANTHROPIC_API_KEY` / Bedrock / Vertex configured for Claude Code).
    Claude Code keeps its own credentials; Hangar never stores them.
- For Rust IDE features (completion, go to definition, references, hover,
  quick fixes, `cargo check` diagnostics): `rustup component add
  rust-analyzer`. The `rust-analyzer` in `~/.cargo/bin` is only rustup's
  proxy until the component is installed; Hangar notices and says so in
  the status bar.
- For debugging Rust: `lldb-dap`, which comes with Xcode and the Command
  Line Tools (`xcode-select --install`); Hangar finds it with `xcrun`.
  Readable `String`/`Vec`/`HashMap`/`Option` values come from your Rust
  toolchain's LLDB formatters, and stepping into the standard library
  needs `rustup component add rust-src`.
- Language servers for other languages are picked per file from the
  project around it (see *Languages and projects* below). Hangar uses the
  project's own (`node_modules/.bin`), then your `PATH`, `~/go/bin` and
  `~/.bun/bin`, then any it installed itself: **Install Missing Language
  Servers** in the command palette installs what open files needed (npm
  packages with npm or Bun, Go tools with `go install`) into
  `~/Library/Application Support/Hangar/language-servers`.

## Languages and projects

Opening a file detects its project by walking up to the markers each
ecosystem leaves, and starts the servers that apply, once per project
root (one TypeScript server per package-manager workspace, one gopls per
Go module or `go.work`). The status bar shows what was found, such as
`Bun · TypeScript · Next.js · Tailwind` or `Go 1.23 · Gin`; clicking it
(or **Run Project Task…**) lists the project's commands and runs one in a
terminal.

| | Detected from | Servers |
|---|---|---|
| TypeScript 7+ | installed `typescript` ≥ 7, or none installed | its own `tsc --lsp` (`tsgo` with `@typescript/native-preview`) |
| TypeScript ≤ 6 | `node_modules/typescript/lib/tsserver.js` | `typescript-language-server` or `vtsls` on the project's tsserver |
| Node / Bun / Deno | lockfiles, `bunfig.toml`, `@types/bun`, `deno.json` | Deno's `deno lsp` replaces TypeScript and ESLint |
| Vue, Svelte, Astro, Angular | dependencies | `vue-language-server`, `svelteserver`, `astro-ls`, `ngserver` |
| ESLint, Biome, oxlint | config files or dependencies | their servers, as linters beside the primary one |
| Tailwind | `tailwindcss` or a config | `tailwindcss-language-server`, its completions merged in |
| Go | `go.mod`, `go.work` | `gopls` (staticcheck on); `golangci-lint-langserver` with a `.golangci.*` config; `templ lsp` for `.templ` |
| JSON, YAML | always | schemas for `package.json`, `tsconfig.json`, `deno.json`, `biome.json` and others |

Tasks come from `package.json` scripts (run with the project's package
manager), `deno.json` tasks, and for Go `go run/test/build/vet`, `go mod
tidy`, `go generate`, plus `golangci-lint`, `templ generate`, `air`,
`sqlc` and `buf` when the project uses them.

## Build and run

```sh
cargo xtask bundle --run          # debug build, run in this terminal (logs on stderr)
cargo xtask bundle --release --open
cargo xtask bundle --release --sign "Developer ID Application: …"   # hardened runtime
```

The first build downloads the Chromium Embedded Framework (~500 MB) into
`.cef/` (see `.cargo/config.toml`; export `CEF_PATH` to share one download
between checkouts). The app is assembled at
`target/<profile>/bundle/Hangar.app`.

Release builds use whole-program LTO in a single codegen unit, so they take
a few minutes longer to link. When `xcrun` finds the Metal compiler (Xcode,
or its downloadable Metal toolchain), they also ship precompiled shaders;
otherwise the shaders compile on the first launch (the `runtime-shaders`
feature, on by default).

Startup does only what the first frame needs. Chromium starts when the
first browser tab opens, and the agent (`codex app-server`, `claude`)
starts when its panel is first shown. The log records
`window ready … after launch`.

Chromium only works from the app bundle — it locates its framework and the
`Hangar Helper*.app` processes through the bundle layout. A plain
`cargo run` starts the IDE with the browser disabled and a note explaining
why, which is handy when working on non-browser UI.

Pass a folder to open it: `cargo xtask bundle --run -- ~/src/project`; an
installed app also opens folders dropped on its Dock icon or given to
`open -a Hangar <folder>`. Otherwise Hangar reopens the last folder, or
shows the welcome window.

### Verification on any host

```sh
scripts/check.sh
```

runs the tests of every crate (the UI's included), type-checks the app, and
type-checks the Chromium backend for `aarch64-apple-darwin` without
downloading CEF (`--features cef-typecheck`).

The UI itself also runs on Linux, minus the browser, which is handy for
testing it headless: `cargo run -- <folder>` under `xvfb-run` with a Vulkan
driver (Mesa's lavapipe works). The live agent tests are opt-in:
`HANGAR_CODEX_BIN=$(which codex) cargo test -p cockpit-agents --test
live_codex` (add `HANGAR_CODEX_LIVE_TURN=1` to run a real turn) and
`HANGAR_CLAUDE_BIN=$(which claude) cargo test -p cockpit-agents --test
live_claude` (version, account and models, which bill nothing; add
`HANGAR_CLAUDE_LIVE_TURN=1` for a real turn on Claude Haiku 5.5, or set
`HANGAR_CLAUDE_LIVE_MODEL`). `cargo test -p cockpit-notebook --test
rust_analyzer_live -- --ignored` checks completion inside a notebook cell
against the installed rust-analyzer. `cargo test -p cockpit-editor --test
live_go_and_typescript` drives gopls and TypeScript 6 and 7 through project
detection; set `HANGAR_LANGUAGE_SERVERS=<dir>` (and `HANGAR_TEST_INSTALL=1`
to install missing servers there first) to use managed installs.

## Keys

| | |
|---|---|
| ⌘P / ⇧⌘P | Go to file / command palette |
| ⌘O | Open folder |
| ⌘S, ⌘W | Save, close tab |
| ⇧⌘[ ⇧⌘] | Previous / next tab |
| ⌘\\ | Split editor |
| ⌘B, ⌘J, ⌥⌘B | Toggle sidebar, terminal, agent panel |
| ⇧⌘E, ⇧⌘G, ⇧⌘F | Explorer, source control, find in files |
| ⌘T | New browser tab (⌘L address bar, ⌘R reload, ⌘[ ⌘] back/forward, ⌥⌘I DevTools) |
| ⌃\` | New terminal |
| F12 or ⌘-click | Go to definition (several matches open a list) |
| ⇧F12 | Find all references |
| ⌃- / ⌃⇧- | Go back / forward after a jump |
| ⌘. | Quick fixes and refactors (e.g. add a missing `use`) |
| Tab / ↩ | In the completion list (it opens as you type): Tab replaces the word under the cursor, ↩ inserts before it; ↑↓ choose, Esc closes. Picking a name that is not imported yet adds its `use`. |
| `(` `[` `{` `"` | Closed for you; typing the closer steps over it, ⌫ removes an empty pair, ↩ between a pair opens an indented line, and with text selected it is wrapped |
| ⌘L | Ask the agent (Enter sends, ⇧Enter newline, ⌃C stops the turn) |
| ⇧⌘N | New agent thread |
| ⇧⌘A | Chat mode: the conversation fills the window, history in the sidebar (again to hide the history) |
| ⇧⌘D | Show or hide the chat's changes (this chat, or everything uncommitted). The panel also slides out on its own when the agent writes a file, on the file it touched; closing it keeps it closed for that conversation |
| ⌃R / ⌃D | Run / debug the selected configuration (in the editor) |
| ⌃⇧R / ⌃⇧D | Run / debug the test or `fn main` at the cursor |
| ⌘F2, ⌘F5 | Stop, rerun under the debugger |
| F8, F7, ⇧F8 | Step over, into, out |
| ⌥⌘R (or F9), ⌥F9 | Resume, run to cursor |
| ⌘F8, ⇧⌘F8 | Toggle breakpoint, view breakpoints |
| ⌥F8, ⌘5 | Evaluate expression, debug panel |
| ⌘↵, ⇧⌘↵, ⇧⌥F | In a notebook cell: run it, add a cell below, format it |

## Tasks

The checklist button under Chat in the left rail opens Tasks: projects and
issues in a Linear-style list (grouped by status, with priority, labels,
project and assignee), a projects table with progress, and the open issue's
details beside the list.

- *This workspace*: a local board kept in Hangar's data directory (one JSON
  file per folder, nothing written into the repo). Add issues with New issue
  or + on a status group, and set status, priority and project in the
  details pane.
- *Linear*: choose Connect Linear and paste a personal API key (Linear →
  Settings → Security & access). It is checked with Linear and kept in the
  macOS Keychain. Hangar pulls your projects and your 500 most recently
  updated issues, read-only; Open in Linear jumps to the item.

## Run and debug

Rust projects get RustRover-style running and debugging, built on Cargo and
LLDB (`lldb-dap`, spoken to over the Debug Adapter Protocol):

- *Configurations*: the title bar's selector lists the workspace's binaries
  and examples (from `cargo metadata`) and recently run tests, with program
  arguments; ▶ runs it in a terminal (`cargo run` / `cargo test`), the bug
  debugs it.
- *Gutter*: a ▶ beside `fn main`, each `#[test]` (also `#[tokio::test]`
  and friends) and each test module offers Run and Debug. Click beside a
  line number for a breakpoint, ⌥-click to disable it, right-click for a
  condition, a hit count or a log message (a logpoint, drawn as a diamond).
  Breakpoints stay on their line as you edit and are kept per workspace.
- *Debugging* builds with `cargo build`/`cargo test --no-run`, launches the
  binary under LLDB with the toolchain's Rust formatters, and stops at
  breakpoints and, by default, on panics (on the first frame of your own
  code). The paused line is highlighted, locals' values show after the
  lines that mention them, and hovering a name or `a.b.c` path shows its
  value.
- *Debug panel* (bottom dock, beside the terminal): resume, pause, stop,
  rerun, step over/into/out, run to cursor; threads and frames (library
  frames dimmed); a variables tree with Rust type names (`{Vec<i32>}
  size=3`) that keeps what you expanded across steps, double-click to set a
  value; watches and an expression field (↩ evaluates, ⌘↩ adds a watch); the
  program's output and an expression console; every breakpoint, with
  "pause when the program panics".

## Notebooks

A notebook lets you call one function from your code with arguments you
write, and see what it prints and returns, without writing a test or a
`main`.

- *Open one*: right-click anywhere in a Rust file and choose **Open Rust
  Notebook** (or use "Open Notebook" in the command
  palette). Inside a function (its body, signature or doc comment), the
  notebook is for that function. Anywhere else, it is for the file: the
  first cell imports the file's module and you write the calls.
- *Try a function*: the notebook's header lists the file's public
  functions and methods. Clicking one adds a cell that calls it.
- *Cells for a function* are written from its signature: a `use` for the
  function's module, a `let` with `todo!()` for each argument (references
  become an owned value you borrow, so `&Config` gives `let config: Config`
  and passes `&config`), and the call. Methods get a receiver to fill in,
  and async functions are wrapped in `__hangar::block_on(…)`. Replace the
  `todo!()`s and press **Run** (⌘↵).
- *Output*: what the cell prints appears under it (stderr in red), then the
  value of its last line, pretty-printed with `{:#?}` and labelled with its
  type. A type without `Debug` shows just its name. End the last line with
  `;` to show nothing. `?` works in cells, and a panic or an error from `?`
  is reported. Compiler errors are shown with line numbers of the cell.
- *As you type*: each notebook starts its own rust-analyzer, so cells get
  completion (including adding a missing `use`), hover information and
  errors underlined before you run them. The header says when it is ready;
  in a large workspace the first load takes a while, since it reads the
  project's dependencies.
- *Formatting*: a cell is formatted with `rustfmt` (using your project's
  `rustfmt.toml`) each time it runs, or on demand with **Format** (⇧⌥F). A
  format is one edit, so ⌘Z undoes it. If the code doesn't parse yet, Run
  leaves it as written and the build explains the problem.
- *Cells* are independent: each runs on its own and they share no
  variables. Add more with **Add cell** (⇧⌘↵ adds one below the current
  cell), for example to try other arguments. The header links back to the
  function.

How it works: each run writes the cell into the `main` of a small,
throwaway Cargo project that depends on the function's library, builds it
with `cargo build`, and runs it from the package's directory. The project
lives in the workspace's target directory (`target/hangar-notebook/`) and
builds into the same target directory, with the workspace's `Cargo.lock`,
`[profile]` and `[patch]` settings, so your crate and its dependencies are
compiled once and shared with your normal builds. The first run of a
session builds whatever is out of date; later runs only rebuild the cell.
If the package depends on Tokio, async functions run on a Tokio runtime;
otherwise on a minimal executor.

For completion, the notebook's rust-analyzer is started in the throwaway
project. It sees each cell as a module of that crate (`src/cell_<n>.rs`),
with the cell as the body of a function, and a crate root that declares
those modules; Hangar sends it that text and maps positions back to the
cell's lines. It is set up not to run `cargo check` on save, and it uses
the workspace's target directory for the build scripts and proc macros it
needs, so it doesn't make a `target/` of its own.

**Cleanup.** The throwaway project's files — its sources, its binary, and
that binary's entries in `deps/`, `.fingerprint/` and `incremental/` — are
deleted when the notebook tab closes (a running cell and the notebook's
rust-analyzer are stopped first). Projects and build files left behind
because Hangar quit or crashed are deleted the next time any notebook
opens. Your crate's own build output is not touched, so it stays cached
like after any `cargo build`.

**Adding a language.** `crates/notebook` is language-neutral. A language
implements `Language` (which files it handles; prepare a notebook for the
function at the cursor or for the whole file, with a starting cell for each
of the file's functions) and returns a `Notebook` carrying a
`Kernel` (run a cell, stream `RunEvent`s, clean up when dropped). Add it to
`LANGUAGES` in `crates/notebook/src/lib.rs`; the right-click menu item, the
command and the notebook tab work for it unchanged. Optionally, the kernel
formats cells (`Kernel::format`) and the notebook carries a `CellAnalysis`
describing how a language server should see cells, which gives them
completion and errors. `crates/notebook/src/rust/`
is the example to follow.

## Architecture

```
crates/
  app           main executable and the Chromium helper executable
  ui            GPUI views: workspace, panes, editor, browser, terminal,
                explorer, source control, search, palette, agent panel
  browser       BrowserEngine/BrowserTab abstraction, the CEF off-screen
                backend (macOS), and the automation surface for the agent
  agents        Codex App Server client (JSON-RPC over stdio), Claude Code
                client (stream-json over stdio), typed protocol, event
                stream, transcript reducer, AgentBackend trait
  orchestrator  prompt checks, permission presets, approval rules, token
                budgets, turn scheduling, browser-tool policy, worktrees
  debugger      Debug Adapter Protocol client for lldb-dap, Cargo targets
                and builds, runnable lines in a file, breakpoints that
                follow edits, Rust type names and inline values
  notebook      notebooks: the Language/Kernel interface, and Rust support
                (reading a file's functions, writing starting cells,
                building and running cells in a throwaway crate,
                deleting its build files)
  editor        documents, file tree, fuzzy finder, search, file watcher,
                project detection, language server catalog, installer
                and client
  terminal      PTY + vt100 emulation, key encoding, URL detection
  git           git CLI wrapper and porcelain v2 status parser
  persistence   SQLite (WAL) with a typed, readable SQL builder; every write
                is one transaction
  common        app paths, ids, login-shell PATH discovery
xtask           `cargo xtask bundle`: builds, assembles and signs Hangar.app
packaging/macos entitlements for the app and the Chromium helpers; the app
                icon (Hangar.icns, drawn by make-icon.swift)
```

To redraw the icon after changing its colors in `make-icon.swift`:

```sh
swift packaging/macos/make-icon.swift /tmp/Hangar.iconset
iconutil -c icns /tmp/Hangar.iconset -o packaging/macos/Hangar.icns
```

**Browser.** CEF runs windowless: Chromium paints BGRA frames that the
browser view uploads as GPUI images, so pages clip, overlay and resize like
any other element (the command palette draws over a page). Input — mouse,
keys, IME composition, clipboard commands — is translated to CEF events.
Chromium's message loop is pumped from a `CFRunLoopTimer` because GPUI owns
`[NSApp run]`. Popups become tabs; the native context menu is suppressed.
The UI only talks to `BrowserEngine`/`BrowserTab`, so another engine could be
dropped in.

**Agents.** The agent panel talks to one provider at a time; the name in its
header (Codex or Claude) is a menu that switches between them. The choice,
and each provider's model and effort, are remembered globally, and each
provider keeps its own threads.

- *Codex*: each window starts `codex app-server` for its folder (when the
  agent panel is first shown) and talks
  JSON-RPC over stdio: threads, turns, streaming items, approvals, token
  usage, account and login.
- *Claude*: each thread runs `claude -p --input-format stream-json
  --output-format stream-json` (the protocol the Claude Agent SDK uses),
  started with `--session-id` and later `--resume`, so threads survive
  restarts and Claude Code's own history rebuilds the transcript. Permission
  prompts arrive as `can_use_tool` requests (`--permission-prompt-tool
  stdio`) and become the same approval cards as Codex's. Models: Claude Opus
  5.5 (`claude-opus-5-5`, the default), Sonnet 5.5, Haiku 5.5 and Fable 5.1,
  followed by anything else the installed Claude Code offers; effort maps to
  `--effort`. A change of model, effort or permissions restarts the thread's
  process with `--resume` before the next turn.
- *Signing in to Claude*: the panel runs `claude auth login` (claude.ai or
  `--console`) in a terminal tab, since that flow is interactive; Claude
  Code stores the credentials. Hangar reads `claude auth status`, notices a
  new sign-in by itself, and asks for a new one when a turn is refused with
  a 401 (an expired sign-in). An `ANTHROPIC_API_KEY`, Bedrock or Vertex set
  up for Claude Code needs no sign-in.

Hangar's orchestration layer sits between the panel and either agent:

- *Prompt policy*: size limits, secret detection (blocked by default),
  blocked and warned patterns.
- *Permissions*: Read only / Auto / Full access presets map to Codex's
  sandbox and approval policy, and to Claude Code's permission modes
  (`default`, `acceptEdits`, `bypassPermissions`); read-only commands can be
  auto-approved and dangerous ones auto-declined by rule, for both. Claude
  Code has no command sandbox here, so in Auto it asks before every command
  your Claude Code settings do not already allow.
- *Budgets*: soft and hard token limits per global/workspace/thread/model
  scope and day/week/month/total period, checked before each turn against
  usage recorded in SQLite.
- *Scheduling*: a cap on concurrent turns, with prompts queued per thread.
- *Browser tools*: the agent gets an `ide_browser` namespace (navigate,
  screenshot, read page text, read console, click, reload, back, list tabs):
  dynamic tools for Codex, an in-process MCP server (served over Claude
  Code's control channel) for Claude. Calls are checked against an origin allowlist (localhost
  by default), clicks and off-list navigation ask the user, and everything
  read from a page is returned inside an "untrusted data" block so the agent
  treats it as data, not instructions.
- *Durability*: every loggable event is appended to `agent_events`; threads
  are listed from SQLite (with their provider) and resumed from Codex's or
  Claude Code's own history after a restart. Claude reports usage per turn,
  recorded the same way for budgets.

**Storage.** `~/Library/Application Support/Hangar/` holds `hangar.sqlite3`
(workspaces, tabs, browser profiles and history, agent threads and events,
token usage, budgets, policies, settings), the Chromium profile and logs.
No secrets are stored there. `HANGAR_DATA_DIR` moves it. Data from before
the app was renamed (`Cockpit/`, `cockpit.sqlite3`) is moved here on first launch.

## Known limitations (v0.1)

- Debugging: a debugged program's output shows in the debug console, but
  it cannot read from stdin there (Run, in a terminal, can). Expressions are
  evaluated by LLDB, which knows Rust values but not Rust syntax: field
  paths, indexing and arithmetic work; method calls and closures do not.
  Only Rust/Cargo projects can be run and debugged so far.
- Notebooks: Rust only so far. They can call `pub` functions of library
  crates (functions in a binary's `main.rs` or `src/bin/`, in tests or in
  examples cannot be reached from another crate); a function in a private
  module needs a public path to it. Cells have completion, hover and errors,
  but not go to definition or quick fixes (⌘.). Each open notebook runs its
  own rust-analyzer, which costs memory in a large workspace. Notebooks are
  not reopened after a restart.
- `vendor/gpui-component` is gpui-component 0.5.1 with small patches for
  gutter decorations, the editor's right-click menu and bracket pairs (see
  its `COCKPIT_PATCHES.md`); it has to move with the pinned `gpui`.

- The product targets macOS. Everything but the browser also runs on Linux
  (for development); the browser needs macOS and the app bundle.
- Budgets, prompt patterns, approval rules and the browser-tool allowlist are
  enforced but have no settings screen yet; they live in the `budgets` and
  `agent_policies` tables (the permission preset and model are set from the
  agent panel).
- The orchestrator can create isolated git worktrees for parallel agents, but
  the UI runs one agent thread at a time per window.
- Switching provider stops the current provider's agent (a running turn is
  cut off); each provider's threads come back when you switch again.
- Not supported for Claude yet: Claude's question tool (`AskUserQuestion` is
  turned off, since a headless session cannot answer it), hooks, slash
  commands typed in the panel, and showing subagent (`Task`) steps; only the
  subagent's result appears. "Approve for session" keeps Claude's suggested
  rule for this session only, never in your Claude Code settings files.
  Interrupting a Claude turn is wired to Claude Code's `interrupt` request
  but has not been exercised against a live turn yet.
- Diffs open as read-only tabs; there is no side-by-side diff view yet.
- Chromium's own context menus, file pickers and print dialogs are not wired
  into the off-screen view.

## Environment variables

| Variable | Effect |
|---|---|
| `HANGAR_LOG` | log filter, e.g. `debug` or `cockpit_agents=trace` |
| `HANGAR_DATA_DIR` | data directory override |
| `HANGAR_SKIP_SHELL_ENV` | do not read `PATH` from the login shell |
| `HANGAR_REMOTE_DEBUGGING_PORT` | expose Chromium's DevTools protocol on localhost |
| `HANGAR_CEF_NO_SANDBOX` | run Chromium without its sandbox (debugging helper setups) |
| `HANGAR_CEF_MOCK_KEYCHAIN` | mock Keychain in release builds (always on in debug builds) |
| `HANGAR_CODEX_BIN` | `codex` executable (also used by the live tests) |
| `HANGAR_CLAUDE_BIN` | `claude` executable (also used by the live tests) |
| `HANGAR_LLDB_DAP` | `lldb-dap` executable for debugging |

## Credits

- The CEF integration follows the approach worked out in
  [bokuweb/gpui-cef](https://github.com/bokuweb/gpui-cef) (MIT OR Apache-2.0):
  retrofitting Chromium's application protocols onto GPUI's `NSApp`, the
  run-loop pump, and text input handling.
- Icons from [Lucide](https://lucide.dev) (ISC), see
  `assets/icons/LICENSE-lucide.txt`; the rest come with gpui-component.
- Fonts, as in Zed: [IBM Plex Sans](https://github.com/IBM/plex) for the
  interface and [Lilex](https://github.com/mishamyrt/Lilex) for code, both
  SIL Open Font License 1.1 (`assets/fonts/*/LICENSE.txt`). A code font set
  in the settings (`mono_font`) still takes precedence.
- Chromium Embedded Framework (BSD) via [cef-rs](https://github.com/tauri-apps/cef-rs).

## License

MIT OR Apache-2.0

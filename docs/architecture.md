# Architecture

How Hangar is put together: the repository layout, what each crate does
and which file does what, and how the main pieces (the browser, the agents,
storage, notebooks) work.

- [Repository layout](#repository-layout)
- [How the crates depend on each other](#how-the-crates-depend-on-each-other)
- [Crates, file by file](#crates-file-by-file)
- [The browser](#the-browser)
- [Agents and orchestration](#agents-and-orchestration)
- [Language servers](#language-servers)
- [Storage and secrets](#storage-and-secrets)
- [Notebooks](#notebooks)
- [Vendored gpui-component](#vendored-gpui-component)

## Repository layout

```
.
├── crates/            the application, split into focused crates (below)
├── xtask/             `cargo xtask bundle`: builds, assembles and signs Hangar.app
├── docs/              this documentation
├── scripts/
│   └── check.sh       tests and type-checks every crate; what PRs must pass
├── assets/
│   ├── icons/         Hangar's own SVG icons (Lucide, ISC licensed)
│   └── fonts/         IBM Plex Sans (interface) and Lilex (code), OFL 1.1
├── packaging/macos/   entitlements for the app and Chromium helpers, the
│                      app icon (Hangar.icns) and the script that draws it
├── vendor/
│   └── gpui-component/  gpui-component 0.5.1 with Hangar's patches
├── .cargo/config.toml the `xtask` alias and the default CEF_PATH
├── Cargo.toml         the workspace: members, shared dependency versions,
│                      build profiles, and the gpui-component patch
└── .cef/              (git-ignored) the Chromium download, created on first build
```

The crates are named `cockpit-*` (the project's earlier name); the app
binary is `hangar`.

```
crates/
├── app/           the `hangar` executable and the Chromium helper executable
├── ui/            every window, view and panel, built with GPUI
├── editor/        documents, file tree, search, project detection, language servers
├── agents/        clients for Codex, Claude Code and ChatGPT
├── orchestrator/  Hangar's policies around the agents: approvals, budgets, browser tools
├── browser/       the embedded Chromium browser and its automation API
├── debugger/      Debug Adapter Protocol client for lldb-dap, Cargo targets, breakpoints
├── notebook/      notebooks: the language-neutral interface and Rust support
├── terminal/      PTY-backed shells with a VT100 screen model
├── git/           git through the git CLI, and a status parser
├── tasks/         local issue boards and the read-only Linear client
├── github/        the read-only GitHub client and its sign-in
├── persistence/   the SQLite database and a typed SQL builder
└── common/        ids, on-disk paths, and the login-shell environment
```

## How the crates depend on each other

Lower layers know nothing about the ones above them. Only `ui` and `app`
depend on GPUI, so the rest can be tested without a display.

```
app ──► ui ──► orchestrator ──► agents, browser, git, persistence
         │
         ├──► notebook ──► debugger ──► editor
         ├──► editor, terminal, tasks, github
         └──► common (used by almost everything)
```

| Crate | Depends on |
|---|---|
| `app` | `ui`, `agents`, `browser`, `persistence`, `common` |
| `ui` | every other crate except `app` |
| `orchestrator` | `agents`, `browser`, `git`, `persistence` |
| `notebook` | `debugger`, `editor`, `common` |
| `debugger` | `editor`, `common` |
| `agents`, `browser`, `editor`, `tasks`, `github`, `terminal` | `common` |
| `common`, `git`, `persistence` | nothing internal |

## Crates, file by file

### `app`: the executables

| File | What it does |
|---|---|
| `src/main.rs` | Starts Hangar: logging, the login-shell environment, the database, Chromium, and the first window. Also serves `--chatgpt-access-token`, which Codex calls to get a token from Hangar's ChatGPT sign-in. |
| `src/bin/helper.rs` | `hangar-helper`, run inside the `Hangar Helper*.app` bundles for Chromium's renderer, GPU and utility processes. Never run by hand. |

Features: `runtime-shaders` (on by default) compiles GPUI's Metal shaders
at first launch so the build doesn't need the Metal compiler.
`cef-typecheck` type-checks the macOS build from another host.

### `ui`: the interface

Everything you see. One `Workspace` per window, wiring the panels below
together.

| File or folder | What it does |
|---|---|
| `workspace.rs` | A window onto one folder: sidebar, editor panes, terminal, agent panel, title and status bars, and the commands that connect them. Also holds the per-folder layout and trust setting. |
| `welcome.rs` | The window shown when no folder is open: recent folders and Open Folder. |
| `state.rs` | App-wide state shared by every window: paths, the database, settings, the browser engine and the agent runtime. |
| `actions.rs` | Every command and its default key binding. |
| `palette.rs` | The command palette (⇧⌘P), quick open (⌘P) and the project task list. |
| `pane.rs` | A row of tabs (files, diffs, browser pages) and the active one. |
| `editor_view.rs` | A code editor tab bound to a file. |
| `explorer.rs` | The file tree, loading folders lazily. |
| `search_panel.rs` | Find in files. |
| `source_control.rs` | Branch, staged and unstaged changes, and committing. |
| `terminal_view.rs` | The terminal panel: tabs of shells drawn from VT100 snapshots. |
| `browser_view.rs` | A browser tab: toolbar plus the page, drawn from Chromium's frames. |
| `lsp.rs` | Language servers for a workspace: detects each file's project, starts and restarts servers, collects diagnostics, and connects editors to them. |
| `notebook.rs` | A notebook tab: cells, their output and returned values. |
| `agent/` | The agent panel. `controller.rs` holds the connection, threads, turns, approvals and usage; `panel.rs` draws the feed of thread cards; `feed.rs` decides which threads show and in what order; `mention.rs` handles `@claude` / `@codex` / `@chatgpt` mentions. |
| `chat/` | Chat mode: `history.rs` is the conversation list, `diff_panel.rs` the panel of changed files beside the chat, `diff.rs` the diff splitting behind it. |
| `debug/` | Running and debugging: `mod.rs` holds breakpoints, run configurations and the session; `panel.rs` is the debug panel; `editor.rs` draws breakpoints, run icons, the paused line and inline values in the editor. |
| `tasks/` | The Tasks view: `model.rs` (the local board and Linear state), `sidebar.rs`, `view.rs` (issue lists, projects, details), `widgets.rs` (status circles, priority bars). |
| `github/` | The GitHub view: `model.rs` (sign-in, repositories, what's open), `sidebar.rs` (repository list), `view.rs` (code, pull requests, issues, Actions), `widgets.rs` (state marks, labels). |
| `theme.rs`, `appearance.rs` | Colours derived from the accent you pick, and the appearance popover. |
| `chrome.rs` | Small shared pieces (labels, chips, tabs) so they look the same everywhere. |
| `assets.rs` | Icons: gpui-component's set plus Hangar's own. |

### `editor`: files and language intelligence

Everything about files that isn't drawing.

| File | What it does |
|---|---|
| `document.rs` | A file opened for editing: load, save, reload, line endings, and noticing changes made outside Hangar. |
| `fs_tree.rs` | Directory listing for the explorer. |
| `fuzzy.rs` | The fuzzy matcher behind quick open and the command palette. |
| `search.rs` | Find in files, respecting `.gitignore`. |
| `watcher.rs` | Recursive file watching, with noisy events merged. |
| `language.rs` | Which highlighter and which language server language a file gets. |
| `project.rs` | Detects a file's project by walking up to the markers each ecosystem leaves (lockfiles, `package.json`, `go.mod`…): runtime, package manager, frameworks, linters, scripts and tasks. |
| `lsp/mod.rs` | The Language Server Protocol client: one per server process. |
| `lsp/codec.rs` | LSP message framing (`Content-Length` headers). |
| `lsp/registry.rs` | The catalog of servers: which ones handle a file, how to find each executable, and its settings. Enforces workspace trust. |
| `lsp/install.rs` | Installs servers with npm, Bun or `go install` into a folder Hangar owns. |

### `agents`: talking to coding agents

| File or folder | What it does |
|---|---|
| `backend.rs` | The `AgentBackend` trait the UI and orchestrator use, so they don't care which agent is behind it. |
| `client.rs`, `rpc.rs`, `protocol.rs` | Codex: the connection to `codex app-server`, its JSON-RPC dialect over stdio, and the typed protocol messages. |
| `claude/` | Claude Code run headless: `mod.rs` starts it, `mapper.rs` maps its stream-json output to Hangar's events, `history.rs` rebuilds a thread after a restart, `login.rs` runs the sign-in, `models.rs` lists models and accounts. |
| `chatgpt/` | Hangar's own Sign in with ChatGPT (`mod.rs`), where the tokens are kept (`store.rs`), and plain `@chatgpt` conversations (`chat.rs`). |
| `events.rs` | `AgentEvent`: the one event type the UI, orchestrator and event log all consume, whichever agent produced it. |
| `transcript.rs` | Folds events into the conversation the UI shows. Pure, no I/O. |

### `orchestrator`: Hangar's rules around the agents

| File | What it does |
|---|---|
| `policy.rs` | Checks before a prompt is sent: size limits, secret detection, blocked patterns. |
| `approvals.rs` | Rules that answer some approval requests without asking (auto-approve read-only commands, decline dangerous ones). |
| `budget.rs` | Token budgets: soft warnings, and hard limits checked before each turn. |
| `scheduler.rs` | A cap on concurrent turns, with prompts queued per thread. |
| `browser_tools.rs` | The browser tools the agent can call, and which calls are allowed (origin allowlist, asking before clicks). |
| `worktrees.rs` | Git worktrees so parallel agents don't edit the same checkout. |

### `browser`: the embedded browser

| File or folder | What it does |
|---|---|
| `engine.rs` | `BrowserEngine` / `BrowserTab`: the abstraction the UI uses, so another engine could replace Chromium. |
| `cef/` | Chromium in off-screen mode on macOS: `mod.rs` (start-up), `tab.rs` (one browser), `handlers.rs` (Chromium's callbacks), `shared.rs` (state shared with the UI), `pump.rs` (driving Chromium's message loop from GPUI's run loop), `nsapp.rs` (making GPUI's `NSApplication` acceptable to Chromium). |
| `frame.rs` | Rendered page frames. |
| `input.rs` | Mouse and keyboard events translated for Chromium. |
| `address.rs` | Turning what you type in the address bar into a URL. |
| `automation.rs` | What the agent can do in the browser (navigate, read, screenshot, click). Whether it's allowed is up to the orchestrator. |

### `debugger`: running and debugging Rust

| File | What it does |
|---|---|
| `client.rs`, `protocol.rs` | The Debug Adapter Protocol client and its typed messages. |
| `adapter.rs` | Finds `lldb-dap` and sets up LLDB's Rust formatters. |
| `cargo.rs` | Cargo targets and run configurations, building for the debugger, and finding runnable lines (`fn main`, tests). |
| `breakpoints.rs` | Breakpoints that stay on their line while you edit. |
| `values.rs` | Rust-looking type names, inline values beside the code, and the value under the mouse. |

### `notebook`: notebooks

| File | What it does |
|---|---|
| `lib.rs` | The language-neutral `Language` / `Notebook` / `Kernel` interface, and the list of languages. |
| `rust/mod.rs` | Rust support. |
| `rust/signature.rs` | Finds the function at the cursor and reads its signature. |
| `rust/cell.rs` | Writes the first cell from a signature, and turns a cell into a program. |
| `rust/scratch.rs` | The throwaway Cargo project cells are built in, and its cleanup. |
| `rust/format.rs` | Formats a cell with `rustfmt`. |
| `rust/analysis.rs` | Shows cells to rust-analyzer for completion and errors. |

### `terminal`: shells

| File | What it does |
|---|---|
| `lib.rs` | A terminal session: a PTY running your shell, read on a background thread. |
| `snapshot.rs` | A copy of the visible screen the UI draws from. |
| `keys.rs` | Key presses as the byte sequences terminal programs expect. |
| `urls.rs` | Spots dev-server URLs in output so the IDE can offer to open them. |

### `git`, `tasks`, `github`, `persistence`, `common`

| Crate | Files |
|---|---|
| `git` | `lib.rs` runs the `git` CLI (status, diffs, commits, worktrees), so behaviour matches your terminal; `status.rs` parses `git status --porcelain=v2`. |
| `tasks` | `model.rs` (projects and issues, whatever their source), `local.rs` (the per-folder board, saved as JSON in Hangar's data directory), `linear.rs` (read-only GraphQL pull), `oauth.rs` (Linear browser sign-in with PKCE), `key.rs` (keeping the API key or tokens in the Keychain). |
| `github` | `auth.rs` (device-flow browser sign-in and the `gh` CLI login), `token.rs` (the token in the Keychain), `api.rs` (read-only REST client), `model.rs` (repositories, pull requests, issues, runs). |
| `persistence` | `lib.rs` (the database, one SQLite connection in WAL mode), `schema.rs` (migrations and tables), `sql.rs` (a typed SQL builder), `service.rs` (a database thread, so the UI never waits on disk). |
| `common` | `paths.rs` (where Hangar keeps its files), `shell_env.rs` (reading `PATH` from your login shell, and common tool folders), `ids.rs` (typed ids), `startup.rs` (start-up timing). |

### `xtask`: packaging

`cargo xtask bundle` builds both executables and assembles the app, since
Chromium only runs from a bundle:

```
Hangar.app/Contents/
  Info.plist
  MacOS/Hangar
  Resources/Hangar.icns
  Frameworks/Chromium Embedded Framework.framework
  Frameworks/Hangar Helper.app   (and GPU, Renderer, Plugin, Alerts variants)
```

It then signs the bundle: ad hoc by default, or with `--sign` using the
hardened runtime and the entitlements in `packaging/macos`.

To redraw the icon after changing its colours in `make-icon.swift`:

```bash
swift packaging/macos/make-icon.swift /tmp/Hangar.iconset
```

```bash
iconutil -c icns /tmp/Hangar.iconset -o packaging/macos/Hangar.icns
```

## The browser

Chromium (via CEF) runs windowless: it paints BGRA frames that the browser
view uploads as GPUI images, so pages clip, overlay and resize like any
other element (the command palette draws over a page). Mouse, keyboard,
IME composition and clipboard commands are translated to CEF events.
Chromium's message loop is pumped from a `CFRunLoopTimer`, because GPUI
owns `[NSApp run]`. Popups become tabs, and the native context menu is
suppressed.

Startup does only what the first frame needs: Chromium starts when the
first browser tab opens.

## Agents and orchestration

The agent panel shows one feed of thread cards. The provider (Codex or
Claude) is picked from the panel header, and `@claude`, `@codex` or
`@chatgpt` in the composer starts a thread with another one. Each
provider's model and effort are remembered.

- **Codex.** Each window starts `codex app-server` for its folder, when
  the agent panel is first shown, and talks JSON-RPC over stdio: threads,
  turns, streaming items, approvals, token usage, account and login.
- **Claude.** Each thread runs `claude -p --input-format stream-json
  --output-format stream-json` (the protocol the Claude Agent SDK uses),
  started with `--session-id` and later `--resume`, so threads survive
  restarts. Permission prompts arrive as `can_use_tool` requests and become
  the same approval cards as Codex's. Models: Claude Opus 5.5 (the
  default), Sonnet 5.5, Haiku 5.5 and Fable 5.1, plus anything else the
  installed Claude Code offers. A change of model, effort or permissions
  restarts the thread's process with `--resume` before the next turn.
- **ChatGPT.** `@chatgpt` asks ChatGPT itself (no tools) on your ChatGPT
  plan, streaming from `api.openai.com/v1/responses`.

The orchestrator sits between the panel and every agent:

- **Prompt policy**: size limits, secret detection (blocked by default),
  blocked and warned patterns.
- **Permissions**: Read only / Auto / Full access presets map to Codex's
  sandbox and approval policy, and to Claude Code's permission modes.
  Read-only commands can be auto-approved and dangerous ones
  auto-declined by rule. Claude Code has no command sandbox here, so in
  Auto it asks before every command your Claude Code settings don't
  already allow.
- **Budgets**: soft and hard token limits per global, workspace, thread or
  model scope and per day, week, month or total, checked before each turn
  against usage recorded in SQLite.
- **Scheduling**: a cap on concurrent turns, with prompts queued per
  thread.
- **Browser tools**: an `ide_browser` toolset (navigate, screenshot, read
  page text, read console, click, reload, back, list tabs), as dynamic
  tools for Codex and an in-process MCP server for Claude. Calls are
  checked against an origin allowlist (localhost by default), clicks and
  off-list navigation ask you, and page content is returned inside an
  "untrusted data" block so the agent treats it as data, not instructions.
- **Durability**: every event is appended to `agent_events`; threads are
  listed from SQLite and resumed from Codex's or Claude Code's own history
  after a restart.

## Language servers

Opening a file runs `editor::project` off the main thread to detect its
project, then `lsp::registry::plan` picks the servers: at most one primary
server (completion, hover, definitions) plus linters and add-ons, each
started once per project root. `ui::lsp::LspManager` starts them, keeps
their diagnostics, and attaches editors.

Executables are looked for in the project's `node_modules/.bin` (trusted
workspaces only), then `PATH`, common tool folders, and Hangar's managed
install folder. **Workspace trust** is enforced in `ToolDirs::trust_project`
and `ServerSpec::runs_project_code`: until a folder is trusted, servers
never use the project's own executables, TypeScript or plugins, and
servers that run project code (rust-analyzer, ESLint, Tailwind, Svelte,
Vue, Astro, golangci-lint, Ruby LSP, zls, SourceKit-LSP) don't start.
Tools are only ever taken from inside the workspace, never from folders
above it.

## Storage and secrets

`~/Library/Application Support/Hangar/` (or `HANGAR_DATA_DIR`) holds:

- `hangar.sqlite3`: workspaces and their layout, tabs, browser profiles
  and history, agent threads and events, token usage, budgets, policies
  and settings
- `tasks/`: each workspace's local issue board, one JSON file per folder
- `language-servers/`: servers installed by Hangar
- `chatgpt-threads/`: `@chatgpt` conversations
- the Chromium profile and logs

Secrets are never stored there on macOS. The ChatGPT sign-in, the GitHub
token and the Linear key or tokens live in the macOS Keychain. On Linux,
used only for development, they are owner-only files in the data
directory. Claude Code and Codex's own sign-ins are kept by those tools.

Data from before the app was renamed (`Cockpit/`, `cockpit.sqlite3`) is
moved on first launch.

## Notebooks

Each run writes the cell into the `main` of a small throwaway Cargo
project that depends on the function's library, builds it with `cargo
build`, and runs it from the package's directory. The project lives in the
workspace's target directory (`target/hangar-notebook/`) and builds into
the same target directory, with the workspace's `Cargo.lock`, `[profile]`
and `[patch]` settings, so your crate and its dependencies are compiled
once and shared with your normal builds. If the package depends on Tokio,
async functions run on a Tokio runtime; otherwise on a minimal executor.

For completion, each notebook starts its own rust-analyzer in the
throwaway project. Each cell is a module of that crate (`src/cell_<n>.rs`)
with the cell as a function body, and Hangar maps positions back to the
cell's lines. It doesn't run `cargo check` on save, and it shares the
workspace's target directory.

The throwaway project's files are deleted when the notebook tab closes.
Leftovers from a crash are deleted the next time any notebook opens. Your
crate's own build output is left alone.

**Adding a language.** `crates/notebook` is language-neutral. A language
implements `Language` (which files it handles, and how to prepare a
notebook for the function at the cursor or for a whole file) and returns a
`Notebook` carrying a `Kernel` (run a cell, stream `RunEvent`s, clean up
when dropped). Add it to `LANGUAGES` in `crates/notebook/src/lib.rs`; the
right-click menu, the command and the notebook tab work unchanged.
Optionally, the kernel formats cells (`Kernel::format`), and a
`CellAnalysis` tells a language server how to see cells, for completion
and errors. `crates/notebook/src/rust/` is the example to follow.

## Vendored gpui-component

`vendor/gpui-component` is gpui-component 0.5.1 with small patches: gutter
decorations for the debugger, extra right-click menu items, bracket
pairing, IntelliJ-style completion, and highlighting performance fixes.
Each change is marked `cockpit patch` and listed in
`vendor/gpui-component/COCKPIT_PATCHES.md`. It has to move together with
the pinned `gpui` version in the workspace `Cargo.toml`.

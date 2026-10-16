# Cockpit

A native macOS IDE written in Rust: a [GPUI](https://www.gpui.rs) shell with
[gpui-component](https://github.com/longbridge/gpui-component), a real
Chromium browser (CEF, off-screen rendering) inside the window, a terminal,
git, and OpenAI's Codex agent driven through the Codex App Server — with an
orchestration layer of its own for permissions, budgets, approvals and the
browser tools Codex can use.

v0.1 covers: file explorer and editor (tree-sitter highlighting, language
servers), browser tabs, terminals, source control, find in files, a command
palette, and a Codex panel where you prompt, watch the turn stream, approve
commands and edits, and interrupt.

## Requirements

- macOS 12 or later (Apple Silicon or Intel)
- Rust 1.88+ (`rustup` recommended)
- Xcode Command Line Tools (`xcode-select --install`)
- The Codex CLI for the agent: `npm install -g @openai/codex` or
  `brew install codex`, then sign in from the Codex panel (ChatGPT or an API
  key; credentials stay with Codex, Cockpit never stores them)
- Optional language servers on your `PATH`: `rust-analyzer`,
  `typescript-language-server`, `pyright-langserver`, `gopls`, `clangd`,
  `zls`, `vscode-{css,html,json}-language-server`

## Build and run

```sh
cargo xtask bundle --run          # debug build, run in this terminal (logs on stderr)
cargo xtask bundle --release --open
cargo xtask bundle --release --sign "Developer ID Application: …"   # hardened runtime
```

The first build downloads the Chromium Embedded Framework (~500 MB) into
`.cef/` (see `.cargo/config.toml`; export `CEF_PATH` to share one download
between checkouts). The app is assembled at
`target/<profile>/bundle/Cockpit.app`.

Chromium only works from the app bundle — it locates its framework and the
`Cockpit Helper*.app` processes through the bundle layout. A plain
`cargo run` starts the IDE with the browser disabled and a note explaining
why, which is handy when working on non-browser UI.

Pass a folder to open it: `cargo xtask bundle --run -- ~/src/project`; an
installed app also opens folders dropped on its Dock icon or given to
`open -a Cockpit <folder>`. Otherwise Cockpit reopens the last folder, or
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
driver (Mesa's lavapipe works). The live Codex test is
opt-in: `COCKPIT_CODEX_BIN=$(which codex) cargo test -p cockpit-agents --test
live_codex` (add `COCKPIT_CODEX_LIVE_TURN=1` to run a real turn).

## Keys

| | |
|---|---|
| ⌘P / ⇧⌘P | Go to file / command palette |
| ⌘O | Open folder |
| ⌘S, ⌘W | Save, close tab |
| ⇧⌘[ ⇧⌘] | Previous / next tab |
| ⌘\\ | Split editor |
| ⌘B, ⌘J, ⌥⌘B | Toggle sidebar, terminal, Codex |
| ⇧⌘E, ⇧⌘G, ⇧⌘F | Explorer, source control, find in files |
| ⌘T | New browser tab (⌘L address bar, ⌘R reload, ⌘[ ⌘] back/forward, ⌥⌘I DevTools) |
| ⌃\` | New terminal |
| ⌘L | Ask Codex (Enter sends, ⇧Enter newline, ⌃C stops the turn) |
| ⇧⌘N | New Codex thread |

## Architecture

```
crates/
  app           main executable and the Chromium helper executable
  ui            GPUI views: workspace, panes, editor, browser, terminal,
                explorer, source control, search, palette, agent panel
  browser       BrowserEngine/BrowserTab abstraction, the CEF off-screen
                backend (macOS), and the automation surface for the agent
  agents        Codex App Server client (JSON-RPC over stdio), typed protocol,
                event stream, transcript reducer, AgentBackend trait
  orchestrator  prompt checks, permission presets, approval rules, token
                budgets, turn scheduling, browser-tool policy, worktrees
  editor        documents, file tree, fuzzy finder, search, file watcher,
                language server client
  terminal      PTY + vt100 emulation, key encoding, URL detection
  git           git CLI wrapper and porcelain v2 status parser
  persistence   SQLite (WAL) with a typed, readable SQL builder; every write
                is one transaction
  common        app paths, ids, login-shell PATH discovery
xtask           `cargo xtask bundle`: builds, assembles and signs Cockpit.app
packaging/macos entitlements for the app and the Chromium helpers
```

**Browser.** CEF runs windowless: Chromium paints BGRA frames that the
browser view uploads as GPUI images, so pages clip, overlay and resize like
any other element (the command palette draws over a page). Input — mouse,
keys, IME composition, clipboard commands — is translated to CEF events.
Chromium's message loop is pumped from a `CFRunLoopTimer` because GPUI owns
`[NSApp run]`. Popups become tabs; the native context menu is suppressed.
The UI only talks to `BrowserEngine`/`BrowserTab`, so another engine could be
dropped in.

**Agent.** Each window starts `codex app-server` for its folder and talks
JSON-RPC over stdio: threads, turns, streaming items, approvals, token usage,
account and login. Cockpit's orchestration layer sits in between:

- *Prompt policy*: size limits, secret detection (blocked by default),
  blocked and warned patterns.
- *Permissions*: Read only / Auto / Full access presets map to Codex's
  sandbox and approval policy; read-only commands can be auto-approved and
  dangerous ones auto-declined by rule.
- *Budgets*: soft and hard token limits per global/workspace/thread/model
  scope and day/week/month/total period, checked before each turn against
  usage recorded in SQLite.
- *Scheduling*: a cap on concurrent turns, with prompts queued per thread.
- *Browser tools*: Codex gets an `ide_browser` namespace (navigate,
  screenshot, read page text, read console, click, reload, back, list tabs)
  as dynamic tools. Calls are checked against an origin allowlist (localhost
  by default), clicks and off-list navigation ask the user, and everything
  read from a page is returned inside an "untrusted data" block so the agent
  treats it as data, not instructions.
- *Durability*: every loggable event is appended to `agent_events`; threads
  are listed from SQLite and resumed from Codex's own history after a
  restart.

**Storage.** `~/Library/Application Support/Cockpit/` holds `cockpit.sqlite3`
(workspaces, tabs, browser profiles and history, agent threads and events,
token usage, budgets, policies, settings), the Chromium profile and logs.
No secrets are stored there. `COCKPIT_DATA_DIR` moves it.

## Known limitations (v0.1)

- The product targets macOS. Everything but the browser also runs on Linux
  (for development); the browser needs macOS and the app bundle.
- Budgets, prompt patterns, approval rules and the browser-tool allowlist are
  enforced but have no settings screen yet; they live in the `budgets` and
  `agent_policies` tables (the permission preset and model are set from the
  Codex panel).
- The orchestrator can create isolated git worktrees for parallel agents, but
  the UI runs one Codex thread at a time per window.
- Diffs open as read-only tabs; there is no side-by-side diff view yet.
- Chromium's own context menus, file pickers and print dialogs are not wired
  into the off-screen view.

## Environment variables

| Variable | Effect |
|---|---|
| `COCKPIT_LOG` | log filter, e.g. `debug` or `cockpit_agents=trace` |
| `COCKPIT_DATA_DIR` | data directory override |
| `COCKPIT_SKIP_SHELL_ENV` | do not read `PATH` from the login shell |
| `COCKPIT_REMOTE_DEBUGGING_PORT` | expose Chromium's DevTools protocol on localhost |
| `COCKPIT_CEF_NO_SANDBOX` | run Chromium without its sandbox (debugging helper setups) |
| `COCKPIT_CEF_MOCK_KEYCHAIN` | mock Keychain in release builds (always on in debug builds) |
| `COCKPIT_CODEX_BIN` | `codex` executable for the live tests |

## Credits

- The CEF integration follows the approach worked out in
  [bokuweb/gpui-cef](https://github.com/bokuweb/gpui-cef) (MIT OR Apache-2.0):
  retrofitting Chromium's application protocols onto GPUI's `NSApp`, the
  run-loop pump, and text input handling.
- Icons from [Lucide](https://lucide.dev) (ISC), see
  `assets/icons/LICENSE-lucide.txt`; the rest come with gpui-component.
- Chromium Embedded Framework (BSD) via [cef-rs](https://github.com/tauri-apps/cef-rs).

## License

MIT OR Apache-2.0

# Cockpit

A native macOS IDE written in Rust: a [GPUI](https://www.gpui.rs) shell with
[gpui-component](https://github.com/longbridge/gpui-component), a real
Chromium browser (CEF, off-screen rendering) inside the window, a terminal,
git, and two coding agents you can switch between: OpenAI's Codex, driven
through the Codex App Server, and Anthropic's Claude, driven through Claude
Code — with an orchestration layer of its own for permissions, budgets,
approvals and the browser tools the agents can use.

v0.1 covers: file explorer and editor (tree-sitter highlighting, language
servers), browser tabs, terminals, source control, find in files, a command
palette, and an agent panel (Codex or Claude) where you prompt, watch the
turn stream, approve commands and edits, and interrupt.

## Requirements

- macOS 12 or later (Apple Silicon or Intel)
- Rust 1.88+ (`rustup` recommended)
- Xcode Command Line Tools (`xcode-select --install`)
- CMake and Ninja, used to build the Chromium wrapper: `brew install cmake ninja`
- At least one agent CLI (credentials stay with the CLI; Cockpit never
  stores them):
  - Codex: `npm install -g @openai/codex` or `brew install codex`, then sign
    in from the agent panel (ChatGPT or an OpenAI API key).
  - Claude Code: `curl -fsSL https://claude.ai/install.sh | bash` or
    `npm install -g @anthropic-ai/claude-code`, then sign in from the agent
    panel (a Claude plan, an Anthropic Console account, or
    `ANTHROPIC_API_KEY` / Bedrock / Vertex configured for Claude Code).
- For Rust IDE features (completion, go to definition, references, hover,
  quick fixes, `cargo check` diagnostics): `rustup component add
  rust-analyzer`. The `rust-analyzer` in `~/.cargo/bin` is only rustup's
  proxy until the component is installed; Cockpit notices and says so in
  the status bar.
- Optional language servers for other languages on your `PATH`:
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
driver (Mesa's lavapipe works). The live agent tests are opt-in:
`COCKPIT_CODEX_BIN=$(which codex) cargo test -p cockpit-agents --test
live_codex` (add `COCKPIT_CODEX_LIVE_TURN=1` to run a real turn) and
`COCKPIT_CLAUDE_BIN=$(which claude) cargo test -p cockpit-agents --test
live_claude` (version, account and models, which bill nothing; add
`COCKPIT_CLAUDE_LIVE_TURN=1` for a real turn on Claude Haiku 5.5, or set
`COCKPIT_CLAUDE_LIVE_MODEL`).

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
| ⌘L | Ask the agent (Enter sends, ⇧Enter newline, ⌃C stops the turn) |
| ⇧⌘N | New agent thread |

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

**Agents.** The agent panel talks to one provider at a time; the name in its
header (Codex or Claude) is a menu that switches between them. The choice,
and each provider's model and effort, are remembered globally, and each
provider keeps its own threads.

- *Codex*: each window starts `codex app-server` for its folder and talks
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
  Code stores the credentials. Cockpit reads `claude auth status`, notices a
  new sign-in by itself, and asks for a new one when a turn is refused with
  a 401 (an expired sign-in). An `ANTHROPIC_API_KEY`, Bedrock or Vertex set
  up for Claude Code needs no sign-in.

Cockpit's orchestration layer sits between the panel and either agent:

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
| `COCKPIT_LOG` | log filter, e.g. `debug` or `cockpit_agents=trace` |
| `COCKPIT_DATA_DIR` | data directory override |
| `COCKPIT_SKIP_SHELL_ENV` | do not read `PATH` from the login shell |
| `COCKPIT_REMOTE_DEBUGGING_PORT` | expose Chromium's DevTools protocol on localhost |
| `COCKPIT_CEF_NO_SANDBOX` | run Chromium without its sandbox (debugging helper setups) |
| `COCKPIT_CEF_MOCK_KEYCHAIN` | mock Keychain in release builds (always on in debug builds) |
| `COCKPIT_CODEX_BIN` | `codex` executable (also used by the live tests) |
| `COCKPIT_CLAUDE_BIN` | `claude` executable (also used by the live tests) |

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

# Getting started

This page covers everything you need to build, run and test Hangar: the
required dependencies, the optional tools that turn on more features, and
what to do on first launch.

- [Requirements](#requirements)
- [Optional tools](#optional-tools)
- [Build and run](#build-and-run)
- [First launch](#first-launch)
- [Testing](#testing)
- [Environment variables](#environment-variables)
- [Troubleshooting](#troubleshooting)

## Requirements

| Dependency | Why | Install |
|---|---|---|
| macOS 12 or later | Hangar is a macOS app (Apple Silicon or Intel) | |
| Rust 1.88 or newer | Builds the app | [rustup](https://rustup.rs), then `rustup update stable` |
| Xcode Command Line Tools | Compiler, linker, `codesign` and `lldb-dap` | `xcode-select --install` |
| CMake and Ninja | Build the Chromium wrapper | `brew install cmake ninja` |
| About 2 GB of free disk | Chromium download (~500 MB) plus the build | |

The first build downloads the Chromium Embedded Framework into `.cef/` in
the checkout. To share one download between checkouts, export `CEF_PATH`
pointing at a folder of your choice. `.cargo/config.toml` sets the default.

## Optional tools

Hangar works without these, but each turns on a feature.

**Coding agents** (install at least one to use the agent panel)

| Tool | Install | Signing in |
|---|---|---|
| Codex | `npm install -g @openai/codex` or `brew install codex`. Hangar also finds the copy inside the ChatGPT desktop app. | From the agent panel. *Continue with ChatGPT* is Hangar's own sign-in: tokens stay in the macOS Keychain, and Codex asks Hangar for an access token when it needs one. Codex's own sign-in (ChatGPT or an OpenAI API key, kept in `~/.codex`) is also offered. |
| Claude Code | `curl -fsSL https://claude.ai/install.sh \| bash` or `npm install -g @anthropic-ai/claude-code` | From the agent panel: a Claude plan, an Anthropic Console account, or `ANTHROPIC_API_KEY` / Bedrock / Vertex configured for Claude Code. Claude Code keeps its own credentials; Hangar never stores them. |

**Rust**

| Tool | Feature | Install |
|---|---|---|
| rust-analyzer | Completion, go to definition, hover, quick fixes and `cargo check` diagnostics | `rustup component add rust-analyzer` |
| rust-src | Stepping into the standard library while debugging | `rustup component add rust-src` |
| lldb-dap | Debugging | Included with the Xcode Command Line Tools; Hangar finds it with `xcrun` |

**Other languages.** Language servers are picked per file from the
project around it. You can install them yourself, or run **Install Missing
Language Servers** from the command palette (⇧⌘P). That installs them into
`~/Library/Application Support/Hangar/language-servers`, using:

| Tool | Needed to install | Install |
|---|---|---|
| Node.js (npm) or Bun | TypeScript, Vue, Svelte, Astro, Angular, ESLint, Tailwind, CSS/HTML/JSON/YAML/Bash servers | [nodejs.org](https://nodejs.org) or [bun.sh](https://bun.sh) |
| Go | gopls, golangci-lint-langserver, templ | [go.dev/dl](https://go.dev/dl) |

Servers for Python (pyright), C/C++ (clangd), Swift (sourcekit-lsp), Zig
(zls), Ruby (ruby-lsp) and TOML (taplo) are used when installed, but Hangar
doesn't install them for you.

**Integrations**

| Tool | Feature |
|---|---|
| GitHub CLI (`gh`) | Sign in to the GitHub view with your existing `gh` login. Browser sign-in and personal access tokens work without it. |
| Xcode (or its Metal toolchain) | Release builds ship precompiled shaders. Without it, shaders compile on first launch. |

## Build and run

Clone the repository:

```bash
git clone https://github.com/RatikGambhir/code.git hangar
```

Build the app bundle and run it in your terminal, with logs on stderr:

```bash
cargo xtask bundle --run
```

Open a folder straight away by passing it after `--`:

```bash
cargo xtask bundle --run -- ~/src/my-project
```

Build an optimized release and open it:

```bash
cargo xtask bundle --release --open
```

Sign a release with a Developer ID (hardened runtime):

```bash
cargo xtask bundle --release --sign "Developer ID Application: Your Name (TEAMID)"
```

The app is assembled at `target/<profile>/bundle/Hangar.app`. Release
builds use whole-program LTO in one codegen unit, so linking takes a few
minutes longer.

| Command | What it does |
|---|---|
| `cargo xtask bundle` | Debug build of `Hangar.app` |
| `--release` | Optimized build |
| `--target <triple>` | Build for `aarch64-apple-darwin` or `x86_64-apple-darwin` |
| `--sign <identity>` | Sign with a Developer ID and the hardened runtime (ad hoc otherwise) |
| `--open` | Open the app when the build is done |
| `--run` | Run the app in this terminal |
| `cargo run` | Run without bundling. The browser is disabled, since Chromium only works from the app bundle. Handy for UI work. |

An installed app also opens folders dropped on its Dock icon, or given to
`open -a Hangar <folder>`. Otherwise it reopens the last folder or shows
the welcome window.

## First launch

1. **Open a folder** with ⌘O.
2. **Trust the folder if it's yours.** Language servers that would run the
   project's own code (its `node_modules` tools, ESLint or Tailwind config,
   Rust build scripts) stay off until you run **Trust This Workspace** from
   the command palette. A notice tells you when something is waiting. See
   [Workspace trust](guide.md#workspace-trust).
3. **Install missing language servers** from the command palette if the
   status bar says one is missing.
4. **Sign in to an agent** from the agent panel (⌥⌘B).
5. **Connect GitHub or Linear** (optional) from the GitHub and Tasks
   buttons in the left rail.

## Testing

Run the checks every pull request should pass:

```bash
scripts/check.sh
```

This runs every crate's tests, type-checks the app, and type-checks the
Chromium backend for `aarch64-apple-darwin` without downloading Chromium.
It works on macOS and Linux.

Live tests drive real tools and are opt-in:

| Test | How to run |
|---|---|
| Codex | `HANGAR_CODEX_BIN=$(which codex) cargo test -p cockpit-agents --test live_codex` (add `HANGAR_CODEX_LIVE_TURN=1` to run a real turn) |
| Claude Code | `HANGAR_CLAUDE_BIN=$(which claude) cargo test -p cockpit-agents --test live_claude` checks version, account and models, which bill nothing. Add `HANGAR_CLAUDE_LIVE_TURN=1` for a real turn, and `HANGAR_CLAUDE_LIVE_MODEL` to pick the model. |
| rust-analyzer in notebooks | `cargo test -p cockpit-notebook --test rust_analyzer_live -- --ignored` |
| gopls and TypeScript | `cargo test -p cockpit-editor --test live_go_and_typescript`. Set `HANGAR_LANGUAGE_SERVERS=<dir>` to use managed installs, and `HANGAR_TEST_INSTALL=1` to install missing servers there first. |
| lldb-dap | `cargo test -p cockpit-debugger --test live_lldb` (skipped when `lldb-dap` isn't installed) |

**On Linux**, everything except the browser builds and runs, which is
useful for headless testing. You need the X11/Wayland and Vulkan
development packages GPUI links against. To run the UI headless, use
`xvfb-run cargo run -- <folder>` with a Vulkan driver such as Mesa's
lavapipe.

## Environment variables

| Variable | Effect |
|---|---|
| `HANGAR_LOG` | Log filter, such as `debug` or `cockpit_agents=trace` |
| `HANGAR_DATA_DIR` | Use another data directory instead of `~/Library/Application Support/Hangar` |
| `HANGAR_SKIP_SHELL_ENV` | Don't read `PATH` from your login shell |
| `HANGAR_CODEX_BIN` | Path to the `codex` executable (also used by the live tests) |
| `HANGAR_CLAUDE_BIN` | Path to the `claude` executable (also used by the live tests) |
| `HANGAR_LLDB_DAP` | Path to the `lldb-dap` executable |
| `HANGAR_GITHUB_CLIENT_ID` | Use your own GitHub OAuth app for browser sign-in (at run time or build time) |
| `HANGAR_LINEAR_CLIENT_ID` | Use your own Linear OAuth app for browser sign-in (at run time or build time) |
| `HANGAR_REMOTE_DEBUGGING_PORT` | Expose Chromium's DevTools protocol on localhost |
| `HANGAR_CEF_NO_SANDBOX` | Run Chromium without its sandbox (for debugging helper setups) |
| `HANGAR_CEF_MOCK_KEYCHAIN` | Use a mock Keychain for Chromium in release builds (always on in debug builds) |
| `CEF_PATH` | Where the Chromium download is kept (default `.cef/`) |

## Troubleshooting

**The browser says it's disabled.** You started Hangar with `cargo run`.
Chromium finds its framework and helper apps through the app bundle, so
use `cargo xtask bundle --run`.

**A language server is "off until this workspace is trusted".** The
server would run the project's own code. Run **Trust This Workspace** from
the command palette if you trust the folder. **Stop Trusting This
Workspace** undoes it.

**rust-analyzer "does not run".** The `rust-analyzer` in `~/.cargo/bin`
is only rustup's proxy until the component is installed. Run
`rustup component add rust-analyzer`.

**A language server isn't found, but it's installed.** Apps started from
Finder don't get your shell's `PATH`. Hangar reads it from your login
shell, and also looks in `/opt/homebrew/bin`, `/usr/local/bin`,
`~/.cargo/bin`, `~/go/bin`, `~/.bun/bin` and other common folders. Check
that the server runs in a new terminal.

**Linear browser sign-in says the port is in use.** It listens on
`localhost:47621` for Linear's answer. Close whatever uses that port, or
paste a personal API key instead.

**The first build is slow.** It downloads Chromium (about 500 MB) and
builds every dependency. Later builds are incremental.

# Hangar

**A native macOS IDE written in Rust, with coding agents, a real browser,
and your issues and pull requests built in.**

Hangar is a fast, native editor built on [GPUI](https://www.gpui.rs) (the
UI framework behind Zed). Alongside the editor you get a terminal, git, a
Chromium browser inside the window, and two coding agents you can switch
between: OpenAI's **Codex** and Anthropic's **Claude Code**. Hangar adds
its own layer of approvals, permissions and token budgets on top of both.

> **Status:** early (v0.1). Hangar targets macOS. Everything except the
> browser also runs on Linux for development. Expect rough edges, and
> please [open an issue](https://github.com/RatikGambhir/code/issues) when
> you hit one.

## Features

**Editing**
- Tree-sitter highlighting, multiple panes, go to file, find in files, and
  a command palette
- Language servers chosen from the project around each file: completion,
  go to definition, find references, hover, and quick fixes
- Rust, TypeScript and JavaScript (Node, Bun, Deno; Vue, Svelte, Astro,
  Angular), Go, Python, C and C++, Swift, Zig, Ruby, and CSS, HTML, JSON,
  YAML, TOML and Bash
- **Install Missing Language Servers** installs what your files need into
  a folder Hangar owns, so nothing is installed globally
- **Workspace trust**: a project's own tools, config files and build
  scripts don't run until you trust its folder

**Coding agents**
- Codex and Claude Code in one panel: prompt, watch the turn stream, and
  approve commands and edits
- Chat mode, where the conversation fills the window and a diff panel
  shows what the agent changed
- Permission presets (read only, auto, full access), approval rules,
  secret detection in prompts, and token budgets per thread, workspace or
  model
- Browser tools for the agent (navigate, read, screenshot, click), limited
  to an allowlist of sites and asking you before anything risky

**Built-in tools**
- A Chromium browser in the window, so pages sit next to your code
- Terminals, source control, and project tasks (package scripts,
  `go test`, linters) one click away
- Running and debugging Rust with LLDB: breakpoints, stepping, inline
  values, and a variables view with Rust type names
- Rust notebooks: call one function with arguments you write and see what
  it prints and returns, without writing a test or a `main`

**Your work, in the editor**
- **Tasks**: a local issue board per workspace, plus your Linear projects
  and editable issues
- **GitHub**: browse your repositories' code, pull requests with their
  diffs, issues with comments, and Actions runs. Edit issues, review and
  merge pull requests, commit files, and manage workflow runs from the IDE.
- Sign-ins are kept in the macOS Keychain

## Quick start

You need macOS 12+, Rust 1.88+, the Xcode Command Line Tools, and CMake
and Ninja. Then:

```bash
git clone https://github.com/RatikGambhir/code.git hangar
```

```bash
cd hangar && cargo xtask bundle --run
```

The first build downloads Chromium (about 500 MB). See
**[Getting started](docs/getting-started.md)** for every dependency,
optional tools such as the agent CLIs and language servers, and how to
build a release.

## Documentation

| | |
|---|---|
| [Getting started](docs/getting-started.md) | Dependencies, building, running, testing, environment variables, troubleshooting |
| [User guide](docs/guide.md) | Keyboard shortcuts, languages and workspace trust, agents, Tasks, GitHub, running and debugging, notebooks, known limitations |
| [Architecture](docs/architecture.md) | How the crates fit together, the browser and agent integrations, storage, and extending notebooks |

## Project structure

Hangar is a Cargo workspace of focused crates. Only `ui` and `app` depend
on the UI framework, so everything else can be tested without a display.

```
.
├── crates/
│   ├── app/           the `hangar` executable and the Chromium helper process
│   ├── ui/            every window, view and panel (GPUI): workspace, editor tabs,
│   │                  explorer, terminal, browser tab, agent panel, chat mode,
│   │                  debugger panel, Tasks and GitHub views, command palette
│   ├── editor/        documents, file tree, find in files, file watching, project
│   │                  detection, and the language server catalog, client and installer
│   ├── agents/        clients for Codex (App Server), Claude Code (stream-json) and
│   │                  ChatGPT, turned into one stream of events and transcripts
│   ├── orchestrator/  Hangar's rules around the agents: prompt checks, approval
│   │                  rules, token budgets, turn scheduling, browser-tool policy
│   ├── browser/       the embedded Chromium (CEF, rendered off-screen) and the
│   │                  automation API the agents use
│   ├── debugger/      Debug Adapter Protocol client for lldb-dap, Cargo run targets,
│   │                  breakpoints that follow edits, Rust value display
│   ├── notebook/      notebooks: a language-neutral interface, and Rust support
│   ├── terminal/      PTY-backed shells with a VT100 screen model
│   ├── git/           git through the git CLI, and a status parser
│   ├── tasks/         local issue boards and the Linear client
│   ├── github/        the GitHub client and its sign-in
│   ├── persistence/   the SQLite database (WAL) and a typed SQL builder
│   └── common/        typed ids, on-disk paths, and the login-shell environment
├── xtask/             `cargo xtask bundle`: builds, assembles and signs Hangar.app
├── docs/              getting started, user guide and architecture
├── scripts/check.sh   tests and type-checks every crate
├── assets/            icons and fonts
├── packaging/macos/   entitlements and the app icon
└── vendor/            gpui-component with Hangar's patches
```

[Architecture](docs/architecture.md) explains each crate file by file, how
they depend on each other, and how the browser, agents and storage work.

## Contributing

Contributions are welcome. Before you open a pull request:

1. Read [Getting started](docs/getting-started.md) to build and run Hangar.
2. Run `scripts/check.sh`, which runs every crate's tests and type-checks
   the app and the Chromium backend. It also works on Linux.
3. Keep changes focused, and add tests next to the code you change.

Bug reports and feature ideas go in
[issues](https://github.com/RatikGambhir/code/issues).

## Credits

- The CEF integration follows the approach in
  [bokuweb/gpui-cef](https://github.com/bokuweb/gpui-cef) (MIT OR
  Apache-2.0).
- UI components from
  [gpui-component](https://github.com/longbridge/gpui-component), vendored
  with small patches (see `vendor/gpui-component/COCKPIT_PATCHES.md`).
- Chromium Embedded Framework (BSD) via
  [cef-rs](https://github.com/tauri-apps/cef-rs).
- Icons from [Lucide](https://lucide.dev) (ISC), see
  `assets/icons/LICENSE-lucide.txt`.
- Fonts: [IBM Plex Sans](https://github.com/IBM/plex) for the interface
  and [Lilex](https://github.com/mishamyrt/Lilex) for code, both under the
  SIL Open Font License 1.1.

## License

Licensed under either of MIT or Apache-2.0, at your option.

# User guide

How to use Hangar's features. For installing and building, see
[Getting started](getting-started.md).

- [Keyboard shortcuts](#keyboard-shortcuts)
- [Languages and projects](#languages-and-projects)
- [Workspace trust](#workspace-trust)
- [Coding agents](#coding-agents)
- [Tasks](#tasks)
- [GitHub](#github)
- [Run and debug](#run-and-debug)
- [Notebooks](#notebooks)
- [Known limitations](#known-limitations)

## Keyboard shortcuts

| Keys | Action |
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
| ⌘. | Quick fixes and refactors (such as adding a missing `use`) |
| Tab / ↩ | In the completion list: Tab replaces the word under the cursor, ↩ inserts before it. ↑↓ choose, Esc closes. Picking a name that isn't imported yet adds its `use`. |
| `(` `[` `{` `"` | Closed for you. Typing the closer steps over it, ⌫ removes an empty pair, ↩ between a pair opens an indented line, and with text selected it's wrapped. |
| ⌘L | Ask the agent (↩ sends, ⇧↩ new line, ⌃C stops the turn) |
| ⇧⌘N | New agent thread |
| ⇧⌘A | Chat mode: the conversation fills the window, with history in the sidebar |
| ⇧⌘D | Show or hide the chat's changes (this chat, or everything uncommitted) |
| ⌃R / ⌃D | Run / debug the selected configuration |
| ⌃⇧R / ⌃⇧D | Run / debug the test or `fn main` at the cursor |
| ⌘F2, ⌘F5 | Stop, rerun under the debugger |
| F8, F7, ⇧F8 | Step over, into, out |
| ⌥⌘R (or F9), ⌥F9 | Resume, run to cursor |
| ⌘F8, ⇧⌘F8 | Toggle breakpoint, view breakpoints |
| ⌥F8, ⌘5 | Evaluate expression, debug panel |
| ⌘↵, ⇧⌘↵, ⇧⌥F | In a notebook cell: run it, add a cell below, format it |

## Languages and projects

Opening a file detects its project by walking up to the markers each
ecosystem leaves, and starts the language servers that apply, once per
project root. For example, there's one TypeScript server per
package-manager workspace and one gopls per Go module or `go.work`. The
status bar shows what was found, such as `Bun · TypeScript · Next.js ·
Tailwind` or `Go 1.23 · Gin`. Clicking it (or **Run Project Task…**) lists
the project's commands and runs one in a terminal.

| Language | Detected from | Servers |
|---|---|---|
| Rust | `.rs` files | `rust-analyzer` |
| TypeScript 7+ | installed `typescript` ≥ 7, or none installed | its own `tsc --lsp` (`tsgo` with `@typescript/native-preview`) |
| TypeScript ≤ 6 | `node_modules/typescript/lib/tsserver.js` | `typescript-language-server` or `vtsls` |
| Node, Bun, Deno | lockfiles, `bunfig.toml`, `@types/bun`, `deno.json` | Deno's `deno lsp` replaces TypeScript and ESLint |
| Vue, Svelte, Astro, Angular | dependencies | `vue-language-server`, `svelteserver`, `astro-ls`, `ngserver` |
| ESLint, Biome, oxlint | config files or dependencies | their servers, as linters beside the main one |
| Tailwind | `tailwindcss` or a config | `tailwindcss-language-server`, its completions merged in |
| Go | `go.mod`, `go.work` | `gopls` (staticcheck on); `golangci-lint-langserver` with a `.golangci.*` config; `templ lsp` for `.templ` |
| Python | `.py` files | `basedpyright`, `pyright` or `pylsp` |
| C and C++, Swift, Zig, Ruby | file type | `clangd`, `sourcekit-lsp`, `zls`, `ruby-lsp` |
| CSS, HTML, JSON, YAML, Bash, TOML | file type | the standard servers, with schemas for `package.json`, `tsconfig.json`, `deno.json`, `biome.json` and others |

Project tasks come from `package.json` scripts (run with the project's
package manager), `deno.json` tasks, and for Go `go run/test/build/vet`,
`go mod tidy` and `go generate`, plus `golangci-lint`, `templ generate`,
`air`, `sqlc` and `buf` when the project uses them.

**Installing servers.** When a file's main server is missing, the status
bar says so. **Install Missing Language Servers** in the command palette
installs the ones Hangar can (npm packages with npm or Bun, Go tools with
`go install`) into `~/Library/Application Support/Hangar/language-servers`.
Nothing is installed globally, and deleting that folder undoes it all.

## Workspace trust

Some language servers run code from the project they're working on:

- the project's own executables in `node_modules/.bin`, and its copy of
  TypeScript
- ESLint and Tailwind, which load the project's JavaScript config and
  plugins
- Svelte, Vue and Astro, which load the project's config and plugins
- rust-analyzer, which runs build scripts, proc macros and `cargo check`
- golangci-lint (plugins in its config), Ruby LSP (the Gemfile), zls
  (`build.zig`) and SourceKit-LSP (`Package.swift`)

A cloned repository could put anything in those, so Hangar doesn't run
them until you say you trust the folder. Until then, servers use only the
tools on your `PATH` and those Hangar installed, and the servers above
stay off. A notice tells you when something is waiting, and the status bar
shows "off until this workspace is trusted" for a language's main server.

- **Trust This Workspace** (command palette) turns them on and restarts
  the language servers. The choice is saved per folder.
- **Stop Trusting This Workspace** turns them off again.

Trust only covers folders inside the workspace. Tools in folders above it,
or around files opened from outside it, are never used.

## Coding agents

The agent panel (⌥⌘B) shows your threads as cards in one feed. Pick the
provider (Codex or Claude) from the panel header, or start a thread with
another one by typing `@claude`, `@codex` or `@chatgpt` in the composer.

- **Prompt and watch.** Turns stream in as they run: messages, commands,
  file edits and their diffs.
- **Approve.** Commands and edits that need your OK show as approval
  cards: approve once, approve for the session, or decline.
- **Permissions.** Choose Read only, Auto or Full access from the panel.
  Read only can't write; Auto asks before commands that could change
  things; Full access doesn't ask.
- **Chat mode** (⇧⌘A) fills the window with the conversation, lists past
  chats in the sidebar, and shows a diff panel of what the agent changed
  (⇧⌘D). The panel slides out on its own when the agent writes a file;
  closing it keeps it closed for that conversation.
- **`@chatgpt`** asks ChatGPT itself, without Codex or tools, on your
  ChatGPT plan with Hangar's sign-in. Conversations are kept in Hangar's
  data directory.
- **Budgets.** Hangar records token usage and can warn at, or stop at,
  limits you set per thread, workspace or model.
- **Browser tools.** Agents can navigate, read, screenshot and click in
  Hangar's browser. They're limited to an allowlist of sites (localhost by
  default), and clicks or navigation off the list ask you first.

Signing in: see [Optional tools](getting-started.md#optional-tools).

## Tasks

The checklist button in the left rail opens Tasks: projects and issues in
a Linear-style list grouped by status, with priority, labels, project and
assignee; a projects table with progress; and the open issue's details
beside the list.

- **This workspace** is a local board kept in Hangar's data directory, one
  JSON file per folder, so nothing is written into the repository. Add
  issues with New issue or **+** on a status group, and set status,
  priority and project in the details pane.
- **Linear** shows your projects, the issues assigned to you, and your 500
  most recently updated issues. Choose **Connect Linear** and sign in or
  paste a personal API key with write access for your teams. For an existing
  browser connection, choose **Enable editing** and authorize write access.
  Hangar keeps the sign-in in the macOS Keychain.
- In **All issues**, **My issues**, or a project, use **New issue** to
  create a Linear issue. Open an issue and click its status or assignee to
  change it. **Edit issue** also lets you update its title, description,
  priority, project, and due date. The details pane has actions for labels,
  archiving, and comments.

## GitHub

Open **GitHub** in the left rail and select a repository.

**Signing in**, three ways:

- **Sign in with GitHub**: Hangar shows a one-time code and copies it,
  opens github.com in your browser, and finishes when you approve.
- **Use my GitHub CLI login**, if `gh` is installed and signed in.
- **Paste a personal access token** with the `repo` and `read:org`
  scopes. Add `workflow` to edit workflow files. For a fine-grained token,
  grant write access to the relevant
  repository permissions, such as **Issues**, **Pull requests**,
  **Contents**, and **Actions**. Editing workflow files also requires
  **Workflows** permission. **Create a token on GitHub** opens the token page.

The token is kept in the macOS Keychain. Sign out from the sidebar.

The repository view has four tabs. GitHub applies your account and token
permissions and any branch protection rules when you save.

- **Code** shows folders and files by branch. Use **Code actions** to
  create a branch, create a file, or delete the open file. Open a text
  file and choose **Edit file** to
  commit your changes to the selected branch. Hangar displays text files
  up to 2 MB.
- **Pull requests** lets you create and edit pull requests, manage
  assignees, labels, milestones, and review requests, submit a review,
  change draft status, close or reopen a pull request, and merge.
- **Issues** lets you create, edit, close, reopen, and comment on issues.
  **Issue actions** includes assignees, labels, and milestones.
- **Actions** lets you dispatch a workflow that supports manual runs.
  Open a run to cancel it or rerun it. Logs open on GitHub.

**Open on GitHub** is available throughout the repository view.

## Run and debug

Rust projects get RustRover-style running and debugging, built on Cargo
and LLDB (`lldb-dap`, over the Debug Adapter Protocol).

- **Configurations.** The title bar's selector lists the workspace's
  binaries and examples (from `cargo metadata`) and recently run tests,
  with program arguments. ▶ runs it in a terminal (`cargo run` / `cargo
  test`), and the bug icon debugs it.
- **Gutter.** A ▶ beside `fn main`, each `#[test]` (also `#[tokio::test]`
  and friends) and each test module offers Run and Debug. Click beside a
  line number for a breakpoint, ⌥-click to disable it, and right-click for
  a condition, a hit count or a log message (a logpoint, drawn as a
  diamond). Breakpoints stay on their line as you edit and are kept per
  workspace.
- **Debugging** builds with `cargo build` or `cargo test --no-run`,
  launches the binary under LLDB with your toolchain's Rust formatters, and
  stops at breakpoints and, by default, on panics (at the first frame of
  your own code). The paused line is highlighted, locals' values show after
  the lines that mention them, and hovering a name or an `a.b.c` path shows
  its value.
- **Debug panel** (bottom dock, beside the terminal): resume, pause, stop,
  rerun, step over/into/out, and run to cursor; threads and frames, with
  library frames dimmed; a variables tree with Rust type names (`{Vec<i32>}
  size=3`) that keeps what you expanded across steps (double-click to set
  a value); watches and an expression field (↩ evaluates, ⌘↩ adds a watch);
  the program's output and an expression console; and every breakpoint,
  with "pause when the program panics".

## Notebooks

A notebook lets you call one function from your code with arguments you
write, and see what it prints and returns, without writing a test or a
`main`.

- **Open one.** Right-click in a Rust file and choose **Open Rust
  Notebook** (or use **Open Notebook** in the command palette). Inside a
  function, the notebook is for that function. Anywhere else, it's for the
  file: the first cell imports the file's module and you write the calls.
- **Try a function.** The notebook's header lists the file's public
  functions and methods. Clicking one adds a cell that calls it.
- **Cells for a function** are written from its signature: a `use` for its
  module, a `let` with `todo!()` for each argument (references become an
  owned value you borrow, so `&Config` gives `let config: Config` and
  passes `&config`), and the call. Methods get a receiver to fill in, and
  async functions are wrapped in `__hangar::block_on(…)`. Replace the
  `todo!()`s and press **Run** (⌘↵).
- **Output.** What the cell prints appears under it (stderr in red), then
  the value of its last line, pretty-printed with `{:#?}` and labelled with
  its type. A type without `Debug` shows just its name. End the last line
  with `;` to show nothing. `?` works in cells, and a panic or an error from
  `?` is reported. Compiler errors use the cell's line numbers.
- **As you type**, cells get completion (including adding a missing
  `use`), hover information and errors, from the notebook's own
  rust-analyzer. The header says when it's ready; in a large workspace the
  first load takes a while.
- **Formatting.** A cell is formatted with `rustfmt` (using your project's
  `rustfmt.toml`) each time it runs, or with **Format** (⇧⌥F). ⌘Z undoes
  it.
- **Cells are independent**: each runs on its own and they share no
  variables. Add more with **Add cell** (⇧⌘↵), for example to try other
  arguments.

Notebooks build in `target/hangar-notebook/` inside your workspace's
target directory and clean up after themselves. See
[Architecture](architecture.md#notebooks) for how they work.

## Known limitations

Hangar is at v0.1. Things that don't work yet:

- **Platform.** The product targets macOS. Everything but the browser also
  runs on Linux, for development.
- **Debugging.** A debugged program's output shows in the debug console,
  but it can't read from stdin there (Run, in a terminal, can).
  Expressions are evaluated by LLDB, which knows Rust values but not Rust
  syntax: field paths, indexing and arithmetic work; method calls and
  closures don't. Only Rust/Cargo projects can be run and debugged.
- **Notebooks.** Rust only. They can call `pub` functions of library
  crates (not functions in a binary's `main.rs`, `src/bin/`, tests or
  examples), and a function in a private module needs a public path to it.
  Cells don't have go to definition or quick fixes. Each open notebook runs
  its own rust-analyzer, which costs memory. Notebooks aren't reopened
  after a restart.
- **Agents.** Budgets, prompt patterns, approval rules and the browser-tool
  allowlist are enforced but have no settings screen yet; they live in the
  `budgets` and `agent_policies` database tables. The UI runs one agent
  thread at a time per window, though the orchestrator can create
  worktrees for parallel agents. Switching provider stops the current
  provider's agent.
- **Claude.** Not supported yet: Claude's question tool, hooks, slash
  commands typed in the panel, and showing subagent steps (only the
  subagent's result appears). "Approve for session" keeps Claude's
  suggested rule for this session only, never in your Claude Code settings.
- **Diffs** open as read-only tabs; there's no side-by-side diff view yet.
- **Browser.** Chromium's own context menus, file pickers and print
  dialogs aren't wired into the embedded view.

//! Which language servers handle a file.
//!
//! A file can have several: one primary server (completion, hover,
//! definitions), linters that add diagnostics and fixes (ESLint, Biome,
//! golangci-lint) and add-ons for a framework inside the language
//! (Tailwind's class names, Angular's templates).
//!
//! Choosing them happens in two steps. [`plan`] is cheap and pure: from a
//! [`FileContext`] it picks the servers that apply and the directory each
//! runs in, which together identify a server process, so a monorepo gets
//! one TypeScript server per package-manager workspace and one gopls per
//! Go module or `go.work`. [`ServerPlan::resolve`] then finds the
//! executable and builds its settings, and runs only for servers that are
//! not running yet.

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
};

use serde_json::{json, Value};

use super::install::{Install, ManagedDir};
use crate::project::{FileContext, Framework, GoTool, JsTool, PackageManager, Runtime, TypeScriptKind};

/// How a server contributes to the files it serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Completion, hover, definitions, references, code actions and
    /// diagnostics. One per file.
    Primary,
    /// Diagnostics and their fixes.
    Linter,
    /// Diagnostics, fixes, and completions merged into the primary's.
    Addon,
}

/// A server process to start.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerConfig {
    /// The server's name, for logs and the status bar.
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Extra environment for the process.
    pub env: Vec<(String, String)>,
    /// The answers to `workspace/configuration`, as a tree of sections
    /// (`{"gopls": {...}}`).
    pub settings: Option<Value>,
    /// The section for configuration requests that name none (ESLint asks
    /// with an empty section and expects its own settings).
    pub default_section: Option<String>,
    pub initialization_options: Option<Value>,
    /// Also send [`Self::settings`] in `workspace/didChangeConfiguration`
    /// after `initialize`, for servers that never ask for them.
    pub push_settings: bool,
}

impl ServerConfig {
    fn new(name: &str, command: PathBuf, args: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            command: command.to_string_lossy().into_owned(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Lays `overrides` over the settings of the default section and the
    /// initialization options, key by key.
    pub fn merge_settings(&mut self, overrides: Value) {
        if let Some(section) = &self.default_section {
            let tree = self.settings.get_or_insert_with(|| json!({}));
            if let Some(tree) = tree.as_object_mut() {
                let current = tree.remove(section).unwrap_or_else(|| json!({}));
                tree.insert(section.clone(), merge_json(current, overrides.clone()));
            }
        }
        let options = self.initialization_options.take().unwrap_or_else(|| json!({}));
        self.initialization_options = Some(merge_json(options, overrides));
    }
}

/// `overrides` laid over `base`: objects merge key by key, anything else
/// replaces.
pub fn merge_json(base: Value, overrides: Value) -> Value {
    match (base, overrides) {
        (Value::Object(mut base), Value::Object(overrides)) => {
            for (key, value) in overrides {
                let merged = match base.remove(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (_, overrides) => overrides,
    }
}

/// The value of a dotted `section` (`"gopls"`, `"rust-analyzer.check"`) in
/// a settings tree. A request without a section gets the default section,
/// or the whole tree.
pub fn settings_section(settings: Option<&Value>, default_section: Option<&str>, section: Option<&str>) -> Value {
    let Some(settings) = settings else { return Value::Null };
    let section = section.filter(|s| !s.is_empty()).or(default_section);
    let Some(section) = section else { return settings.clone() };
    section
        .split('.')
        .try_fold(settings, |value, key| value.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Where executables are looked for, besides `PATH`.
#[derive(Clone, Debug, Default)]
pub struct ToolDirs {
    /// Well-known install locations GUI apps often lack on `PATH`
    /// (`~/.cargo/bin`, `~/go/bin`, `~/.bun/bin`).
    pub search: Vec<PathBuf>,
    /// Servers Hangar installed itself.
    pub managed: Option<ManagedDir>,
    /// The user trusts the workspace, so servers may run the project's own
    /// code: executables in its `node_modules/.bin`, its TypeScript, plugins
    /// from its `node_modules`, and servers that load its config files or
    /// build scripts ([`ServerSpec::runs_project_code`]). Off by default,
    /// since a cloned repository can ship any of these.
    pub trust_project: bool,
}

impl ToolDirs {
    /// `PATH`, then the search dirs, then managed installs.
    fn dirs(&self) -> Vec<PathBuf> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        dirs.extend(self.search.iter().cloned());
        if let Some(managed) = &self.managed {
            dirs.extend(managed.bin_dirs());
        }
        dirs
    }

    pub fn find(&self, program: &str) -> Option<PathBuf> {
        find_executable(program, &self.dirs())
    }
}

/// Why a server could not be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// None of the server's executables is installed.
    NotInstalled { install: Option<Install> },
    /// It is installed but cannot run.
    Broken(String),
    /// It would run the project's own code, and the workspace is not
    /// trusted.
    NeedsTrust,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::NotInstalled { install: Some(install) } => write!(f, "not installed; {}", install.hint()),
            ResolveError::NotInstalled { install: None } => f.write_str("not installed"),
            ResolveError::Broken(message) => f.write_str(message),
            ResolveError::NeedsTrust => f.write_str("off until this workspace is trusted"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// A server Hangar knows how to find and configure.
pub struct ServerSpec {
    /// Stable identifier (`"typescript"`, `"gopls"`).
    pub id: &'static str,
    pub role: Role,
    /// LSP language ids served.
    languages: &'static [&'static str],
    /// Executables with their arguments, first installed wins.
    candidates: &'static [(&'static str, &'static [&'static str])],
    /// Whether a found executable is the right one (a global `tsc` may be
    /// an old TypeScript without a language server).
    accept: fn(&Path) -> bool,
    pub install: Option<Install>,
    /// Whether the server applies to a file's project (beyond its language).
    applies: fn(&FileContext) -> bool,
    /// The directory the server runs in.
    root: fn(&FileContext) -> PathBuf,
    /// Settings and extra arguments, once the executable is known.
    configure: fn(&mut ServerConfig, &Resolution) -> Result<(), ResolveError>,
}

impl fmt::Debug for ServerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerSpec").field("id", &self.id).field("role", &self.role).finish()
    }
}

impl ServerSpec {
    /// Whether its absence is worth telling the user about: the main
    /// server of a programming language, not a linter or a helper for
    /// config files (which the installer still offers).
    pub fn is_essential(&self) -> bool {
        self.role == Role::Primary && !matches!(self.id, "json" | "yaml" | "bash" | "css" | "html" | "taplo")
    }

    /// Whether starting it runs the project's own code, so it waits until
    /// the workspace is trusted. rust-analyzer runs build scripts, proc
    /// macros and `cargo check`; ESLint, Tailwind and Svelte load JavaScript
    /// config and plugins; Vue loads plugins named in `tsconfig.json`;
    /// golangci-lint loads plugins named in its config; Ruby LSP evaluates
    /// the Gemfile; zls runs `build.zig`; SourceKit-LSP runs
    /// `Package.swift`. Astro loads the project's own packages too.
    pub fn runs_project_code(&self) -> bool {
        matches!(
            self.id,
            "rust-analyzer"
                | "eslint"
                | "tailwind"
                | "svelte"
                | "vue"
                | "astro"
                | "golangci-lint"
                | "ruby"
                | "zls"
                | "sourcekit"
        )
    }
}

impl PartialEq for ServerSpec {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

/// What [`ServerSpec::configure`] gets to work with.
pub struct Resolution<'a> {
    pub context: &'a FileContext,
    pub root: &'a Path,
    pub tools: &'a ToolDirs,
}

impl Resolution<'_> {
    /// The TypeScript `lib` directory servers should use: the project's,
    /// else one installed next to Hangar's managed servers.
    fn typescript_lib(&self) -> Option<PathBuf> {
        self.context
            .js
            .as_ref()
            .filter(|_| self.tools.trust_project)
            .and_then(|js| js.typescript_lib.clone())
            .or_else(|| self.tools.managed.as_ref().and_then(ManagedDir::classic_typescript_lib))
    }

    fn root_uri(&self) -> String {
        super::path_to_uri(self.root).map(|uri| uri.as_str().to_string()).unwrap_or_default()
    }

    fn root_name(&self) -> String {
        self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// A server chosen for a file, not yet started.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerPlan {
    pub spec: &'static ServerSpec,
    pub root: PathBuf,
    context: FileContext,
}

impl ServerPlan {
    /// Identifies the server process: one per server and root.
    pub fn key(&self) -> String {
        format!("{}@{}", self.spec.id, self.root.display())
    }

    pub fn role(&self) -> Role {
        self.spec.role
    }

    /// Finds the executable and builds the settings. Runs subprocesses for
    /// some servers (`--version` checks), so keep it off the main thread.
    pub fn resolve(&self, tools: &ToolDirs) -> Result<ServerConfig, ResolveError> {
        let spec = self.spec;
        // Checked before looking for the executable: even a `--version`
        // check could run the project's toolchain overrides.
        if spec.runs_project_code() && !tools.trust_project {
            return Err(ResolveError::NeedsTrust);
        }
        // The project's own executables come first, but only in a trusted
        // workspace: otherwise opening a file would run whatever the
        // repository put there (even the `--version` checks below run it).
        let project_bins = self
            .context
            .js
            .as_ref()
            .filter(|_| tools.trust_project)
            .map(|js| js.bin_dirs.as_slice())
            .unwrap_or_default();
        let mut dirs = project_bins.to_vec();
        dirs.extend(tools.dirs());
        let (program, path, args) = spec
            .candidates
            .iter()
            .find_map(|(program, args)| {
                let path = dirs
                    .iter()
                    .map(|dir| dir.join(program))
                    .find(|candidate| is_executable(candidate) && (spec.accept)(candidate))?;
                Some((*program, path, *args))
            })
            .ok_or_else(|| self.not_found(tools))?;
        let mut config = ServerConfig::new(program, path, args);
        run_node_scripts_with_bun(&mut config, &dirs);
        config.env.push(("PATH".into(), joined_path(&dirs, Path::new(&config.command))));
        let resolution = Resolution { context: &self.context, root: &self.root, tools };
        (spec.configure)(&mut config, &resolution)?;
        Ok(config)
    }
}

impl ServerPlan {
    /// Why no executable was found: the project has one of its own that an
    /// untrusted workspace doesn't use (Biome and oxlint live only there),
    /// or it is not installed.
    fn not_found(&self, tools: &ToolDirs) -> ResolveError {
        let in_project = !tools.trust_project
            && self.context.js.iter().flat_map(|js| &js.bin_dirs).any(|dir| {
                self.spec.candidates.iter().any(|(program, _)| is_executable(&dir.join(program)))
            });
        if in_project {
            ResolveError::NeedsTrust
        } else {
            ResolveError::NotInstalled { install: self.spec.install.clone() }
        }
    }
}

/// The servers for a file, primary first.
pub fn plan(context: &FileContext) -> Vec<ServerPlan> {
    let mut plans: Vec<ServerPlan> = SERVERS
        .iter()
        .filter(|spec| spec.languages.contains(&context.language) && (spec.applies)(context))
        .map(|spec| ServerPlan {
            spec,
            root: (spec.root)(context),
            context: context.clone(),
        })
        .collect();
    // Only the first primary serves; specs are listed most specific first.
    let mut seen_primary = false;
    plans.retain(|plan| {
        let keep = plan.spec.role != Role::Primary || !seen_primary;
        seen_primary |= plan.spec.role == Role::Primary;
        keep
    });
    plans.sort_by_key(|plan| plan.spec.role != Role::Primary);
    plans
}

/// The installed primary server for a highlighter `language` with no
/// project around it (notebooks). `Ok(None)` means none is known for the
/// language; an error means one is known but missing or broken.
pub fn resolve_server(language: &str, search_dirs: &[PathBuf]) -> Result<Option<ServerConfig>, String> {
    let language = crate::lsp_language_id(language);
    let Some(spec) = SERVERS.iter().find(|s| s.role == Role::Primary && s.languages.contains(&language)) else {
        return Ok(None);
    };
    let root = std::env::temp_dir();
    let plan = ServerPlan {
        spec,
        root: root.clone(),
        context: FileContext {
            path: root.join("file"),
            language: spec.languages[0],
            workspace_root: root,
            js: None,
            go: None,
        },
    };
    // Notebooks run in a scratch project Hangar writes itself, and run the
    // user's own cells anyway.
    let tools = ToolDirs { search: search_dirs.to_vec(), managed: None, trust_project: true };
    plan.resolve(&tools).map(Some).map_err(|err| err.to_string())
}

// --- the catalog ---------------------------------------------------------

const JS_LANGUAGES: &[&str] = &["typescript", "typescriptreact", "javascript", "javascriptreact"];
const JS_AND_COMPONENTS: &[&str] =
    &["typescript", "typescriptreact", "javascript", "javascriptreact", "vue", "svelte", "astro"];
const LANGSERVERS_EXTRACTED: Install = Install::Npm(&["vscode-langservers-extracted"]);
/// A classic TypeScript (with `tsserver.js`) for the servers that load one
/// (Vue, Astro, Angular), beside TypeScript 7 in the managed directory.
const CLASSIC_TYPESCRIPT: &str = "typescript-classic@npm:typescript@6";

fn always(_: &FileContext) -> bool {
    true
}

fn any_executable(_: &Path) -> bool {
    true
}

fn typescript_kind(context: &FileContext) -> TypeScriptKind {
    context.js.as_ref().map_or(TypeScriptKind::Missing, |js| js.typescript_kind())
}

/// `tsc` or `tsgo` from TypeScript 7 or later, which serve LSP themselves.
fn is_native_typescript(command: &Path) -> bool {
    command_output(command, &["--version"])
        .and_then(|version| parse_major_version(&version.to_ascii_lowercase()))
        .is_some_and(|major| major >= 7)
}

fn no_settings(_: &mut ServerConfig, _: &Resolution) -> Result<(), ResolveError> {
    Ok(())
}

/// The workspace, or the file's directory when it is outside it.
fn workspace_root(context: &FileContext) -> PathBuf {
    if context.path.starts_with(&context.workspace_root) {
        context.workspace_root.clone()
    } else {
        context.path.parent().map(Path::to_path_buf).unwrap_or_else(|| context.workspace_root.clone())
    }
}

fn js_root(context: &FileContext) -> PathBuf {
    context.js.as_ref().map(|js| js.root.clone()).unwrap_or_else(|| workspace_root(context))
}

fn go_root(context: &FileContext) -> PathBuf {
    context.go.as_ref().map(|go| go.root.clone()).unwrap_or_else(|| workspace_root(context))
}

fn is_deno(context: &FileContext) -> bool {
    context.js.as_ref().is_some_and(|js| js.runtime == Runtime::Deno)
}

fn js_has(context: &FileContext, tool: JsTool) -> bool {
    context.js.as_ref().is_some_and(|js| js.has(tool))
}

/// Most specific first: where two primaries serve a language, the first one
/// that applies wins (Deno's server over TypeScript's in a Deno project).
static SERVERS: &[ServerSpec] = &[
    ServerSpec {
        id: "rust-analyzer",
        role: Role::Primary,
        languages: &["rust"],
        candidates: &[("rust-analyzer", &[])],
        accept: any_executable,
        install: Some(Install::Hint("run `rustup component add rust-analyzer`")),
        applies: always,
        root: workspace_root,
        configure: configure_rust_analyzer,
    },
    // --- JavaScript and TypeScript ---
    ServerSpec {
        id: "deno",
        role: Role::Primary,
        languages: JS_LANGUAGES,
        candidates: &[("deno", &["lsp"])],
        accept: any_executable,
        install: Some(Install::Hint("install Deno from https://deno.com")),
        applies: is_deno,
        root: js_root,
        configure: configure_deno,
    },
    // A project on TypeScript 6 or earlier keeps its tsserver.
    ServerSpec {
        id: "typescript-classic",
        role: Role::Primary,
        languages: JS_LANGUAGES,
        candidates: &[("typescript-language-server", &["--stdio"]), ("vtsls", &["--stdio"])],
        accept: any_executable,
        install: Some(Install::Npm(&["typescript-language-server"])),
        applies: |context| typescript_kind(context) == TypeScriptKind::Classic,
        root: js_root,
        configure: configure_typescript,
    },
    // TypeScript 7 is its own server: the project's, else one Hangar
    // installs, which also serves loose scripts.
    ServerSpec {
        id: "typescript",
        role: Role::Primary,
        languages: JS_LANGUAGES,
        candidates: &[("tsgo", &["--lsp", "--stdio"]), ("tsc", &["--lsp", "--stdio"])],
        accept: is_native_typescript,
        install: Some(Install::Npm(&["typescript"])),
        applies: always,
        root: js_root,
        configure: configure_native_typescript,
    },
    ServerSpec {
        id: "vue",
        role: Role::Primary,
        languages: &["vue"],
        candidates: &[("vue-language-server", &["--stdio"])],
        accept: any_executable,
        install: Some(Install::Npm(&["@vue/language-server", CLASSIC_TYPESCRIPT])),
        applies: always,
        root: js_root,
        configure: configure_vue,
    },
    ServerSpec {
        id: "svelte",
        role: Role::Primary,
        languages: &["svelte"],
        candidates: &[("svelteserver", &["--stdio"])],
        accept: any_executable,
        install: Some(Install::Npm(&["svelte-language-server"])),
        applies: always,
        root: js_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "astro",
        role: Role::Primary,
        languages: &["astro"],
        candidates: &[("astro-ls", &["--stdio"])],
        accept: any_executable,
        install: Some(Install::Npm(&["@astrojs/language-server", CLASSIC_TYPESCRIPT])),
        applies: always,
        root: js_root,
        configure: configure_astro,
    },
    ServerSpec {
        id: "angular",
        role: Role::Addon,
        languages: &["html", "typescript"],
        candidates: &[("ngserver", &["--stdio"])],
        accept: any_executable,
        install: Some(Install::Npm(&["@angular/language-server", CLASSIC_TYPESCRIPT])),
        applies: |context| context.js.as_ref().is_some_and(|js| js.uses(Framework::Angular)),
        root: js_root,
        configure: configure_angular,
    },
    ServerSpec {
        id: "eslint",
        role: Role::Linter,
        languages: JS_AND_COMPONENTS,
        candidates: &[("vscode-eslint-language-server", &["--stdio"])],
        accept: any_executable,
        install: Some(LANGSERVERS_EXTRACTED),
        applies: |context| js_has(context, JsTool::Eslint) && !is_deno(context),
        root: js_root,
        configure: configure_eslint,
    },
    ServerSpec {
        id: "biome",
        role: Role::Linter,
        languages: &[
            "typescript",
            "typescriptreact",
            "javascript",
            "javascriptreact",
            "json",
            "jsonc",
            "css",
            "graphql",
            "vue",
            "svelte",
            "astro",
        ],
        candidates: &[("biome", &["lsp-proxy"])],
        accept: any_executable,
        install: Some(Install::Hint("add @biomejs/biome to the project's devDependencies")),
        applies: |context| js_has(context, JsTool::Biome),
        root: js_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "oxlint",
        role: Role::Linter,
        languages: JS_AND_COMPONENTS,
        candidates: &[("oxc_language_server", &[])],
        accept: any_executable,
        install: Some(Install::Hint("add oxlint to the project's devDependencies")),
        applies: |context| js_has(context, JsTool::Oxlint),
        root: js_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "tailwind",
        role: Role::Addon,
        languages: &[
            "html",
            "css",
            "scss",
            "less",
            "javascriptreact",
            "typescriptreact",
            "vue",
            "svelte",
            "astro",
        ],
        candidates: &[("tailwindcss-language-server", &["--stdio"])],
        accept: any_executable,
        install: Some(Install::Npm(&["@tailwindcss/language-server"])),
        applies: |context| js_has(context, JsTool::Tailwind),
        root: js_root,
        configure: configure_tailwind,
    },
    // --- Go ---
    ServerSpec {
        id: "gopls",
        role: Role::Primary,
        languages: &["go", "go.mod", "go.sum", "go.work", "gotmpl"],
        candidates: &[("gopls", &[])],
        accept: any_executable,
        install: Some(Install::Go("golang.org/x/tools/gopls@latest")),
        applies: always,
        root: go_root,
        configure: configure_gopls,
    },
    ServerSpec {
        id: "golangci-lint",
        role: Role::Linter,
        languages: &["go"],
        candidates: &[("golangci-lint-langserver", &[])],
        accept: any_executable,
        install: Some(Install::Go("github.com/nametake/golangci-lint-langserver@latest")),
        applies: |context| context.go.as_ref().is_some_and(|go| go.has(GoTool::GolangciLint)),
        root: go_root,
        configure: configure_golangci_lint,
    },
    ServerSpec {
        id: "templ",
        role: Role::Primary,
        languages: &["templ"],
        candidates: &[("templ", &["lsp"])],
        accept: any_executable,
        install: Some(Install::Go("github.com/a-h/templ/cmd/templ@latest")),
        applies: always,
        root: go_root,
        configure: no_settings,
    },
    // --- everything else ---
    ServerSpec {
        id: "python",
        role: Role::Primary,
        languages: &["python"],
        candidates: &[
            ("basedpyright-langserver", &["--stdio"]),
            ("pyright-langserver", &["--stdio"]),
            ("pylsp", &[]),
        ],
        accept: any_executable,
        install: Some(Install::Npm(&["pyright"])),
        applies: always,
        root: workspace_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "clangd",
        role: Role::Primary,
        languages: &["c", "cpp"],
        candidates: &[("clangd", &[])],
        accept: any_executable,
        install: None,
        applies: always,
        root: workspace_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "sourcekit",
        role: Role::Primary,
        languages: &["swift"],
        candidates: &[("sourcekit-lsp", &[])],
        accept: any_executable,
        install: None,
        applies: always,
        root: workspace_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "zls",
        role: Role::Primary,
        languages: &["zig"],
        candidates: &[("zls", &[])],
        accept: any_executable,
        install: None,
        applies: always,
        root: workspace_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "ruby",
        role: Role::Primary,
        languages: &["ruby"],
        candidates: &[("ruby-lsp", &[])],
        accept: any_executable,
        install: None,
        applies: always,
        root: workspace_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "css",
        role: Role::Primary,
        languages: &["css", "scss", "less"],
        candidates: &[("vscode-css-language-server", &["--stdio"])],
        accept: any_executable,
        install: Some(LANGSERVERS_EXTRACTED),
        applies: always,
        root: workspace_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "html",
        role: Role::Primary,
        languages: &["html"],
        candidates: &[("vscode-html-language-server", &["--stdio"])],
        accept: any_executable,
        install: Some(LANGSERVERS_EXTRACTED),
        applies: always,
        root: workspace_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "json",
        role: Role::Primary,
        languages: &["json", "jsonc"],
        candidates: &[("vscode-json-language-server", &["--stdio"])],
        accept: any_executable,
        install: Some(LANGSERVERS_EXTRACTED),
        applies: always,
        root: workspace_root,
        configure: configure_json,
    },
    ServerSpec {
        id: "yaml",
        role: Role::Primary,
        languages: &["yaml"],
        candidates: &[("yaml-language-server", &["--stdio"])],
        accept: any_executable,
        install: Some(Install::Npm(&["yaml-language-server"])),
        applies: always,
        root: workspace_root,
        configure: configure_yaml,
    },
    ServerSpec {
        id: "bash",
        role: Role::Primary,
        languages: &["shellscript"],
        candidates: &[("bash-language-server", &["start"])],
        accept: any_executable,
        install: Some(Install::Npm(&["bash-language-server"])),
        applies: always,
        root: workspace_root,
        configure: no_settings,
    },
    ServerSpec {
        id: "taplo",
        role: Role::Primary,
        languages: &["toml"],
        candidates: &[("taplo", &["lsp", "stdio"])],
        accept: any_executable,
        install: None,
        applies: always,
        root: workspace_root,
        configure: no_settings,
    },
];

/// Every server Hangar knows, for listing what can be installed.
pub fn all_servers() -> &'static [ServerSpec] {
    SERVERS
}

// --- per-server settings ---------------------------------------------------

/// rust-analyzer's defaults are good; these only turn on what an IDE wants
/// (build scripts and proc macros so macro-heavy crates resolve, `cargo
/// check` diagnostics on save). Functions complete as `name()` with the
/// cursor between the parentheses, as in IntelliJ, rather than with each
/// argument's name typed out for you to replace.
fn configure_rust_analyzer(config: &mut ServerConfig, _: &Resolution) -> Result<(), ResolveError> {
    // `~/.cargo/bin/rust-analyzer` is a rustup proxy that exists even when
    // the component is not installed, and then exits at once with an error.
    if !runs(Path::new(&config.command), &["--version"]) {
        return Err(ResolveError::Broken(format!(
            "{} does not run; run `rustup component add rust-analyzer`",
            config.command
        )));
    }
    let settings = json!({
        "cargo": { "buildScripts": { "enable": true }, "allTargets": true },
        "procMacro": { "enable": true },
        "checkOnSave": true,
        "check": { "command": "check" },
        "completion": {
            "callable": { "snippets": "add_parentheses" },
            "postfix": { "enable": true },
            "fullFunctionSignatures": { "enable": false }
        },
        "diagnostics": { "enable": true },
        "lens": { "enable": false },
        "hover": { "actions": { "enable": false } }
    });
    config.settings = Some(json!({ "rust-analyzer": settings.clone() }));
    config.default_section = Some("rust-analyzer".into());
    config.initialization_options = Some(settings);
    Ok(())
}

/// Preferences tsserver takes for both languages: auto-imports from every
/// package, completions that insert text rather than snippets of argument
/// names.
fn typescript_preferences() -> Value {
    json!({
        "includeCompletionsForModuleExports": true,
        "includeCompletionsForImportStatements": true,
        "includeCompletionsWithInsertText": true,
        "includeAutomaticOptionalChainCompletions": true,
        "importModuleSpecifierPreference": "shortest",
        "importModuleSpecifierEnding": "auto",
        "quotePreference": "auto",
        "allowRenameOfImportPath": true,
        "providePrefixAndSuffixTextForRename": true
    })
}

fn configure_typescript(config: &mut ServerConfig, resolution: &Resolution) -> Result<(), ResolveError> {
    let lib = resolution.typescript_lib();
    let preferences = typescript_preferences();
    match config.name.as_str() {
        "vtsls" => {
            let language = json!({ "preferences": preferences, "suggest": { "completeFunctionCalls": false } });
            let mut tree = json!({
                "typescript": language.clone(),
                "javascript": language,
                "vtsls": { "autoUseWorkspaceTsdk": true }
            });
            if let Some(lib) = lib {
                tree["typescript"]["tsdk"] = json!(lib);
            }
            config.settings = Some(tree);
        }
        _ => {
            let mut options = json!({
                "hostInfo": "hangar",
                "preferences": preferences,
                "maxTsServerMemory": 4096
            });
            if let Some(lib) = lib {
                options["tsserver"] = json!({ "path": lib.join("tsserver.js") });
            }
            config.initialization_options = Some(options);
        }
    }
    Ok(())
}

/// TypeScript 7 answers `workspace/configuration` for the same sections as
/// tsserver did.
fn configure_native_typescript(config: &mut ServerConfig, _: &Resolution) -> Result<(), ResolveError> {
    config.name = "typescript".into();
    let language = json!({ "preferences": typescript_preferences() });
    config.settings = Some(json!({ "typescript": language.clone(), "javascript": language }));
    Ok(())
}

fn configure_deno(config: &mut ServerConfig, _: &Resolution) -> Result<(), ResolveError> {
    let settings = json!({ "enable": true, "lint": true, "unstable": false });
    config.settings = Some(json!({ "deno": settings.clone() }));
    config.default_section = Some("deno".into());
    config.initialization_options = Some(settings);
    Ok(())
}

/// Vue's server needs a TypeScript to load. Without hybrid mode it handles
/// the script blocks itself instead of relying on a tsserver plugin.
fn configure_vue(config: &mut ServerConfig, resolution: &Resolution) -> Result<(), ResolveError> {
    let lib = resolution.typescript_lib().ok_or_else(|| {
        ResolveError::Broken("Vue's server needs TypeScript; add typescript to the project".into())
    })?;
    config.initialization_options = Some(json!({
        "typescript": { "tsdk": lib },
        "vue": { "hybridMode": false }
    }));
    Ok(())
}

fn configure_astro(config: &mut ServerConfig, resolution: &Resolution) -> Result<(), ResolveError> {
    if let Some(lib) = resolution.typescript_lib() {
        config.initialization_options = Some(json!({ "typescript": { "tsdk": lib } }));
    }
    Ok(())
}

/// ngserver finds TypeScript and Angular's compiler through probe
/// locations: the project's `node_modules` (in a trusted workspace), then
/// Hangar's.
fn configure_angular(config: &mut ServerConfig, resolution: &Resolution) -> Result<(), ResolveError> {
    let mut locations = Vec::new();
    if resolution.tools.trust_project {
        locations.push(resolution.root.join("node_modules"));
    }
    if let Some(managed) = &resolution.tools.managed {
        locations.push(managed.node_modules());
    }
    let joined = locations.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join(",");
    config.args.extend(["--tsProbeLocations".into(), joined.clone(), "--ngProbeLocations".into(), joined]);
    Ok(())
}

/// vscode-eslint's server asks for its settings with an empty section and
/// expects the object VS Code's extension would send.
fn configure_eslint(config: &mut ServerConfig, resolution: &Resolution) -> Result<(), ResolveError> {
    let package_manager = match resolution.context.js.as_ref().map(|js| js.package_manager) {
        Some(PackageManager::Pnpm) => "pnpm",
        Some(PackageManager::Yarn) => "yarn",
        _ => "npm",
    };
    let settings = json!({
        "validate": "on",
        "run": "onType",
        "packageManager": package_manager,
        "useESLintClass": false,
        "experimental": {},
        "useFlatConfig": null,
        "format": false,
        "quiet": false,
        "onIgnoredFiles": "off",
        "options": {},
        "rulesCustomizations": [],
        "problems": { "shortenToSingleLine": false },
        "nodePath": null,
        "workingDirectory": { "mode": "auto" },
        "workspaceFolder": { "uri": resolution.root_uri(), "name": resolution.root_name() },
        "codeAction": {
            "disableRuleComment": { "enable": true, "location": "separateLine" },
            "showDocumentation": { "enable": true }
        },
        "codeActionOnSave": { "enable": false, "mode": "all" }
    });
    config.settings = Some(json!({ "eslint": settings }));
    config.default_section = Some("eslint".into());
    Ok(())
}

fn configure_tailwind(config: &mut ServerConfig, _: &Resolution) -> Result<(), ResolveError> {
    config.settings = Some(json!({
        "tailwindCSS": {
            "validate": true,
            "emmetCompletions": false,
            "classAttributes": ["class", "className", "class:list", "classList", "ngClass"],
            "includeLanguages": {},
            "experimental": { "classRegex": [] },
            "lint": {
                "cssConflict": "warning",
                "invalidApply": "error",
                "invalidScreen": "error",
                "invalidVariant": "error",
                "invalidConfigPath": "error",
                "invalidTailwindDirective": "error",
                "recommendedVariantOrder": "warning"
            }
        },
        "editor": { "tabSize": 2 }
    }));
    Ok(())
}

/// gopls: auto-imports from unimported packages, staticcheck's analyzers,
/// and directories that are never Go source left out of the workspace.
fn configure_gopls(config: &mut ServerConfig, resolution: &Resolution) -> Result<(), ResolveError> {
    let templ = resolution
        .context
        .go
        .as_ref()
        .is_some_and(|go| go.uses(crate::project::GoFramework::Templ));
    let settings = json!({
        "completeUnimported": true,
        "usePlaceholders": false,
        "staticcheck": true,
        "semanticTokens": false,
        "analyses": {
            "unusedparams": true,
            "unusedwrite": true,
            "nilness": true,
            "useany": true
        },
        "directoryFilters": ["-**/node_modules", "-**/.git", "-**/vendor"],
        "templateExtensions": if templ { json!([]) } else { json!(["gotmpl", "tmpl", "gohtml"]) }
    });
    config.settings = Some(json!({ "gopls": settings.clone() }));
    config.default_section = Some("gopls".into());
    config.initialization_options = Some(settings);
    Ok(())
}

/// golangci-lint-langserver runs golangci-lint with the flags given here,
/// which changed between golangci-lint 1 and 2.
fn configure_golangci_lint(config: &mut ServerConfig, resolution: &Resolution) -> Result<(), ResolveError> {
    let linter = resolution.tools.find("golangci-lint").ok_or(ResolveError::NotInstalled {
        install: Some(Install::Hint("install golangci-lint from https://golangci-lint.run")),
    })?;
    let linter_path = linter.to_string_lossy().into_owned();
    let command: Vec<String> = match golangci_lint_major_version(&linter) {
        Some(1) => vec![linter_path, "run".into(), "--out-format".into(), "json".into(), "--issues-exit-code=1".into()],
        _ => vec![
            linter_path,
            "run".into(),
            "--output.json.path=stdout".into(),
            "--show-stats=false".into(),
            "--issues-exit-code=1".into(),
        ],
    };
    config.initialization_options = Some(json!({ "command": command }));
    Ok(())
}

/// Schemas for the config files of the ecosystems Hangar knows, so
/// `package.json` and `tsconfig.json` get completion and validation. The
/// JSON server fetches them itself and takes settings only by push.
fn configure_json(config: &mut ServerConfig, _: &Resolution) -> Result<(), ResolveError> {
    let schema = |files: &[&str], url: &str| json!({ "fileMatch": files, "url": url });
    config.settings = Some(json!({
        "json": {
            "validate": { "enable": true },
            "format": { "enable": true },
            "schemas": [
                schema(&["package.json"], "https://json.schemastore.org/package.json"),
                schema(&["tsconfig.json", "tsconfig.*.json"], "https://json.schemastore.org/tsconfig.json"),
                schema(&["jsconfig.json", "jsconfig.*.json"], "https://json.schemastore.org/jsconfig.json"),
                schema(&["deno.json", "deno.jsonc"], "https://raw.githubusercontent.com/denoland/deno/main/cli/schemas/config-file.v1.json"),
                schema(&["biome.json", "biome.jsonc"], "https://biomejs.dev/schemas/latest/schema.json"),
                schema(&[".eslintrc", ".eslintrc.json"], "https://json.schemastore.org/eslintrc.json"),
                schema(&[".prettierrc", ".prettierrc.json"], "https://json.schemastore.org/prettierrc.json"),
                schema(&["turbo.json"], "https://turbo.build/schema.json"),
                schema(&["vercel.json"], "https://openapi.vercel.sh/vercel.json"),
                schema(&[".golangci.json"], "https://golangci-lint.run/jsonschema/golangci.jsonschema.json")
            ]
        }
    }));
    config.initialization_options = Some(json!({ "provideFormatter": true }));
    config.push_settings = true;
    Ok(())
}

fn configure_yaml(config: &mut ServerConfig, _: &Resolution) -> Result<(), ResolveError> {
    let settings = json!({
        "validate": true,
        "completion": true,
        "hover": true,
        "schemaStore": { "enable": true, "url": "https://www.schemastore.org/api/json/catalog.json" }
    });
    config.settings = Some(json!({ "yaml": settings }));
    config.push_settings = true;
    Ok(())
}

// --- finding executables -------------------------------------------------

pub(crate) fn find_executable(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|dir| dir.join(name)).find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// `PATH` for a server process: the directory of its own executable first
/// (an nvm-installed server next to its `node`), then everything searched.
fn joined_path(dirs: &[PathBuf], command: &Path) -> String {
    let mut seen = std::collections::HashSet::new();
    let own = command.parent().map(Path::to_path_buf);
    let parts: Vec<PathBuf> = own
        .into_iter()
        .chain(dirs.iter().cloned())
        .filter(|dir| seen.insert(dir.clone()))
        .collect();
    std::env::join_paths(parts).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Most JavaScript servers are Node scripts (`#!/usr/bin/env node`). On a
/// machine with Bun but no Node, run them with Bun, which runs Node
/// programs as they are.
fn run_node_scripts_with_bun(config: &mut ServerConfig, dirs: &[PathBuf]) {
    if !is_node_script(Path::new(&config.command)) || find_executable("node", dirs).is_some() {
        return;
    }
    let Some(bun) = find_executable("bun", dirs) else { return };
    let script = std::mem::replace(&mut config.command, bun.to_string_lossy().into_owned());
    config.args.insert(0, script);
}

fn is_node_script(path: &Path) -> bool {
    use std::io::Read as _;
    let mut head = [0u8; 128];
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let Ok(read) = file.read(&mut head) else { return false };
    let first_line = head[..read].split(|b| *b == b'\n').next().unwrap_or_default();
    first_line.starts_with(b"#!") && first_line.windows(4).any(|w| w == b"node")
}

/// Whether `command args` exits successfully.
fn runs(command: &Path, args: &[&str]) -> bool {
    command_output(command, args).is_some()
}

/// What `command args` prints, if it exits successfully. Remembered per
/// command line, since a server is resolved for each project it serves and
/// version checks would otherwise run every time.
fn command_output(command: &Path, args: &[&str]) -> Option<String> {
    type Cache = Mutex<HashMap<(PathBuf, Vec<String>), Option<String>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let key = (command.to_path_buf(), args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
    let cache = CACHE.get_or_init(Default::default);
    if let Some(known) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return known;
    }
    let output = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned());
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, output.clone());
    }
    output
}

/// "golangci-lint has version 2.1.6 built with go1.24..." → 2.
fn golangci_lint_major_version(command: &Path) -> Option<u32> {
    parse_major_version(&command_output(command, &["--version"])?)
}

fn parse_major_version(text: &str) -> Option<u32> {
    let rest = &text[text.find("version ")? + "version ".len()..];
    rest.trim_start_matches('v').split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::FileContext;

    fn write(root: &Path, path: &str, text: &str) -> PathBuf {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn executable(root: &Path, path: &str, text: &str) -> PathBuf {
        let path = write(root, path, text);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn ids(plans: &[ServerPlan]) -> Vec<&'static str> {
        plans.iter().map(|plan| plan.spec.id).collect()
    }

    #[test]
    fn a_react_app_with_eslint_and_tailwind_gets_three_servers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", r#"{"dependencies": {"react": "19", "tailwindcss": "4", "eslint": "9"}}"#);
        write(root, "package-lock.json", "{}");
        let file = write(root, "src/App.tsx", "");
        let context = FileContext::detect(&file, "typescriptreact", root);
        let plans = plan(&context);
        assert_eq!(ids(&plans), ["typescript", "eslint", "tailwind"]);
        assert!(plans.iter().all(|p| p.root == root));
        assert_eq!(plans[0].key(), format!("typescript@{}", root.display()));

        // Plain `.ts` files are not Tailwind's business.
        let context = FileContext::detect(&root.join("src/util.ts"), "typescript", root);
        assert_eq!(ids(&plan(&context)), ["typescript", "eslint"]);
    }

    #[test]
    fn deno_replaces_typescript_and_eslint() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "deno.json", "{}");
        write(dir.path(), "eslint.config.js", "");
        let context = FileContext::detect(&dir.path().join("main.ts"), "typescript", dir.path());
        assert_eq!(ids(&plan(&context)), ["deno"]);
    }

    #[test]
    fn go_modules_get_gopls_at_the_module_and_golangci_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a/go.mod", "module a\n");
        write(root, "b/go.mod", "module b\n");
        write(root, "b/.golangci.yml", "");
        let a = plan(&FileContext::detect(&root.join("a/main.go"), "go", root));
        let b = plan(&FileContext::detect(&root.join("b/main.go"), "go", root));
        assert_eq!(ids(&a), ["gopls"]);
        assert_eq!(ids(&b), ["gopls", "golangci-lint"]);
        assert_ne!(a[0].key(), b[0].key(), "one gopls per module");
        let go_mod = plan(&FileContext::detect(&root.join("a/go.mod"), "go.mod", root));
        assert_eq!(go_mod[0].key(), a[0].key(), "go.mod shares the module's gopls");
    }

    #[test]
    fn project_binaries_win_and_settings_point_at_the_project_typescript() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", r#"{"devDependencies": {"typescript": "5"}}"#);
        write(root, "node_modules/typescript/lib/tsserver.js", "");
        write(root, "node_modules/typescript/package.json", r#"{"version": "5.6.3"}"#);
        let server = executable(root, "node_modules/.bin/typescript-language-server", "#!/bin/sh\n");
        let context = FileContext::detect(&root.join("index.ts"), "typescript", root);
        let plans = plan(&context);
        assert_eq!(ids(&plans), ["typescript-classic"]);
        let config = plans[0].resolve(&ToolDirs { trust_project: true, ..ToolDirs::default() }).unwrap();
        assert_eq!(config.command, server.to_string_lossy());
        assert_eq!(config.args, ["--stdio"]);
        let tsserver = &config.initialization_options.as_ref().unwrap()["tsserver"]["path"];
        assert_eq!(tsserver.as_str().unwrap(), root.join("node_modules/typescript/lib/tsserver.js").to_string_lossy());
        let path = config.env.iter().find(|(k, _)| k == "PATH").unwrap();
        assert!(path.1.starts_with(&*root.join("node_modules/.bin").to_string_lossy()));
    }

    #[test]
    fn servers_that_run_project_code_wait_for_trust() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", "{}");
        write(root, "eslint.config.js", "export default []");
        write(root, "biome.json", "{}");
        let ran = root.join("ran");
        let planted = format!("#!/bin/sh\ntouch {}\n", ran.display());
        for program in ["vscode-eslint-language-server", "biome"] {
            executable(root, &format!("node_modules/.bin/{program}"), &planted);
        }
        let context = FileContext::detect(&root.join("index.js"), "javascript", root);
        let plans = plan(&context);
        let find = |id: &str| plans.iter().find(|p| p.spec.id == id).unwrap_or_else(|| panic!("{id} planned"));
        let (untrusted, trusted) = (ToolDirs::default(), ToolDirs { trust_project: true, ..ToolDirs::default() });

        // ESLint loads the project's config, so it waits even when installed
        // globally. Biome lives only in the project, so it says why it is
        // off rather than "not installed".
        assert_eq!(find("eslint").resolve(&untrusted).unwrap_err(), ResolveError::NeedsTrust);
        assert_eq!(find("biome").resolve(&untrusted).unwrap_err(), ResolveError::NeedsTrust);
        assert!(!ran.exists());

        assert_eq!(find("eslint").resolve(&trusted).unwrap().command, root.join("node_modules/.bin/vscode-eslint-language-server").to_string_lossy());
        assert_eq!(find("biome").resolve(&trusted).unwrap().command, root.join("node_modules/.bin/biome").to_string_lossy());

        // rust-analyzer runs build scripts: off until trusted.
        let rust = plan(&FileContext::detect(&root.join("main.rs"), "rust", root));
        assert_eq!(rust[0].resolve(&untrusted).unwrap_err(), ResolveError::NeedsTrust);
        assert_ne!(rust[0].resolve(&trusted).err(), Some(ResolveError::NeedsTrust));
    }

    #[test]
    fn notebooks_are_trusted() {
        // Notebooks run in Hangar's own scratch project.
        assert_ne!(resolve_server("rust", &[]).err().as_deref(), Some("off until this workspace is trusted"));
    }

    #[test]
    fn untrusted_workspaces_never_run_project_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cloned");
        let ran = dir.path().join("ran");
        let planted = format!("#!/bin/sh\ntouch {}\necho 'Version 7.0.2'\n", ran.display());
        write(&root, "package.json", r#"{"devDependencies": {"typescript": "5"}}"#);
        write(&root, "node_modules/typescript/lib/tsserver.js", "");
        write(&root, "node_modules/typescript/package.json", r#"{"version": "5.6.3"}"#);
        executable(&root, "node_modules/.bin/typescript-language-server", &planted);
        let untrusted = ToolDirs::default();

        let classic = plan(&FileContext::detect(&root.join("index.ts"), "typescript", &root));
        if let Ok(config) = classic[0].resolve(&untrusted) {
            // A server installed on this machine may still be found.
            assert!(!config.command.starts_with(&*root.to_string_lossy()), "{}", config.command);
            let options = config.initialization_options.unwrap_or_default().to_string();
            assert!(!options.contains(&*root.to_string_lossy()), "no project TypeScript: {options}");
            let path = config.env.iter().find(|(k, _)| k == "PATH").unwrap();
            assert!(!path.1.contains(&*root.join("node_modules").to_string_lossy()));
        }

        // TypeScript 7: picking `tsc` runs `tsc --version`, which must not
        // be the project's.
        write(&root, "node_modules/typescript/package.json", r#"{"version": "7.0.2"}"#);
        executable(&root, "node_modules/.bin/tsc", &planted);
        let native = plan(&FileContext::detect(&root.join("index.ts"), "typescript", &root));
        assert_eq!(ids(&native), ["typescript"]);
        if let Ok(config) = native[0].resolve(&untrusted) {
            assert!(!config.command.starts_with(&*root.to_string_lossy()), "{}", config.command);
        }
        assert!(!ran.exists(), "a project binary ran in an untrusted workspace");
    }

    #[test]
    fn typescript_seven_serves_itself_and_old_compilers_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        write(&root, "package.json", r#"{"devDependencies": {"typescript": "^7"}}"#);
        write(&root, "node_modules/typescript/package.json", r#"{"version": "7.0.2"}"#);
        let native = executable(&root, "node_modules/.bin/tsc", "#!/bin/sh\necho 'Version 7.0.2'\n");
        // An old global TypeScript earlier in the search: no language server.
        executable(dir.path(), "global/tsc", "#!/bin/sh\necho 'Version 5.6.3'\n");
        let context = FileContext::detect(&root.join("src/index.ts"), "typescript", &root);
        let plans = plan(&context);
        assert_eq!(ids(&plans), ["typescript"]);

        let config = plans[0].resolve(&ToolDirs { trust_project: true, ..ToolDirs::default() }).unwrap();
        assert_eq!(config.command, native.to_string_lossy());
        assert_eq!(config.args, ["--lsp", "--stdio"]);

        // Only the old compiler around: not installed, with the fix.
        let loose = FileContext::detect(&dir.path().join("x.ts"), "typescript", dir.path());
        let tools = ToolDirs { search: vec![dir.path().join("global")], managed: None, trust_project: false };
        let only_old = ServerPlan { spec: plans[0].spec, root: dir.path().to_path_buf(), context: loose };
        if tools.find("tsc").is_some_and(|tsc| tsc.starts_with(dir.path())) {
            // PATH has no tsc of its own, so the old one is the only candidate.
            assert_eq!(
                only_old.resolve(&tools).unwrap_err(),
                ResolveError::NotInstalled { install: Some(Install::Npm(&["typescript"])) }
            );
        }
    }

    #[test]
    fn missing_servers_say_how_to_install_them() {
        let dir = tempfile::tempdir().unwrap();
        let context = FileContext::detect(&dir.path().join("main.go"), "go", dir.path());
        let gopls = plan(&context).remove(0);
        assert_eq!(gopls.spec.install, Some(Install::Go("golang.org/x/tools/gopls@latest")));
        let message = ResolveError::NotInstalled { install: gopls.spec.install.clone() }.to_string();
        assert!(message.contains("go install golang.org/x/tools/gopls@latest"), "{message}");
    }

    #[test]
    fn a_broken_rust_analyzer_is_reported_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        // Like rustup's proxy without the component: on PATH, exits with an error.
        let fake = executable(dir.path(), "rust-analyzer", "#!/bin/sh\nexit 1\n");
        let mut config = ServerConfig::new("rust-analyzer", fake, &[]);
        let context = FileContext::detect(&dir.path().join("main.rs"), "rust", dir.path());
        let tools = ToolDirs::default();
        let resolution = Resolution { context: &context, root: dir.path(), tools: &tools };
        let err = configure_rust_analyzer(&mut config, &resolution).unwrap_err();
        assert!(err.to_string().contains("rustup component add rust-analyzer"), "{err}");
    }

    #[test]
    fn node_scripts_run_with_bun_when_node_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let script = executable(dir.path(), "bin/server", "#!/usr/bin/env node\nconsole.log(1)\n");
        let bun = executable(dir.path(), "bin/bun", "#!/bin/sh\n");
        let mut config = ServerConfig::new("server", script.clone(), &["--stdio"]);
        run_node_scripts_with_bun(&mut config, &[dir.path().join("bin")]);
        assert_eq!(config.command, bun.to_string_lossy());
        assert_eq!(config.args, [script.to_string_lossy().into_owned(), "--stdio".into()]);

        // With node around, nothing changes.
        executable(dir.path(), "bin/node", "#!/bin/sh\n");
        let mut config = ServerConfig::new("server", script.clone(), &["--stdio"]);
        run_node_scripts_with_bun(&mut config, &[dir.path().join("bin")]);
        assert_eq!(config.command, script.to_string_lossy());
    }

    #[test]
    fn settings_sections_resolve() {
        let tree = json!({ "rust-analyzer": { "check": { "command": "check" } }, "editor": { "tabSize": 2 } });
        let check = settings_section(Some(&tree), Some("rust-analyzer"), Some("rust-analyzer.check"));
        assert_eq!(check["command"], "check");
        assert_eq!(settings_section(Some(&tree), Some("rust-analyzer"), None), tree["rust-analyzer"]);
        assert_eq!(settings_section(Some(&tree), Some("rust-analyzer"), Some("")), tree["rust-analyzer"]);
        assert_eq!(settings_section(Some(&tree), None, None), tree);
        assert_eq!(settings_section(Some(&tree), None, Some("editor.tabSize")), 2);
        assert!(settings_section(Some(&tree), None, Some("files")).is_null());
        assert!(settings_section(None, None, Some("gopls")).is_null());
    }

    #[test]
    fn overrides_merge_into_the_default_section_and_init_options() {
        let mut config = ServerConfig {
            settings: Some(json!({ "rust-analyzer": { "checkOnSave": true, "cargo": { "allTargets": true } } })),
            default_section: Some("rust-analyzer".into()),
            initialization_options: Some(json!({ "checkOnSave": true })),
            ..Default::default()
        };
        config.merge_settings(json!({ "checkOnSave": false, "cargo": { "targetDir": "/t" } }));
        let section = &config.settings.as_ref().unwrap()["rust-analyzer"];
        assert_eq!(section["checkOnSave"], false);
        assert_eq!(section["cargo"]["allTargets"], true);
        assert_eq!(section["cargo"]["targetDir"], "/t");
        assert_eq!(config.initialization_options.unwrap()["checkOnSave"], false);
    }

    #[test]
    fn eslint_gets_its_settings_for_an_empty_section() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", r#"{"devDependencies": {"eslint": "9"}}"#);
        write(root, "pnpm-lock.yaml", "");
        let context = FileContext::detect(&root.join("a.js"), "javascript", root);
        let eslint = plan(&context).into_iter().find(|p| p.spec.id == "eslint").unwrap();
        let mut config = ServerConfig::new("vscode-eslint-language-server", root.join("x"), &["--stdio"]);
        let tools = ToolDirs::default();
        configure_eslint(&mut config, &Resolution { context: &context, root: &eslint.root, tools: &tools }).unwrap();
        let answer = settings_section(config.settings.as_ref(), config.default_section.as_deref(), Some(""));
        assert_eq!(answer["validate"], "on");
        assert_eq!(answer["packageManager"], "pnpm");
        assert!(answer["workspaceFolder"]["uri"].as_str().unwrap().starts_with("file://"));
    }

    #[test]
    fn golangci_versions_parse() {
        assert_eq!(parse_major_version("golangci-lint has version 2.1.6 built with go1.24.3"), Some(2));
        assert_eq!(parse_major_version("golangci-lint has version v1.61.0 built"), Some(1));
        assert_eq!(parse_major_version("nonsense"), None);
    }

    #[test]
    fn notebooks_resolve_by_highlighter_language() {
        assert_eq!(resolve_server("text", &[]), Ok(None));
        let dir = tempfile::tempdir().unwrap();
        // An empty, non-executable file is not found at all.
        write(dir.path(), "zls", "");
        assert!(find_executable("zls", &[dir.path().to_path_buf()]).is_none());
    }
}

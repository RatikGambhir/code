//! What kind of project a file belongs to.
//!
//! Walks up from a file to the markers each ecosystem leaves behind
//! (`package.json`, lockfiles, `tsconfig.json`, `deno.json`, `go.mod`,
//! `go.work`, tool configs) and reads just enough of them to pick language
//! servers, their roots and settings, and the commands a project runs.
//!
//! Everything here is plain filesystem reads, so it is cheap enough to run
//! for every file that is opened (off the main thread).

use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// A file and the projects it is part of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContext {
    pub path: PathBuf,
    /// The LSP `languageId` of the file.
    pub language: &'static str,
    pub workspace_root: PathBuf,
    pub js: Option<JsProject>,
    pub go: Option<GoProject>,
}

impl FileContext {
    /// Detects the projects around `path`. Only the ecosystems that can
    /// involve `language` are looked at: a Go file never pays for reading
    /// `package.json`.
    pub fn detect(path: &Path, language: &'static str, workspace_root: &Path) -> Self {
        let dir = path.parent().unwrap_or(workspace_root);
        let js = involves_js(language).then(|| JsProject::detect(dir, workspace_root));
        let go = involves_go(language).then(|| GoProject::detect(dir, workspace_root));
        Self {
            path: path.to_path_buf(),
            language,
            workspace_root: workspace_root.to_path_buf(),
            js,
            go,
        }
    }

    /// A short description for the status bar: "Bun · TypeScript · Next.js".
    pub fn label(&self) -> Option<String> {
        if let Some(go) = &self.go {
            return Some(go.label());
        }
        self.js.as_ref().filter(|js| js.has_manifest).map(JsProject::label)
    }

    /// Commands the project offers: package scripts, `go test`, linters.
    pub fn tasks(&self) -> Vec<ProjectTask> {
        let mut tasks = Vec::new();
        if let Some(go) = &self.go {
            tasks.extend(go.tasks(&self.path));
        }
        if let Some(js) = &self.js {
            tasks.extend(js.tasks());
        }
        tasks
    }
}

/// Languages a JavaScript project's servers can serve: the languages
/// themselves, single-file components, and the HTML, CSS and JSON that
/// Tailwind and Biome also look at.
fn involves_js(language: &str) -> bool {
    matches!(
        language,
        "typescript"
            | "typescriptreact"
            | "javascript"
            | "javascriptreact"
            | "vue"
            | "svelte"
            | "astro"
            | "html"
            | "css"
            | "scss"
            | "less"
            | "json"
            | "jsonc"
    )
}

fn involves_go(language: &str) -> bool {
    matches!(language, "go" | "go.mod" | "go.sum" | "go.work" | "gotmpl" | "templ")
}

/// The directories from `dir` upward that can hold a project's markers:
/// up to the workspace root for files inside it, otherwise up to (not
/// including) the home directory, so a stray file never adopts a
/// `package.json` that happens to sit in `~`.
fn ancestors<'a>(dir: &'a Path, workspace_root: &'a Path) -> impl Iterator<Item = &'a Path> {
    let inside = dir.starts_with(workspace_root);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut done = false;
    dir.ancestors().take_while(move |candidate| {
        if done {
            return false;
        }
        if inside {
            done = *candidate == workspace_root;
            true
        } else {
            home.as_deref() != Some(*candidate) && candidate.parent().is_some()
        }
    })
}

fn has_any(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|name| dir.join(name).exists())
}

// --- JavaScript / TypeScript ---------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Runtime {
    Node,
    Bun,
    Deno,
}

impl Runtime {
    pub fn label(self) -> &'static str {
        match self {
            Runtime::Node => "Node",
            Runtime::Bun => "Bun",
            Runtime::Deno => "Deno",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Deno,
}

impl PackageManager {
    /// The executable, as typed in a terminal.
    pub fn program(self) -> &'static str {
        match self {
            PackageManager::Npm => "npm",
            PackageManager::Pnpm => "pnpm",
            PackageManager::Yarn => "yarn",
            PackageManager::Bun => "bun",
            PackageManager::Deno => "deno",
        }
    }

    /// `npm run build`, `deno task build`.
    fn run_script(self, script: &str) -> Vec<String> {
        let verb = if self == PackageManager::Deno { "task" } else { "run" };
        vec![verb.to_string(), script.to_string()]
    }

    fn from_package_manager_field(field: &str) -> Option<Self> {
        let name = field.split('@').next()?;
        Some(match name {
            "npm" => PackageManager::Npm,
            "pnpm" => PackageManager::Pnpm,
            "yarn" => PackageManager::Yarn,
            "bun" => PackageManager::Bun,
            _ => return None,
        })
    }

    /// The package manager a lockfile in `dir` belongs to.
    fn from_lockfile(dir: &Path) -> Option<Self> {
        if has_any(dir, &["bun.lock", "bun.lockb"]) {
            Some(PackageManager::Bun)
        } else if dir.join("pnpm-lock.yaml").exists() {
            Some(PackageManager::Pnpm)
        } else if dir.join("yarn.lock").exists() {
            Some(PackageManager::Yarn)
        } else if dir.join("package-lock.json").exists() {
            Some(PackageManager::Npm)
        } else {
            None
        }
    }
}

/// Frameworks worth knowing about: they bring their own language servers
/// (Vue, Svelte, Astro, Angular) or change what a project runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Framework {
    Next,
    Nuxt,
    SvelteKit,
    Remix,
    Astro,
    Angular,
    Vue,
    Svelte,
    Solid,
    Qwik,
    React,
    ReactNative,
    Expo,
    Electron,
    Nest,
    Express,
    Fastify,
    Hono,
    Elysia,
    Vite,
}

impl Framework {
    pub fn label(self) -> &'static str {
        match self {
            Framework::Next => "Next.js",
            Framework::Nuxt => "Nuxt",
            Framework::SvelteKit => "SvelteKit",
            Framework::Remix => "Remix",
            Framework::Astro => "Astro",
            Framework::Angular => "Angular",
            Framework::Vue => "Vue",
            Framework::Svelte => "Svelte",
            Framework::Solid => "Solid",
            Framework::Qwik => "Qwik",
            Framework::React => "React",
            Framework::ReactNative => "React Native",
            Framework::Expo => "Expo",
            Framework::Electron => "Electron",
            Framework::Nest => "NestJS",
            Framework::Express => "Express",
            Framework::Fastify => "Fastify",
            Framework::Hono => "Hono",
            Framework::Elysia => "Elysia",
            Framework::Vite => "Vite",
        }
    }

    fn from_dependency(name: &str) -> Option<Self> {
        Some(match name {
            "next" => Framework::Next,
            "nuxt" => Framework::Nuxt,
            "@sveltejs/kit" => Framework::SvelteKit,
            "@remix-run/react" | "@remix-run/node" | "@react-router/dev" => Framework::Remix,
            "astro" => Framework::Astro,
            "@angular/core" => Framework::Angular,
            "vue" => Framework::Vue,
            "svelte" => Framework::Svelte,
            "solid-js" => Framework::Solid,
            "@builder.io/qwik" | "@qwik.dev/core" => Framework::Qwik,
            "react" => Framework::React,
            "react-native" => Framework::ReactNative,
            "expo" => Framework::Expo,
            "electron" => Framework::Electron,
            "@nestjs/core" => Framework::Nest,
            "express" => Framework::Express,
            "fastify" => Framework::Fastify,
            "hono" => Framework::Hono,
            "elysia" => Framework::Elysia,
            "vite" => Framework::Vite,
            _ => return None,
        })
    }

    /// Frameworks implied by a more specific one, left out of labels.
    fn implied_by(self, others: &BTreeSet<Framework>) -> bool {
        let any = |list: &[Framework]| list.iter().any(|f| others.contains(f));
        match self {
            Framework::React => any(&[Framework::Next, Framework::Remix, Framework::ReactNative, Framework::Expo]),
            Framework::ReactNative => others.contains(&Framework::Expo),
            Framework::Vue => others.contains(&Framework::Nuxt),
            Framework::Svelte => others.contains(&Framework::SvelteKit),
            Framework::Vite => others.iter().any(|f| *f != Framework::Vite),
            _ => false,
        }
    }
}

/// Linters, formatters and test runners: some have language servers of
/// their own (ESLint, Biome, oxlint, Tailwind), all have commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum JsTool {
    Eslint,
    Biome,
    Oxlint,
    Prettier,
    Tailwind,
    Vitest,
    Jest,
    Playwright,
}

impl JsTool {
    pub fn label(self) -> &'static str {
        match self {
            JsTool::Eslint => "ESLint",
            JsTool::Biome => "Biome",
            JsTool::Oxlint => "oxlint",
            JsTool::Prettier => "Prettier",
            JsTool::Tailwind => "Tailwind",
            JsTool::Vitest => "Vitest",
            JsTool::Jest => "Jest",
            JsTool::Playwright => "Playwright",
        }
    }

    fn from_dependency(name: &str) -> Option<Self> {
        Some(match name {
            "eslint" => JsTool::Eslint,
            "@biomejs/biome" => JsTool::Biome,
            "oxlint" => JsTool::Oxlint,
            "prettier" => JsTool::Prettier,
            "tailwindcss" | "@tailwindcss/vite" | "@tailwindcss/postcss" | "@tailwindcss/cli" => JsTool::Tailwind,
            "vitest" => JsTool::Vitest,
            "jest" => JsTool::Jest,
            "@playwright/test" => JsTool::Playwright,
            _ => return None,
        })
    }

    /// Config files that turn a tool on even without a dependency (a
    /// globally installed ESLint, Tailwind through a CDN build).
    const CONFIG_FILES: &'static [(JsTool, &'static [&'static str])] = &[
        (
            JsTool::Eslint,
            &[
                "eslint.config.js",
                "eslint.config.mjs",
                "eslint.config.cjs",
                "eslint.config.ts",
                "eslint.config.mts",
                ".eslintrc",
                ".eslintrc.js",
                ".eslintrc.cjs",
                ".eslintrc.json",
                ".eslintrc.yml",
                ".eslintrc.yaml",
            ],
        ),
        (JsTool::Biome, &["biome.json", "biome.jsonc"]),
        (JsTool::Oxlint, &[".oxlintrc.json", "oxlintrc.json"]),
        (
            JsTool::Prettier,
            &[
                ".prettierrc",
                ".prettierrc.json",
                ".prettierrc.js",
                ".prettierrc.cjs",
                ".prettierrc.mjs",
                ".prettierrc.yaml",
                ".prettierrc.yml",
                "prettier.config.js",
                "prettier.config.mjs",
                "prettier.config.cjs",
            ],
        ),
        (
            JsTool::Tailwind,
            &["tailwind.config.js", "tailwind.config.cjs", "tailwind.config.mjs", "tailwind.config.ts"],
        ),
        (JsTool::Vitest, &["vitest.config.ts", "vitest.config.js", "vitest.config.mts", "vitest.workspace.ts"]),
        (JsTool::Jest, &["jest.config.js", "jest.config.ts", "jest.config.cjs", "jest.config.mjs"]),
        (JsTool::Playwright, &["playwright.config.ts", "playwright.config.js"]),
    ];
}

/// The parts of `package.json` that matter here.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PackageJson {
    dependencies: serde_json::Map<String, serde_json::Value>,
    dev_dependencies: serde_json::Map<String, serde_json::Value>,
    peer_dependencies: serde_json::Map<String, serde_json::Value>,
    scripts: serde_json::Map<String, serde_json::Value>,
    workspaces: Option<serde_json::Value>,
    package_manager: Option<String>,
}

impl PackageJson {
    fn read(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
        match serde_json::from_str(&text) {
            Ok(package) => Some(package),
            Err(err) => {
                tracing::debug!("{}/package.json: {err}", dir.display());
                // A half-typed package.json is still a project.
                Some(Self::default())
            }
        }
    }

    fn dependency_names(&self) -> impl Iterator<Item = &str> {
        self.dependencies
            .keys()
            .chain(self.dev_dependencies.keys())
            .chain(self.peer_dependencies.keys())
            .map(String::as_str)
    }
}

/// `deno.json`'s tasks.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DenoJson {
    tasks: serde_json::Map<String, serde_json::Value>,
}

/// Which TypeScript a project has installed, which decides its server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeScriptKind {
    /// TypeScript 7 and later (or `@typescript/native-preview`): the Go
    /// compiler, which is its own language server (`tsc --lsp`).
    Native,
    /// TypeScript 6 and earlier: `tsserver.js`, driven through
    /// typescript-language-server or vtsls.
    Classic,
    /// None installed (not yet, or a loose script).
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsProject {
    /// Where the project's servers run: the package manager's workspace
    /// (the directory with the lockfile or `workspaces`), else the nearest
    /// `package.json`, `deno.json` or `tsconfig.json`, else the workspace.
    pub root: PathBuf,
    /// The nearest directory with a `package.json` (or `deno.json`).
    pub package_dir: Option<PathBuf>,
    /// Any manifest or config was found; a loose script has none.
    pub has_manifest: bool,
    pub runtime: Runtime,
    pub package_manager: PackageManager,
    pub typescript: bool,
    pub frameworks: BTreeSet<Framework>,
    pub tools: BTreeSet<JsTool>,
    /// `package.json` scripts (or `deno.json` tasks) of `package_dir`, by
    /// name, in file order.
    pub scripts: Vec<(String, String)>,
    /// `node_modules/.bin` directories from `package_dir` up to `root`,
    /// nearest first, that exist.
    pub bin_dirs: Vec<PathBuf>,
    /// The project's own classic TypeScript (`node_modules/typescript/lib`
    /// with `tsserver.js`), which servers should use over a bundled one so
    /// versions match the build.
    pub typescript_lib: Option<PathBuf>,
    /// The installed TypeScript's version (`node_modules/typescript`).
    pub typescript_version: Option<String>,
    /// `@typescript/native-preview` is installed (its `tsgo` binary).
    pub native_preview: bool,
}

impl JsProject {
    pub fn detect(dir: &Path, workspace_root: &Path) -> Self {
        let mut package_dir = None;
        let mut package: Option<PackageJson> = None;
        let mut deno_dir = None;
        let mut tsconfig_dir = None;
        // The outermost directory that ties packages together.
        let mut workspace_dir = None;
        let mut package_manager = None;
        let mut frameworks = BTreeSet::new();
        let mut tools = BTreeSet::new();
        let mut bun_hint = false;
        let mut typescript = false;
        let mut bin_dirs = Vec::new();
        let mut typescript_lib = None;
        let mut typescript_version = None;
        let mut native_preview = false;

        for candidate in ancestors(dir, workspace_root) {
            let manifest = candidate.join("package.json").is_file().then(|| PackageJson::read(candidate)).flatten();
            if let Some(manifest) = &manifest {
                for name in manifest.dependency_names() {
                    frameworks.extend(Framework::from_dependency(name));
                    tools.extend(JsTool::from_dependency(name));
                    typescript |= name == "typescript";
                    bun_hint |= matches!(name, "@types/bun" | "bun-types");
                }
                if let Some(field) = &manifest.package_manager {
                    package_manager = package_manager.or(PackageManager::from_package_manager_field(field));
                }
            }
            for (tool, files) in JsTool::CONFIG_FILES {
                if has_any(candidate, files) {
                    tools.insert(*tool);
                }
            }
            if deno_dir.is_none() && has_any(candidate, &["deno.json", "deno.jsonc"]) {
                deno_dir = Some(candidate.to_path_buf());
            }
            if tsconfig_dir.is_none() && has_any(candidate, &["tsconfig.json", "jsconfig.json"]) {
                tsconfig_dir = Some(candidate.to_path_buf());
                typescript |= candidate.join("tsconfig.json").exists();
            }
            bun_hint |= candidate.join("bunfig.toml").exists();

            let bin = candidate.join("node_modules/.bin");
            if bin.is_dir() {
                native_preview |= bin.join("tsgo").exists();
                bin_dirs.push(bin);
            }
            // The nearest installed TypeScript is the one the build uses.
            if typescript_version.is_none() {
                let package = candidate.join("node_modules/typescript");
                if let Some(version) = installed_version(&package) {
                    let lib = package.join("lib");
                    if lib.join("tsserver.js").is_file() {
                        typescript_lib = Some(lib);
                    }
                    typescript_version = Some(version);
                }
            }

            let lockfile = PackageManager::from_lockfile(candidate);
            let declares_workspaces = manifest.as_ref().is_some_and(|m| m.workspaces.is_some())
                || candidate.join("pnpm-workspace.yaml").exists();
            if lockfile.is_some() || declares_workspaces {
                workspace_dir = Some(candidate.to_path_buf());
                // The outermost lockfile wins: nested ones are usually fixtures.
                if let Some(lockfile) = lockfile {
                    package_manager = Some(lockfile);
                }
            }
            if package_dir.is_none() && manifest.is_some() {
                package_dir = Some(candidate.to_path_buf());
                package = manifest;
            }
        }

        let runtime = if deno_dir.is_some() {
            Runtime::Deno
        } else if bun_hint
            || package_manager == Some(PackageManager::Bun)
            || package.as_ref().is_some_and(|p| scripts_use_bun(&p.scripts))
        {
            Runtime::Bun
        } else {
            Runtime::Node
        };
        let package_manager = match runtime {
            Runtime::Deno if package_dir.is_none() => PackageManager::Deno,
            Runtime::Bun if package_manager.is_none() => PackageManager::Bun,
            _ => package_manager.unwrap_or(PackageManager::Npm),
        };

        let has_manifest = package_dir.is_some() || deno_dir.is_some() || tsconfig_dir.is_some();
        let root = match runtime {
            // Deno's server wants the directory with its config.
            Runtime::Deno => deno_dir.clone(),
            _ => workspace_dir.or_else(|| package_dir.clone()).or(tsconfig_dir),
        }
        .unwrap_or_else(|| {
            if dir.starts_with(workspace_root) {
                workspace_root.to_path_buf()
            } else {
                dir.to_path_buf()
            }
        });

        let scripts = match (&package, &deno_dir) {
            (Some(package), _) => string_entries(&package.scripts),
            (None, Some(deno_dir)) => read_deno_tasks(deno_dir),
            (None, None) => Vec::new(),
        };

        Self {
            root,
            package_dir: package_dir.or(deno_dir),
            has_manifest,
            runtime,
            package_manager,
            typescript,
            frameworks,
            tools,
            scripts,
            bin_dirs,
            typescript_lib,
            typescript_version,
            native_preview,
        }
    }

    pub fn typescript_kind(&self) -> TypeScriptKind {
        let major = self
            .typescript_version
            .as_deref()
            .and_then(|v| v.split('.').next()?.parse::<u32>().ok());
        if self.native_preview || major.is_some_and(|major| major >= 7) {
            TypeScriptKind::Native
        } else if self.typescript_lib.is_some() {
            TypeScriptKind::Classic
        } else {
            TypeScriptKind::Missing
        }
    }

    pub fn has(&self, tool: JsTool) -> bool {
        self.tools.contains(&tool)
    }

    pub fn uses(&self, framework: Framework) -> bool {
        self.frameworks.contains(&framework)
    }

    /// "Bun · TypeScript · Next.js · Tailwind".
    pub fn label(&self) -> String {
        let mut parts = vec![self.runtime.label()];
        if self.typescript {
            parts.push("TypeScript");
        }
        parts.extend(
            self.frameworks
                .iter()
                .filter(|f| !f.implied_by(&self.frameworks))
                .map(|f| f.label()),
        );
        if self.has(JsTool::Tailwind) {
            parts.push(JsTool::Tailwind.label());
        }
        parts.join(" · ")
    }

    fn tasks(&self) -> Vec<ProjectTask> {
        let Some(cwd) = self.package_dir.clone() else { return Vec::new() };
        let pm = self.package_manager;
        let mut tasks: Vec<ProjectTask> = self
            .scripts
            .iter()
            .map(|(name, command)| ProjectTask {
                label: format!("{} {}", pm.program(), pm.run_script(name).join(" ")),
                detail: command.clone(),
                program: pm.program().to_string(),
                args: pm.run_script(name),
                cwd: cwd.clone(),
            })
            .collect();
        let mut add = |program: &str, args: &[&str], detail: &str, cwd: &Path| {
            tasks.push(ProjectTask {
                label: std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" "),
                detail: detail.to_string(),
                program: program.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                cwd: cwd.to_path_buf(),
            });
        };
        match pm {
            PackageManager::Deno => {}
            _ => add(pm.program(), &["install"], "Install dependencies", &self.root),
        }
        match self.runtime {
            Runtime::Bun if !self.scripts.iter().any(|(name, _)| name == "test") => {
                add("bun", &["test"], "Run tests with Bun's test runner", &cwd)
            }
            Runtime::Deno => {
                add("deno", &["test"], "Run tests", &cwd);
                add("deno", &["lint"], "Lint", &cwd);
                add("deno", &["fmt"], "Format", &cwd);
                add("deno", &["check", "."], "Type-check", &cwd);
            }
            _ => {}
        }
        if self.typescript && self.runtime != Runtime::Deno {
            add(self.exec_program(), &self.exec_args(&["tsc", "--noEmit"]), "Type-check the project", &cwd);
        }
        tasks
    }

    /// The runner for a package binary: `npx`, `pnpm exec`, `bunx`.
    fn exec_program(&self) -> &'static str {
        match self.package_manager {
            PackageManager::Npm | PackageManager::Deno => "npx",
            PackageManager::Pnpm => "pnpm",
            PackageManager::Yarn => "yarn",
            PackageManager::Bun => "bunx",
        }
    }

    fn exec_args<'a>(&self, args: &[&'a str]) -> Vec<&'a str> {
        let mut out = match self.package_manager {
            PackageManager::Pnpm => vec!["exec"],
            _ => Vec::new(),
        };
        out.extend_from_slice(args);
        out
    }
}

/// The `version` in an installed package's `package.json`.
fn installed_version(package_dir: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Manifest {
        version: String,
    }
    let text = std::fs::read_to_string(package_dir.join("package.json")).ok()?;
    serde_json::from_str::<Manifest>(&text).ok().map(|m| m.version)
}

fn scripts_use_bun(scripts: &serde_json::Map<String, serde_json::Value>) -> bool {
    scripts
        .values()
        .filter_map(serde_json::Value::as_str)
        .any(|script| script.starts_with("bun ") || script.contains(" bun ") || script.starts_with("bunx "))
}

fn string_entries(map: &serde_json::Map<String, serde_json::Value>) -> Vec<(String, String)> {
    map.iter()
        .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
        .collect()
}

fn read_deno_tasks(dir: &Path) -> Vec<(String, String)> {
    ["deno.json", "deno.jsonc"]
        .iter()
        .find_map(|name| std::fs::read_to_string(dir.join(name)).ok())
        .and_then(|text| serde_json::from_str::<DenoJson>(&strip_json_comments(&text)).ok())
        .map(|deno| {
            deno.tasks
                .iter()
                .filter_map(|(name, task)| {
                    // A task is a command, or `{ "command": ..., "description": ... }`.
                    let command = task.as_str().or_else(|| task.get("command")?.as_str())?;
                    Some((name.clone(), command.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Drops `//` and `/* */` comments outside strings, for JSONC files.
fn strip_json_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = '\0';
                for c in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

// --- Go --------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GoFramework {
    Gin,
    Echo,
    Fiber,
    Chi,
    GorillaMux,
    Grpc,
    Connect,
    Cobra,
    Templ,
    Gorm,
    Ent,
    Bubbletea,
    Testify,
    Ginkgo,
}

impl GoFramework {
    pub fn label(self) -> &'static str {
        match self {
            GoFramework::Gin => "Gin",
            GoFramework::Echo => "Echo",
            GoFramework::Fiber => "Fiber",
            GoFramework::Chi => "chi",
            GoFramework::GorillaMux => "gorilla/mux",
            GoFramework::Grpc => "gRPC",
            GoFramework::Connect => "Connect",
            GoFramework::Cobra => "Cobra",
            GoFramework::Templ => "templ",
            GoFramework::Gorm => "GORM",
            GoFramework::Ent => "ent",
            GoFramework::Bubbletea => "Bubble Tea",
            GoFramework::Testify => "testify",
            GoFramework::Ginkgo => "Ginkgo",
        }
    }

    fn from_module(path: &str) -> Option<Self> {
        const PREFIXES: &[(&str, GoFramework)] = &[
            ("github.com/gin-gonic/gin", GoFramework::Gin),
            ("github.com/labstack/echo", GoFramework::Echo),
            ("github.com/gofiber/fiber", GoFramework::Fiber),
            ("github.com/go-chi/chi", GoFramework::Chi),
            ("github.com/gorilla/mux", GoFramework::GorillaMux),
            ("google.golang.org/grpc", GoFramework::Grpc),
            ("connectrpc.com/connect", GoFramework::Connect),
            ("github.com/spf13/cobra", GoFramework::Cobra),
            ("github.com/a-h/templ", GoFramework::Templ),
            ("gorm.io/gorm", GoFramework::Gorm),
            ("entgo.io/ent", GoFramework::Ent),
            ("github.com/charmbracelet/bubbletea", GoFramework::Bubbletea),
            ("github.com/stretchr/testify", GoFramework::Testify),
            ("github.com/onsi/ginkgo", GoFramework::Ginkgo),
        ];
        PREFIXES
            .iter()
            .find(|(prefix, _)| path == *prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')))
            .map(|(_, framework)| *framework)
    }

    /// Test libraries are not worth a place in the status bar.
    fn is_test_library(self) -> bool {
        matches!(self, GoFramework::Testify | GoFramework::Ginkgo)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GoTool {
    GolangciLint,
    Air,
    GoReleaser,
    Sqlc,
    Buf,
    Task,
}

impl GoTool {
    const CONFIG_FILES: &'static [(GoTool, &'static [&'static str])] = &[
        (GoTool::GolangciLint, &[".golangci.yml", ".golangci.yaml", ".golangci.toml", ".golangci.json"]),
        (GoTool::Air, &[".air.toml"]),
        (GoTool::GoReleaser, &[".goreleaser.yml", ".goreleaser.yaml"]),
        (GoTool::Sqlc, &["sqlc.yaml", "sqlc.yml", "sqlc.json"]),
        (GoTool::Buf, &["buf.yaml", "buf.gen.yaml"]),
        (GoTool::Task, &["Taskfile.yml", "Taskfile.yaml"]),
    ];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoProject {
    /// Where gopls runs: the `go.work` directory, else the module's,
    /// else the file's own directory.
    pub root: PathBuf,
    /// The directory of the nearest `go.mod`.
    pub module_dir: Option<PathBuf>,
    pub module: Option<String>,
    pub go_version: Option<String>,
    /// A `go.work` ties several modules together.
    pub workspace: bool,
    pub frameworks: BTreeSet<GoFramework>,
    pub tools: BTreeSet<GoTool>,
}

impl GoProject {
    pub fn detect(dir: &Path, workspace_root: &Path) -> Self {
        let mut module_dir = None;
        let mut work_dir = None;
        let mut tools = BTreeSet::new();
        for candidate in ancestors(dir, workspace_root) {
            if module_dir.is_none() && candidate.join("go.mod").is_file() {
                module_dir = Some(candidate.to_path_buf());
            }
            if work_dir.is_none() && candidate.join("go.work").is_file() {
                work_dir = Some(candidate.to_path_buf());
            }
            for (tool, files) in GoTool::CONFIG_FILES {
                if has_any(candidate, files) {
                    tools.insert(*tool);
                }
            }
        }
        let manifest = module_dir
            .as_deref()
            .and_then(|dir| std::fs::read_to_string(dir.join("go.mod")).ok())
            .map(|text| GoMod::parse(&text))
            .unwrap_or_default();
        let frameworks = manifest
            .requires
            .iter()
            .chain(&manifest.tools)
            .filter_map(|path| GoFramework::from_module(path))
            .collect();
        let root = work_dir
            .clone()
            .or_else(|| module_dir.clone())
            .unwrap_or_else(|| dir.to_path_buf());
        Self {
            root,
            module_dir,
            module: manifest.module,
            go_version: manifest.go_version,
            workspace: work_dir.is_some(),
            frameworks,
            tools,
        }
    }

    pub fn has(&self, tool: GoTool) -> bool {
        self.tools.contains(&tool)
    }

    pub fn uses(&self, framework: GoFramework) -> bool {
        self.frameworks.contains(&framework)
    }

    /// "Go 1.23 · Gin · templ".
    pub fn label(&self) -> String {
        let mut parts = vec![match &self.go_version {
            Some(version) => format!("Go {version}"),
            None => "Go".to_string(),
        }];
        if self.workspace {
            parts.push("workspace".into());
        }
        parts.extend(
            self.frameworks
                .iter()
                .filter(|f| !f.is_test_library())
                .map(|f| f.label().to_string()),
        );
        parts.join(" · ")
    }

    fn tasks(&self, file: &Path) -> Vec<ProjectTask> {
        let Some(module_dir) = self.module_dir.clone() else { return Vec::new() };
        let package_dir = file.parent().unwrap_or(&module_dir).to_path_buf();
        let mut tasks = Vec::new();
        let mut add = |program: &str, args: &[&str], detail: &str, cwd: &Path| {
            tasks.push(ProjectTask {
                label: std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" "),
                detail: detail.to_string(),
                program: program.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                cwd: cwd.to_path_buf(),
            });
        };
        add("go", &["run", "."], "Run this file's package", &package_dir);
        add("go", &["test", "."], "Test this file's package", &package_dir);
        add("go", &["test", "./..."], "Test the module", &module_dir);
        add("go", &["build", "./..."], "Build the module", &module_dir);
        add("go", &["vet", "./..."], "Vet the module", &module_dir);
        add("go", &["mod", "tidy"], "Tidy go.mod and go.sum", &module_dir);
        add("go", &["generate", "./..."], "Run go:generate directives", &module_dir);
        if self.has(GoTool::GolangciLint) {
            add("golangci-lint", &["run"], "Lint with golangci-lint", &module_dir);
        }
        if self.uses(GoFramework::Templ) {
            add("templ", &["generate"], "Generate Go from templ files", &module_dir);
        }
        if self.has(GoTool::Air) {
            add("air", &[], "Run with live reload", &module_dir);
        }
        if self.has(GoTool::Sqlc) {
            add("sqlc", &["generate"], "Generate code from SQL", &module_dir);
        }
        if self.has(GoTool::Buf) {
            add("buf", &["generate"], "Generate code from protobufs", &module_dir);
        }
        tasks
    }
}

/// The parts of `go.mod` that matter here.
#[derive(Debug, Default, PartialEq, Eq)]
struct GoMod {
    module: Option<String>,
    go_version: Option<String>,
    requires: Vec<String>,
    /// Go 1.24 `tool` directives.
    tools: Vec<String>,
}

impl GoMod {
    fn parse(text: &str) -> Self {
        let mut parsed = Self::default();
        let mut block: Option<String> = None;
        for line in text.lines() {
            let line = line.split("//").next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if block.is_some() && line == ")" {
                block = None;
                continue;
            }
            let (directive, rest) = match &block {
                Some(directive) => (directive.as_str(), line),
                None => line.split_once(char::is_whitespace).map_or((line, ""), |(d, r)| (d, r.trim())),
            };
            if block.is_none() && rest == "(" {
                block = Some(directive.to_string());
                continue;
            }
            let first = rest.split_whitespace().next().map(|s| s.trim_matches('"').to_string());
            match directive {
                "module" => parsed.module = first,
                "go" => parsed.go_version = first,
                "require" => parsed.requires.extend(first),
                "tool" => parsed.tools.extend(first),
                _ => {}
            }
        }
        parsed
    }
}

// --- tasks -------------------------------------------------------------------

/// A command a project offers, run in a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTask {
    /// The command line as typed: `bun run dev`.
    pub label: String,
    /// What it does, or the script behind it.
    pub detail: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl fmt::Display for ProjectTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_bun_next_app_with_tailwind_and_eslint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "package.json",
            r#"{
                "scripts": {"dev": "next dev", "lint": "eslint ."},
                "dependencies": {"next": "15", "react": "19"},
                "devDependencies": {"typescript": "5", "tailwindcss": "4", "@types/bun": "1"}
            }"#,
        );
        write(root, "bun.lock", "{}");
        write(root, "eslint.config.mjs", "export default []");
        write(root, "tsconfig.json", "{}");
        write(root, "node_modules/typescript/lib/tsserver.js", "");
        write(root, "node_modules/typescript/package.json", r#"{"version": "5.6.3"}"#);
        std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
        write(root, "app/page.tsx", "");

        let js = JsProject::detect(&root.join("app"), root);
        assert_eq!(js.root, root);
        assert_eq!(js.runtime, Runtime::Bun);
        assert_eq!(js.package_manager, PackageManager::Bun);
        assert!(js.typescript && js.has_manifest);
        assert!(js.uses(Framework::Next) && js.uses(Framework::React));
        assert!(js.has(JsTool::Eslint) && js.has(JsTool::Tailwind));
        assert_eq!(js.typescript_lib, Some(root.join("node_modules/typescript/lib")));
        assert_eq!(js.typescript_version.as_deref(), Some("5.6.3"));
        assert_eq!(js.typescript_kind(), TypeScriptKind::Classic);
        assert_eq!(js.bin_dirs, vec![root.join("node_modules/.bin")]);
        assert_eq!(js.label(), "Bun · TypeScript · Next.js · Tailwind");

        let tasks = js.tasks();
        let labels: Vec<_> = tasks.iter().map(|t| t.label.as_str()).collect();
        assert!(labels.contains(&"bun run dev"), "{labels:?}");
        assert!(labels.contains(&"bun install"));
        assert!(labels.contains(&"bun test"));
        assert!(labels.contains(&"bunx tsc --noEmit"));
    }

    #[test]
    fn typescript_seven_is_native() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", r#"{"devDependencies": {"typescript": "^7"}}"#);
        write(root, "node_modules/typescript/package.json", r#"{"version": "7.0.2"}"#);
        write(root, "node_modules/typescript/lib/tsc.js", "");
        let js = JsProject::detect(root, root);
        assert_eq!(js.typescript_kind(), TypeScriptKind::Native);
        assert_eq!(js.typescript_lib, None, "TypeScript 7 has no tsserver.js");

        // Nothing installed yet.
        let empty = tempfile::tempdir().unwrap();
        write(empty.path(), "package.json", r#"{"devDependencies": {"typescript": "^7"}}"#);
        assert_eq!(JsProject::detect(empty.path(), empty.path()).typescript_kind(), TypeScriptKind::Missing);
    }

    #[test]
    fn a_pnpm_monorepo_runs_servers_at_the_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", r#"{"private": true, "packageManager": "pnpm@9.1.0"}"#);
        write(root, "pnpm-workspace.yaml", "packages: ['packages/*']");
        write(root, "packages/web/package.json", r#"{"dependencies": {"vue": "3"}, "scripts": {"build": "vite build"}}"#);
        write(root, "packages/web/src/App.vue", "");

        let js = JsProject::detect(&root.join("packages/web/src"), root);
        assert_eq!(js.root, root);
        assert_eq!(js.package_dir, Some(root.join("packages/web")));
        assert_eq!(js.package_manager, PackageManager::Pnpm);
        assert_eq!(js.runtime, Runtime::Node);
        assert!(js.uses(Framework::Vue));
        assert_eq!(js.scripts, vec![("build".to_string(), "vite build".to_string())]);
        let build = js.tasks().into_iter().find(|t| t.label == "pnpm run build").unwrap();
        assert_eq!(build.cwd, root.join("packages/web"));
    }

    #[test]
    fn deno_projects_use_their_config_and_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "api/deno.jsonc",
            r#"{
                // comments are allowed
                "tasks": {"start": "deno run -A main.ts", "check": {"command": "deno check", "description": "x"}}
            }"#,
        );
        write(root, "api/main.ts", "");
        let js = JsProject::detect(&root.join("api"), root);
        assert_eq!(js.runtime, Runtime::Deno);
        assert_eq!(js.package_manager, PackageManager::Deno);
        assert_eq!(js.root, root.join("api"));
        assert_eq!(js.scripts.len(), 2);
        let labels: Vec<_> = js.tasks().into_iter().map(|t| t.label).collect();
        assert!(labels.contains(&"deno task start".to_string()), "{labels:?}");
        assert!(!labels.iter().any(|l| l.ends_with("install")));
    }

    #[test]
    fn a_loose_script_has_no_manifest_and_runs_at_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "scripts/x.js", "");
        let js = JsProject::detect(&dir.path().join("scripts"), dir.path());
        assert!(!js.has_manifest);
        assert_eq!(js.root, dir.path());
        assert_eq!(js.runtime, Runtime::Node);
        assert!(js.tasks().is_empty());
    }

    #[test]
    fn go_mod_is_parsed() {
        let parsed = GoMod::parse(
            "module example.com/app // the app\n\ngo 1.23.2\n\nrequire github.com/spf13/cobra v1.8.0\n\n\
             require (\n\tgithub.com/gin-gonic/gin v1.10.0\n\tgorm.io/gorm v1.25.0 // indirect\n)\n\
             tool github.com/a-h/templ/cmd/templ\n",
        );
        assert_eq!(parsed.module.as_deref(), Some("example.com/app"));
        assert_eq!(parsed.go_version.as_deref(), Some("1.23.2"));
        assert_eq!(parsed.requires, ["github.com/spf13/cobra", "github.com/gin-gonic/gin", "gorm.io/gorm"]);
        assert_eq!(parsed.tools, ["github.com/a-h/templ/cmd/templ"]);
    }

    #[test]
    fn go_workspaces_and_frameworks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "go.work", "go 1.23\nuse ./svc\n");
        write(root, "svc/go.mod", "module example.com/svc\ngo 1.23\nrequire github.com/labstack/echo/v4 v4.12.0\n");
        write(root, "svc/.golangci.yml", "");
        write(root, "svc/cmd/main.go", "");

        let go = GoProject::detect(&root.join("svc/cmd"), root);
        assert_eq!(go.root, root);
        assert_eq!(go.module_dir, Some(root.join("svc")));
        assert!(go.workspace);
        assert!(go.uses(GoFramework::Echo));
        assert!(go.has(GoTool::GolangciLint));
        assert_eq!(go.label(), "Go 1.23 · workspace · Echo");
        let tasks = go.tasks(&root.join("svc/cmd/main.go"));
        let run = tasks.iter().find(|t| t.label == "go run .").unwrap();
        assert_eq!(run.cwd, root.join("svc/cmd"));
        assert!(tasks.iter().any(|t| t.label == "golangci-lint run"));
        assert_eq!(GoFramework::from_module("github.com/gin-gonic/ginx"), None);
    }

    #[test]
    fn file_context_only_reads_relevant_ecosystems() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod", "module m\n");
        write(dir.path(), "package.json", "{}");
        let go_file = FileContext::detect(&dir.path().join("main.go"), "go", dir.path());
        assert!(go_file.go.is_some() && go_file.js.is_none());
        assert_eq!(go_file.label().as_deref(), Some("Go"));
        let ts_file = FileContext::detect(&dir.path().join("x.ts"), "typescript", dir.path());
        assert!(ts_file.js.is_some() && ts_file.go.is_none());
    }

    #[test]
    fn comments_are_stripped_outside_strings() {
        let text = "{\"a\": \"http://x\", // c\n/* b */ \"b\": 1}";
        assert_eq!(strip_json_comments(text), "{\"a\": \"http://x\", \n \"b\": 1}");
    }
}

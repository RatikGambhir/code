//! Cargo: the workspace's targets, run configurations, building a target for
//! the debugger, and finding what can be run in a source file (`fn main`,
//! tests, test modules) for the editor's gutter.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
};

use anyhow::{bail, Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Lib,
    Bin,
    Example,
    Test,
    Bench,
}

impl TargetKind {
    fn from_cargo(kinds: &[String]) -> Option<Self> {
        kinds.iter().find_map(|k| match k.as_str() {
            "bin" => Some(TargetKind::Bin),
            "example" => Some(TargetKind::Example),
            "test" => Some(TargetKind::Test),
            "bench" => Some(TargetKind::Bench),
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => Some(TargetKind::Lib),
            _ => None,
        })
    }

    /// The flag that selects a target of this kind (`--bin`).
    fn flag(self) -> &'static str {
        match self {
            TargetKind::Lib => "--lib",
            TargetKind::Bin => "--bin",
            TargetKind::Example => "--example",
            TargetKind::Test => "--test",
            TargetKind::Bench => "--bench",
        }
    }

    /// The `kind` cargo reports for artifacts of this target.
    fn artifact_kind(self) -> &'static str {
        match self {
            TargetKind::Lib => "lib",
            TargetKind::Bin => "bin",
            TargetKind::Example => "example",
            TargetKind::Test => "test",
            TargetKind::Bench => "bench",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CargoTarget {
    pub package: String,
    /// The directory of the package's `Cargo.toml` (where `cargo test` runs
    /// tests).
    pub package_dir: PathBuf,
    pub name: String,
    pub kind: TargetKind,
    pub src_path: PathBuf,
}

impl CargoTarget {
    fn root(&self) -> &Path {
        self.src_path.parent().unwrap_or(Path::new(""))
    }
}

/// The targets of every package in the workspace at `root`.
pub fn workspace_targets(root: &Path) -> Result<Vec<CargoTarget>> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!("cargo metadata failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    parse_metadata(&output.stdout)
}

fn parse_metadata(json: &[u8]) -> Result<Vec<CargoTarget>> {
    #[derive(Deserialize)]
    struct Metadata {
        packages: Vec<Package>,
    }
    #[derive(Deserialize)]
    struct Package {
        name: String,
        manifest_path: PathBuf,
        targets: Vec<Target>,
    }
    #[derive(Deserialize)]
    struct Target {
        name: String,
        kind: Vec<String>,
        src_path: PathBuf,
    }
    let metadata: Metadata = serde_json::from_slice(json).context("decoding cargo metadata")?;
    let mut targets = Vec::new();
    for package in metadata.packages {
        for target in package.targets {
            if let Some(kind) = TargetKind::from_cargo(&target.kind) {
                targets.push(CargoTarget {
                    package: package.name.clone(),
                    package_dir: package.manifest_path.parent().map(Path::to_path_buf).unwrap_or_default(),
                    name: target.name,
                    kind,
                    src_path: target.src_path,
                });
            }
        }
    }
    Ok(targets)
}

/// Something to run or debug: a binary or example, or tests of a target.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunConfig {
    pub package: String,
    pub target_kind: TargetKind,
    pub target: String,
    /// Run tests (`cargo test`) rather than the target itself.
    pub test: bool,
    /// For tests: the filter (`tests::adds`), matched exactly when `exact`.
    #[serde(default)]
    pub test_filter: Option<String>,
    #[serde(default)]
    pub exact: bool,
    /// Arguments for the program.
    #[serde(default)]
    pub args: Vec<String>,
}

impl RunConfig {
    pub fn binary(target: &CargoTarget) -> Self {
        Self {
            package: target.package.clone(),
            target_kind: target.kind,
            target: target.name.clone(),
            test: false,
            test_filter: None,
            exact: false,
            args: Vec::new(),
        }
    }

    pub fn tests(target: &CargoTarget, filter: Option<String>, exact: bool) -> Self {
        Self {
            package: target.package.clone(),
            target_kind: target.kind,
            target: target.name.clone(),
            test: true,
            test_filter: filter,
            exact,
            args: Vec::new(),
        }
    }

    /// A short name for menus: `server`, `example demo`, `test parse::tests::empty`.
    pub fn label(&self) -> String {
        if self.test {
            return match &self.test_filter {
                Some(filter) => format!("test {filter}"),
                None => match self.target_kind {
                    TargetKind::Lib => format!("tests in {}", self.package),
                    _ => format!("tests in {}", self.target),
                },
            };
        }
        match self.target_kind {
            TargetKind::Example => format!("example {}", self.target),
            _ => self.target.clone(),
        }
    }

    fn select_target(&self, args: &mut Vec<String>) {
        args.extend(["--package".into(), self.package.clone()]);
        args.push(self.target_kind.flag().into());
        if self.target_kind != TargetKind::Lib {
            args.push(self.target.clone());
        }
    }

    /// Arguments for the program itself (after `--` for cargo).
    pub fn program_args(&self, debugging: bool) -> Vec<String> {
        let mut args = Vec::new();
        if self.test {
            if let Some(filter) = &self.test_filter {
                args.push(filter.clone());
            }
            if self.exact {
                args.push("--exact".into());
            }
            args.push("--nocapture".into());
            if debugging {
                // One thread, so stepping is not interleaved with other tests.
                args.push("--test-threads=1".into());
            }
        }
        args.extend(self.args.iter().cloned());
        args
    }

    /// `cargo …` for Run: builds and runs in a terminal.
    pub fn cargo_run_args(&self) -> Vec<String> {
        let mut args = vec![if self.test { "test".to_string() } else { "run".to_string() }];
        self.select_target(&mut args);
        let program = self.program_args(false);
        if !program.is_empty() {
            args.push("--".into());
            args.extend(program);
        }
        args
    }

    /// `cargo …` for Debug: builds without running, reporting artifacts as
    /// JSON on stdout.
    pub fn cargo_build_args(&self) -> Vec<String> {
        let mut args = vec![if self.test { "test".to_string() } else { "build".to_string() }];
        if self.test {
            args.push("--no-run".into());
        }
        self.select_target(&mut args);
        args.push("--message-format=json-render-diagnostics".into());
        args
    }

    /// The executable in one line of cargo's JSON output, if it is this
    /// configuration's.
    pub fn artifact_in(&self, line: &str) -> Option<PathBuf> {
        let message: Value = serde_json::from_str(line).ok()?;
        if message.get("reason")?.as_str()? != "compiler-artifact" {
            return None;
        }
        let executable = message.get("executable")?.as_str()?;
        let target = message.get("target")?;
        if target.get("name")?.as_str()? != self.target {
            return None;
        }
        let kinds: Vec<&str> = target.get("kind")?.as_array()?.iter().filter_map(Value::as_str).collect();
        let kind_matches = match self.target_kind {
            TargetKind::Lib => kinds.iter().any(|k| TargetKind::from_cargo(&[k.to_string()]) == Some(TargetKind::Lib)),
            kind => kinds.contains(&kind.artifact_kind()),
        };
        let is_test = message.pointer("/profile/test").and_then(Value::as_bool).unwrap_or(false);
        (kind_matches && is_test == self.test).then(|| PathBuf::from(executable))
    }
}

/// A running `cargo build` for the debugger.
pub struct Build {
    child: Arc<Mutex<Option<Child>>>,
}

pub enum BuildEvent {
    /// A line of cargo's human output (progress and diagnostics).
    Line(String),
    /// The build ended: the executable, or why there is none.
    Finished(Result<PathBuf, String>),
}

impl Build {
    pub fn start(config: &RunConfig, root: &Path) -> Result<(Self, async_channel::Receiver<BuildEvent>)> {
        let mut child = Command::new("cargo")
            .args(config.cargo_build_args())
            .current_dir(root)
            .env("CARGO_TERM_COLOR", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting cargo")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        let child = Arc::new(Mutex::new(Some(child)));
        let (tx, rx) = async_channel::unbounded();

        let lines = tx.clone();
        let stderr_thread = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = lines.send_blocking(BuildEvent::Line(line));
            }
        });
        let config = config.clone();
        let waiter = child.clone();
        std::thread::spawn(move || {
            let mut executable = None;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(path) = config.artifact_in(&line) {
                    executable = Some(path);
                }
            }
            let _ = stderr_thread.join();
            let status = waiter.lock().take().map(|mut c| c.wait());
            let result = match (status, executable) {
                (Some(Ok(status)), Some(path)) if status.success() => Ok(path),
                (Some(Ok(status)), _) if !status.success() => Err(match status.code() {
                    Some(code) => format!("cargo exited with status {code}"),
                    None => "the build was stopped".to_string(),
                }),
                (None, _) => Err("the build was stopped".to_string()),
                _ => Err(format!("cargo built nothing to run for {}", config.label())),
            };
            let _ = tx.send_blocking(BuildEvent::Finished(result));
        });
        Ok((Self { child }, rx))
    }

    pub fn cancel(&self) {
        if let Some(child) = self.child.lock().as_mut() {
            let _ = child.kill();
        }
    }
}

impl Drop for Build {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The target a source file belongs to: the target it is the root of, else
/// the target whose directory holds it (a library before a binary sharing
/// `src/`).
pub fn owning_target<'a>(path: &Path, targets: &'a [CargoTarget]) -> Option<&'a CargoTarget> {
    if let Some(target) = targets.iter().find(|t| t.src_path == path) {
        return Some(target);
    }
    let rank = |t: &CargoTarget| match t.kind {
        TargetKind::Lib => 0,
        TargetKind::Bin => 1,
        _ => 2,
    };
    targets
        .iter()
        .filter(|t| path.starts_with(t.root()))
        .max_by(|a, b| {
            a.root()
                .components()
                .count()
                .cmp(&b.root().components().count())
                .then(rank(b).cmp(&rank(a)))
        })
}

/// The module path of `path` within `target` (`["parse", "lexer"]`).
pub fn module_path(path: &Path, target: &CargoTarget) -> Vec<String> {
    if path == target.src_path {
        return Vec::new();
    }
    let Ok(relative) = path.strip_prefix(target.root()) else { return Vec::new() };
    let mut parts: Vec<String> = relative
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.last().is_some_and(|p| p == "mod") {
        parts.pop();
    }
    parts
}

/// A line the gutter offers to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runnable {
    /// Zero-based line of the `fn` or `mod`.
    pub line: usize,
    pub config: RunConfig,
}

/// `fn main` of binaries and examples, `#[test]` functions and test modules
/// in `text` (the contents of `path`).
pub fn find_runnables(path: &Path, text: &str, targets: &[CargoTarget]) -> Vec<Runnable> {
    let Some(target) = owning_target(path, targets) else { return Vec::new() };
    let base = module_path(path, target);
    let can_run_main = path == target.src_path && matches!(target.kind, TargetKind::Bin | TargetKind::Example);
    let mut runnables = Vec::new();
    let mut modules: Vec<(String, i32)> = Vec::new();
    let mut depth: i32 = 0;
    let mut test_attribute = false;
    let mut cfg_test = false;
    let mut in_block_comment = false;

    for (line_ix, raw) in text.lines().enumerate() {
        let line = strip_comments(raw, &mut in_block_comment);
        let trimmed = line.trim();
        if trimmed.starts_with("#[") {
            let attr = trimmed.trim_start_matches("#[");
            if is_test_attribute(attr) {
                test_attribute = true;
            }
            if attr.starts_with("cfg(test)") {
                cfg_test = true;
            }
        } else if !trimmed.is_empty() {
            if let Some(name) = fn_name(trimmed) {
                let mut path_parts = base.clone();
                path_parts.extend(modules.iter().map(|(m, _)| m.clone()));
                if test_attribute {
                    path_parts.push(name.to_string());
                    runnables.push(Runnable {
                        line: line_ix,
                        config: RunConfig::tests(target, Some(path_parts.join("::")), true),
                    });
                } else if name == "main" && depth == 0 && can_run_main {
                    runnables.push(Runnable { line: line_ix, config: RunConfig::binary(target) });
                }
            } else if let Some(name) = mod_name(trimmed) {
                if cfg_test || name == "tests" {
                    let mut path_parts = base.clone();
                    path_parts.extend(modules.iter().map(|(m, _)| m.clone()));
                    path_parts.push(name.to_string());
                    runnables.push(Runnable {
                        line: line_ix,
                        config: RunConfig::tests(target, Some(path_parts.join("::")), false),
                    });
                }
                if trimmed.ends_with('{') {
                    modules.push((name.to_string(), depth));
                }
            }
            test_attribute = false;
            cfg_test = false;
        }
        for c in line.chars() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    while modules.last().is_some_and(|(_, d)| depth <= *d) {
                        modules.pop();
                    }
                }
                _ => {}
            }
        }
    }
    runnables
}

fn is_test_attribute(attr: &str) -> bool {
    let name = attr.split(['(', ']']).next().unwrap_or("").trim();
    name == "test" || name.ends_with("::test") || name == "rstest" || name == "test_case"
}

/// The name in `[pub] [async] [unsafe] [const] fn name(`.
pub fn fn_name(line: &str) -> Option<&str> {
    let mut rest = line;
    loop {
        // `pub(crate)` and friends count as one word.
        if rest.starts_with("pub(") {
            rest = rest[rest.find(')')? + 1..].trim_start();
            continue;
        }
        let word_end = rest.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(rest.len());
        match &rest[..word_end] {
            "fn" => {
                let after = rest[word_end..].trim_start();
                let end = after.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
                return (end > 0).then(|| &after[..end]);
            }
            "pub" | "async" | "unsafe" | "const" | "extern" | "\"C\"" | "default" => {}
            _ => return None,
        }
        rest = rest[word_end..].trim_start();
    }
}

fn mod_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("pub ").or_else(|| line.strip_prefix("pub(crate) ")).unwrap_or(line);
    let rest = rest.strip_prefix("mod ")?.trim_start();
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// The line without comments and string contents, so braces in them do not
/// count. `in_block` carries an open `/* */` comment to the next line.
pub fn strip_comments(line: &str, in_block: &mut bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if *in_block {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                *in_block = false;
            }
            continue;
        }
        if in_string {
            if c == '\\' {
                chars.next();
            } else if c == '"' {
                in_string = false;
                out.push('"');
            }
            continue;
        }
        match c {
            '/' if chars.peek() == Some(&'/') => break,
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                *in_block = true;
            }
            '"' => {
                in_string = true;
                out.push('"');
            }
            '\'' => {
                // A char literal like '{' (but not a lifetime).
                let rest: String = chars.clone().take(3).collect();
                if rest.len() >= 2 && rest.chars().nth(1) == Some('\'') {
                    chars.next();
                    chars.next();
                } else if rest.starts_with('\\') {
                    for c in chars.by_ref() {
                        if c == '\'' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> Vec<CargoTarget> {
        let t = |name: &str, kind, src: &str| CargoTarget {
            package: "demo".into(),
            package_dir: PathBuf::from("/w"),
            name: name.into(),
            kind,
            src_path: PathBuf::from(src),
        };
        vec![
            t("demo", TargetKind::Lib, "/w/src/lib.rs"),
            t("demo", TargetKind::Bin, "/w/src/main.rs"),
            t("tool", TargetKind::Bin, "/w/src/bin/tool.rs"),
            t("api", TargetKind::Test, "/w/tests/api.rs"),
            t("hello", TargetKind::Example, "/w/examples/hello.rs"),
        ]
    }

    #[test]
    fn parses_metadata() {
        let json = br#"{"packages":[{"name":"demo","manifest_path":"/w/Cargo.toml","targets":[
            {"name":"demo","kind":["lib"],"src_path":"/w/src/lib.rs"},
            {"name":"demo","kind":["bin"],"src_path":"/w/src/main.rs"},
            {"name":"build-script-build","kind":["custom-build"],"src_path":"/w/build.rs"}]}]}"#;
        let targets = parse_metadata(json).unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[1].kind, TargetKind::Bin);
        assert_eq!(targets[1].package_dir, PathBuf::from("/w"));
    }

    #[test]
    fn picks_the_owning_target() {
        let targets = targets();
        let owner = |p: &str| owning_target(Path::new(p), &targets).map(|t| (t.name.as_str(), t.kind));
        assert_eq!(owner("/w/src/main.rs"), Some(("demo", TargetKind::Bin)));
        assert_eq!(owner("/w/src/parse/mod.rs"), Some(("demo", TargetKind::Lib)));
        assert_eq!(owner("/w/src/bin/tool.rs"), Some(("tool", TargetKind::Bin)));
        assert_eq!(owner("/w/tests/api.rs"), Some(("api", TargetKind::Test)));
        assert_eq!(owner("/elsewhere/x.rs"), None);
    }

    #[test]
    fn finds_main_tests_and_test_modules() {
        let text = r#"
fn main() {
    let s = "}";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {}

    mod nested {
        #[tokio::test]
        async fn waits() { let c = '{'; }
    }

    #[test]
    #[should_panic]
    pub(crate) fn panics() {}
}
"#;
        let runnables = find_runnables(Path::new("/w/src/main.rs"), text, &targets());
        let labels: Vec<_> = runnables.iter().map(|r| (r.line, r.config.label())).collect();
        assert_eq!(
            labels,
            vec![
                (1, "demo".to_string()),
                (6, "test tests".to_string()),
                (10, "test tests::adds".to_string()),
                (14, "test tests::nested::waits".to_string()),
                (19, "test tests::panics".to_string()),
            ]
        );
        assert!(runnables[2].config.exact);
        assert!(!runnables[1].config.exact);
    }

    #[test]
    fn module_paths_come_from_the_file() {
        let text = "#[test]\nfn works() {}\n";
        let r = find_runnables(Path::new("/w/src/parse/lexer.rs"), text, &targets());
        assert_eq!(r[0].config.test_filter.as_deref(), Some("parse::lexer::works"));
        assert_eq!(r[0].config.target_kind, TargetKind::Lib);
        // `fn main` outside a binary root is not runnable.
        assert!(find_runnables(Path::new("/w/src/lib.rs"), "fn main() {}", &targets()).is_empty());
        let r = find_runnables(Path::new("/w/tests/api.rs"), text, &targets());
        assert_eq!(r[0].config.test_filter.as_deref(), Some("works"));
        assert_eq!(r[0].config.cargo_run_args(), ["test", "--package", "demo", "--test", "api", "--", "works", "--exact", "--nocapture"]);
    }

    #[test]
    fn cargo_arguments() {
        let targets = targets();
        let bin = RunConfig { args: vec!["--port".into(), "8080".into()], ..RunConfig::binary(&targets[2]) };
        assert_eq!(bin.cargo_run_args(), ["run", "--package", "demo", "--bin", "tool", "--", "--port", "8080"]);
        assert_eq!(
            bin.cargo_build_args(),
            ["build", "--package", "demo", "--bin", "tool", "--message-format=json-render-diagnostics"]
        );
        let lib_tests = RunConfig::tests(&targets[0], Some("a::b".into()), true);
        assert_eq!(
            lib_tests.cargo_build_args(),
            ["test", "--no-run", "--package", "demo", "--lib", "--message-format=json-render-diagnostics"]
        );
        assert_eq!(lib_tests.program_args(true), ["a::b", "--exact", "--nocapture", "--test-threads=1"]);
    }

    #[test]
    fn finds_artifacts() {
        let bin = RunConfig::binary(&targets()[1]);
        let line = r#"{"reason":"compiler-artifact","target":{"name":"demo","kind":["bin"]},"profile":{"test":false},"executable":"/w/target/debug/demo"}"#;
        assert_eq!(bin.artifact_in(line), Some(PathBuf::from("/w/target/debug/demo")));
        let lib = r#"{"reason":"compiler-artifact","target":{"name":"demo","kind":["lib"]},"profile":{"test":false},"executable":null}"#;
        assert_eq!(bin.artifact_in(lib), None);

        let tests = RunConfig::tests(&targets()[0], None, false);
        let test_bin = r#"{"reason":"compiler-artifact","target":{"name":"demo","kind":["lib"]},"profile":{"test":true},"executable":"/w/target/debug/deps/demo-abc"}"#;
        assert_eq!(tests.artifact_in(test_bin), Some(PathBuf::from("/w/target/debug/deps/demo-abc")));
        // The binary's test harness is not the library's.
        let bin_tests = r#"{"reason":"compiler-artifact","target":{"name":"demo","kind":["bin"]},"profile":{"test":true},"executable":"/w/target/debug/deps/demo-def"}"#;
        assert_eq!(tests.artifact_in(bin_tests), None);

        let integration = RunConfig::tests(&targets()[3], None, false);
        let api = r#"{"reason":"compiler-artifact","target":{"name":"api","kind":["test"]},"profile":{"test":true},"executable":"/w/target/debug/deps/api-1"}"#;
        assert_eq!(integration.artifact_in(api), Some(PathBuf::from("/w/target/debug/deps/api-1")));
    }
}

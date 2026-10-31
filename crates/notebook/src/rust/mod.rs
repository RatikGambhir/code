//! Rust notebooks. A cell is the body of a `main` in a throwaway crate that
//! depends on the function's library; see [`scratch`] for where it is built
//! and how its files are removed.

pub mod analysis;
pub mod cell;
pub mod format;
pub mod scratch;
pub mod signature;

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use anyhow::Result;
use cockpit_debugger::{
    cargo::{module_path, owning_target},
    TargetKind,
};
use parking_lot::Mutex;

use crate::{Kernel, Language, Notebook, OpenError, OpenRequest, RunEvent, RunHandle, RunOutcome, Snippet};
use scratch::{CargoWorkspace, Dependency, Scratch};

/// Rust: `.rs` files of a Cargo workspace's library crates.
pub struct Rust;

impl Language for Rust {
    fn name(&self) -> &'static str {
        "Rust"
    }

    fn handles(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "rs")
    }

    fn open(&self, request: OpenRequest<'_>) -> Result<Notebook, OpenError> {
        let at_cursor = signature::function_at(request.text, request.line);
        // What to call the code in messages: the function, else the file.
        let what = match &at_cursor {
            Some(sig) => format!("`{}`", sig.name),
            None => "This file".to_string(),
        };
        let targets = &request.project.cargo_targets;
        if targets.is_empty() {
            return Err(OpenError::Unsupported(
                "Rust notebooks need a Cargo project, and this folder doesn't have one (or `cargo metadata` failed).".into(),
            ));
        }
        // Cargo reports resolved paths; the editor may have opened the file
        // through a symlink (macOS's /var is /private/var).
        let resolved = request.path.canonicalize().unwrap_or_else(|_| request.path.to_path_buf());
        let (path, target) = [request.path, resolved.as_path()]
            .into_iter()
            .find_map(|path| owning_target(path, targets).map(|target| (path, target)))
            .ok_or_else(|| OpenError::Unsupported("This file isn't part of any crate in the workspace.".into()))?;
        match target.kind {
            TargetKind::Lib => {}
            TargetKind::Bin => {
                let is = if at_cursor.is_some() { "is in" } else { "belongs to" };
                return Err(OpenError::Unsupported(format!(
                    "{what} {is} the `{}` binary. Notebooks can only call code in library crates (src/lib.rs and its modules); move it into a library to try it here.",
                    target.name
                )));
            }
            TargetKind::Example | TargetKind::Test | TargetKind::Bench => {
                return Err(OpenError::Unsupported(format!(
                    "{what} is in an example, test or benchmark. Notebooks can only call code in library crates."
                )));
            }
        }
        let workspace = CargoWorkspace::load(&target.package_dir)?;
        let tokio = workspace.uses_tokio(&target.package);
        let crate_name = target.name.replace('-', "_");
        let file_modules = module_path(path, target);
        let functions = signature::functions(request.text)
            .iter()
            .filter(|f| is_listed(f))
            .map(|f| snippet(f, &crate_name, &file_modules, tokio))
            .collect::<Vec<_>>();
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let opened = match &at_cursor {
            Some(sig) => {
                let scaffold = cell::scaffold(sig, &crate_name, &file_modules, tokio);
                Opened {
                    title: sig.name.clone(),
                    subject: scaffold.subject,
                    signature: Some(sig.text.clone()),
                    line: sig.line,
                    cell: scaffold.cell,
                    notes: scaffold.notes,
                }
            }
            None => file_notebook(&crate_name, &file_modules, &file_name, request.line, !functions.is_empty()),
        };
        let dependency = Dependency {
            package: target.package.clone(),
            package_dir: target.package_dir.clone(),
            crate_name,
            tokio,
        };
        let scratch = Arc::new(Scratch::create(&workspace, &dependency)?);
        let analysis = analysis::RustAnalysis {
            scratch: scratch.clone(),
            target_dir: workspace.target_dir.clone(),
            tokio,
        };
        Ok(Notebook {
            title: opened.title,
            subject: opened.subject,
            signature: opened.signature,
            line: opened.line,
            cell_language: "rust",
            cell: opened.cell,
            notes: opened.notes,
            functions,
            analysis: Some(Arc::new(analysis)),
            kernel: Arc::new(RustKernel {
                scratch,
                run_dir: target.package_dir.clone(),
                tokio,
                gate: Arc::new(Mutex::new(())),
                current: Mutex::new(None),
            }),
        })
    }
}

/// The parts of a [`Notebook`] that depend on where it was opened.
struct Opened {
    title: String,
    subject: String,
    signature: Option<String>,
    line: usize,
    cell: String,
    notes: Vec<String>,
}

/// A notebook for a whole file: its first cell imports the file's module.
fn file_notebook(crate_name: &str, file_modules: &[String], file_name: &str, line: usize, has_functions: bool) -> Opened {
    let mut module = vec![crate_name.to_string()];
    module.extend(file_modules.iter().cloned());
    let subject = module.join("::");
    let mut cell = format!("use {subject}::*;\n\n");
    let mut notes = Vec::new();
    if has_functions {
        cell.push_str("// Pick one of this file's functions above to start a cell for it, or write code here.\n");
    } else {
        notes.push(format!(
            "{file_name} has no public functions a notebook can call, but cells can use anything public in `{crate_name}`."
        ));
    }
    Opened { title: file_name.to_string(), subject, signature: None, line, cell, notes }
}

/// Whether a function is offered as a starting cell: callable from outside
/// the crate (public, or a method of a trait impl) and with a type to call
/// it on if it is a method.
fn is_listed(sig: &signature::FnSignature) -> bool {
    let callable = sig.visibility == signature::Visibility::Public
        || sig.impl_block.as_ref().is_some_and(|b| b.trait_name.is_some());
    let has_owner = match &sig.impl_block {
        Some(block) => block.self_type.is_some(),
        None => sig.in_trait.is_none(),
    };
    callable && has_owner
}

fn snippet(sig: &signature::FnSignature, crate_name: &str, file_modules: &[String], tokio: bool) -> Snippet {
    let scaffold = cell::scaffold(sig, crate_name, file_modules, tokio);
    let mut label: Vec<String> = sig.modules.clone();
    if let Some(ty) = sig.impl_block.as_ref().and_then(|b| b.self_type.as_ref()) {
        label.push(ty.rsplit("::").next().unwrap_or(ty).to_string());
    }
    label.push(sig.name.clone());
    // Things to know go into the cell as comments, since the header's notes
    // are about the notebook as a whole.
    let mut code: String = scaffold.notes.iter().map(|note| format!("// {note}\n")).collect();
    code.push_str(&scaffold.cell);
    Snippet { label: label.join("::"), signature: sig.text.clone(), cell: code }
}

/// Builds and runs cells in a [`Scratch`] project.
pub struct RustKernel {
    scratch: Arc<Scratch>,
    /// Where the program runs: the package's directory, like `cargo test`.
    run_dir: PathBuf,
    tokio: bool,
    /// Held for the length of a run, so runs take turns.
    gate: Arc<Mutex<()>>,
    current: Mutex<Option<RunHandle>>,
}

impl Kernel for RustKernel {
    fn run(&self, code: &str) -> Result<(RunHandle, async_channel::Receiver<RunEvent>)> {
        let program = cell::program(code, self.tokio);
        let (tx, rx) = async_channel::unbounded();
        let handle = RunHandle::default();
        *self.current.lock() = Some(handle.clone());
        let scratch = self.scratch.clone();
        let gate = self.gate.clone();
        let run_dir = self.run_dir.clone();
        let run = handle.clone();
        std::thread::Builder::new().name("notebook-run".into()).spawn(move || {
            let _turn = gate.lock();
            let outcome = run_cell(&scratch, &program, &run_dir, &run, &tx);
            let _ = tx.send_blocking(RunEvent::Finished(outcome));
        })?;
        Ok((handle, rx))
    }

    fn format(&self, code: &str) -> Result<Option<String>> {
        format::format_cell(code, &self.run_dir).map(Some)
    }
}

impl Drop for RustKernel {
    fn drop(&mut self) {
        // The run thread holds the project until it ends; stop it so the
        // project's files are removed now.
        if let Some(run) = self.current.lock().take() {
            run.stop();
        }
    }
}

fn run_cell(
    scratch: &Scratch,
    program: &cell::Program,
    run_dir: &Path,
    run: &RunHandle,
    tx: &async_channel::Sender<RunEvent>,
) -> RunOutcome {
    if run.is_stopped() {
        return RunOutcome::Stopped;
    }
    if let Err(err) = scratch.prepare(&program.source) {
        return RunOutcome::Failed(format!("{err:#}"));
    }

    let executable = Arc::new(Mutex::new(None));
    let mapper = Mutex::new(DiagnosticMapper::new(program.lines));
    let lines = tx.clone();
    let found = executable.clone();
    let name = scratch.name().to_string();
    let build = run.run_process(&mut scratch.build_command(), move |line, stderr| {
        if stderr {
            let line = mapper.lock().map(&line, &name);
            let _ = lines.send_blocking(RunEvent::Build(line));
        } else if let Some(path) = artifact(&line, &name) {
            *found.lock() = Some(path);
        }
    });
    let build = match build {
        Ok(exit) if exit.stopped => return RunOutcome::Stopped,
        Ok(exit) => exit,
        Err(_) if run.is_stopped() => return RunOutcome::Stopped,
        Err(err) => return RunOutcome::Failed(format!("Could not run cargo: {err:#}")),
    };
    if !build.status.success() {
        return RunOutcome::BuildFailed;
    }
    let Some(executable) = executable.lock().take() else {
        return RunOutcome::Failed("Cargo built nothing to run.".into());
    };
    scratch.record_executable(&executable);

    let result_path = scratch.result_path();
    let _ = std::fs::remove_file(&result_path);
    let output = tx.clone();
    let mut command = Command::new(&executable);
    command.current_dir(run_dir).env(cell::RESULT_ENV, &result_path);
    let exit = run.run_process(&mut command, move |line, stderr| {
        let _ = output.send_blocking(RunEvent::Output { line, stderr });
    });
    let exit = match exit {
        Ok(exit) if exit.stopped => return RunOutcome::Stopped,
        Ok(exit) => exit,
        Err(_) if run.is_stopped() => return RunOutcome::Stopped,
        Err(err) => return RunOutcome::Failed(format!("Could not start the cell's program: {err:#}")),
    };
    if !exit.status.success() {
        return RunOutcome::Failed(describe_failure(exit.status));
    }
    let value = std::fs::read_to_string(&result_path).ok().and_then(|text| cell::parse_result(&text));
    let _ = std::fs::remove_file(&result_path);
    RunOutcome::Returned(value)
}

/// The project's executable in a line of cargo's JSON output.
fn artifact(line: &str, name: &str) -> Option<PathBuf> {
    let message: serde_json::Value = serde_json::from_str(line).ok()?;
    if message.get("reason")?.as_str()? != "compiler-artifact" || message.pointer("/target/name")?.as_str()? != name {
        return None;
    }
    message.get("executable")?.as_str().map(PathBuf::from)
}

fn describe_failure(status: std::process::ExitStatus) -> String {
    match status.code() {
        // What a panic exits with.
        Some(101) => "The cell panicked.".into(),
        // `main` returned an error (via `?`).
        Some(1) => "The cell returned an error.".into(),
        Some(code) => format!("The cell's program exited with status {code}."),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                if let Some(signal) = status.signal() {
                    return format!("The cell's program was killed by signal {signal}.");
                }
            }
            "The cell's program was killed.".into()
        }
    }
}

/// Rewrites compiler output so locations in the generated `main.rs` point
/// at lines of the cell instead (`--> cell:3:5`, and the snippet's line
/// numbers).
struct DiagnosticMapper {
    lines: cell::LineMap,
    /// The last `-->` was in the cell, so snippet line numbers are too.
    in_cell: bool,
}

impl DiagnosticMapper {
    fn new(lines: cell::LineMap) -> Self {
        Self { lines, in_cell: false }
    }

    fn map(&mut self, line: &str, package: &str) -> String {
        let trimmed = line.trim_start();
        if let Some(location) = trimmed.strip_prefix("--> ").or_else(|| trimmed.strip_prefix("::: ")) {
            self.in_cell = false;
            if let Some(rest) = location.strip_prefix("src/main.rs:") {
                let (number, after) = rest.split_once(':').unwrap_or((rest, ""));
                if let Some(mapped) = number.parse::<usize>().ok().and_then(|n| self.lines.cell_line(n)) {
                    self.in_cell = true;
                    let indent = &line[..line.len() - trimmed.len()];
                    let arrow = &trimmed[..4];
                    let after = if after.is_empty() { String::new() } else { format!(":{after}") };
                    return format!("{indent}{arrow}cell:{mapped}{after}");
                }
            }
            return line.to_string();
        }
        if trimmed.starts_with("error") || trimmed.starts_with("warning") {
            self.in_cell = false;
        }
        if self.in_cell {
            // `12 |     code`: the number right-aligned before the bar.
            if let Some(bar) = line.find(" |") {
                let number = line[..bar].trim();
                if let Some(mapped) = number.parse::<usize>().ok().and_then(|n| self.lines.cell_line(n)) {
                    let width = bar;
                    return format!("{mapped:>width$}{}", &line[bar..]);
                }
            }
        }
        // Cargo names the throwaway package while compiling it; say what it is.
        if trimmed.starts_with("Compiling ") && trimmed.contains(package) {
            let indent = &line[..line.len() - trimmed.len()];
            return format!("{indent}Compiling notebook cell");
        }
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_diagnostics_to_the_cell() {
        let mut mapper = DiagnosticMapper::new(cell::LineMap { cell_first_line: 7, inserted_line: None });
        let pkg = "hangar-notebook-1-0";
        let lines = [
            "error[E0425]: cannot find value `x` in this scope",
            "  --> src/main.rs:9:5",
            "   |",
            " 9 |     x + 1",
            "   |     ^ not found in this scope",
            "",
            "warning: unused variable",
            " --> crates/demo/src/lib.rs:9:5",
            "9 | fn f(a: u8) {}",
        ];
        let mapped: Vec<String> = lines.iter().map(|l| mapper.map(l, pkg)).collect();
        assert_eq!(mapped[1], "  --> cell:3:5");
        assert_eq!(mapped[3], " 3 |     x + 1");
        assert_eq!(mapped[7], " --> crates/demo/src/lib.rs:9:5");
        assert_eq!(mapped[8], "9 | fn f(a: u8) {}");
        assert!(mapper.map("   Compiling hangar-notebook-1-0 v0.0.0 (/t/hangar-notebook/hangar-notebook-1-0)", pkg).ends_with("Compiling notebook cell"));
    }

    #[test]
    fn finds_the_executable() {
        let line = r#"{"reason":"compiler-artifact","target":{"name":"hangar-notebook-1-0"},"executable":"/t/debug/hangar-notebook-1-0"}"#;
        assert_eq!(artifact(line, "hangar-notebook-1-0"), Some(PathBuf::from("/t/debug/hangar-notebook-1-0")));
        assert_eq!(artifact(line, "hangar-notebook-1-1"), None);
        assert_eq!(artifact("Compiling", "x"), None);
    }
}

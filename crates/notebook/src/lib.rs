//! Code notebooks for Hangar: pick a function in the workspace, write the
//! arguments in a cell, run it, and see what it printed and returned.
//!
//! The crate is split so more languages can be added:
//!
//! - A [`Language`] recognises its source files and prepares a [`Notebook`]
//!   from anywhere in one: for the function at the cursor, or for the file
//!   when the cursor is not in a function. Either way it offers a starting
//!   cell for each of the file's functions.
//! - The notebook carries a [`Kernel`], which runs a cell and reports
//!   [`RunEvent`]s. The UI only talks to these two traits.
//!
//! Rust is the first language ([`rust::Rust`]): each run builds a small
//! throwaway program that depends on the function's crate. Its build files
//! are deleted when the kernel is dropped (the notebook tab closes).

use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};

use anyhow::{Context as _, Result};
use cockpit_debugger::CargoTarget;
use parking_lot::Mutex;

pub mod rust;

/// What the workspace knows that a language may need.
#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    /// The workspace's Cargo targets; empty outside a Cargo project.
    pub cargo_targets: Vec<CargoTarget>,
}

/// Where a notebook was asked for: a file, its current text, and the cursor.
#[derive(Clone, Copy, Debug)]
pub struct OpenRequest<'a> {
    pub path: &'a Path,
    pub text: &'a str,
    /// Zero-based line of the cursor.
    pub line: usize,
    pub project: &'a Project,
}

/// A language notebooks can be opened for.
pub trait Language: Sync {
    /// A short name for messages ("Rust").
    fn name(&self) -> &'static str;
    /// Whether `path` is one of this language's source files.
    fn handles(&self, path: &Path) -> bool;
    /// Prepares a notebook for the function at the cursor, or for the whole
    /// file when the cursor is not in a function. This may run external
    /// tools (it can block), so call it off the UI thread.
    ///
    /// # Errors
    ///
    /// [`OpenError`] when the file's code cannot be called from a notebook
    /// (it is not part of a library, say).
    fn open(&self, request: OpenRequest<'_>) -> Result<Notebook, OpenError>;
}

/// The languages notebooks support, in the order they are asked.
static LANGUAGES: &[&dyn Language] = &[&rust::Rust];

/// The language for `path`, if notebooks support it.
pub fn language_for(path: &Path) -> Option<&'static dyn Language> {
    LANGUAGES.iter().copied().find(|language| language.handles(path))
}

/// Whether a notebook can be opened from `path` (for showing the menu item).
pub fn supports(path: &Path) -> bool {
    language_for(path).is_some()
}

/// Prepares a notebook for the function at the cursor. Blocks; see
/// [`Language::open`].
///
/// # Errors
///
/// [`OpenError::UnsupportedFile`] when no language handles the file, else the
/// language's error.
pub fn open(request: OpenRequest<'_>) -> Result<Notebook, OpenError> {
    let language = language_for(request.path).ok_or_else(|| OpenError::UnsupportedFile(request.path.to_path_buf()))?;
    language.open(request)
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("notebooks don't support {} files yet", display_extension(.0))]
    UnsupportedFile(PathBuf),
    /// The function was found but cannot be called; the message says why.
    #[error("{0}")]
    Unsupported(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

fn display_extension(path: &Path) -> String {
    path.extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_else(|| "these".to_string())
}

/// A notebook ready to show: its first cell and the kernel that runs cells.
pub struct Notebook {
    /// The tab title: the function's name, or the file's.
    pub title: String,
    /// What the notebook is for, for the header: the function's path
    /// (`demo::parse::parse_line`) or the file's module (`demo::parse`).
    pub subject: String,
    /// The function's signature as written; `None` for a file's notebook.
    pub signature: Option<String>,
    /// Zero-based line the header links back to: the function's, or the
    /// cursor's for a file.
    pub line: usize,
    /// The highlighting language of cells (`"rust"`).
    pub cell_language: &'static str,
    /// The first cell's code.
    pub cell: String,
    /// Things to know before running ("this function is private").
    pub notes: Vec<String>,
    /// The file's functions a cell can be started from, in file order.
    pub functions: Vec<Snippet>,
    pub kernel: Arc<dyn Kernel>,
    /// How a language server can see the cells, for completion, hover and
    /// errors while typing; `None` when the language has no such support.
    pub analysis: Option<Arc<dyn CellAnalysis>>,
}

/// How cells are shown to a language server. The server is started in
/// [`Self::root`]; each cell is a document with fixed text around it, so a
/// position in the cell is a position in the document shifted by the lines
/// of [`CellDocument::prefix`].
pub trait CellAnalysis: Send + Sync {
    /// The highlighter language whose server to start (`"rust"`).
    fn language(&self) -> &'static str;
    /// Where the server is started.
    fn root(&self) -> &Path;
    /// Settings laid over the server's usual ones (JSON, rooted at the
    /// server's own section).
    fn settings(&self) -> Option<serde_json::Value>;
    /// The document for cell `id` (ids are the notebook's own, never
    /// reused). Called when a cell is added.
    fn cell_document(&self, id: usize) -> CellDocument;
    /// A document tying the cells together, for the cells there are now
    /// (Rust: a crate root that declares a module per cell). `None` when
    /// the language needs none.
    fn root_document(&self, cells: &[usize]) -> Option<(PathBuf, String)>;
}

/// A cell as a language server sees it: `prefix + cell + suffix` in `path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellDocument {
    pub path: PathBuf,
    pub prefix: String,
    pub suffix: String,
}

/// A starting cell for one of the file's functions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    /// A short name (`first_word`, `Rect::area`).
    pub label: String,
    /// The signature as written, for a tooltip.
    pub signature: String,
    /// A cell that calls it, with `todo!()` for each argument.
    pub cell: String,
}

/// Runs cells for one notebook. Dropping the last reference cleans up
/// whatever the kernel left on disk.
pub trait Kernel: Send + Sync {
    /// Starts running `code`. Events arrive on the receiver, ending with
    /// [`RunEvent::Finished`]. A new run waits for the previous one to end.
    ///
    /// # Errors
    ///
    /// When the run could not be started at all.
    fn run(&self, code: &str) -> Result<(RunHandle, async_channel::Receiver<RunEvent>)>;

    /// The cell's code tidied up by the language's formatter. `Ok(None)`
    /// when the language has none. Blocks; call it off the UI thread.
    ///
    /// # Errors
    ///
    /// When the formatter fails, usually because the code doesn't parse.
    fn format(&self, code: &str) -> Result<Option<String>> {
        let _ = code;
        Ok(None)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEvent {
    /// A line from building the cell (compiler progress and diagnostics).
    Build(String),
    /// A line the program printed.
    Output { line: String, stderr: bool },
    Finished(RunOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The cell ran; its last expression's value, unless that was `()` or the
    /// cell ended with a statement.
    Returned(Option<Value>),
    /// The cell did not compile; the build lines say why.
    BuildFailed,
    /// The program failed (panicked, returned an error, exited non-zero).
    Failed(String),
    /// [`RunHandle::stop`] was called.
    Stopped,
}

/// A cell's value as the program described it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    /// The value's type (`demo::parse::Token`).
    pub type_name: String,
    /// The value pretty-printed, or `None` when the type cannot be printed
    /// (it does not implement `Debug`).
    pub text: Option<String>,
}

/// Stops a run.
#[derive(Clone, Default)]
pub struct RunHandle {
    shared: Arc<RunShared>,
}

#[derive(Default)]
struct RunShared {
    stopped: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl RunHandle {
    /// Kills whatever the run is doing (building or running) and makes it
    /// finish with [`RunOutcome::Stopped`].
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        if let Some(child) = self.shared.child.lock().as_mut() {
            let _ = child.kill();
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.shared.stopped.load(Ordering::SeqCst)
    }

    /// Spawns `command` with piped output as the run's current process, so
    /// [`Self::stop`] can kill it. Each output line goes to `on_line` with
    /// whether it came from stderr; the call returns when the process exits.
    ///
    /// # Errors
    ///
    /// When the process cannot be started, or the run was stopped first.
    pub fn run_process(
        &self,
        command: &mut Command,
        on_line: impl Fn(String, bool) + Send + Sync + 'static,
    ) -> Result<ProcessExit> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", command.get_program().to_string_lossy()))?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        {
            let mut slot = self.shared.child.lock();
            if self.is_stopped() {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("stopped");
            }
            *slot = Some(child);
        }
        let on_line = Arc::new(on_line);
        let out = read_lines(stdout, false, on_line.clone());
        let err = read_lines(stderr, true, on_line);
        let _ = out.join();
        let _ = err.join();
        let child = self.shared.child.lock().take();
        let status = match child {
            Some(mut child) => child.wait().context("waiting for the process")?,
            None => anyhow::bail!("stopped"),
        };
        Ok(ProcessExit { status, stopped: self.is_stopped() })
    }
}

/// How a process from [`RunHandle::run_process`] ended.
#[derive(Clone, Copy, Debug)]
pub struct ProcessExit {
    pub status: ExitStatus,
    /// It was killed by [`RunHandle::stop`].
    pub stopped: bool,
}

fn read_lines(
    stream: impl Read + Send + 'static,
    stderr: bool,
    on_line: Arc<impl Fn(String, bool) + Send + Sync + 'static>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            on_line(line, stderr);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_by_extension() {
        assert!(supports(Path::new("/w/src/lib.rs")));
        assert!(!supports(Path::new("/w/main.py")));
        let project = Project { root: PathBuf::from("/w"), cargo_targets: Vec::new() };
        let request = OpenRequest { path: Path::new("/w/main.py"), text: "", line: 0, project: &project };
        let err = open(request).err().expect("python is not supported");
        assert_eq!(err.to_string(), "notebooks don't support .py files yet");
    }

    #[test]
    fn processes_report_lines_and_can_be_stopped() {
        let handle = RunHandle::default();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        let exit = handle
            .run_process(Command::new("sh").args(["-c", "echo out; echo err >&2"]), move |line, stderr| {
                sink.lock().push((line, stderr));
            })
            .unwrap();
        assert!(exit.status.success() && !exit.stopped);
        let mut lines = lines.lock().clone();
        lines.sort();
        assert_eq!(lines, vec![("err".to_string(), true), ("out".to_string(), false)]);

        let stopper = handle.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            stopper.stop();
        });
        let exit = handle.run_process(Command::new("sleep").arg("30"), |_, _| {}).unwrap();
        thread.join().unwrap();
        assert!(exit.stopped && !exit.status.success());
        // Once stopped, nothing more starts.
        assert!(handle.run_process(&mut Command::new("true"), |_, _| {}).is_err());
    }
}

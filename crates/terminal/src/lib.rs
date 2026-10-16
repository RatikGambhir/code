//! Interactive terminal sessions.
//!
//! A [`Terminal`] owns a pseudo-terminal running the user's shell. A reader
//! thread feeds its output through a `vt100` parser; the UI takes a
//! [`Snapshot`] of the visible screen whenever it is told something changed.
//!
//! Wakeups are coalesced: after an [`TerminalEvent::Updated`] is sent, no
//! further one is sent until the UI takes a snapshot. A command that prints
//! megabytes therefore costs a handful of repaints, not one per read.

pub mod keys;
mod snapshot;
pub mod urls;

use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{Context as _, Result};
use cockpit_common::TerminalId;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};

pub use snapshot::{CellStyle, Line, Run, Snapshot, TermColor};

/// Lines of history kept above the visible screen.
const SCROLLBACK_LINES: usize = 10_000;

#[derive(Clone, Debug)]
pub struct TerminalOptions {
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
    /// Program and arguments to run; the user's login shell when `None`.
    pub program: Option<(String, Vec<String>)>,
    pub env: Vec<(String, String)>,
}

impl TerminalOptions {
    pub fn shell(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            rows: 24,
            cols: 80,
            program: None,
            env: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    /// The screen changed; take a new snapshot.
    Updated,
    /// The terminal rang its bell.
    Bell,
    /// The program exited, with its exit code when known.
    Exited(Option<u32>),
}

struct Callbacks {
    title: String,
}

impl vt100::Callbacks for Callbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}

struct Shared {
    parser: Mutex<vt100::Parser<Callbacks>>,
    exited: Mutex<Option<Option<u32>>>,
    /// Set when an `Updated` event is in flight; cleared by `snapshot()`.
    update_pending: AtomicBool,
}

pub struct Terminal {
    id: TerminalId,
    shared: Arc<Shared>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

impl Terminal {
    /// Starts the program in a new PTY. Events arrive on the returned channel
    /// until the program exits.
    pub fn spawn(
        options: TerminalOptions,
    ) -> Result<(Self, async_channel::Receiver<TerminalEvent>)> {
        let size = PtySize {
            rows: options.rows.max(1),
            cols: options.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .context("opening a pseudo-terminal")?;

        let mut command = match &options.program {
            Some((program, args)) => {
                let mut command = CommandBuilder::new(program);
                command.args(args);
                command
            }
            None => {
                let mut command = CommandBuilder::new_default_prog();
                // A login shell picks up the user's profile, as Terminal.app does.
                command.arg("-l");
                command
            }
        };
        command.cwd(&options.cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", cockpit_common::APP_NAME);
        for (key, value) in &options.env {
            command.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(command)
            .context("starting the shell")?;
        // The slave end belongs to the child now. Holding on to it would keep
        // the PTY open after the child exits, and the reader would never see EOF.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let killer = child.clone_killer();

        let shared = Arc::new(Shared {
            parser: Mutex::new(vt100::Parser::new_with_callbacks(
                size.rows,
                size.cols,
                SCROLLBACK_LINES,
                Callbacks {
                    title: String::new(),
                },
            )),
            exited: Mutex::new(None),
            update_pending: AtomicBool::new(false),
        });

        let (tx, rx) = async_channel::unbounded();
        spawn_reader(reader, child, shared.clone(), tx);

        Ok((
            Self {
                id: TerminalId::next(),
                shared,
                writer: Mutex::new(writer),
                master: Mutex::new(pair.master),
                killer: Mutex::new(killer),
            },
            rx,
        ))
    }

    pub fn id(&self) -> TerminalId {
        self.id
    }

    /// Sends raw bytes to the program.
    pub fn write(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock();
        if let Err(err) = writer.write_all(bytes).and_then(|_| writer.flush()) {
            tracing::debug!("terminal write failed: {err}");
        }
    }

    /// Pastes text, wrapped in bracketed-paste markers when the program asked
    /// for them (so a shell does not run a pasted newline straight away).
    pub fn paste(&self, text: &str) {
        let bracketed = self.shared.parser.lock().screen().bracketed_paste();
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        self.scroll_to_bottom();
        if bracketed {
            self.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.write(text.as_bytes());
        }
    }

    /// Encodes and sends a key press. Returns false when the key is not one
    /// the terminal consumes (Cmd shortcuts, say), so the UI can handle it.
    pub fn send_key(&self, key: &keys::Key<'_>) -> bool {
        let application_cursor = self.shared.parser.lock().screen().application_cursor();
        match keys::encode(key, application_cursor) {
            Some(bytes) => {
                self.scroll_to_bottom();
                self.write(&bytes);
                true
            }
            None => false,
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        {
            let mut parser = self.shared.parser.lock();
            if parser.screen().size() == (rows, cols) {
                return;
            }
            parser.screen_mut().set_size(rows, cols);
        }
        let _ = self.master.lock().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    pub fn size(&self) -> (u16, u16) {
        self.shared.parser.lock().screen().size()
    }

    /// Scrolls the view into history by `lines` (positive scrolls up).
    pub fn scroll(&self, lines: i32) {
        let mut parser = self.shared.parser.lock();
        let current = parser.screen().scrollback() as i64;
        let target = (current + lines as i64).max(0) as usize;
        parser.screen_mut().set_scrollback(target);
    }

    pub fn scroll_to_bottom(&self) {
        self.shared.parser.lock().screen_mut().set_scrollback(0);
    }

    /// A copy of the visible screen, ready to render. Also re-arms the
    /// `Updated` notification.
    pub fn snapshot(&self) -> Snapshot {
        self.shared.update_pending.store(false, Ordering::Release);
        let parser = self.shared.parser.lock();
        Snapshot::capture(&parser, *self.shared.exited.lock())
    }

    /// The visible screen as plain text.
    pub fn contents(&self) -> String {
        self.shared.parser.lock().screen().contents()
    }

    pub fn title(&self) -> String {
        self.shared.parser.lock().callbacks().title.clone()
    }

    /// `Some(code)` once the program has exited.
    pub fn exit_status(&self) -> Option<Option<u32>> {
        *self.shared.exited.lock()
    }

    pub fn kill(&self) {
        let _ = self.killer.lock().kill();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Closing a terminal must not leave its shell (or a dev server started
        // from it) running in the background.
        if self.exit_status().is_none() {
            self.kill();
        }
    }
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
    shared: Arc<Shared>,
    events: async_channel::Sender<TerminalEvent>,
) {
    let notify = {
        let shared = shared.clone();
        let events = events.clone();
        move || {
            if !shared.update_pending.swap(true, Ordering::AcqRel) {
                let _ = events.try_send(TerminalEvent::Updated);
            }
        }
    };

    std::thread::Builder::new()
        .name("cockpit-pty-reader".into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        shared.parser.lock().process(chunk);
                        if chunk.contains(&0x07) && !is_inside_osc(chunk) {
                            let _ = events.try_send(TerminalEvent::Bell);
                        }
                        notify();
                    }
                }
            }
            // EOF: the program has gone (or closed its terminal).
            let code = child.wait().ok().map(|status| status.exit_code());
            *shared.exited.lock() = Some(code);
            shared.update_pending.store(false, Ordering::Release);
            notify();
            let _ = events.try_send(TerminalEvent::Exited(code));
        })
        .expect("spawning the PTY reader thread");
}

/// BEL also terminates OSC sequences (window titles). Only treat it as a bell
/// when the chunk carries no OSC introducer.
fn is_inside_osc(chunk: &[u8]) -> bool {
    chunk.windows(2).any(|w| w == b"\x1b]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn sh(script: &str) -> TerminalOptions {
        TerminalOptions {
            cwd: std::env::temp_dir(),
            rows: 10,
            cols: 40,
            program: Some(("/bin/sh".into(), vec!["-c".into(), script.into()])),
            env: vec![],
        }
    }

    #[test]
    fn output_reaches_the_screen_and_exit_code_is_reported() {
        let (terminal, events) = Terminal::spawn(sh("printf 'hello from pty'; exit 3")).unwrap();
        let mut exit = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && exit.is_none() {
            match events.try_recv() {
                Ok(TerminalEvent::Exited(code)) => exit = Some(code),
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        assert_eq!(exit, Some(Some(3)));
        assert!(terminal.contents().contains("hello from pty"));
        assert_eq!(terminal.snapshot().exited, Some(Some(3)));
    }

    #[test]
    fn updates_are_coalesced_until_a_snapshot_is_taken() {
        let (terminal, events) =
            Terminal::spawn(sh("for i in 1 2 3 4 5 6 7 8 9; do echo line$i; done; sleep 5")).unwrap();
        assert!(wait_until(|| terminal.contents().contains("line9")));
        let pending: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert_eq!(
            pending.iter().filter(|e| **e == TerminalEvent::Updated).count(),
            1,
            "{pending:?}"
        );
        terminal.snapshot();
        terminal.write(b"x");
        terminal.kill();
    }

    #[test]
    fn input_is_echoed_back() {
        let (terminal, _events) = Terminal::spawn(TerminalOptions {
            program: Some(("/bin/cat".into(), vec![])),
            ..sh("")
        })
        .unwrap();
        terminal.write(b"typed text\r");
        assert!(wait_until(|| terminal.contents().contains("typed text")));
    }

    #[test]
    fn colors_and_styles_are_captured() {
        let (terminal, _events) =
            Terminal::spawn(sh("printf '\\033[1;31mred\\033[0m plain'; sleep 5")).unwrap();
        assert!(wait_until(|| terminal.contents().contains("plain")));
        let snapshot = terminal.snapshot();
        let red = snapshot.lines[0]
            .runs
            .iter()
            .find(|run| run.text.contains("red"))
            .expect("red run");
        assert!(red.style.bold);
        assert_eq!(red.style.fg, TermColor::Indexed(1));
    }

    #[test]
    fn resize_changes_the_screen_size() {
        let (terminal, _events) = Terminal::spawn(sh("sleep 5")).unwrap();
        terminal.resize(30, 100);
        assert_eq!(terminal.size(), (30, 100));
        assert_eq!(terminal.snapshot().lines.len(), 30);
    }

    #[test]
    fn cwd_is_honoured() {
        let dir = tempfile::tempdir().unwrap();
        let (terminal, _events) = Terminal::spawn(TerminalOptions {
            cwd: dir.path().to_path_buf(),
            ..sh("pwd; sleep 5")
        })
        .unwrap();
        let name = dir.path().file_name().unwrap().to_string_lossy().to_string();
        assert!(wait_until(|| terminal.contents().contains(&name)));
    }

    #[test]
    fn dropping_kills_the_program() {
        let marker = tempfile::tempdir().unwrap();
        let path = marker.path().join("alive");
        let (terminal, events) = Terminal::spawn(sh(&format!(
            "sleep 2; touch {}",
            path.display()
        )))
        .unwrap();
        drop(terminal);
        // The reader reports the exit once the child is gone.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while Instant::now() < deadline && !exited {
            exited = matches!(events.try_recv(), Ok(TerminalEvent::Exited(_)));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(exited);
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!path.exists(), "the killed program kept running");
    }
}

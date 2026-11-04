//! Claude Code's sign-in, run in the background instead of a terminal.
//!
//! `claude auth login` opens the browser itself and finishes on its own
//! once the user approves. It also prints a fallback link whose page shows a
//! code, and reads that code from stdin ("Paste code here if prompted").
//! Claude Code keeps the credentials (in the Keychain on macOS); Hangar only
//! drives the process.

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;

use super::ClaudeLaunch;

/// What the sign-in reports while it runs.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginEvent {
    /// The sign-in page's fallback link (its page shows a code to paste).
    Url(String),
    /// The process ended: signed in, or why not.
    Finished(Result<(), String>),
}

/// A running `claude auth login`. Dropping it stops the sign-in.
pub struct ClaudeLogin {
    child: Arc<Mutex<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
}

impl ClaudeLogin {
    /// Starts the sign-in: a Claude plan, or with `console` an Anthropic
    /// Console account billed by API use.
    pub fn start(launch: &ClaudeLaunch, console: bool) -> Result<(Self, async_channel::Receiver<LoginEvent>)> {
        let mut child = Command::new(&launch.program)
            .args(["auth", "login", if console { "--console" } else { "--claudeai" }])
            .envs(launch.env.iter().cloned())
            .env_remove("CLAUDECODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting `claude auth login`")?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        let child = Arc::new(Mutex::new(child));
        let (tx, rx) = async_channel::unbounded();
        // The last lines of output, for the error when the sign-in fails.
        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));

        let err_tail = tail.clone();
        let err_reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                remember(&err_tail, line);
            }
        });
        let waiter_child = child.clone();
        std::thread::Builder::new().name("claude-login".into()).spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(url) = sign_in_url(&line) {
                    let _ = tx.send_blocking(LoginEvent::Url(url));
                }
                remember(&tail, line);
            }
            let _ = err_reader.join();
            let status = loop {
                match waiter_child.lock().try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) => {}
                    Err(err) => break Err(err),
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            let result = match status {
                Ok(status) if status.success() => Ok(()),
                Ok(_) => Err(failure_message(&tail.lock())),
                Err(err) => Err(format!("waiting for `claude auth login`: {err}")),
            };
            let _ = tx.send_blocking(LoginEvent::Finished(result));
        })?;
        Ok((
            Self {
                child,
                stdin: Mutex::new(stdin),
            },
            rx,
        ))
    }

    /// Sends the code the sign-in page showed.
    pub fn submit_code(&self, code: &str) -> Result<()> {
        let mut stdin = self.stdin.lock();
        let stdin = stdin.as_mut().ok_or_else(|| anyhow!("the sign-in has ended"))?;
        writeln!(stdin, "{}", code.trim())?;
        stdin.flush()?;
        Ok(())
    }

    pub fn cancel(&self) {
        let _ = self.child.lock().kill();
    }
}

impl Drop for ClaudeLogin {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn remember(tail: &Mutex<VecDeque<String>>, line: String) {
    let line = line.trim().to_string();
    if line.is_empty() {
        return;
    }
    let mut tail = tail.lock();
    if tail.len() == 5 {
        tail.pop_front();
    }
    tail.push_back(line);
}

/// "If the browser didn't open, visit: https://…" → the link.
fn sign_in_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let url = line[start..].split_whitespace().next()?;
    Some(url.to_string())
}

fn failure_message(tail: &VecDeque<String>) -> String {
    tail.iter()
        .rev()
        .find(|line| !line.starts_with("Paste code") && !line.starts_with("Opening browser"))
        .cloned()
        .unwrap_or_else(|| "`claude auth login` stopped without signing in".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_fallback_link() {
        assert_eq!(
            sign_in_url("If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&x=1"),
            Some("https://claude.com/cai/oauth/authorize?code=true&x=1".into())
        );
        assert_eq!(sign_in_url("Opening browser to sign in…"), None);
    }

    #[test]
    fn failures_name_the_last_real_line() {
        let tail: VecDeque<String> = ["Opening browser to sign in…", "Login failed: invalid code", "Paste code here if prompted >"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(failure_message(&tail), "Login failed: invalid code");
        assert!(failure_message(&VecDeque::new()).contains("stopped"));
    }

    /// A stand-in `claude` that prints the link, reads a code and exits 0
    /// only for the right one.
    #[cfg(unix)]
    #[test]
    fn drives_a_fake_claude() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("claude");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'Opening browser to sign in…'\necho \"If the browser didn't open, visit: https://example.com/auth?mode=$3\"\nprintf 'Paste code here if prompted > '\nread code\n[ \"$code\" = good ] && exit 0\necho 'Login failed: invalid code' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let launch = ClaudeLaunch::new(&script);

        for (code, ok) in [("good", true), ("bad", false)] {
            let (login, events) = ClaudeLogin::start(&launch, false).unwrap();
            let url = events.recv_blocking().unwrap();
            assert_eq!(url, LoginEvent::Url("https://example.com/auth?mode=--claudeai".into()));
            login.submit_code(&format!(" {code}\n")).unwrap();
            match events.recv_blocking().unwrap() {
                LoginEvent::Finished(Ok(())) => assert!(ok),
                LoginEvent::Finished(Err(err)) => {
                    assert!(!ok);
                    assert_eq!(err, "Login failed: invalid code");
                }
                other => panic!("unexpected {other:?}"),
            }
        }

        let (login, events) = ClaudeLogin::start(&launch, true).unwrap();
        assert!(matches!(events.recv_blocking().unwrap(), LoginEvent::Url(u) if u.ends_with("--console")));
        login.cancel();
        assert!(matches!(events.recv_blocking().unwrap(), LoginEvent::Finished(Err(_))));
    }
}

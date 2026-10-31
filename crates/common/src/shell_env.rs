//! Recovering the user's shell environment.
//!
//! An app started from Finder or the Dock inherits launchd's minimal `PATH`
//! (`/usr/bin:/bin:/usr/sbin:/sbin`), not the one the user's shell builds. That
//! hides Homebrew, `~/.cargo/bin`, nvm and friends — which is where `codex`,
//! `rust-analyzer` and most dev servers live. So once at startup, ask the login
//! shell what `PATH` it would use and adopt it.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// Marker that brackets the value in the shell's output, so anything rc files
/// print (banners, `nvm` chatter) is ignored.
const MARKER: &str = "__HANGAR_PATH__";

/// Runs the user's login shell and returns the `PATH` it reports, if any.
pub fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!("printf '{MARKER}%s{MARKER}' \"$PATH\"");
    let mut child = Command::new(&shell)
        .args(["-l", "-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // A broken rc file must not hang startup.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    extract_marked(&String::from_utf8_lossy(&output.stdout))
}

fn extract_marked(output: &str) -> Option<String> {
    let start = output.find(MARKER)? + MARKER.len();
    let end = start + output[start..].find(MARKER)?;
    let value = &output[start..end];
    (!value.is_empty()).then(|| value.to_string())
}

/// Merges `shell_path` in front of the current `PATH`, keeping order and
/// dropping duplicates.
pub fn merge_paths(shell_path: &str, current: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    shell_path
        .split(':')
        .chain(current.split(':'))
        .filter(|entry| !entry.is_empty() && seen.insert(entry.to_string()))
        .collect::<Vec<_>>()
        .join(":")
}

/// Adopts the login shell's `PATH` for this process. Call it at the very top
/// of `main`, before any thread is spawned (`set_var` is not thread safe).
pub fn adopt_login_shell_path() {
    if std::env::var_os("HANGAR_SKIP_SHELL_ENV").is_some() {
        return;
    }
    let Some(shell_path) = login_shell_path() else {
        tracing::warn!("could not read PATH from the login shell");
        return;
    };
    let current = std::env::var("PATH").unwrap_or_default();
    let merged = merge_paths(&shell_path, &current);
    // SAFETY: called from `main` before any other thread exists.
    unsafe { std::env::set_var("PATH", merged) };
}

/// Finds an executable on `PATH`, then in `extra_dirs`.
pub fn find_executable(name: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(extra_dirs.iter().cloned())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
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

/// Places developer tools commonly end up on macOS, for when even the login
/// shell does not know about them.
pub fn common_tool_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".npm-global/bin"));
        dirs.push(home.join(".bun/bin"));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_extraction_ignores_rc_file_noise() {
        let output = format!("Welcome!\n{MARKER}/opt/homebrew/bin:/usr/bin{MARKER}\n");
        assert_eq!(
            extract_marked(&output).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(extract_marked("no marker here"), None);
        assert_eq!(extract_marked(&format!("{MARKER}{MARKER}")), None);
    }

    #[test]
    fn merging_keeps_shell_order_and_drops_duplicates() {
        let merged = merge_paths("/opt/homebrew/bin:/usr/bin", "/usr/bin:/bin:");
        assert_eq!(merged, "/opt/homebrew/bin:/usr/bin:/bin");
    }

    #[test]
    fn finds_executables_in_extra_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("my-tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            find_executable("my-tool", &[dir.path().to_path_buf()]),
            Some(tool)
        );
        assert_eq!(find_executable("definitely-not-a-tool-xyz", &[]), None);
    }
}

//! Finding a debug adapter and preparing LLDB for Rust.
//!
//! Hangar debugs with `lldb-dap` (called `lldb-vscode` in older LLVMs),
//! which ships with Xcode and its Command Line Tools. Rust support comes from
//! the toolchain: `rustc --print sysroot` holds the LLDB formatters that make
//! `String`, `Vec`, `HashMap`, `Option` and friends readable (what
//! `rust-lldb` loads), and with the `rust-src` component the standard
//! library's sources, which LLDB is pointed at so stepping into std works.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Result};
use cockpit_common::shell_env::{common_tool_dirs, find_executable};

use crate::client::AdapterCommand;

/// Finds `lldb-dap`: `$HANGAR_LLDB_DAP`, then Xcode's (`xcrun -f`), then
/// `PATH` and Homebrew's LLVM.
pub fn find_lldb_dap() -> Result<AdapterCommand> {
    let command = |program: PathBuf| AdapterCommand {
        name: "lldb-dap".into(),
        program: program.to_string_lossy().into_owned(),
        args: Vec::new(),
    };
    if let Some(path) = std::env::var_os("HANGAR_LLDB_DAP").filter(|p| !p.is_empty()) {
        return Ok(command(PathBuf::from(path)));
    }
    if cfg!(target_os = "macos") {
        for tool in ["lldb-dap", "lldb-vscode"] {
            if let Ok(output) = Command::new("xcrun").args(["-f", tool]).output() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if output.status.success() && !path.is_empty() && Path::new(&path).exists() {
                    return Ok(command(PathBuf::from(path)));
                }
            }
        }
    }
    let mut dirs = common_tool_dirs();
    dirs.push(PathBuf::from("/opt/homebrew/opt/llvm/bin"));
    dirs.push(PathBuf::from("/usr/local/opt/llvm/bin"));
    for tool in ["lldb-dap", "lldb-vscode"] {
        if let Some(path) = find_executable(tool, &dirs) {
            return Ok(command(path));
        }
    }
    if cfg!(target_os = "macos") {
        bail!("lldb-dap was not found. Install the Xcode Command Line Tools (xcode-select --install) or set HANGAR_LLDB_DAP.")
    }
    bail!("lldb-dap was not found. Install LLDB (it includes lldb-dap) or set HANGAR_LLDB_DAP.")
}

/// What LLDB needs to show Rust values and find the standard library's
/// sources, for the toolchain `cwd` uses (`rust-toolchain.toml` applies).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RustToolchain {
    pub sysroot: Option<PathBuf>,
    pub commit_hash: Option<String>,
}

impl RustToolchain {
    pub fn detect(cwd: &Path) -> Self {
        let run = |args: &[&str]| {
            Command::new("rustc")
                .args(args)
                .current_dir(cwd)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let sysroot = run(&["--print", "sysroot"]).map(|s| PathBuf::from(s.trim()));
        let commit_hash = run(&["-vV"]).and_then(|v| parse_commit_hash(&v));
        Self { sysroot, commit_hash }
    }

    /// LLDB commands to run before the target is created.
    pub fn init_commands(&self) -> Vec<String> {
        let mut commands = Vec::new();
        let Some(sysroot) = &self.sysroot else { return commands };
        let etc = sysroot.join("lib/rustlib/etc");
        let lookup = etc.join("lldb_lookup.py");
        if lookup.exists() {
            commands.push(format!("command script import {}", quote(&lookup)));
        }
        // Older toolchains register the formatters from a separate file.
        let older = etc.join("lldb_commands");
        if older.exists() {
            commands.push(format!("command source -s 0 {}", quote(&older)));
        }
        let rust_src = sysroot.join("lib/rustlib/src/rust");
        if let (Some(hash), true) = (&self.commit_hash, rust_src.exists()) {
            commands.push(format!(
                "settings append target.source-map /rustc/{hash} {}",
                quote(&rust_src)
            ));
        }
        commands
    }
}

fn parse_commit_hash(verbose_version: &str) -> Option<String> {
    verbose_version
        .lines()
        .find_map(|line| line.strip_prefix("commit-hash: "))
        .map(str::trim)
        .filter(|hash| !hash.is_empty() && *hash != "unknown")
        .map(str::to_string)
}

fn quote(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_commit_hash() {
        let text = "rustc 1.99.1 (58cf8f9c3 2026-10-13)\nbinary: rustc\ncommit-hash: 58cf8f9c35b3\nhost: aarch64-apple-darwin\n";
        assert_eq!(parse_commit_hash(text).as_deref(), Some("58cf8f9c35b3"));
        assert_eq!(parse_commit_hash("commit-hash: unknown\n"), None);
    }

    #[test]
    fn init_commands_load_what_exists() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("lib/rustlib/etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("lldb_lookup.py"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("lib/rustlib/src/rust")).unwrap();
        let toolchain = RustToolchain { sysroot: Some(dir.path().into()), commit_hash: Some("abc".into()) };
        let commands = toolchain.init_commands();
        assert_eq!(commands.len(), 2);
        assert!(commands[0].starts_with("command script import \""));
        assert!(commands[1].starts_with("settings append target.source-map /rustc/abc \""));
        assert!(RustToolchain::default().init_commands().is_empty());
    }
}

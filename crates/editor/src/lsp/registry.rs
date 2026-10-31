//! Which language server handles which language.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Sent as `initializationOptions` and as the answer to
    /// `workspace/configuration` for the server's section.
    pub settings: Option<Value>,
}

/// Well-known servers, by highlighter language. The first one installed wins.
fn candidates(language: &str) -> &'static [(&'static str, &'static [&'static str])] {
    match language {
        "rust" => &[("rust-analyzer", &[])],
        "typescript" | "tsx" | "javascript" => &[("typescript-language-server", &["--stdio"])],
        "python" => &[
            ("basedpyright-langserver", &["--stdio"]),
            ("pyright-langserver", &["--stdio"]),
            ("pylsp", &[]),
        ],
        "go" => &[("gopls", &[])],
        "c" | "cpp" => &[("clangd", &[])],
        "swift" => &[("sourcekit-lsp", &[])],
        "zig" => &[("zls", &[])],
        "ruby" => &[("ruby-lsp", &[])],
        "css" => &[("vscode-css-language-server", &["--stdio"])],
        "html" => &[("vscode-html-language-server", &["--stdio"])],
        "json" => &[("vscode-json-language-server", &["--stdio"])],
        "toml" => &[("taplo", &["lsp", "stdio"])],
        _ => &[],
    }
}

/// The installed server for `language`, if any. `search_dirs` are checked in
/// addition to `PATH` (e.g. `~/.cargo/bin`, which GUI apps often lack).
pub fn server_for_language(language: &str, search_dirs: &[PathBuf]) -> Option<ServerConfig> {
    candidates(language).iter().find_map(|(command, args)| {
        let path = find_on_path(command, search_dirs)?;
        Some(ServerConfig {
            name: (*command).to_string(),
            command: path.to_string_lossy().into_owned(),
            args: args.iter().map(|a| a.to_string()).collect(),
            settings: settings_for(command),
        })
    })
}

/// Like [`server_for_language`], but also checks that the server actually
/// runs. `~/.cargo/bin/rust-analyzer` is a rustup proxy that exists even when
/// the component is not installed, and then exits at once with an error.
///
/// `Ok(None)` means no server is installed for the language; an error means
/// one was found but cannot start, with a hint on how to fix it.
pub fn resolve_server(language: &str, search_dirs: &[PathBuf]) -> Result<Option<ServerConfig>, String> {
    let Some(config) = server_for_language(language, search_dirs) else {
        return match install_hint(language) {
            Some(hint) => Err(format!("not installed; {hint}")),
            None => Ok(None),
        };
    };
    verify(language, config).map(Some)
}

fn verify(language: &str, config: ServerConfig) -> Result<ServerConfig, String> {
    if config.name == "rust-analyzer" && !runs(Path::new(&config.command), &["--version"]) {
        let hint = install_hint(language).unwrap_or_default();
        return Err(format!("{} does not run; {hint}", config.command));
    }
    Ok(config)
}

/// How to install the server for languages people expect one for.
pub fn install_hint(language: &str) -> Option<&'static str> {
    match language {
        "rust" => Some("run `rustup component add rust-analyzer`"),
        _ => None,
    }
}

fn runs(command: &Path, args: &[&str]) -> bool {
    Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Per-server settings. rust-analyzer's defaults are good; these only turn on
/// what an IDE wants (build scripts and proc macros so macro-heavy crates
/// resolve, `cargo check` diagnostics on save) and skip completions that need
/// snippet support the editor does not have.
fn settings_for(server: &str) -> Option<Value> {
    match server {
        "rust-analyzer" => Some(json!({
            "cargo": { "buildScripts": { "enable": true }, "allTargets": true },
            "procMacro": { "enable": true },
            "checkOnSave": true,
            "check": { "command": "check" },
            "completion": {
                "callable": { "snippets": "none" },
                "postfix": { "enable": false },
                "fullFunctionSignatures": { "enable": false }
            },
            "diagnostics": { "enable": true },
            "lens": { "enable": false },
            "hover": { "actions": { "enable": false } }
        })),
        _ => None,
    }
}

/// The value of a dotted `section` (`"rust-analyzer"`, `"rust-analyzer.check"`)
/// inside a server's settings, which are rooted at the server's own section.
pub fn settings_section(server: &str, settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(settings) = settings else { return Value::Null };
    let Some(section) = section else { return settings.clone() };
    let mut parts = section.split('.');
    if parts.next() != Some(server) {
        return Value::Null;
    }
    parts
        .try_fold(settings, |value, key| value.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

fn find_on_path(name: &str, extra: &[PathBuf]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(extra.iter().cloned())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Servers for the same language share a key, so one process serves every
/// TypeScript/JavaScript file in a workspace.
pub fn server_key(language: &str) -> &str {
    match language {
        "typescript" | "tsx" | "javascript" => "typescript",
        "c" | "cpp" => "c",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_servers_in_extra_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rust-analyzer"), "").unwrap();
        let config = server_for_language("rust", &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(config.name, "rust-analyzer");
        assert!(config.command.ends_with("rust-analyzer"));
        assert!(server_for_language("text", &[]).is_none());
        assert_eq!(server_key("tsx"), "typescript");
        assert!(config.settings.is_some());
    }

    #[test]
    fn a_broken_rust_analyzer_is_reported_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        // An empty, non-executable file: like rustup's proxy without the
        // component, it is on PATH but cannot run.
        let fake = dir.path().join("rust-analyzer");
        std::fs::write(&fake, "").unwrap();
        let config = ServerConfig {
            name: "rust-analyzer".into(),
            command: fake.to_string_lossy().into_owned(),
            args: Vec::new(),
            settings: None,
        };
        let err = verify("rust", config).unwrap_err();
        assert!(err.contains("rustup component add rust-analyzer"), "{err}");
        assert_eq!(resolve_server("text", &[]), Ok(None));
    }

    #[test]
    fn settings_sections_resolve() {
        let settings = settings_for("rust-analyzer");
        let check = settings_section("rust-analyzer", settings.as_ref(), Some("rust-analyzer.check"));
        assert_eq!(check["command"], "check");
        assert_eq!(settings_section("rust-analyzer", settings.as_ref(), None), settings.clone().unwrap());
        assert!(settings_section("rust-analyzer", settings.as_ref(), Some("files")).is_null());
        assert!(settings_section("gopls", None, Some("gopls")).is_null());
    }
}

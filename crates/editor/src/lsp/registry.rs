//! Which language server handles which language.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
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
        })
    })
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
    }
}

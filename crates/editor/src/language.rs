//! File-to-language mapping.
//!
//! The names returned by [`language_for_path`] are the ones gpui-component's
//! tree-sitter highlighter registers; anything unknown is plain `text`.

use std::path::Path;

pub fn language_for_path(path: &Path) -> &'static str {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match file_name.as_str() {
        "makefile" | "gnumakefile" => return "make",
        "cmakelists.txt" => return "cmake",
        "cargo.lock" => return "toml",
        ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".zprofile" => return "bash",
        _ => {}
    }
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "rs" => "rust",
        "toml" => "toml",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "json" | "jsonc" | "json5" => "json",
        "md" | "markdown" | "mdx" => "markdown",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "mm" => "cpp",
        "cs" => "csharp",
        "java" => "java",
        "rb" => "ruby",
        "sh" | "bash" | "zsh" => "bash",
        "html" | "htm" => "html",
        "css" | "scss" | "less" => "css",
        "yaml" | "yml" => "yaml",
        "sql" => "sql",
        "swift" => "swift",
        "zig" => "zig",
        "scala" | "sc" => "scala",
        "ex" | "exs" => "elixir",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "cmake" => "cmake",
        "mk" => "make",
        "diff" | "patch" => "diff",
        "erb" => "erb",
        "ejs" => "ejs",
        _ => "text",
    }
}

/// The LSP `languageId` for a highlighter language, where they differ.
pub fn lsp_language_id(language: &str) -> &str {
    match language {
        "tsx" => "typescriptreact",
        "bash" => "shellscript",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_files() {
        assert_eq!(language_for_path(Path::new("src/main.rs")), "rust");
        assert_eq!(language_for_path(Path::new("App.TSX")), "tsx");
        assert_eq!(language_for_path(Path::new("Makefile")), "make");
        assert_eq!(language_for_path(Path::new("Cargo.lock")), "toml");
        assert_eq!(language_for_path(Path::new("notes")), "text");
        assert_eq!(lsp_language_id("tsx"), "typescriptreact");
        assert_eq!(lsp_language_id("rust"), "rust");
    }
}

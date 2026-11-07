//! File-to-language mapping.
//!
//! Two names per file: [`language_for_path`] is the tree-sitter highlighter
//! gpui-component registers (anything unknown is plain `text`), and
//! [`lsp_language_id_for_path`] is the `languageId` language servers expect.
//! They differ where there is no grammar (`go.mod`, `.vue`) or the protocol
//! splits what one grammar highlights (`.jsx` is `javascriptreact`).

use std::path::Path;

pub fn language_for_path(path: &Path) -> &'static str {
    let file_name = lowercase_file_name(path);
    match file_name.as_str() {
        "makefile" | "gnumakefile" => return "make",
        "cmakelists.txt" => return "cmake",
        "cargo.lock" => return "toml",
        "bun.lock" | "deno.lock" => return "json",
        ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".zprofile" => return "bash",
        _ => {}
    }
    match lowercase_extension(path).as_str() {
        "rs" => "rust",
        "toml" => "toml",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "json" | "jsonc" | "json5" => "json",
        "md" | "markdown" | "mdx" => "markdown",
        "py" | "pyi" => "python",
        "go" | "templ" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "mm" => "cpp",
        "cs" => "csharp",
        "java" => "java",
        "rb" => "ruby",
        "sh" | "bash" | "zsh" => "bash",
        // Single-file components are HTML with script and style blocks.
        "html" | "htm" | "vue" | "svelte" | "astro" => "html",
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

/// The LSP `languageId` for a file, or `None` when no language server
/// would know what to do with it.
pub fn lsp_language_id_for_path(path: &Path) -> Option<&'static str> {
    let file_name = lowercase_file_name(path);
    let by_name = match file_name.as_str() {
        "go.mod" => Some("go.mod"),
        "go.sum" => Some("go.sum"),
        "go.work" => Some("go.work"),
        // TypeScript's own config files allow comments.
        "tsconfig.json" | "jsconfig.json" | "deno.json" | "deno.jsonc" | "bun.lock" => Some("jsonc"),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    if file_name.starts_with("tsconfig.") && file_name.ends_with(".json") {
        return Some("jsonc");
    }
    let id = match lowercase_extension(path).as_str() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "go" => "go",
        "templ" => "templ",
        "gotmpl" | "gohtml" => "gotmpl",
        "jsonc" | "json5" => "jsonc",
        "md" | "markdown" | "mdx" => return None,
        _ => match language_for_path(path) {
            "text" | "diff" | "markdown" | "make" | "cmake" | "erb" | "ejs" => return None,
            other => lsp_language_id(other),
        },
    };
    Some(id)
}

/// The LSP `languageId` for a highlighter language, where they differ.
pub fn lsp_language_id(language: &str) -> &str {
    match language {
        "tsx" => "typescriptreact",
        "bash" => "shellscript",
        other => other,
    }
}

fn lowercase_file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn lowercase_extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
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
        assert_eq!(language_for_path(Path::new("App.vue")), "html");
        assert_eq!(lsp_language_id("tsx"), "typescriptreact");
        assert_eq!(lsp_language_id("rust"), "rust");
    }

    #[test]
    fn lsp_ids_follow_the_protocol_names() {
        let id = |p: &str| lsp_language_id_for_path(Path::new(p));
        assert_eq!(id("src/index.ts"), Some("typescript"));
        assert_eq!(id("src/App.tsx"), Some("typescriptreact"));
        assert_eq!(id("src/App.jsx"), Some("javascriptreact"));
        assert_eq!(id("server.mjs"), Some("javascript"));
        assert_eq!(id("App.vue"), Some("vue"));
        assert_eq!(id("+page.svelte"), Some("svelte"));
        assert_eq!(id("index.astro"), Some("astro"));
        assert_eq!(id("cmd/main.go"), Some("go"));
        assert_eq!(id("go.mod"), Some("go.mod"));
        assert_eq!(id("go.work"), Some("go.work"));
        assert_eq!(id("views/page.templ"), Some("templ"));
        assert_eq!(id("tsconfig.json"), Some("jsonc"));
        assert_eq!(id("tsconfig.build.json"), Some("jsonc"));
        assert_eq!(id("package.json"), Some("json"));
        assert_eq!(id("run.sh"), Some("shellscript"));
        assert_eq!(id("README.md"), None);
        assert_eq!(id("notes"), None);
    }
}

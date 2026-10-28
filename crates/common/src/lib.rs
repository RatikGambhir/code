//! Pieces every Cockpit subsystem shares: identifiers, on-disk locations and
//! the process environment a GUI app has to reconstruct on macOS.

pub mod ids;
pub mod paths;
pub mod shell_env;

pub use ids::*;
pub use paths::AppPaths;

/// Product name, used for the window title, bundle and data directory.
pub const APP_NAME: &str = "Cockpit";

/// Reverse-DNS identifier used for the macOS bundle and Keychain items.
pub const APP_IDENTIFIER: &str = "dev.cockpit.ide";

/// Version reported to Codex and shown in the UI.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shortens `text` to at most `max_chars` characters, appending an ellipsis
/// when something was cut. Works on characters, never splitting UTF-8.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abc", 3), "abc");
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate_chars("日本語テキスト", 3), "日本語…");
    }
}

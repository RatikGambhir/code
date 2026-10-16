//! Everything about files the UI needs that is not drawing: documents with
//! save/reload and external-change detection, language detection, directory
//! listing for the explorer, workspace search, fuzzy file finding, file
//! watching and a Language Server Protocol client.
//!
//! The text buffer itself is gpui-component's rope-backed editor; this crate
//! owns the relationship between that buffer and the file on disk.

mod document;
pub mod fs_tree;
pub mod fuzzy;
mod language;
pub mod lsp;
pub mod search;
pub mod watcher;

pub use document::{Document, DocumentError, LineEnding};
pub use language::{language_for_path, lsp_language_id};

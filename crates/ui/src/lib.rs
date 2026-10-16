//! Cockpit's user interface, built with GPUI and gpui-component.

pub mod actions;
mod assets;
pub mod browser_view;
pub mod editor_view;
pub mod explorer;
pub mod lsp;
pub mod state;
pub mod terminal_view;
mod theme;

pub use assets::Assets;
pub use state::{AppState, Settings};

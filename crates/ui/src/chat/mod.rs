//! Chat mode: the agent conversation filling the window, with its history
//! in the sidebar and the files it changed in a diff panel on the right.
//! The conversation itself is the agent panel, shown full width.

pub mod diff;
mod diff_panel;
mod history;

pub use diff_panel::{DiffPanel, DiffPanelEvent};
pub use history::{ChatHistory, ChatHistoryEvent};

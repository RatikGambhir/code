//! The Codex agent: controller (state and protocol) and panel (view).

mod controller;
mod feed;
mod mention;
mod panel;

pub use controller::{AgentController, AgentControllerEvent, ConnectionState};
pub use panel::{AgentPanel, AgentPanelEvent};

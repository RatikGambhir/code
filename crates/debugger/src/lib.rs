//! Debugging for Hangar: a Debug Adapter Protocol client that drives
//! `lldb-dap`, Cargo integration (run targets, building for the debugger,
//! runnable lines in a file), breakpoints that follow edits, and helpers for
//! presenting Rust values.

pub mod adapter;
pub mod breakpoints;
pub mod cargo;
pub mod client;
pub mod protocol;
pub mod values;

pub use adapter::{find_lldb_dap, RustToolchain};
pub use breakpoints::{BreakpointSet, LineBreakpoint};
pub use cargo::{find_runnables, workspace_targets, Build, BuildEvent, CargoTarget, RunConfig, Runnable, TargetKind};
pub use client::{AdapterCommand, DapClient};
pub use protocol::Event;

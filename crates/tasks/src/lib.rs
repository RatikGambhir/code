//! Projects and issues for the Tasks view: a local board per workspace and a
//! Linear client. Network and disk operations run off the UI thread.

pub mod key;
pub mod linear;
pub mod local;
pub mod oauth;
pub mod model;

use std::path::PathBuf;

use anyhow::Result;

pub use model::{Board, Issue, Label, Priority, Project, ProjectState, Source, Status, StatusKind};

/// Hangar's data directory (`HANGAR_DATA_DIR` overrides it).
pub fn data_dir() -> Result<PathBuf> {
    Ok(cockpit_common::paths::AppPaths::resolve()?.root().to_path_buf())
}

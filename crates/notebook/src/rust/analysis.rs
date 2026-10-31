//! Showing cells to rust-analyzer. A notebook gets its own rust-analyzer,
//! started in the throwaway project. Each cell is a module of that crate
//! (`src/cell_<id>.rs`) whose only function's body is the cell, and the
//! crate root the server sees declares those modules. Neither is what runs:
//! the editor sends this text to the server, which uses it over the files
//! on disk.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde_json::json;

use super::{cell, scratch::Scratch};
use crate::{CellAnalysis, CellDocument};

pub struct RustAnalysis {
    pub(super) scratch: Arc<Scratch>,
    /// The workspace's target directory, where rust-analyzer's own `cargo
    /// check` runs (for build scripts and proc macros) also go.
    pub(super) target_dir: PathBuf,
    pub(super) tokio: bool,
}

/// The text before a cell in its module.
const CELL_PREFIX: &str = "\
#![allow(unused, unreachable_code, clippy::all)]
use crate::__hangar;
fn __cell() -> ::std::result::Result<(), ::std::boxed::Box<dyn ::std::error::Error>> {
let _ = {
";

/// The text after a cell in its module.
const CELL_SUFFIX: &str = "
};
::std::result::Result::Ok(())
}
";

impl RustAnalysis {
    fn cell_path(&self, id: usize) -> PathBuf {
        self.scratch.dir().join("src").join(format!("cell_{id}.rs"))
    }
}

impl CellAnalysis for RustAnalysis {
    fn language(&self) -> &'static str {
        "rust"
    }

    fn root(&self) -> &Path {
        self.scratch.dir()
    }

    fn settings(&self) -> Option<serde_json::Value> {
        Some(json!({
            // Cells are built when they run; no `cargo check` on save.
            "checkOnSave": false,
            "cargo": {
                "buildScripts": { "enable": true },
                "allTargets": false,
                // Share the workspace's build output instead of making a
                // `target/` inside the throwaway project.
                "targetDir": self.target_dir.to_string_lossy(),
            },
            "procMacro": { "enable": true },
            // Index only what cells use, as they use it.
            "cachePriming": { "enable": false },
            "diagnostics": { "disabled": ["unlinked-file", "inactive-code"] },
            "files": { "watcher": "server" },
        }))
    }

    fn cell_document(&self, id: usize) -> CellDocument {
        let path = self.cell_path(id);
        // The server finds `mod cell_<id>;` by its file; the text it uses is
        // the editor's.
        if !path.exists() {
            let _ = fs::write(&path, "");
        }
        CellDocument { path, prefix: CELL_PREFIX.to_string(), suffix: CELL_SUFFIX.to_string() }
    }

    fn root_document(&self, cells: &[usize]) -> Option<(PathBuf, String)> {
        let mut text = String::from(
            "// The notebook's cells as rust-analyzer sees them (not what runs).\n#![allow(unused, unreachable_code, clippy::all)]\n",
        );
        for id in cells {
            text.push_str(&format!("mod cell_{id};\n"));
        }
        text.push_str("fn main() {}\n");
        text.push_str(&cell::helpers(self.tokio));
        Some((self.scratch.dir().join("src/main.rs"), text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_keep_their_columns() {
        // The cell starts a line of its own, and the prefix ends one, so
        // only lines shift.
        assert!(CELL_PREFIX.ends_with('\n'));
        assert!(CELL_SUFFIX.starts_with('\n'));
    }
}

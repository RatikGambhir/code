//! Where Cockpit keeps its state on disk.
//!
//! On macOS that is `~/Library/Application Support/Cockpit`. Everything the
//! app owns lives under one root so a user can inspect or delete it in one go.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

#[derive(Clone, Debug)]
pub struct AppPaths {
    root: PathBuf,
}

impl AppPaths {
    /// The platform data directory, overridable with `COCKPIT_DATA_DIR` (used by
    /// tests and for running several isolated instances side by side).
    pub fn resolve() -> Result<Self> {
        let root = match std::env::var_os("COCKPIT_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::data_dir()
                .context("no platform data directory")?
                .join(crate::APP_NAME),
        };
        Self::at(root)
    }

    /// Uses `root` as the data directory, creating it and its children.
    pub fn at(root: impl Into<PathBuf>) -> Result<Self> {
        let paths = Self { root: root.into() };
        for dir in [
            paths.root.clone(),
            paths.browser_profiles_dir(),
            paths.logs_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(paths)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The SQLite database holding workspaces, tabs, history and agent usage.
    pub fn database(&self) -> PathBuf {
        self.root.join("cockpit.sqlite3")
    }

    /// Parent of every browser profile. Chromium keeps cookies and local storage
    /// here in its own (encrypted where the platform supports it) format, so
    /// none of that ever goes into the application database.
    pub fn browser_profiles_dir(&self) -> PathBuf {
        self.root.join("browser")
    }

    /// The directory for one named browser profile.
    pub fn browser_profile(&self, name: &str) -> PathBuf {
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        self.browser_profiles_dir().join(safe)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("data")).unwrap();
        assert!(paths.browser_profiles_dir().is_dir());
        assert!(paths.logs_dir().is_dir());
        assert_eq!(paths.database().parent().unwrap(), paths.root());
    }

    #[test]
    fn profile_names_cannot_escape_the_profiles_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path()).unwrap();
        let profile = paths.browser_profile("../../etc");
        assert_eq!(profile.parent().unwrap(), paths.browser_profiles_dir());
    }
}

//! Where Hangar keeps its state on disk.
//!
//! On macOS that is `~/Library/Application Support/Hangar`. Everything the
//! app owns lives under one root so a user can inspect or delete it in one go.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// The app's name before it became Hangar: its data directory and database
/// are moved over on first launch.
const LEGACY_DIR_NAME: &str = "Cockpit";
const LEGACY_DATABASE: &str = "cockpit.sqlite3";
const DATABASE: &str = "hangar.sqlite3";
/// SQLite's files beside the database, moved with it.
const SQLITE_SIDECARS: [&str; 2] = ["-wal", "-shm"];

#[derive(Clone, Debug)]
pub struct AppPaths {
    root: PathBuf,
}

impl AppPaths {
    /// The platform data directory, overridable with `HANGAR_DATA_DIR` (used by
    /// tests and for running several isolated instances side by side).
    pub fn resolve() -> Result<Self> {
        let root = match std::env::var_os("HANGAR_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => {
                let base = dirs::data_dir().context("no platform data directory")?;
                let root = base.join(crate::APP_NAME);
                adopt_legacy_dir(&base.join(LEGACY_DIR_NAME), &root);
                root
            }
        };
        let paths = Self::at(root)?;
        adopt_legacy_database(paths.root());
        Ok(paths)
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
        self.root.join(DATABASE)
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

/// Moves the old app's data directory to `root` if `root` does not exist
/// yet. A failure is logged and the app starts fresh; nothing is deleted.
fn adopt_legacy_dir(legacy: &Path, root: &Path) {
    if root.exists() || !legacy.is_dir() {
        return;
    }
    match std::fs::rename(legacy, root) {
        Ok(()) => tracing::info!("moved {} to {}", legacy.display(), root.display()),
        Err(err) => tracing::warn!("moving {} to {}: {err}", legacy.display(), root.display()),
    }
}

/// Renames the old database (and its WAL files) to the current name, if
/// there is no current one. The WAL files go first: should the main file's
/// rename fail, SQLite still finds them next to whichever name it opens
/// once this runs again.
fn adopt_legacy_database(root: &Path) {
    let (legacy, current) = (root.join(LEGACY_DATABASE), root.join(DATABASE));
    if current.exists() || !legacy.is_file() {
        return;
    }
    let with_suffix = |path: &Path, suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let moves = SQLITE_SIDECARS
        .iter()
        .map(|suffix| (with_suffix(&legacy, suffix), with_suffix(&current, suffix)))
        .chain([(legacy.clone(), current.clone())]);
    for (from, to) in moves {
        if from.exists() {
            if let Err(err) = std::fs::rename(&from, &to) {
                tracing::warn!("renaming {} to {}: {err}", from.display(), to.display());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopts_the_legacy_data_dir_and_database() {
        let base = tempfile::tempdir().unwrap();
        let (legacy, root) = (base.path().join(LEGACY_DIR_NAME), base.path().join("Hangar"));
        std::fs::create_dir(&legacy).unwrap();
        for name in ["cockpit.sqlite3", "cockpit.sqlite3-wal", "cockpit.sqlite3-shm"] {
            std::fs::write(legacy.join(name), name).unwrap();
        }

        adopt_legacy_dir(&legacy, &root);
        adopt_legacy_database(&root);

        assert!(!legacy.exists());
        assert_eq!(std::fs::read_to_string(root.join("hangar.sqlite3")).unwrap(), "cockpit.sqlite3");
        assert_eq!(std::fs::read_to_string(root.join("hangar.sqlite3-wal")).unwrap(), "cockpit.sqlite3-wal");
        assert!(root.join("hangar.sqlite3-shm").exists());
        assert!(!root.join("cockpit.sqlite3").exists());
    }

    #[test]
    fn never_overwrites_current_data() {
        let base = tempfile::tempdir().unwrap();
        let (legacy, root) = (base.path().join(LEGACY_DIR_NAME), base.path().join("Hangar"));
        std::fs::create_dir(&legacy).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("hangar.sqlite3"), "current").unwrap();
        std::fs::write(root.join("cockpit.sqlite3"), "old").unwrap();

        adopt_legacy_dir(&legacy, &root);
        adopt_legacy_database(&root);

        assert!(legacy.exists(), "the old directory stays when the new one exists");
        assert_eq!(std::fs::read_to_string(root.join("hangar.sqlite3")).unwrap(), "current");
    }

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

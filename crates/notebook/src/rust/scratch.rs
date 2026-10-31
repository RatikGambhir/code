//! The throwaway Cargo project a Rust notebook builds its cells in.
//!
//! It lives in the workspace's target directory and builds into it, so the
//! function's crate and its dependencies are compiled once and shared with
//! normal builds. To keep that sharing working it copies the workspace's
//! `Cargo.lock`, `[profile]` and `[patch]` settings.
//!
//! Only the project's own files are created: its sources, its binary, and
//! that binary's fingerprints, dependency info and incremental cache. All of
//! them are removed when the [`Scratch`] is dropped. Projects of a Hangar
//! that quit without cleaning up are swept away the next time one is made.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{bail, Context as _, Result};
use parking_lot::Mutex;
use serde_json::Value as Json;

/// The folder of the target directory that holds the projects.
const SCRATCH_DIR: &str = "hangar-notebook";
/// Where a project records the build directories it has put files in.
const PROFILE_DIRS_FILE: &str = "profile-dirs";
const NAME_PREFIX: &str = "hangar-notebook-";

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// What `cargo metadata` says about the workspace a package is in.
#[derive(Clone, Debug)]
pub struct CargoWorkspace {
    pub root: PathBuf,
    pub target_dir: PathBuf,
    /// Packages whose normal dependencies include Tokio.
    tokio_packages: BTreeSet<String>,
}

impl CargoWorkspace {
    /// Reads the workspace of the package in `dir`.
    ///
    /// # Errors
    ///
    /// When `cargo metadata` fails or prints something unexpected.
    pub fn load(dir: &Path) -> Result<Self> {
        let output = Command::new("cargo")
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .context("running cargo metadata")?;
        if !output.status.success() {
            bail!("cargo metadata failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Self::parse(&output.stdout)
    }

    fn parse(json: &[u8]) -> Result<Self> {
        let metadata: Json = serde_json::from_slice(json).context("decoding cargo metadata")?;
        let path = |key: &str| {
            metadata
                .get(key)
                .and_then(Json::as_str)
                .map(PathBuf::from)
                .with_context(|| format!("cargo metadata has no {key}"))
        };
        let tokio_packages = metadata
            .get("packages")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter(|package| {
                package.get("dependencies").and_then(Json::as_array).is_some_and(|deps| {
                    deps.iter().any(|dep| {
                        dep.get("name").and_then(Json::as_str) == Some("tokio")
                            && dep.get("kind").is_none_or(Json::is_null)
                            && dep.get("rename").is_none_or(Json::is_null)
                            && !dep.get("optional").and_then(Json::as_bool).unwrap_or(false)
                    })
                })
            })
            .filter_map(|package| package.get("name").and_then(Json::as_str).map(str::to_string))
            .collect();
        Ok(Self { root: path("workspace_root")?, target_dir: path("target_directory")?, tokio_packages })
    }

    pub fn uses_tokio(&self, package: &str) -> bool {
        self.tokio_packages.contains(package)
    }
}

/// The library a notebook calls into.
#[derive(Clone, Debug)]
pub struct Dependency {
    pub package: String,
    pub package_dir: PathBuf,
    /// The crate's name in code (`demo_lib`).
    pub crate_name: String,
    /// Add Tokio, for running async functions.
    pub tokio: bool,
}

pub struct Scratch {
    /// The package name (`hangar-notebook-<pid>-<n>`).
    name: String,
    dir: PathBuf,
    target_dir: PathBuf,
    workspace_root: PathBuf,
    /// Build directories (`target/debug`) the binary was built in.
    profile_dirs: Mutex<BTreeSet<PathBuf>>,
}

impl Scratch {
    /// Makes a project depending on `dependency`, after sweeping away
    /// projects left by Hangars that are no longer running.
    ///
    /// # Errors
    ///
    /// When the project's files cannot be written.
    pub fn create(workspace: &CargoWorkspace, dependency: &Dependency) -> Result<Self> {
        let parent = workspace.target_dir.join(SCRATCH_DIR);
        sweep_orphans(&parent, &workspace.target_dir);
        let name = format!("{NAME_PREFIX}{}-{}", std::process::id(), NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let dir = parent.join(&name);
        // A leftover from an earlier Hangar with the same process id.
        if dir.exists() {
            remove_project(&dir, &name, &workspace.target_dir);
        }
        fs::create_dir_all(dir.join("src")).with_context(|| format!("creating {}", dir.display()))?;
        let root_manifest = fs::read_to_string(workspace.root.join("Cargo.toml"))
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok());
        let manifest = manifest(&name, dependency, root_manifest.as_ref(), &workspace.root);
        fs::write(dir.join("Cargo.toml"), manifest).context("writing the notebook's Cargo.toml")?;
        // A package needs a target before Cargo (or rust-analyzer) will load
        // it; each run replaces this.
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").context("writing the notebook's main.rs")?;
        Ok(Self {
            name,
            dir,
            target_dir: workspace.target_dir.clone(),
            workspace_root: workspace.root.clone(),
            profile_dirs: Mutex::new(BTreeSet::new()),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes the cell's program and refreshes `Cargo.lock` from the
    /// workspace, so the build uses the same dependency versions.
    ///
    /// # Errors
    ///
    /// When the files cannot be written.
    pub fn prepare(&self, main_rs: &str) -> Result<()> {
        fs::write(self.dir.join("src/main.rs"), main_rs).context("writing the cell's program")?;
        let lock = self.workspace_root.join("Cargo.lock");
        if lock.exists() {
            fs::copy(&lock, self.dir.join("Cargo.lock")).context("copying Cargo.lock")?;
        }
        Ok(())
    }

    /// `cargo build` for the project, reporting artifacts as JSON on stdout
    /// and diagnostics as text on stderr.
    pub fn build_command(&self) -> Command {
        let mut command = Command::new("cargo");
        command
            .args(["build", "--message-format=json-render-diagnostics", "--manifest-path"])
            .arg(self.dir.join("Cargo.toml"))
            // Inside the target directory, so `.cargo/config.toml` of the
            // workspace still applies.
            .current_dir(&self.dir)
            .env("CARGO_TARGET_DIR", &self.target_dir)
            .env("CARGO_TERM_COLOR", "never");
        command
    }

    /// Remembers that the binary was built in `executable`'s directory, for
    /// cleaning up (also on disk, in case Hangar quits first).
    pub fn record_executable(&self, executable: &Path) {
        let Some(profile_dir) = executable.parent() else { return };
        let mut dirs = self.profile_dirs.lock();
        if dirs.insert(profile_dir.to_path_buf()) {
            let list: Vec<String> = dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect();
            let _ = fs::write(self.dir.join(PROFILE_DIRS_FILE), list.join("\n"));
        }
    }

    /// Where the program writes the cell's value.
    pub fn result_path(&self) -> PathBuf {
        self.dir.join("result")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let dir = self.dir.clone();
        let name = self.name.clone();
        let target_dir = self.target_dir.clone();
        let extra: Vec<PathBuf> = self.profile_dirs.lock().iter().cloned().collect();
        // Off the caller's thread: the incremental cache can be many files.
        // If Hangar quits first, the next notebook sweeps up the rest.
        std::thread::spawn(move || {
            remove_artifacts(&name, &target_dir, extra);
            let _ = fs::remove_dir_all(&dir);
            if let Some(parent) = dir.parent() {
                // Only succeeds once the folder is empty.
                let _ = fs::remove_dir(parent);
            }
        });
    }
}

fn manifest(name: &str, dependency: &Dependency, root_manifest: Option<&toml::Table>, workspace_root: &Path) -> String {
    let mut table = toml::Table::new();

    let mut package = toml::Table::new();
    package.insert("name".into(), name.into());
    package.insert("version".into(), "0.0.0".into());
    package.insert("edition".into(), "2021".into());
    package.insert("publish".into(), false.into());
    table.insert("package".into(), package.into());

    let mut deps = toml::Table::new();
    let mut lib = toml::Table::new();
    lib.insert("path".into(), dependency.package_dir.to_string_lossy().into_owned().into());
    lib.insert("package".into(), dependency.package.clone().into());
    deps.insert(dependency.crate_name.clone(), lib.into());
    if dependency.tokio {
        let mut tokio = toml::Table::new();
        tokio.insert("version".into(), "1".into());
        tokio.insert("features".into(), toml::Value::Array(vec!["rt-multi-thread".into()]));
        deps.insert("tokio".into(), tokio.into());
    }
    table.insert("dependencies".into(), deps.into());

    // Its own workspace, so Cargo does not look for one above it.
    let mut workspace = toml::Table::new();
    if let Some(resolver) = root_manifest.and_then(|m| m.get("workspace")?.get("resolver")).cloned() {
        workspace.insert("resolver".into(), resolver);
    }
    table.insert("workspace".into(), workspace.into());

    if let Some(root) = root_manifest {
        // The same profiles and patches as the workspace, so the shared
        // dependencies build identically and are reused.
        if let Some(profile) = root.get("profile") {
            table.insert("profile".into(), profile.clone());
        }
        if let Some(toml::Value::Table(patch)) = root.get("patch") {
            table.insert("patch".into(), absolute_patch_paths(patch, workspace_root).into());
        }
    }
    format!("# Generated by Hangar for a notebook; deleted when the notebook closes.\n{table}")
}

/// `[patch]` with relative `path`s resolved against the workspace root.
fn absolute_patch_paths(patch: &toml::Table, workspace_root: &Path) -> toml::Table {
    let mut patch = patch.clone();
    for (_, source) in patch.iter_mut() {
        let Some(source) = source.as_table_mut() else { continue };
        for (_, spec) in source.iter_mut() {
            let Some(spec) = spec.as_table_mut() else { continue };
            let relative = match spec.get("path") {
                Some(toml::Value::String(path)) if Path::new(path).is_relative() => path.clone(),
                _ => continue,
            };
            let absolute = workspace_root.join(relative).to_string_lossy().into_owned();
            spec.insert("path".into(), absolute.into());
        }
    }
    patch
}

/// Removes projects (and their build files) whose Hangar is not running,
/// then build files of such projects whose folder is already gone (a build
/// that finished after its notebook closed).
fn sweep_orphans(parent: &Path, target_dir: &Path) {
    let own_pid = std::process::id();
    let mut running: std::collections::HashMap<u32, bool> = std::collections::HashMap::new();
    let mut abandoned = |pid: u32| pid != own_pid && !*running.entry(pid).or_insert_with(|| process_is_running(pid));
    for entry in fs::read_dir(parent).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if owner_pid(&name).is_some_and(&mut abandoned) {
            tracing::debug!(project = %name, "removing an abandoned notebook project");
            remove_project(&entry.path(), &name, target_dir);
        }
    }
    let profile_dir = target_dir.join("debug");
    for sub in ["", "deps", ".fingerprint", "incremental"] {
        for entry in fs::read_dir(profile_dir.join(sub)).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if artifact_owner_pid(&name).is_some_and(&mut abandoned) {
                let path = entry.path();
                let _ = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
            }
        }
    }
}

/// The process id in a build file of a project (`hangar-notebook-<pid>-…`
/// or `hangar_notebook_<pid>_…`).
fn artifact_owner_pid(name: &str) -> Option<u32> {
    let name = name.strip_prefix("lib").unwrap_or(name);
    let rest = name.strip_prefix(NAME_PREFIX).or_else(|| name.strip_prefix("hangar_notebook_"))?;
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

fn remove_project(dir: &Path, name: &str, target_dir: &Path) {
    let recorded = fs::read_to_string(dir.join(PROFILE_DIRS_FILE)).unwrap_or_default();
    remove_artifacts(name, target_dir, recorded.lines().map(PathBuf::from).collect());
    let _ = fs::remove_dir_all(dir);
}

/// The process id in `hangar-notebook-<pid>-<n>`.
fn owner_pid(name: &str) -> Option<u32> {
    let (pid, n) = name.strip_prefix(NAME_PREFIX)?.split_once('-')?;
    n.parse::<usize>().ok()?;
    pid.parse().ok()
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    // `kill -0` checks without sending anything. If it cannot be run, assume
    // the process is alive and leave its files alone.
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(true, |status| status.success())
}

#[cfg(not(unix))]
fn process_is_running(_pid: u32) -> bool {
    true
}

/// Deletes what building `name` put in the target directory: the binary
/// and its `.d` file, `deps/`, `.fingerprint/` and `incremental/` entries.
/// Nothing shared with other packages is touched.
fn remove_artifacts(name: &str, target_dir: &Path, mut profile_dirs: Vec<PathBuf>) {
    profile_dirs.push(target_dir.join("debug"));
    profile_dirs.sort();
    profile_dirs.dedup();
    let underscored = name.replace('-', "_");
    let stems = [name, underscored.as_str()];
    for profile_dir in profile_dirs {
        // Never reach outside the target directory, whatever a recorded
        // path says.
        if !profile_dir.starts_with(target_dir) {
            continue;
        }
        for sub in ["", "deps", ".fingerprint", "incremental", "build", "examples"] {
            let Ok(entries) = fs::read_dir(profile_dir.join(sub)) else { continue };
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if !stems.iter().any(|stem| is_artifact_of(&file_name, stem)) {
                    continue;
                }
                let path = entry.path();
                let result = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
                if let Err(err) = result {
                    tracing::debug!(path = %path.display(), "could not remove notebook build file: {err}");
                }
            }
        }
    }
}

/// `stem`, `stem-<hash>…` or `stem.<ext>` (with Cargo's `lib` in front for
/// metadata files), but not `stem0` (another project whose number starts
/// the same).
fn is_artifact_of(file_name: &str, stem: &str) -> bool {
    let file_name = file_name.strip_prefix("lib").filter(|rest| rest.starts_with(stem)).unwrap_or(file_name);
    file_name
        .strip_prefix(stem)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('-') || rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_metadata() {
        let json = br#"{"workspace_root":"/w","target_directory":"/w/target","packages":[
            {"name":"a","dependencies":[{"name":"tokio","kind":null,"rename":null,"optional":false}]},
            {"name":"b","dependencies":[{"name":"tokio","kind":"dev","rename":null,"optional":false}]},
            {"name":"c","dependencies":[]}]}"#;
        let ws = CargoWorkspace::parse(json).unwrap();
        assert_eq!(ws.target_dir, PathBuf::from("/w/target"));
        assert!(ws.uses_tokio("a") && !ws.uses_tokio("b") && !ws.uses_tokio("c"));
    }

    #[test]
    fn writes_a_manifest_with_the_workspace_settings() {
        let root: toml::Table = r#"
            [workspace]
            resolver = "2"
            [profile.dev.package."*"]
            opt-level = 2
            [patch.crates-io]
            gpui-component = { path = "vendor/gpui-component" }
            other = { git = "https://example.com/x" }
        "#
        .parse()
        .unwrap();
        let dependency = Dependency {
            package: "demo-lib".into(),
            package_dir: PathBuf::from("/w/crates/demo"),
            crate_name: "demo_lib".into(),
            tokio: true,
        };
        let text = manifest("hangar-notebook-1-0", &dependency, Some(&root), Path::new("/w"));
        let parsed: toml::Table = text.parse().unwrap();
        assert_eq!(parsed["package"]["name"].as_str(), Some("hangar-notebook-1-0"));
        assert_eq!(parsed["dependencies"]["demo_lib"]["package"].as_str(), Some("demo-lib"));
        assert_eq!(parsed["dependencies"]["demo_lib"]["path"].as_str(), Some("/w/crates/demo"));
        assert!(parsed["dependencies"].get("tokio").is_some());
        assert_eq!(parsed["workspace"]["resolver"].as_str(), Some("2"));
        assert_eq!(parsed["profile"]["dev"]["package"]["*"]["opt-level"].as_integer(), Some(2));
        assert_eq!(
            parsed["patch"]["crates-io"]["gpui-component"]["path"].as_str(),
            Some("/w/vendor/gpui-component")
        );
        assert!(parsed["patch"]["crates-io"]["other"].get("path").is_none());
    }

    #[test]
    fn matches_only_its_own_artifacts() {
        assert!(is_artifact_of("hangar-notebook-12-1", "hangar-notebook-12-1"));
        assert!(is_artifact_of("hangar-notebook-12-1.d", "hangar-notebook-12-1"));
        assert!(is_artifact_of("hangar_notebook_12_1-0f3a", "hangar_notebook_12_1"));
        assert!(is_artifact_of("libhangar_notebook_12_1-0f3a.rmeta", "hangar_notebook_12_1"));
        assert!(!is_artifact_of("hangar-notebook-12-10-0f3a", "hangar-notebook-12-1"));
        assert!(!is_artifact_of("serde-0f3a", "hangar-notebook-12-1"));
        assert_eq!(owner_pid("hangar-notebook-123-4"), Some(123));
        assert_eq!(owner_pid("hangar-notebook-x-4"), None);
        assert_eq!(owner_pid("serde"), None);
        assert_eq!(artifact_owner_pid("hangar_notebook_77_3-abc.rmeta"), Some(77));
        assert_eq!(artifact_owner_pid("hangar-notebook-77-3-abc"), Some(77));
        assert_eq!(artifact_owner_pid("serde-abc"), None);
    }

    #[test]
    fn removes_its_build_files_and_sweeps_orphans() {
        let target = tempfile::tempdir().unwrap();
        let t = target.path();
        let debug = t.join("debug");
        for dir in ["deps", ".fingerprint", "incremental"] {
            fs::create_dir_all(debug.join(dir)).unwrap();
        }
        let touch = |p: PathBuf| fs::write(p, "x").unwrap();
        // An abandoned project of a process that is not running.
        let orphan = "hangar-notebook-999999-0";
        fs::create_dir_all(t.join(SCRATCH_DIR).join(orphan).join("src")).unwrap();
        touch(debug.join(orphan));
        touch(debug.join("deps/hangar_notebook_999999_0-abc"));
        fs::create_dir_all(debug.join(".fingerprint/hangar-notebook-999999-0-abc")).unwrap();
        fs::create_dir_all(debug.join("incremental/hangar_notebook_999999_0-abc/s-1")).unwrap();
        // Build files whose project folder is already gone.
        touch(debug.join("deps/libhangar_notebook_999998_2-abc.rmeta"));
        // Someone else's files stay.
        touch(debug.join("deps/serde-abc.rlib"));
        touch(debug.join("hangar-notebook-999999-01"));

        let workspace = CargoWorkspace { root: t.to_path_buf(), target_dir: t.to_path_buf(), tokio_packages: BTreeSet::new() };
        let dependency = Dependency {
            package: "demo".into(),
            package_dir: t.join("demo"),
            crate_name: "demo".into(),
            tokio: false,
        };
        let scratch = Scratch::create(&workspace, &dependency).unwrap();
        assert!(!t.join(SCRATCH_DIR).join(orphan).exists());
        assert!(!debug.join(orphan).exists());
        assert!(!debug.join("deps/hangar_notebook_999999_0-abc").exists());
        assert!(!debug.join(".fingerprint/hangar-notebook-999999-0-abc").exists());
        assert!(!debug.join("incremental/hangar_notebook_999999_0-abc").exists());
        assert!(!debug.join("deps/libhangar_notebook_999998_2-abc.rmeta").exists());
        assert!(debug.join("deps/serde-abc.rlib").exists());

        // Its own build files go when it is dropped.
        let name = scratch.name().to_string();
        let underscored = name.replace('-', "_");
        let dir = scratch.dir().to_path_buf();
        touch(debug.join(&name));
        touch(debug.join(format!("deps/{underscored}-def")));
        scratch.record_executable(&debug.join(&name));
        drop(scratch);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while dir.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!dir.exists());
        assert!(!debug.join(&name).exists());
        assert!(!debug.join(format!("deps/{underscored}-def")).exists());
        assert!(debug.join("deps/serde-abc.rlib").exists());
    }
}

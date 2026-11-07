//! Installing language servers into a directory Hangar owns.
//!
//! npm packages go into `<dir>/node` (with npm, or Bun when there is no
//! npm) and Go tools into `<dir>/go/bin` (with `go install`), so nothing is
//! installed globally and deleting the directory undoes it all. The
//! registry looks in these directories after the user's own installs.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context as _, Result};

/// How to get a server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Install {
    /// npm packages, installed together. A package with a version or an
    /// alias (`name@npm:typescript@6`) is installed as written, others at
    /// their latest version.
    Npm(&'static [&'static str]),
    /// A Go package for `go install` (`golang.org/x/tools/gopls@latest`).
    Go(&'static str),
    /// Only the user can install it; how.
    Hint(&'static str),
}

impl Install {
    /// Something to tell the user: how to install it themselves.
    pub fn hint(&self) -> String {
        match self {
            Install::Npm(packages) => {
                let packages: Vec<&str> = packages.iter().map(|p| aliased(p).unwrap_or(p)).collect();
                format!("run `npm install -g {}`", packages.join(" "))
            }
            Install::Go(package) => format!("run `go install {package}`"),
            Install::Hint(hint) => (*hint).to_string(),
        }
    }

    /// Hangar can install it on its own.
    pub fn is_automatic(&self) -> bool {
        !matches!(self, Install::Hint(_))
    }
}

/// The directory managed installs live in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedDir {
    root: PathBuf,
}

impl ManagedDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn node_dir(&self) -> PathBuf {
        self.root.join("node")
    }

    pub fn node_modules(&self) -> PathBuf {
        self.node_dir().join("node_modules")
    }

    fn go_bin(&self) -> PathBuf {
        self.root.join("go/bin")
    }

    /// Where installed executables end up.
    pub fn bin_dirs(&self) -> [PathBuf; 2] {
        [self.node_modules().join(".bin"), self.go_bin()]
    }

    /// A classic TypeScript (with `tsserver.js`) installed beside the
    /// servers that load one: TypeScript itself when it is 6 or earlier,
    /// else the `typescript-classic` alias kept for them.
    pub fn classic_typescript_lib(&self) -> Option<PathBuf> {
        ["typescript", "typescript-classic"]
            .iter()
            .map(|name| self.node_modules().join(name).join("lib"))
            .find(|lib| lib.join("tsserver.js").is_file())
    }

    /// Installs everything in `installs` that can be installed
    /// automatically, finding npm, Bun and Go through `find`. Blocks for as
    /// long as the package managers take, so run it in the background.
    ///
    /// Returns what was installed; fails with the package manager's output
    /// when an install fails.
    pub fn install(&self, installs: &[Install], find: impl Fn(&str) -> Option<PathBuf>) -> Result<Vec<String>> {
        let npm_packages: BTreeSet<&str> = installs
            .iter()
            .filter_map(|install| match install {
                Install::Npm(packages) => Some(packages.iter().copied()),
                _ => None,
            })
            .flatten()
            .collect();
        let go_packages: BTreeSet<&str> = installs
            .iter()
            .filter_map(|install| match install {
                Install::Go(package) => Some(*package),
                _ => None,
            })
            .collect();

        let mut installed = Vec::new();
        if !npm_packages.is_empty() {
            self.install_npm(&npm_packages, &find)?;
            installed.extend(npm_packages.iter().map(|p| package_name(p).to_string()));
        }
        if !go_packages.is_empty() {
            let go = find("go").context("Go is not installed; get it from https://go.dev/dl")?;
            std::fs::create_dir_all(self.go_bin())?;
            for package in &go_packages {
                let mut command = Command::new(&go);
                command.args(["install", package]).env("GOBIN", self.go_bin());
                run(command, &format!("go install {package}"))?;
                installed.push(package.split('@').next().unwrap_or(package).to_string());
            }
        }
        Ok(installed)
    }

    fn install_npm(&self, packages: &BTreeSet<&str>, find: &impl Fn(&str) -> Option<PathBuf>) -> Result<()> {
        let dir = self.node_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let manifest = dir.join("package.json");
        if !manifest.exists() {
            std::fs::write(&manifest, "{ \"private\": true }\n")?;
        }
        let specs: Vec<String> = packages
            .iter()
            .map(|p| if has_version(p) { p.to_string() } else { format!("{p}@latest") })
            .collect();
        let command = if let Some(npm) = find("npm") {
            let mut command = Command::new(npm);
            command
                .args(["install", "--no-audit", "--no-fund", "--loglevel=error", "--prefix"])
                .arg(&dir)
                .args(&specs);
            command
        } else if let Some(bun) = find("bun") {
            let mut command = Command::new(bun);
            command.arg("add").arg("--cwd").arg(&dir).args(&specs);
            command
        } else {
            bail!("installing {} needs npm or Bun; install Node.js or Bun first", specs.join(", "));
        };
        run(command, &format!("installing {}", specs.join(" ")))
    }
}

/// `typescript@6`, `@scope/name@1`, or an alias: a version is given.
fn has_version(spec: &str) -> bool {
    spec.get(1..).is_some_and(|rest| rest.contains('@'))
}

/// The package an alias installs: `typescript@6` for
/// `typescript-classic@npm:typescript@6`.
fn aliased(spec: &str) -> Option<&str> {
    spec.split_once("@npm:").map(|(_, target)| target)
}

/// The installed package's name, without its version.
fn package_name(spec: &str) -> &str {
    let name_end = spec.get(1..).and_then(|rest| rest.find('@')).map_or(spec.len(), |at| at + 1);
    &spec[..name_end]
}

fn run(mut command: Command, what: &str) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("{what}: could not start"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(6).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("{what} failed: {}", tail.join("\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_name_the_command() {
        assert_eq!(
            Install::Npm(&["typescript-language-server", "typescript"]).hint(),
            "run `npm install -g typescript-language-server typescript`"
        );
        assert_eq!(Install::Go("golang.org/x/tools/gopls@latest").hint(), "run `go install golang.org/x/tools/gopls@latest`");
        assert!(!Install::Hint("x").is_automatic());
        assert_eq!(
            Install::Npm(&["@vue/language-server", "typescript-classic@npm:typescript@6"]).hint(),
            "run `npm install -g @vue/language-server typescript@6`"
        );
    }

    #[test]
    fn package_specs() {
        assert!(!has_version("typescript") && !has_version("@vue/language-server"));
        assert!(has_version("typescript@6") && has_version("@scope/x@1") && has_version("a@npm:b@6"));
        assert_eq!(package_name("@vue/language-server"), "@vue/language-server");
        assert_eq!(package_name("@scope/x@1"), "@scope/x");
        assert_eq!(package_name("typescript-classic@npm:typescript@6"), "typescript-classic");
    }

    #[test]
    fn installs_without_a_package_manager_explain_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let managed = ManagedDir::new(dir.path());
        let err = managed.install(&[Install::Npm(&["x"])], |_| None).unwrap_err();
        assert!(err.to_string().contains("needs npm or Bun"), "{err}");
        let err = managed.install(&[Install::Go("example.com/x@latest")], |_| None).unwrap_err();
        assert!(err.to_string().contains("Go is not installed"), "{err}");
        // Hints are skipped, not failed.
        assert_eq!(managed.install(&[Install::Hint("x")], |_| None).unwrap(), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn npm_and_go_are_called_with_the_managed_directories() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let fake = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho \"{name} $@ GOBIN=$GOBIN\" >> {}\n", log.display())).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let (npm, go) = (fake("npm"), fake("go"));
        let managed = ManagedDir::new(dir.path().join("servers"));
        let installed = managed
            .install(
                &[
                    Install::Npm(&["typescript-language-server", "typescript"]),
                    Install::Npm(&["typescript", "typescript-classic@npm:typescript@6"]),
                    Install::Go("golang.org/x/tools/gopls@latest"),
                ],
                |name| match name {
                    "npm" => Some(npm.clone()),
                    "go" => Some(go.clone()),
                    _ => None,
                },
            )
            .unwrap();
        assert_eq!(
            installed,
            ["typescript", "typescript-classic", "typescript-language-server", "golang.org/x/tools/gopls"]
        );
        let log = std::fs::read_to_string(log).unwrap();
        let servers = dir.path().join("servers");
        assert!(log.contains(&format!(
            "npm install --no-audit --no-fund --loglevel=error --prefix {} typescript@latest typescript-classic@npm:typescript@6 typescript-language-server@latest",
            servers.join("node").display()
        )), "{log}");
        assert!(log.contains(&format!("go install golang.org/x/tools/gopls@latest GOBIN={}", servers.join("go/bin").display())), "{log}");
    }
}

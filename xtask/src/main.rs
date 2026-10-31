//! Build tasks.
//!
//! ```text
//! cargo xtask bundle [--release] [--target <triple>] [--sign <identity>] [--open | --run]
//! ```
//!
//! Chromium only runs from an app bundle: it finds its framework and the
//! helper apps it launches for renderer, GPU and utility processes through
//! the `.app` layout. `bundle` builds both executables and assembles
//!
//! ```text
//! Hangar.app/Contents/
//!   Info.plist
//!   MacOS/Hangar
//!   Resources/Hangar.icns                      (if packaging/macos has one)
//!   Frameworks/Chromium Embedded Framework.framework
//!   Frameworks/Hangar Helper.app               (and (GPU), (Renderer),
//!                                                (Plugin), (Alerts))
//! ```
//!
//! then signs it: ad hoc by default, or with `--sign "Developer ID
//! Application: …"` using the hardened runtime and the entitlements in
//! `packaging/macos`.

use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context as _, Result};

const APP_NAME: &str = "Hangar";
const BUNDLE_ID: &str = "dev.hangar.ide";
const MAIN_BIN: &str = "hangar";
const HELPER_BIN: &str = "hangar-helper";
const FRAMEWORK: &str = "Chromium Embedded Framework.framework";
const MIN_MACOS: &str = "12.0";

/// Helper apps Chromium expects: (name suffix, bundle id suffix, entitlements).
const HELPERS: &[(&str, &str, &str)] = &[
    ("", "helper", "Helper.entitlements"),
    (" (GPU)", "helper.gpu", "Helper-JIT.entitlements"),
    (" (Renderer)", "helper.renderer", "Helper-JIT.entitlements"),
    (" (Plugin)", "helper.plugin", "Helper-Plugin.entitlements"),
    (" (Alerts)", "helper.alerts", "Helper.entitlements"),
];

fn main() {
    if let Err(err) = run() {
        eprintln!("xtask: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("bundle") => bundle(BundleOptions::parse(args)?),
        Some("help") | Some("--help") | Some("-h") | None => {
            println!("{}", usage());
            Ok(())
        }
        Some(other) => bail!("unknown task `{other}`\n\n{}", usage()),
    }
}

fn usage() -> &'static str {
    "Usage: cargo xtask bundle [--release] [--target <triple>] [--sign <identity>] [--open | --run]\n\
     \n\
     \x20 --release        build with the release profile\n\
     \x20 --target <t>     build for a target triple (aarch64-apple-darwin, x86_64-apple-darwin)\n\
     \x20 --sign <id>      sign with a Developer ID identity and the hardened runtime\n\
     \x20 --open           open the app when done\n\
     \x20 --run            run the app's executable in this terminal (logs on stderr)"
}

#[derive(Debug, Default)]
struct BundleOptions {
    release: bool,
    target: Option<String>,
    identity: Option<String>,
    open: bool,
    run: bool,
    /// Arguments after `--`, passed to the app with `--run`.
    app_args: Vec<String>,
}

impl BundleOptions {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut options = Self::default();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--release" => options.release = true,
                "--target" => options.target = Some(args.next().context("--target needs a triple")?),
                "--sign" => options.identity = Some(args.next().context("--sign needs an identity")?),
                "--open" => options.open = true,
                "--run" => options.run = true,
                "--" => {
                    options.app_args = args.by_ref().collect();
                }
                other => bail!("unknown option `{other}`\n\n{}", usage()),
            }
        }
        if options.open && options.run {
            bail!("--open and --run are mutually exclusive");
        }
        Ok(options)
    }

    fn profile_dir(&self) -> &'static str {
        if self.release {
            "release"
        } else {
            "debug"
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf()
}

fn target_dir(root: &Path) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|dir| if dir.is_absolute() { dir } else { root.join(dir) })
        .unwrap_or_else(|| root.join("target"))
}

fn bundle(options: BundleOptions) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("bundling needs macOS (codesign, ditto and the macOS CEF distribution)");
    }
    let root = workspace_root();

    // 1. Build both executables.
    let mut cargo = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cargo
        .current_dir(&root)
        .args(["build", "--package", "hangar", "--bins"]);
    if options.release {
        cargo.arg("--release");
    }
    if let Some(target) = &options.target {
        cargo.args(["--target", target]);
    }
    run_command(&mut cargo, "cargo build")?;

    let mut out_dir = target_dir(&root);
    if let Some(target) = &options.target {
        out_dir.push(target);
    }
    out_dir.push(options.profile_dir());
    let main_bin = out_dir.join(MAIN_BIN);
    let helper_bin = out_dir.join(HELPER_BIN);

    // 2. Find the framework cef-dll-sys downloaded.
    let arch = match options.target.as_deref() {
        Some(target) if target.starts_with("x86_64") => "x86_64",
        Some(_) => "aarch64",
        None if cfg!(target_arch = "x86_64") => "x86_64",
        None => "aarch64",
    };
    let framework = find_framework(&root, arch)?;
    println!("Using {}", framework.display());

    // 3. Assemble the bundle.
    let app = out_dir.join("bundle").join(format!("{APP_NAME}.app"));
    if app.exists() {
        fs::remove_dir_all(&app).with_context(|| format!("removing the old {}", app.display()))?;
    }
    let contents = app.join("Contents");
    let frameworks = contents.join("Frameworks");
    let resources = contents.join("Resources");
    fs::create_dir_all(contents.join("MacOS"))?;
    fs::create_dir_all(&frameworks)?;
    fs::create_dir_all(&resources)?;

    let icon = root.join("packaging/macos").join(format!("{APP_NAME}.icns"));
    let has_icon = icon.exists();
    if has_icon {
        fs::copy(&icon, resources.join(format!("{APP_NAME}.icns")))?;
    }
    fs::write(contents.join("Info.plist"), main_info_plist(has_icon))?;
    copy_executable(&main_bin, &contents.join("MacOS").join(APP_NAME))?;

    // `ditto` keeps the framework's symlinks and extended attributes.
    run_command(
        Command::new("ditto").arg(&framework).arg(frameworks.join(FRAMEWORK)),
        "copying the Chromium framework",
    )?;

    for (suffix, id_suffix, _) in HELPERS {
        let name = format!("{APP_NAME} Helper{suffix}");
        let helper = frameworks.join(format!("{name}.app")).join("Contents");
        fs::create_dir_all(helper.join("MacOS"))?;
        fs::write(helper.join("Info.plist"), helper_info_plist(&name, id_suffix))?;
        copy_executable(&helper_bin, &helper.join("MacOS").join(&name))?;
    }

    // 4. Sign, innermost first.
    sign(&app, options.identity.as_deref(), &root.join("packaging/macos"))?;
    println!("Bundled {}", app.display());

    // 5. Launch.
    if options.open {
        run_command(Command::new("open").arg(&app), "open")?;
    } else if options.run {
        let status = Command::new(contents.join("MacOS").join(APP_NAME))
            .args(&options.app_args)
            .status()
            .context("starting the app")?;
        if !status.success() {
            bail!("{APP_NAME} exited with {status}");
        }
    }
    Ok(())
}

/// Looks for the framework under `$CEF_PATH` (set in `.cargo/config.toml`),
/// where cef-dll-sys unpacks `<version>/cef_macos_<arch>/`.
fn find_framework(root: &Path, arch: &str) -> Result<PathBuf> {
    let base = env::var_os("CEF_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".cef"));
    let wanted = format!("cef_macos_{arch}");
    let mut candidates = Vec::new();
    collect_frameworks(&base, 0, &mut candidates);
    candidates.sort();
    // Prefer the distribution for this architecture; take the newest version.
    candidates
        .iter()
        .rev()
        .find(|path| path.components().any(|c| c.as_os_str() == OsStr::new(&wanted)))
        .or_else(|| candidates.last())
        .cloned()
        .with_context(|| {
            format!(
                "{FRAMEWORK} not found under {}. The first `cargo build` on macOS downloads it \
                 there; if CEF_PATH points elsewhere, check that path.",
                base.display()
            )
        })
}

fn collect_frameworks(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name() == Some(OsStr::new(FRAMEWORK)) {
            found.push(path);
        } else {
            collect_frameworks(&path, depth + 1, found);
        }
    }
}

fn copy_executable(from: &Path, to: &Path) -> Result<()> {
    fs::copy(from, to).with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    Ok(())
}

fn sign(app: &Path, identity: Option<&str>, entitlements: &Path) -> Result<()> {
    let frameworks = app.join("Contents/Frameworks");
    let codesign = |path: &Path, entitlements_file: Option<&str>| -> Result<()> {
        let mut command = Command::new("codesign");
        command.args(["--force", "--sign", identity.unwrap_or("-")]);
        // The hardened runtime needs a real identity: with an ad hoc
        // signature, library validation would refuse to load the framework.
        if identity.is_some() {
            command.args(["--options", "runtime", "--timestamp"]);
            if let Some(file) = entitlements_file {
                command.arg("--entitlements").arg(entitlements.join(file));
            }
        }
        command.arg(path);
        run_command(&mut command, &format!("signing {}", path.display()))
    };

    codesign(&frameworks.join(FRAMEWORK), None)?;
    for (suffix, _, entitlements_file) in HELPERS {
        codesign(
            &frameworks.join(format!("{APP_NAME} Helper{suffix}.app")),
            Some(entitlements_file),
        )?;
    }
    codesign(app, Some("Hangar.entitlements"))
}

fn run_command(command: &mut Command, what: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("{what}: could not start {:?}", command.get_program()))?;
    if !status.success() {
        bail!("{what} failed ({status})");
    }
    Ok(())
}

// --- Info.plist ----------------------------------------------------------------

enum Value<'a> {
    Str(&'a str),
    Bool(bool),
    Array(Vec<Value<'a>>),
    Dict(Vec<(&'a str, Value<'a>)>),
}

fn plist(entries: Vec<(&str, Value)>) -> String {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }
    fn write(out: &mut String, value: &Value, indent: usize) {
        let pad = "\t".repeat(indent);
        match value {
            Value::Str(text) => out.push_str(&format!("{pad}<string>{}</string>\n", escape(text))),
            Value::Bool(true) => out.push_str(&format!("{pad}<true/>\n")),
            Value::Bool(false) => out.push_str(&format!("{pad}<false/>\n")),
            Value::Array(items) => {
                out.push_str(&format!("{pad}<array>\n"));
                for item in items {
                    write(out, item, indent + 1);
                }
                out.push_str(&format!("{pad}</array>\n"));
            }
            Value::Dict(entries) => {
                out.push_str(&format!("{pad}<dict>\n"));
                for (key, value) in entries {
                    out.push_str(&format!("{pad}\t<key>{}</key>\n", escape(key)));
                    write(out, value, indent + 1);
                }
                out.push_str(&format!("{pad}</dict>\n"));
            }
        }
    }
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n",
    );
    write(&mut out, &Value::Dict(entries), 0);
    out.push_str("</plist>\n");
    out
}

fn common_keys<'a>(name: &'a str, identifier: &'a str) -> Vec<(&'a str, Value<'a>)> {
    vec![
        ("CFBundleDevelopmentRegion", Value::Str("en")),
        ("CFBundleExecutable", Value::Str(name)),
        ("CFBundleIdentifier", Value::Str(identifier)),
        ("CFBundleInfoDictionaryVersion", Value::Str("6.0")),
        ("CFBundleName", Value::Str(name)),
        ("CFBundlePackageType", Value::Str("APPL")),
        ("CFBundleShortVersionString", Value::Str(env!("CARGO_PKG_VERSION"))),
        ("CFBundleVersion", Value::Str(env!("CARGO_PKG_VERSION"))),
        ("LSMinimumSystemVersion", Value::Str(MIN_MACOS)),
        // Chromium's allocator does not get along with the nano zone.
        ("LSEnvironment", Value::Dict(vec![("MallocNanoZone", Value::Str("0"))])),
        ("NSSupportsAutomaticGraphicsSwitching", Value::Bool(true)),
    ]
}

fn main_info_plist(has_icon: bool) -> String {
    let mut keys = common_keys(APP_NAME, BUNDLE_ID);
    keys.extend([
        ("CFBundleDisplayName", Value::Str(APP_NAME)),
        ("LSApplicationCategoryType", Value::Str("public.app-category.developer-tools")),
        ("NSHighResolutionCapable", Value::Bool(true)),
        ("NSCameraUsageDescription", Value::Str("A web page in Hangar's browser wants to use the camera.")),
        (
            "NSMicrophoneUsageDescription",
            Value::Str("A web page in Hangar's browser wants to use the microphone."),
        ),
        // Folders can be dropped on the Dock icon or opened with
        // `open -a Hangar <folder>`.
        (
            "CFBundleDocumentTypes",
            Value::Array(vec![Value::Dict(vec![
                ("CFBundleTypeName", Value::Str("Folder")),
                ("CFBundleTypeRole", Value::Str("Editor")),
                ("LSHandlerRank", Value::Str("Alternate")),
                ("LSItemContentTypes", Value::Array(vec![Value::Str("public.folder")])),
            ])]),
        ),
    ]);
    if has_icon {
        keys.push(("CFBundleIconFile", Value::Str(APP_NAME)));
    }
    plist(keys)
}

fn helper_info_plist(name: &str, id_suffix: &str) -> String {
    let identifier = format!("{BUNDLE_ID}.{id_suffix}");
    let mut keys = common_keys(name, &identifier);
    // Helpers have no Dock icon or menu bar.
    keys.push(("LSUIElement", Value::Str("1")));
    plist(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plists_are_well_formed_enough() {
        let main = main_info_plist(false);
        assert!(main.contains("<key>CFBundleExecutable</key>\n\t<string>Hangar</string>"));
        assert!(!main.contains("CFBundleIconFile"));
        assert_eq!(main.matches("<dict>").count(), main.matches("</dict>").count());
        assert!(main.contains("<array>\n\t\t<dict>"));
        assert!(main.contains("<string>public.folder</string>"));

        let helper = helper_info_plist("Hangar Helper (GPU)", "helper.gpu");
        assert!(helper.contains("<string>dev.hangar.ide.helper.gpu</string>"));
        assert!(helper.contains("<key>LSUIElement</key>"));
    }

    #[test]
    fn helper_names_match_what_chromium_looks_for() {
        let names: Vec<String> = HELPERS.iter().map(|(s, _, _)| format!("{APP_NAME} Helper{s}")).collect();
        assert!(names.contains(&"Hangar Helper (Renderer)".to_string()));
        assert!(names.contains(&"Hangar Helper".to_string()));
    }

    #[test]
    fn options_parse() {
        let args = ["--release", "--sign", "Developer ID", "--run", "--", "/tmp/x"].map(String::from);
        let options = BundleOptions::parse(args.into_iter()).unwrap();
        assert!(options.release && options.run && !options.open);
        assert_eq!(options.identity.as_deref(), Some("Developer ID"));
        assert_eq!(options.app_args, vec!["/tmp/x".to_string()]);
        assert!(BundleOptions::parse(["--open", "--run"].map(String::from).into_iter()).is_err());
    }

    #[test]
    fn framework_search_prefers_the_matching_arch() {
        let dir = std::env::temp_dir().join(format!("xtask-cef-{}", std::process::id()));
        for arch in ["cef_macos_aarch64", "cef_macos_x86_64"] {
            fs::create_dir_all(dir.join("154.0.34").join(arch).join(FRAMEWORK)).unwrap();
        }
        // SAFETY: tests in this module do not read CEF_PATH concurrently.
        unsafe { env::set_var("CEF_PATH", &dir) };
        let found = find_framework(Path::new("/nonexistent"), "x86_64").unwrap();
        assert!(found.to_string_lossy().contains("cef_macos_x86_64"));
        let found = find_framework(Path::new("/nonexistent"), "aarch64").unwrap();
        assert!(found.to_string_lossy().contains("cef_macos_aarch64"));
        fs::remove_dir_all(&dir).ok();
    }
}

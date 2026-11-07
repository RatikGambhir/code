//! The GitHub token: in the macOS Keychain, or an owner-only file elsewhere
//! (the UI also runs on Linux for testing). Never logged.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

use anyhow::Result;

/// Held while saving or removing the token. A sign-out sets the sign-in's
/// cancel flag and then removes the token; a sign-in checks the flag and
/// saves under this lock, so it either sees the cancel or saves before the
/// removal. Either way, no token is left behind.
static STORE: Mutex<()> = Mutex::new(());

pub fn read() -> Result<Option<String>> {
    Ok(backend::read()?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty()))
}

/// Saves the token unless `cancelled` is set. Returns whether it was saved.
pub fn write_unless_cancelled(token: &str, cancelled: &AtomicBool) -> Result<bool> {
    let _store = STORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if cancelled.load(Ordering::SeqCst) {
        return Ok(false);
    }
    backend::write(token.trim().as_bytes())?;
    Ok(true)
}

pub fn delete() -> Result<()> {
    let _store = STORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    backend::delete()
}

#[cfg(target_os = "macos")]
mod backend {
    use anyhow::{Context, Result};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    const ITEM_NOT_FOUND: i32 = -25300;
    const ACCOUNT: &str = "github-token";

    fn service() -> String {
        format!("{} GitHub", cockpit_common::APP_IDENTIFIER)
    }

    pub fn read() -> Result<Option<Vec<u8>>> {
        match get_generic_password(&service(), ACCOUNT) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.code() == ITEM_NOT_FOUND => Ok(None),
            Err(err) => Err(err).context("reading the GitHub token from the Keychain"),
        }
    }

    pub fn write(bytes: &[u8]) -> Result<()> {
        set_generic_password(&service(), ACCOUNT, bytes)
            .context("saving the GitHub token to the Keychain")
    }

    pub fn delete() -> Result<()> {
        match delete_generic_password(&service(), ACCOUNT) {
            Ok(()) => Ok(()),
            Err(err) if err.code() == ITEM_NOT_FOUND => Ok(()),
            Err(err) => Err(err).context("removing the GitHub token from the Keychain"),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod backend {
    use std::io::Write;

    use anyhow::{Context, Result};

    fn path() -> Result<std::path::PathBuf> {
        Ok(cockpit_common::paths::AppPaths::resolve()?
            .root()
            .join("github-token"))
    }

    pub fn read() -> Result<Option<Vec<u8>>> {
        match std::fs::read(path()?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).context("reading the GitHub token"),
        }
    }

    pub fn write(bytes: &[u8]) -> Result<()> {
        let path = path()?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&path).context("saving the GitHub token")?;
        file.write_all(bytes)?;
        file.sync_all().context("saving the GitHub token")
    }

    pub fn delete() -> Result<()> {
        match std::fs::remove_file(path()?) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(err).context("removing the GitHub token")
            }
            _ => Ok(()),
        }
    }
}

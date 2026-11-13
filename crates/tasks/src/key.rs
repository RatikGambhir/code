//! The Linear sign-in: a personal API key or browser sign-in tokens, in the
//! macOS Keychain, or an owner-only file elsewhere (the UI also runs on
//! Linux for testing). Never logged.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, MutexGuard,
};

use anyhow::Result;

use crate::oauth::{self, Tokens};

/// How Hangar is signed in to Linear.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    ApiKey(String),
    OAuth(Tokens),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret stays out of logs.
        f.write_str(match self {
            Self::ApiKey(_) => "Credential::ApiKey(..)",
            Self::OAuth(_) => "Credential::OAuth(..)",
        })
    }
}

impl Credential {
    pub fn allows_writes(&self) -> bool {
        match self {
            Self::ApiKey(_) => true,
            Self::OAuth(tokens) => tokens.write_authorized,
        }
    }

    /// The `Authorization` header value: personal keys go in as they are,
    /// OAuth tokens as bearer tokens.
    pub fn header(&self) -> String {
        match self {
            Self::ApiKey(key) => key.trim().to_string(),
            Self::OAuth(tokens) => format!("Bearer {}", tokens.access_token),
        }
    }
}

/// The saved sign-in. Browser tokens are stored as JSON, keys as they are.
pub fn read_credential() -> Result<Option<Credential>> {
    Ok(read()?.map(|text| match serde_json::from_str::<Tokens>(&text) {
        Ok(tokens) => Credential::OAuth(tokens),
        Err(_) => Credential::ApiKey(text),
    }))
}

/// Held while saving or removing the sign-in. A disconnect sets the
/// sign-in's cancel flag and then removes the sign-in; a sign-in checks the
/// flag and saves under this lock, so it either sees the cancel or saves
/// before the removal. Either way, nothing is left behind.
static STORE: Mutex<()> = Mutex::new(());

/// Held while renewing browser tokens, so two windows don't both spend the
/// same refresh token at once.
static REFRESH: Mutex<()> = Mutex::new(());

fn lock(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Saves the sign-in unless `cancelled` is set. Returns whether it was saved.
pub fn write_credential_unless_cancelled(credential: &Credential, cancelled: &AtomicBool) -> Result<bool> {
    let text = encode(credential)?;
    let _store = lock(&STORE);
    if cancelled.load(Ordering::SeqCst) {
        return Ok(false);
    }
    write(&text)?;
    Ok(true)
}

fn encode(credential: &Credential) -> Result<String> {
    Ok(match credential {
        Credential::ApiKey(key) => key.clone(),
        Credential::OAuth(tokens) => serde_json::to_string(tokens)?,
    })
}

/// The saved sign-in, with browser tokens renewed (and saved) first when
/// they are about to lapse.
pub fn usable_credential() -> Result<Option<Credential>> {
    let _refresh = lock(&REFRESH);
    // Read after taking the lock: another window may have just renewed.
    let tokens = match read_credential()? {
        Some(Credential::OAuth(tokens)) => tokens,
        other => return Ok(other),
    };
    let Some(renewed) = oauth::fresh(&tokens)? else {
        return Ok(Some(Credential::OAuth(tokens)));
    };
    let renewed = Credential::OAuth(renewed);
    let _store = lock(&STORE);
    // Save only over the tokens that were renewed. If Linear was
    // disconnected meanwhile, don't bring the sign-in back.
    match read_credential()? {
        Some(Credential::OAuth(current)) if current == tokens => {
            write(&encode(&renewed)?)?;
            Ok(Some(renewed))
        }
        Some(_) => Ok(Some(renewed)),
        None => anyhow::bail!("Linear is not connected"),
    }
}

pub fn read() -> Result<Option<String>> {
    Ok(backend::read()?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|k| !k.trim().is_empty()))
}

fn write(key: &str) -> Result<()> {
    backend::write(key.trim().as_bytes())
}

pub fn delete() -> Result<()> {
    let _store = lock(&STORE);
    backend::delete()
}

#[cfg(target_os = "macos")]
mod backend {
    use anyhow::{Context, Result};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    const ITEM_NOT_FOUND: i32 = -25300;
    const ACCOUNT: &str = "linear-api-key";

    fn service() -> String {
        format!("{} Linear", cockpit_common::APP_IDENTIFIER)
    }

    pub fn read() -> Result<Option<Vec<u8>>> {
        match get_generic_password(&service(), ACCOUNT) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.code() == ITEM_NOT_FOUND => Ok(None),
            Err(err) => Err(err).context("reading the Linear key from the Keychain"),
        }
    }

    pub fn write(bytes: &[u8]) -> Result<()> {
        set_generic_password(&service(), ACCOUNT, bytes)
            .context("saving the Linear key to the Keychain")
    }

    pub fn delete() -> Result<()> {
        match delete_generic_password(&service(), ACCOUNT) {
            Ok(()) => Ok(()),
            Err(err) if err.code() == ITEM_NOT_FOUND => Ok(()),
            Err(err) => Err(err).context("removing the Linear key from the Keychain"),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod backend {
    use std::io::Write;

    use anyhow::{Context, Result};

    fn path() -> Result<std::path::PathBuf> {
        Ok(crate::data_dir()?.join("linear-key"))
    }

    pub fn read() -> Result<Option<Vec<u8>>> {
        match std::fs::read(path()?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).context("reading the Linear key"),
        }
    }

    pub fn write(bytes: &[u8]) -> Result<()> {
        let path = path()?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&path).context("saving the Linear key")?;
        file.write_all(bytes)?;
        file.sync_all().context("saving the Linear key")
    }

    pub fn delete() -> Result<()> {
        match std::fs::remove_file(path()?) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(err).context("removing the Linear key")
            }
            _ => Ok(()),
        }
    }
}

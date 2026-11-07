//! The Linear sign-in: a personal API key or browser sign-in tokens, in the
//! macOS Keychain, or an owner-only file elsewhere (the UI also runs on
//! Linux for testing). Never logged.

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

pub fn write_credential(credential: &Credential) -> Result<()> {
    match credential {
        Credential::ApiKey(key) => write(key),
        Credential::OAuth(tokens) => write(&serde_json::to_string(tokens)?),
    }
}

/// The saved sign-in, with browser tokens renewed (and saved) first when
/// they are about to lapse.
pub fn usable_credential() -> Result<Option<Credential>> {
    match read_credential()? {
        Some(Credential::OAuth(tokens)) => match oauth::fresh(&tokens)? {
            Some(renewed) => {
                let credential = Credential::OAuth(renewed);
                write_credential(&credential)?;
                Ok(Some(credential))
            }
            None => Ok(Some(Credential::OAuth(tokens))),
        },
        other => Ok(other),
    }
}

pub fn read() -> Result<Option<String>> {
    Ok(backend::read()?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|k| !k.trim().is_empty()))
}

pub fn write(key: &str) -> Result<()> {
    backend::write(key.trim().as_bytes())
}

pub fn delete() -> Result<()> {
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

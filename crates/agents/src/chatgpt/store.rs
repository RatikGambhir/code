//! Where Hangar keeps its ChatGPT sign-in: one record in the macOS
//! Keychain (a 0600 file elsewhere), guarded by a file lock so the app and
//! the token helper Codex runs never race a rotating refresh token.

use std::{fs::File, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Everything Hangar remembers about its ChatGPT sign-in.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Record {
    /// This machine's opaque id (`urn:uuid:…`), made once and kept.
    pub host_id: String,
    /// The client id OpenAI issued at the first sign-in; reused for every
    /// later sign-in, refresh and revocation. Kept after signing out.
    pub client_id: Option<String>,
    /// The validated ID token's `sub`.
    pub subject: Option<String>,
    pub email: Option<String>,
    /// Present while signed in.
    pub tokens: Option<Tokens>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: i64,
    pub scopes: Vec<String>,
}

/// An exclusive hold on the record. Load, change and save under it.
pub struct Locked {
    _file: File,
}

pub fn lock() -> Result<Locked> {
    let path = data_dir()?.join("chatgpt-sign-in.lock");
    let file = File::create(&path).with_context(|| format!("opening {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a file descriptor we own; released when the file closes.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("locking the ChatGPT sign-in");
        }
    }
    Ok(Locked { _file: file })
}

impl Locked {
    /// The saved record, or a fresh one with a new host id.
    pub fn load(&self) -> Result<Record> {
        match backend::read()? {
            Some(bytes) => serde_json::from_slice(&bytes).context("reading the saved ChatGPT sign-in"),
            None => Ok(Record {
                host_id: new_host_id()?,
                ..Record::default()
            }),
        }
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        backend::write(&serde_json::to_vec(record)?)
    }
}

pub(crate) fn data_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("HANGAR_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::data_dir()
            .context("no platform data directory")?
            .join(cockpit_common::APP_NAME),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// A random version 4 UUID as a URN, the host id format OpenAI accepts.
fn new_host_id() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(target_os = "macos")]
mod backend {
    use anyhow::{Context, Result};
    use security_framework::passwords::{get_generic_password, set_generic_password};

    const ITEM_NOT_FOUND: i32 = -25300;
    const ACCOUNT: &str = "chatgpt-sign-in";

    fn service() -> String {
        format!("{} ChatGPT sign-in", cockpit_common::APP_IDENTIFIER)
    }

    pub fn read() -> Result<Option<Vec<u8>>> {
        match get_generic_password(&service(), ACCOUNT) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.code() == ITEM_NOT_FOUND => Ok(None),
            Err(err) => Err(err).context("reading the ChatGPT sign-in from the Keychain"),
        }
    }

    pub fn write(bytes: &[u8]) -> Result<()> {
        set_generic_password(&service(), ACCOUNT, bytes).context("saving the ChatGPT sign-in to the Keychain")
    }
}

/// Off macOS (the UI runs on Linux for testing): an owner-only file.
#[cfg(not(target_os = "macos"))]
mod backend {
    use std::io::Write;

    use anyhow::{Context, Result};

    fn path() -> Result<std::path::PathBuf> {
        Ok(super::data_dir()?.join("chatgpt-sign-in.json"))
    }

    pub fn read() -> Result<Option<Vec<u8>>> {
        match std::fs::read(path()?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).context("reading the ChatGPT sign-in"),
        }
    }

    pub fn write(bytes: &[u8]) -> Result<()> {
        let path = path()?;
        let tmp = path.with_extension("json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp).context("saving the ChatGPT sign-in")?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path).context("saving the ChatGPT sign-in")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_ids_are_v4_uuid_urns() {
        let id = new_host_id().unwrap();
        assert!(id.starts_with("urn:uuid:"));
        let uuid = &id["urn:uuid:".len()..];
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, new_host_id().unwrap());
    }
}

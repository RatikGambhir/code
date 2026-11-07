//! Signing in to GitHub.
//!
//! - The OAuth device flow: GitHub shows a one-time code, the user enters it
//!   at github.com/login/device in their browser, and the app polls until the
//!   token is granted. It needs the client ID of a GitHub OAuth App with
//!   device flow enabled (client IDs are public; no secret is involved).
//! - The GitHub CLI's login (`gh auth token`), when `gh` is installed.
//! - A pasted personal access token.

use std::{
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{bail, Context as _, Result};
use serde::Deserialize;

/// Scopes asked for: private repositories (code, pull requests, issues,
/// Actions) and organization membership, for org repositories.
pub const SCOPES: &str = "repo read:org";

/// Where a personal access token with the right scopes is made.
pub const NEW_TOKEN_URL: &str =
    "https://github.com/settings/tokens/new?scopes=repo,read:org&description=Hangar";

const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Hangar's GitHub OAuth App (device flow enabled; client IDs are public).
const DEFAULT_CLIENT_ID: &str = "Ov23lizKR5RNq9GvH0nG";

/// The OAuth App's client ID: `HANGAR_GITHUB_CLIENT_ID` at run time or
/// build time, else Hangar's own.
pub fn client_id() -> Option<String> {
    std::env::var("HANGAR_GITHUB_CLIENT_ID")
        .ok()
        .or_else(|| option_env!("HANGAR_GITHUB_CLIENT_ID").map(str::to_string))
        .or_else(|| Some(DEFAULT_CLIENT_ID.to_string()))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// What the user needs to finish signing in in the browser.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// Seconds until the code lapses.
    pub expires_in: u64,
    /// Seconds to wait between polls.
    pub interval: u64,
}

/// Asks GitHub for a one-time code to enter in the browser.
pub fn start_device_flow(client_id: &str) -> Result<DeviceCode> {
    let mut response = agent()
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .send_form([("client_id", client_id), ("scope", SCOPES)])
        .context("contacting GitHub")?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    if let Some(message) = oauth_error(&text) {
        bail!("GitHub: {message}");
    }
    if !(200..300).contains(&status) {
        bail!("GitHub answered HTTP {status} when starting sign-in");
    }
    serde_json::from_str(&text).context("reading GitHub's sign-in code")
}

/// Polls until the user approves the code, it lapses, or `cancelled` is set.
pub fn finish_device_flow(
    client_id: &str,
    code: &DeviceCode,
    cancelled: &AtomicBool,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Answer {
        access_token: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
        interval: Option<u64>,
    }

    let deadline = Instant::now() + Duration::from_secs(code.expires_in.max(60));
    let mut interval = Duration::from_secs(code.interval.max(5));
    loop {
        // Sleep in short steps so a cancel lands quickly.
        let wake = Instant::now() + interval;
        while Instant::now() < wake {
            if cancelled.load(Ordering::Relaxed) {
                bail!("Sign-in canceled");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if Instant::now() >= deadline {
            bail!("The sign-in code expired. Start again.");
        }
        let mut response = agent()
            .post(ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .send_form([
                ("client_id", client_id),
                ("device_code", code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .context("contacting GitHub")?;
        let text = response.body_mut().read_to_string().unwrap_or_default();
        let answer: Answer = serde_json::from_str(&text).context("reading GitHub's answer")?;
        if let Some(token) = answer.access_token.filter(|t| !t.is_empty()) {
            return Ok(token);
        }
        match answer.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval = Duration::from_secs(answer.interval.unwrap_or(interval.as_secs() + 5));
            }
            Some("expired_token") => bail!("The sign-in code expired. Start again."),
            Some("access_denied") => bail!("Sign-in was declined on GitHub."),
            Some(other) => bail!(
                "GitHub: {}",
                answer.error_description.as_deref().unwrap_or(other)
            ),
            None => bail!("GitHub sent no token"),
        }
    }
}

/// The GitHub CLI, if it is installed.
pub fn gh_cli() -> Option<std::path::PathBuf> {
    cockpit_common::shell_env::find_executable(
        "gh",
        &cockpit_common::shell_env::common_tool_dirs(),
    )
}

/// The token the GitHub CLI is signed in with.
pub fn gh_cli_token() -> Result<String> {
    let gh = gh_cli().context("The GitHub CLI (gh) is not installed")?;
    let output = Command::new(gh)
        .args(["auth", "token", "--hostname", "github.com"])
        .output()
        .context("running gh auth token")?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        bail!("The GitHub CLI is not signed in. Run `gh auth login` first, or use another option.");
    }
    Ok(token)
}

fn oauth_error(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let error = value["error"].as_str()?;
    Some(
        value["error_description"]
            .as_str()
            .map_or_else(|| error.to_string(), str::to_string),
    )
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(format!(
            "{}/{}",
            cockpit_common::APP_NAME,
            cockpit_common::APP_VERSION
        ))
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_errors_prefer_the_description() {
        assert_eq!(
            oauth_error(r#"{"error":"unauthorized_client","error_description":"Device flow is off"}"#)
                .as_deref(),
            Some("Device flow is off")
        );
        assert_eq!(oauth_error(r#"{"error":"x"}"#).as_deref(), Some("x"));
        assert_eq!(oauth_error(r#"{"device_code":"d"}"#), None);
    }
}

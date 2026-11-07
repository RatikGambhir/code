//! Signing in to Linear in the browser: OAuth 2.0 with PKCE, back to a
//! listener on localhost. PKCE means the app holds no client secret.
//!
//! Access tokens last a day; [`fresh`] renews one from its refresh token
//! (good for 30 days) before it lapses.

use std::{
    collections::HashMap,
    io::{BufRead as _, BufReader, Write as _},
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const AUTHORIZE_URL: &str = "https://linear.app/oauth/authorize";
const TOKEN_URL: &str = "https://api.linear.app/oauth/token";
const REVOKE_URL: &str = "https://api.linear.app/oauth/revoke";
const SCOPES: &str = "read";
/// The port and path registered as the OAuth app's callback URL. The port
/// must match exactly, so it is fixed.
pub const CALLBACK_PORT: u16 = 47_621;
const CALLBACK_PATH: &str = "/linear/callback";
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Renew a token this long before it lapses.
const REFRESH_MARGIN_SECS: i64 = 10 * 60;

/// Hangar's Linear OAuth app. `HANGAR_LINEAR_CLIENT_ID` overrides it, at
/// run time or build time.
const DEFAULT_CLIENT_ID: Option<&str> = Some("7d554baedbff6e3ef29ef1408fcb5981");

pub fn client_id() -> Option<String> {
    std::env::var("HANGAR_LINEAR_CLIENT_ID")
        .ok()
        .or_else(|| option_env!("HANGAR_LINEAR_CLIENT_ID").map(str::to_string))
        .or_else(|| DEFAULT_CLIENT_ID.map(str::to_string))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

pub fn redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

/// An OAuth sign-in, as kept in the Keychain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds since the epoch; 0 when Linear gave no lifetime.
    #[serde(default)]
    pub expires_at: i64,
    pub client_id: String,
}

impl Tokens {
    fn needs_refresh(&self, now: i64) -> bool {
        self.expires_at > 0 && now + REFRESH_MARGIN_SECS >= self.expires_at
    }
}

/// A sign-in waiting for the browser.
pub struct PendingSignIn {
    listener: TcpListener,
    /// Browsers may resolve `localhost` to ::1 first.
    listener_v6: Option<TcpListener>,
    auth_url: String,
    state: String,
    verifier: String,
    client_id: String,
}

/// Starts listening for the callback and builds the page to open.
pub fn begin_sign_in(client_id: &str) -> Result<PendingSignIn> {
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).with_context(|| {
        format!("port {CALLBACK_PORT} is in use, so the Linear sign-in can't listen for its answer")
    })?;
    listener.set_nonblocking(true)?;
    let listener_v6 = TcpListener::bind(("::1", CALLBACK_PORT)).ok();
    if let Some(l) = &listener_v6 {
        l.set_nonblocking(true)?;
    }
    let (state, verifier) = (random_token()?, random_token()?);
    let challenge = base64_url(&Sha256::digest(verifier.as_bytes()));
    let query = [
        ("client_id", client_id),
        ("redirect_uri", &redirect_uri()),
        ("response_type", "code"),
        ("scope", SCOPES),
        ("state", &state),
        ("prompt", "consent"),
        ("code_challenge_method", "S256"),
        ("code_challenge", &challenge),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", encode(v)))
    .collect::<Vec<_>>()
    .join("&");
    Ok(PendingSignIn {
        listener,
        listener_v6,
        auth_url: format!("{AUTHORIZE_URL}?{query}"),
        state,
        verifier,
        client_id: client_id.to_string(),
    })
}

impl PendingSignIn {
    pub fn auth_url(&self) -> &str {
        &self.auth_url
    }

    /// Waits for the browser (until `cancel` is set or 15 minutes pass) and
    /// exchanges the code for tokens. Blocks.
    pub fn finish(self, cancel: &AtomicBool) -> Result<Tokens> {
        let deadline = Instant::now() + SIGN_IN_TIMEOUT;
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!("Sign-in canceled");
            }
            if Instant::now() > deadline {
                bail!("The sign-in timed out. Try again.");
            }
            let accepted = std::iter::once(&self.listener)
                .chain(self.listener_v6.as_ref())
                .find_map(|l| match l.accept() {
                    Ok((stream, _)) => Some(Ok(stream)),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => None,
                    Err(err) => Some(Err(err)),
                });
            let mut stream = match accepted {
                None => {
                    std::thread::sleep(Duration::from_millis(150));
                    continue;
                }
                Some(stream) => stream.context("waiting for the sign-in callback")?,
            };
            let Some(params) = read_callback(&mut stream) else {
                respond(&mut stream, "404 Not Found", "Not found", "");
                continue;
            };
            // Requests that are not this attempt's callback are ignored.
            if params.get("state") != Some(&self.state) {
                respond(&mut stream, "400 Bad Request", "Sign-in link expired", "Start the sign-in again from Hangar.");
                continue;
            }
            let result = self.complete(&params);
            match &result {
                Ok(_) => respond(
                    &mut stream,
                    "200 OK",
                    "Linear is connected",
                    "You can close this tab and go back to Hangar.",
                ),
                Err(err) => respond(&mut stream, "200 OK", "Sign-in didn't finish", &format!("{err:#}")),
            }
            return result;
        }
    }

    fn complete(&self, params: &HashMap<String, String>) -> Result<Tokens> {
        if let Some(error) = params.get("error") {
            if error == "access_denied" {
                bail!("Linear access was declined.");
            }
            let detail = params.get("error_description").unwrap_or(error);
            bail!("Linear: {detail}");
        }
        let code = params.get("code").ok_or_else(|| anyhow!("Linear sent no code"))?;
        token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri()),
            ("client_id", &self.client_id),
            ("code_verifier", &self.verifier),
        ], &self.client_id, None)
    }
}

/// The tokens, renewed first when they are about to lapse. Returns whether
/// they changed, so the caller can save them.
pub fn fresh(tokens: &Tokens) -> Result<Option<Tokens>> {
    if !tokens.needs_refresh(now()) {
        return Ok(None);
    }
    let refresh = tokens
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("The Linear sign-in expired. Connect again."))?;
    token_request(
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", &tokens.client_id),
        ],
        &tokens.client_id,
        Some(refresh),
    )
    .map(Some)
}

/// Best effort: tells Linear the token is no longer used.
pub fn revoke(tokens: &Tokens) {
    if let Err(err) = post_form(REVOKE_URL, &[("token", tokens.access_token.as_str())]) {
        tracing::debug!("revoking the Linear token: {err:#}");
    }
}

fn token_request(fields: &[(&str, &str)], client_id: &str, old_refresh: Option<&str>) -> Result<Tokens> {
    #[derive(Deserialize)]
    struct Answer {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_in: Option<i64>,
    }
    let (status, body) = post_form(TOKEN_URL, fields)?;
    if !(200..300).contains(&status) {
        let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let message = value["error_description"]
            .as_str()
            .or(value["error"].as_str())
            .unwrap_or("no details");
        if old_refresh.is_some() && matches!(value["error"].as_str(), Some("invalid_grant")) {
            bail!("The Linear sign-in expired. Connect again.");
        }
        bail!("Linear refused the sign-in (HTTP {status}): {message}");
    }
    let answer: Answer = serde_json::from_str(&body).context("reading Linear's tokens")?;
    Ok(Tokens {
        access_token: answer.access_token,
        refresh_token: answer.refresh_token.or_else(|| old_refresh.map(str::to_string)),
        expires_at: answer.expires_in.map_or(0, |secs| now() + secs),
        client_id: client_id.to_string(),
    })
}

fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<(u16, String)> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(format!("{}/{}", cockpit_common::APP_NAME, cockpit_common::APP_VERSION))
        .build()
        .into();
    let mut response = agent
        .post(url)
        .send_form(fields.iter().copied())
        .context("contacting Linear")?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();
    Ok((status, body))
}

/// The query of a `GET /linear/callback` request, or `None` for anything else.
fn read_callback(stream: &mut TcpStream) -> Option<HashMap<String, String>> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut line = String::new();
    BufReader::new(&*stream).read_line(&mut line).ok()?;
    let target = line.strip_prefix("GET ")?.split(' ').next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    (path == CALLBACK_PATH).then(|| parse_query(query))
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Some((decode(k)?, decode(v)?))
        })
        .collect()
}

fn respond(stream: &mut TcpStream, status: &str, title: &str, message: &str) {
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{t}</title>\
         <body style=\"font:15px -apple-system,system-ui,sans-serif;max-width:32em;margin:15vh auto;padding:0 16px;color-scheme:light dark\">\
         <h1 style=\"font-size:20px\">{t}</h1><p>{m}</p></body>",
        t = escape(title),
        m = escape(message),
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("no randomness: {e}"))?;
    Ok(base64_url(&bytes))
}

/// Unpadded base64url, as PKCE wants it.
fn base64_url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_7636() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64_url(&Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn queries_round_trip() {
        let params = parse_query(&format!("code={}&state=a%2Bb&x", encode("c d/é")));
        assert_eq!(params["code"], "c d/é");
        assert_eq!(params["state"], "a+b");
        assert_eq!(params["x"], "");
    }

    #[test]
    fn tokens_refresh_before_they_lapse() {
        let tokens = |expires_at| Tokens {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_at,
            client_id: "c".into(),
        };
        assert!(tokens(1_000).needs_refresh(1_000 - 60));
        assert!(!tokens(100_000).needs_refresh(1_000));
        assert!(!tokens(0).needs_refresh(1_000));
    }

    #[test]
    fn callback_requests_are_parsed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            write!(s, "GET /linear/callback?code=abc&state=xyz HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut reply = String::new();
            std::io::Read::read_to_string(&mut s, &mut reply).ok();
            reply
        });
        let (mut stream, _) = listener.accept().unwrap();
        let params = read_callback(&mut stream).unwrap();
        respond(&mut stream, "200 OK", "Done", "ok");
        drop(stream);
        assert_eq!((params["code"].as_str(), params["state"].as_str()), ("abc", "xyz"));
        assert!(client.join().unwrap().starts_with("HTTP/1.1 200 OK"));
    }
}

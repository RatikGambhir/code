//! Sign in with ChatGPT, run by Hangar itself.
//!
//! OpenAI's flow for open-source and local apps
//! (developers.openai.com/siwc/token-sharing-open-source): an OAuth code
//! flow with PKCE in the system browser, back to a listener on 127.0.0.1.
//! The first sign-in registers Hangar (`dynamic_agent_client`) and OpenAI
//! issues a client id that every later request reuses. Tokens live in the
//! Keychain ([`store`]).
//!
//! Codex uses the sign-in through a custom model provider whose bearer token
//! comes from a command: Hangar's own executable run with
//! [`TOKEN_HELPER_FLAG`], which prints a fresh access token (refreshing it
//! when it is about to expire). See [`codex_overrides`].

pub mod chat;
mod store;

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use base64::{
    alphabet,
    engine::{general_purpose::URL_SAFE_NO_PAD, DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
    Engine,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const REVOKE_URL: &str = "https://auth.openai.com/api/accounts/oauth/revoke";
/// The API the tokens are for, and where Codex sends requests.
pub const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The permission to bill requests to the user's ChatGPT plan.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// Only for the first sign-in: OpenAI answers with the client id to keep.
const REGISTRATION_CLIENT_ID: &str = "dynamic_agent_client";
const CALLBACK_PATH: &str = "/auth/callback";
const PREFERRED_PORT: u16 = 1455;
/// How long the browser has to come back.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Refresh when the access token has less than this left.
const REFRESH_MARGIN_SECS: i64 = 5 * 60;

/// Run Hangar's executable with this argument to print an access token.
pub const TOKEN_HELPER_FLAG: &str = "--chatgpt-access-token";
/// The Codex model provider id Hangar configures.
pub const CODEX_PROVIDER_ID: &str = "hangar_chatgpt";

/// The ChatGPT account Hangar is signed in to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountInfo {
    pub email: Option<String>,
}

/// The saved sign-in, if any. Reads the Keychain (which may ask the user
/// the first time a new build reads it), so call it off the UI thread.
pub fn signed_in_account() -> Result<Option<AccountInfo>> {
    let lock = store::lock()?;
    let record = lock.load()?;
    Ok(record.tokens.is_some().then_some(AccountInfo { email: record.email }))
}

/// `-c` overrides that make Codex use Hangar's sign-in: requests go to the
/// OpenAI API with a bearer token printed by `helper` (Hangar's executable).
pub fn codex_overrides(helper: &Path) -> Vec<(String, String)> {
    let key = |k: &str| format!("model_providers.{CODEX_PROVIDER_ID}.{k}");
    let toml_str = |s: &str| serde_json::to_string(s).unwrap_or_default();
    vec![
        ("model_provider".into(), toml_str(CODEX_PROVIDER_ID)),
        (key("name"), toml_str("ChatGPT plan")),
        (key("base_url"), toml_str(RESOURCE)),
        (key("wire_api"), toml_str("responses")),
        (key("requires_openai_auth"), "false".into()),
        (key("supports_websockets"), "false".into()),
        (key("auth.command"), toml_str(&helper.to_string_lossy())),
        (key("auth.args"), format!("[{}]", toml_str(TOKEN_HELPER_FLAG))),
        (key("auth.timeout_ms"), "60000".into()),
        (key("auth.refresh_interval_ms"), "300000".into()),
    ]
}

/// Entry point for [`TOKEN_HELPER_FLAG`]: prints the token, returns the
/// process exit code.
pub fn print_access_token() -> i32 {
    match access_token() {
        Ok(token) => {
            println!("{token}");
            0
        }
        Err(err) => {
            eprintln!("{}: {err:#}", cockpit_common::APP_NAME);
            1
        }
    }
}

/// A valid access token, refreshed first when it is about to expire.
pub fn access_token() -> Result<String> {
    let lock = store::lock()?;
    let mut record = lock.load()?;
    let Some(tokens) = record.tokens.clone() else {
        bail!("not signed in to ChatGPT; sign in from Hangar's agent panel");
    };
    let now = now();
    if tokens.expires_at - now > REFRESH_MARGIN_SECS {
        return Ok(tokens.access_token);
    }
    let client_id = record.client_id.clone().context("the saved ChatGPT sign-in has no client id")?;
    let (status, body) = post_form(
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &client_id),
            ("refresh_token", &tokens.refresh_token),
            ("resource", RESOURCE),
        ],
    )
    .or_else(|err| {
        // Offline: the current token may still have a few minutes.
        if tokens.expires_at > now {
            tracing::warn!("refreshing the ChatGPT token: {err:#}");
            Ok((0, String::new()))
        } else {
            Err(err)
        }
    })?;
    if status == 0 {
        return Ok(tokens.access_token);
    }
    if status != 200 {
        let (code, message) = oauth_error(&body);
        if code.as_deref().is_some_and(is_terminal_refresh_error) {
            record.tokens = None;
            lock.save(&record)?;
            bail!("the ChatGPT sign-in expired; sign in again from Hangar's agent panel");
        }
        if tokens.expires_at > now {
            return Ok(tokens.access_token);
        }
        bail!("refreshing the ChatGPT sign-in failed ({status}): {message}");
    }
    let response: TokenResponse = serde_json::from_str(&body).context("reading the refreshed tokens")?;
    let refreshed = store::Tokens {
        id_token: response.id_token.unwrap_or(tokens.id_token),
        refresh_token: response.refresh_token.unwrap_or(tokens.refresh_token),
        expires_at: now + response.expires_in.unwrap_or(3600),
        scopes: response.scope.as_deref().map(split_scopes).unwrap_or(tokens.scopes),
        access_token: response.access_token,
    };
    let token = refreshed.access_token.clone();
    record.tokens = Some(refreshed);
    lock.save(&record)?;
    Ok(token)
}

/// What signing out managed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignOut {
    /// OpenAI revoked the session and the tokens are gone.
    Revoked,
    /// The tokens are gone here, but OpenAI did not confirm the revocation.
    LocalOnly,
}

/// Revokes the session with OpenAI and forgets the tokens. The client id
/// and host id stay, so the next sign-in skips registration.
pub fn sign_out() -> Result<SignOut> {
    let lock = store::lock()?;
    let mut record = lock.load()?;
    let Some(tokens) = record.tokens.take() else {
        return Ok(SignOut::Revoked);
    };
    let mut outcome = SignOut::LocalOnly;
    if let Some(client_id) = &record.client_id {
        for delay in [0u64, 500, 1500] {
            std::thread::sleep(Duration::from_millis(delay));
            let fields = [
                ("token", tokens.refresh_token.as_str()),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id.as_str()),
            ];
            match post_form(REVOKE_URL, &fields) {
                Ok((200, _)) => {
                    outcome = SignOut::Revoked;
                    break;
                }
                Ok((status, _)) if status < 500 => break,
                Ok((status, _)) => tracing::warn!("revoking the ChatGPT session: HTTP {status}"),
                Err(err) => tracing::warn!("revoking the ChatGPT session: {err:#}"),
            }
        }
    }
    lock.save(&record)?;
    Ok(outcome)
}

/// A sign-in waiting for the browser.
pub struct PendingSignIn {
    listener: TcpListener,
    auth_url: String,
    redirect_uri: String,
    state: String,
    nonce: String,
    verifier: String,
    /// `None` while registering: the callback brings the issued id.
    client_id: Option<String>,
}

/// Starts a sign-in: listens on 127.0.0.1 and builds the page to open.
/// Open [`PendingSignIn::auth_url`] in the browser, then call
/// [`PendingSignIn::finish`].
pub fn begin_sign_in() -> Result<PendingSignIn> {
    let (client_id, host_id) = {
        let lock = store::lock()?;
        let record = lock.load()?;
        // Saves a new host id before it is first used.
        lock.save(&record)?;
        (record.client_id, record.host_id)
    };
    let listener = TcpListener::bind(("127.0.0.1", PREFERRED_PORT))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
        .context("listening for the sign-in callback")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let (state, nonce, verifier) = (random_token()?, random_token()?, random_token()?);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

    let mut query = url::form_urlencoded::Serializer::new(String::new());
    match &client_id {
        Some(id) => query.append_pair("client_id", id),
        None => query
            .append_pair("client_id", REGISTRATION_CLIENT_ID)
            .append_pair("agent_name_hint", cockpit_common::APP_NAME),
    };
    query
        .append_pair("ext_agent_host_id", &host_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("resource", RESOURCE)
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge);
    let auth_url = format!("{AUTHORIZE_URL}?{}", query.finish());
    Ok(PendingSignIn {
        listener,
        auth_url,
        redirect_uri,
        state,
        nonce,
        verifier,
        client_id,
    })
}

impl PendingSignIn {
    pub fn auth_url(&self) -> &str {
        &self.auth_url
    }

    /// Waits for the browser (until `cancel` is set or 15 minutes pass),
    /// exchanges the code and saves the tokens. Blocks.
    pub fn finish(self, cancel: &AtomicBool) -> Result<AccountInfo> {
        let deadline = Instant::now() + SIGN_IN_TIMEOUT;
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!("sign-in cancelled");
            }
            if Instant::now() > deadline {
                bail!("the sign-in timed out; try again");
            }
            let mut stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(150));
                    continue;
                }
                Err(err) => return Err(err).context("waiting for the sign-in callback"),
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
                Ok(account) => {
                    let who = account.email.as_deref().unwrap_or("your ChatGPT account");
                    respond(
                        &mut stream,
                        "200 OK",
                        "You're signed in to Hangar",
                        &format!("Signed in as {who}. You can close this tab and go back to Hangar."),
                    );
                }
                Err(err) => respond(&mut stream, "200 OK", "Sign-in didn't finish", &format!("{err:#}")),
            }
            return result;
        }
    }

    fn complete(&self, params: &std::collections::HashMap<String, String>) -> Result<AccountInfo> {
        if let Some(error) = params.get("error") {
            if error == "access_denied" {
                bail!("you declined the sign-in, so Hangar can't use your ChatGPT plan");
            }
            let detail = params.get("error_description").map(String::as_str).unwrap_or(error);
            bail!("ChatGPT sign-in failed: {detail}");
        }
        let code = params.get("code").context("the sign-in callback had no code")?;
        let client_id = match (&self.client_id, params.get("client_id")) {
            (Some(saved), Some(issued)) if saved != issued => {
                bail!("the sign-in came back for a different app registration")
            }
            (Some(saved), _) => saved.clone(),
            (None, Some(issued)) => issued.clone(),
            (None, None) => bail!("OpenAI didn't finish registering Hangar; try again"),
        };

        let lock = store::lock()?;
        let mut record = lock.load()?;
        // Keep the registration even if the exchange below fails.
        if record.client_id.as_deref() != Some(client_id.as_str()) {
            record.client_id = Some(client_id.clone());
            lock.save(&record)?;
        }

        let (status, body) = post_form(
            TOKEN_URL,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", code),
                ("code_verifier", &self.verifier),
                ("redirect_uri", &self.redirect_uri),
                ("resource", RESOURCE),
            ],
        )?;
        if status != 200 {
            let (_, message) = oauth_error(&body);
            bail!("OpenAI refused the sign-in ({status}): {message}");
        }
        let response: TokenResponse = serde_json::from_str(&body).context("reading the sign-in tokens")?;
        let id_token = response.id_token.context("OpenAI sent no ID token")?;
        let refresh_token = response.refresh_token.context("OpenAI sent no refresh token")?;
        let claims = id_token_claims(&id_token)?;
        validate_id_token(&claims, &client_id, &self.nonce, now())?;
        let scopes = response.scope.as_deref().map(split_scopes).unwrap_or_default();
        if !scopes.iter().any(|s| s == PLAN_SCOPE) {
            bail!("ChatGPT plan use wasn't allowed for Hangar; sign in again and allow it");
        }

        record.subject = claims["sub"].as_str().map(str::to_string);
        record.email = claims["email"].as_str().map(str::to_string);
        record.tokens = Some(store::Tokens {
            id_token,
            access_token: response.access_token,
            refresh_token,
            expires_at: now() + response.expires_in.unwrap_or(3600),
            scopes,
        });
        lock.save(&record)?;
        Ok(AccountInfo { email: record.email })
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

/// The ID token's claims. Its signature is not checked: it came straight
/// from the token endpoint over TLS, which OpenID Connect accepts in place
/// of the signature for the code flow (Core §3.1.3.7).
fn id_token_claims(token: &str) -> Result<Value> {
    let payload = token.split('.').nth(1).context("the ID token is malformed")?;
    let lenient = GeneralPurpose::new(
        &alphabet::URL_SAFE,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let bytes = lenient.decode(payload).context("the ID token is malformed")?;
    serde_json::from_slice(&bytes).context("the ID token is malformed")
}

fn validate_id_token(claims: &Value, client_id: &str, nonce: &str, now: i64) -> Result<()> {
    if claims["iss"].as_str() != Some(ISSUER) {
        bail!("the ID token is from an unexpected issuer");
    }
    let audience_ok = match &claims["aud"] {
        Value::String(aud) => aud == client_id,
        Value::Array(auds) => auds.iter().any(|a| a.as_str() == Some(client_id)),
        _ => false,
    };
    if !audience_ok {
        bail!("the ID token is for a different app");
    }
    if claims["nonce"].as_str() != Some(nonce) {
        bail!("the ID token doesn't belong to this sign-in");
    }
    if claims["exp"].as_i64().is_some_and(|exp| exp + 60 < now) {
        bail!("the ID token has expired; check your Mac's clock");
    }
    if claims["sub"].as_str().is_none_or(str::is_empty) {
        bail!("the ID token has no subject");
    }
    Ok(())
}

/// Refresh errors after which the refresh token can never work again.
fn is_terminal_refresh_error(code: &str) -> bool {
    matches!(
        code,
        "invalid_grant"
            | "invalid_refresh_token"
            | "token_expired"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused"
    )
}

/// (code, message) from an OAuth or OpenAI error body.
fn oauth_error(body: &str) -> (Option<String>, String) {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return (None, body.chars().take(200).collect());
    };
    let code = json["error"]["code"]
        .as_str()
        .or_else(|| json["error"].as_str())
        .map(str::to_string);
    let message = json["error_description"]
        .as_str()
        .or_else(|| json["error"]["message"].as_str())
        .or_else(|| json["detail"].as_str())
        .or(code.as_deref())
        .unwrap_or("unknown error")
        .to_string();
    (code, message)
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
        .with_context(|| format!("contacting {}", url.split('/').take(3).collect::<Vec<_>>().join("/")))?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();
    Ok((status, body))
}

/// The query of a `GET /auth/callback` request, or `None` for anything else.
fn read_callback(stream: &mut TcpStream) -> Option<std::collections::HashMap<String, String>> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut line = String::new();
    BufReader::new(&*stream).read_line(&mut line).ok()?;
    let target = line.strip_prefix("GET ")?.split(' ').next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    (path == CALLBACK_PATH).then(|| url::form_urlencoded::parse(query.as_bytes()).into_owned().collect())
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
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn split_scopes(scope: &str) -> Vec<String> {
    scope.split_whitespace().map(str::to_string).collect()
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn token_with(claims: Value) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn id_tokens_must_match_this_sign_in() {
        let good = json!({"iss": ISSUER, "aud": "oaiapp_1", "nonce": "n", "exp": 2000, "sub": "user-1", "email": "a@b.c"});
        let claims = id_token_claims(&token_with(good.clone())).unwrap();
        assert_eq!(claims["email"], "a@b.c");
        assert!(validate_id_token(&claims, "oaiapp_1", "n", 1000).is_ok());
        assert!(validate_id_token(&claims, "oaiapp_2", "n", 1000).is_err());
        assert!(validate_id_token(&claims, "oaiapp_1", "other", 1000).is_err());
        assert!(validate_id_token(&claims, "oaiapp_1", "n", 5000).is_err());
        let mut list_aud = good.clone();
        list_aud["aud"] = json!(["x", "oaiapp_1"]);
        assert!(validate_id_token(&list_aud, "oaiapp_1", "n", 1000).is_ok());
        let mut wrong_issuer = good;
        wrong_issuer["iss"] = json!("https://evil.example");
        assert!(validate_id_token(&wrong_issuer, "oaiapp_1", "n", 1000).is_err());
    }

    #[test]
    fn oauth_errors_in_either_shape() {
        assert_eq!(
            oauth_error(r#"{"error":"invalid_grant","error_description":"expired"}"#),
            (Some("invalid_grant".into()), "expired".into())
        );
        assert_eq!(
            oauth_error(r#"{"error":{"code":"refresh_token_reused","message":"reused"}}"#),
            (Some("refresh_token_reused".into()), "reused".into())
        );
        assert_eq!(oauth_error(r#"{"detail":"nope"}"#), (None, "nope".into()));
        assert!(is_terminal_refresh_error("invalid_grant"));
        assert!(!is_terminal_refresh_error("temporarily_unavailable"));
    }

    #[test]
    fn codex_overrides_are_toml() {
        let overrides = codex_overrides(Path::new("/Applications/Hangar.app/Contents/MacOS/Hangar"));
        let get = |k: &str| overrides.iter().find(|(key, _)| key.ends_with(k)).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("model_provider"), "\"hangar_chatgpt\"");
        assert_eq!(get("auth.command"), "\"/Applications/Hangar.app/Contents/MacOS/Hangar\"");
        assert_eq!(get("auth.args"), "[\"--chatgpt-access-token\"]");
        assert_eq!(get("base_url"), "\"https://api.openai.com/v1\"");
    }

    #[test]
    fn callback_requests_are_parsed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            write!(s, "GET /auth/callback?code=abc&state=xyz&client_id=oaiapp_1 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut out = String::new();
            std::io::Read::read_to_string(&mut s, &mut out).unwrap();
            out
        });
        let (mut stream, _) = listener.accept().unwrap();
        let params = read_callback(&mut stream).unwrap();
        assert_eq!(params["code"], "abc");
        assert_eq!(params["state"], "xyz");
        assert_eq!(params["client_id"], "oaiapp_1");
        respond(&mut stream, "200 OK", "Done <ok>", "a & b");
        drop(stream);
        let page = client.join().unwrap();
        assert!(page.starts_with("HTTP/1.1 200 OK") && page.contains("Done &lt;ok&gt;") && page.contains("a &amp; b"));
    }
}

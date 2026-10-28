//! Signing in to the server that shares swarms between Swarmkeeper, cfcli and
//! the people you fly with. Ported from cfcli's `auth` module.
//!
//! Signing in goes through the browser, as `gcloud auth login` does: OAuth 2.0
//! authorization code with PKCE (RFC 7636) and a loopback redirect
//! (RFC 8252). Swarmkeeper listens on 127.0.0.1 and opens the server's
//! `/cli/authorize` page; once the user allows Swarmkeeper there, the browser
//! brings a one-time code back, and Swarmkeeper exchanges it and its PKCE
//! verifier for an API key. The key is kept in `credentials.json` next to the
//! Swarmkeeper settings, readable only by the user, and is listed on the
//! server's API keys page as "Swarmkeeper on <computer>", where it can be
//! revoked. Swarmkeeper and cfcli each have their own key.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngExt;
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use crate::host;

/// The server signed in to unless the user picks another.
pub const DEFAULT_SERVER: &str = "https://arc.bitcraze.io";
/// The name Swarmkeeper signs in with, shown to the user in the browser.
const CLIENT: &str = "Swarmkeeper";
/// How long to wait for the user to allow Swarmkeeper in the browser.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// What signing in stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// The server, without a trailing slash, e.g. `https://arc.bitcraze.io`.
    pub server: String,
    pub key: String,
    /// The key's name on the server, e.g. "Swarmkeeper on lab-pc".
    pub key_name: String,
}

/// The server no longer accepts the stored key: it was revoked, or the
/// account is gone. Signing in again fixes it.
#[derive(Debug)]
pub struct KeyRejected {
    pub host: String,
    pub key_name: String,
}

impl std::fmt::Display for KeyRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} no longer accepts Swarmkeeper's key '{}': sign in again", self.host, self.key_name)
    }
}

impl std::error::Error for KeyRejected {}

// ---- Stored credentials ----

fn credentials_path() -> Result<PathBuf> {
    host::store::file_beside("credentials.json").context("could not find the Swarmkeeper settings folder")
}

/// The stored credentials, if Swarmkeeper is signed in.
pub fn load() -> Result<Option<Credentials>> {
    let path = credentials_path()?;
    let text = match host::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let credentials = serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(credentials))
}

/// Store the credentials, readable by the user only: the key acts as them.
fn save(credentials: &Credentials) -> Result<()> {
    let path = credentials_path()?;
    if let Some(dir) = path.parent() {
        host::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(credentials)?;
    host::fs::write_private(&path, json.as_bytes()).with_context(|| format!("writing {}", path.display()))
}



fn forget() -> Result<()> {
    let path = credentials_path()?;
    match host::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

// ---- Talking to the server ----

/// A client for the server: who Swarmkeeper is, and how long to wait.
pub fn http(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("Swarmkeeper/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5).min(timeout))
        .timeout(timeout)
        .build()?)
}

/// `path` on the server.
pub fn endpoint(server: &str, path: &str) -> Result<Url> {
    Url::parse(&format!("{server}{path}")).with_context(|| format!("server '{}' is not a URL", server))
}

/// The server's JSON answer, or its `{"error"}` as an error.
pub async fn answer<T: serde::de::DeserializeOwned>(server: &str, response: reqwest::Response) -> Result<T> {
    let status = response.status();
    if status.is_success() {
        return response.json().await.with_context(|| format!("reading the answer from {}", host(server)));
    }
    #[derive(Deserialize)]
    struct Problem {
        error: String,
    }
    match response.json::<Problem>().await {
        Ok(problem) => bail!("{} answered: {}", host(server), problem.error),
        Err(_) => bail!("{} answered {}", host(server), status),
    }
}

/// The server's name without the scheme, e.g. `arc.bitcraze.io`.
pub fn host(server: &str) -> &str {
    server.split("://").nth(1).unwrap_or(server)
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    pub name: String,
    pub orgs: Vec<OrgInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrgInfo {
    /// The user's own ID for the organization, the `<org>` in swarm IDs.
    pub slug: String,
    pub name: String,
    pub role: String,
}

/// Who the key belongs to. Fails with [`KeyRejected`] when the server no
/// longer accepts it.
pub async fn whoami(credentials: &Credentials) -> Result<UserInfo> {
    let server = &credentials.server;
    let response = http(Duration::from_secs(15))?
        .get(endpoint(server, "/api/v1/user")?)
        .bearer_auth(&credentials.key)
        .send()
        .await
        .with_context(|| format!("can't reach {}", host(server)))?;
    if response.status() == StatusCode::UNAUTHORIZED {
        bail!(KeyRejected { host: host(server).to_string(), key_name: credentials.key_name.clone() });
    }
    answer(server, response).await
}

/// Revoke the key on the server. A key that is already revoked is fine.
async fn revoke(credentials: &Credentials) -> Result<()> {
    let server = &credentials.server;
    let response = http(Duration::from_secs(15))?
        .delete(endpoint(server, "/api/v1/key")?)
        .bearer_auth(&credentials.key)
        .send()
        .await
        .with_context(|| format!("can't reach {}", host(server)))?;
    match response.status() {
        status if status.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED => Ok(()),
        _ => answer::<serde_json::Value>(server, response).await.map(|_| ()),
    }
}

// ---- Signing in ----

/// 32 random bytes as URL-safe base64 (43 characters).
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes[..]);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The PKCE S256 challenge for a verifier.
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// This computer's name, for the key's name on the server.
fn device_name() -> String {
    let name = host::env::hostname().trim().to_string();
    if name.is_empty() {
        "this computer".to_string()
    } else {
        name.chars().take(64).collect()
    }
}

/// What the browser brought back.
enum Callback {
    Code(String),
    Cancelled,
}

/// A sign-in waiting for the user to allow Swarmkeeper in the browser.
pub struct SignIn {
    server: String,
    listener: TcpListener,
    redirect_uri: String,
    state: String,
    verifier: String,
    /// The server's sign-in page, for the user to open.
    pub url: Url,
}

/// A finished sign-in.
pub struct SignedIn {
    /// The key of the sign-in this one replaced, when it couldn't be revoked.
    pub old_key_kept: Option<Credentials>,
}

/// Start signing in to `server`: listen for the browser and make the link
/// to the sign-in page. Opening it is up to the caller.
pub async fn start(server: &str) -> Result<SignIn> {
    let server = server.trim().trim_end_matches('/').to_string();
    if !server.starts_with("https://") && !server.starts_with("http://") {
        bail!("the server must start with https:// or http://");
    }
    let verifier = random_token();
    let state = random_token();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("listening for the browser on 127.0.0.1")?;
    let redirect_uri = format!("http://127.0.0.1:{}/callback", listener.local_addr()?.port());

    let mut url = endpoint(&server, "/cli/authorize")?;
    url.query_pairs_mut()
        .append_pair("client", CLIENT)
        .append_pair("device", &device_name())
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state)
        .append_pair("code_challenge", &pkce_challenge(&verifier))
        .append_pair("code_challenge_method", "S256");
    Ok(SignIn { server, listener, redirect_uri, state, verifier, url })
}

impl SignIn {
    /// Wait for the browser to come back, get the key and store it. A new
    /// sign-in replaces the old one, whose key is revoked.
    pub async fn finish(self) -> Result<SignedIn> {
        let server = self.server;
        let (mut browser, callback) = host::timeout(SIGN_IN_TIMEOUT, wait_for_browser(&self.listener, &self.state))
            .await
            .map_err(|_| anyhow::anyhow!("nobody allowed Swarmkeeper in the browser within 5 minutes"))??;
        let done = format!("{}/cli/done?client={}", server, CLIENT);
        let code = match callback {
            Callback::Code(code) => code,
            Callback::Cancelled => {
                redirect(&mut browser, &format!("{}&cancelled", done)).await;
                bail!("the sign-in was cancelled in the browser");
            }
        };

        let signed_in = async {
            #[derive(Deserialize)]
            struct Token {
                key: String,
                name: String,
            }
            let response = http(Duration::from_secs(30))?
                .post(endpoint(&server, "/api/v1/cli/token")?)
                .json(&json!({ "code": code, "code_verifier": self.verifier, "redirect_uri": self.redirect_uri }))
                .send()
                .await
                .with_context(|| format!("can't reach {}", host(&server)))?;
            let token: Token = answer(&server, response).await?;
            let credentials = Credentials { server: server.clone(), key: token.key, key_name: token.name };
            whoami(&credentials)
                .await
                .context("the server didn't accept the key it just made")?;
            anyhow::Ok(credentials)
        }
        .await;
        let credentials = match signed_in {
            Ok(signed_in) => signed_in,
            Err(e) => {
                respond(
                    &mut browser,
                    "500 Internal Server Error",
                    &format!("Signing in Swarmkeeper failed: {:#}\n\nGo back to Swarmkeeper.\n", e),
                )
                .await;
                return Err(e);
            }
        };
        redirect(&mut browser, &done).await;

        // A new sign-in replaces the old one; its key isn't needed any more.
        let mut old_key_kept = None;
        if let Ok(Some(old)) = load() {
            if old.key != credentials.key && revoke(&old).await.is_err() {
                old_key_kept = Some(old);
            }
        }
        save(&credentials)?;
        Ok(SignedIn { old_key_kept })
    }
}

/// Take requests until the browser comes back to `/callback` with this
/// sign-in's state. Anything else (a favicon, an old tab) gets an answer and
/// is ignored.
async fn wait_for_browser(listener: &TcpListener, state: &str) -> Result<(TcpStream, Callback)> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        // Browsers sometimes open a connection and send nothing (preconnect),
        // which mustn't hold up the real request.
        let Ok(Some(target)) = host::timeout(Duration::from_secs(5), read_request_target(&mut stream)).await
        else {
            continue;
        };
        let Ok(url) = Url::parse(&format!("http://127.0.0.1{}", target)) else {
            respond(&mut stream, "400 Bad Request", "Not a sign-in request.\n").await;
            continue;
        };
        if url.path() != "/callback" {
            respond(&mut stream, "404 Not Found", "Not found.\n").await;
            continue;
        }
        let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
        if param("state").as_deref() != Some(state) {
            respond(
                &mut stream,
                "400 Bad Request",
                "This page belongs to another sign-in. Use the newest link Swarmkeeper opened.\n",
            )
            .await;
            continue;
        }
        if let Some(code) = param("code") {
            return Ok((stream, Callback::Code(code)));
        }
        if param("error").is_some() {
            return Ok((stream, Callback::Cancelled));
        }
        respond(&mut stream, "400 Bad Request", "The sign-in came back without a code.\n").await;
    }
}

/// The target of the HTTP request on `stream`, e.g. `/callback?code=...`.
/// Reads the request line and the headers; there is no body.
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let target = line.split_whitespace().nth(1)?.to_string();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await.ok()? == 0 || header.trim().is_empty() {
            break;
        }
    }
    Some(target)
}

async fn respond(stream: &mut TcpStream, status: &str, text: &str) {
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        text.len(),
        text
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn redirect(stream: &mut TcpStream, location: &str) {
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        location
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

// ---- Signing out ----

/// Revoke the key on the server and forget it. The credentials are
/// forgotten even when the server can't revoke the key; the error then says
/// which key to revoke by hand.
pub async fn sign_out() -> Result<()> {
    let Some(credentials) = load()? else {
        return Ok(());
    };
    let revoked = revoke(&credentials).await;
    forget()?;
    revoked.with_context(|| {
        format!(
            "signed out here, but couldn't revoke the key; revoke '{}' at {}/keys",
            credentials.key_name, credentials.server
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc_7636_example() {
        // Appendix B of RFC 7636.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn tokens_are_url_safe_and_differ() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[tokio::test]
    async fn waits_for_its_own_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
            let get = |path: &str| client.get(format!("http://127.0.0.1:{}{}", port, path)).send();
            assert_eq!(get("/favicon.ico").await.unwrap().status(), 404);
            assert_eq!(get("/callback?code=old&state=other").await.unwrap().status(), 400);
            get("/callback?code=abc&state=mine").await.unwrap().status()
        });
        let (mut stream, callback) = wait_for_browser(&listener, "mine").await.unwrap();
        assert!(matches!(callback, Callback::Code(code) if code == "abc"));
        redirect(&mut stream, "http://example.com/done").await;
        drop(stream);
        assert_eq!(browser.await.unwrap(), 302);
    }
}

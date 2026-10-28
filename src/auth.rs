//! Google OAuth for a user-owned Desktop client: loopback sign-in with PKCE,
//! a private token file, and refresh shared safely between processes.

use crate::{
    config,
    error::{Error, Result},
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use std::{io::Write, path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const SCOPES: &[&str] = &[
    "openid",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/drive",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub client_id: String,
    pub client_secret: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub email: String,
}

pub struct Auth {
    home: PathBuf,
    http: reqwest::Client,
    cache: tokio::sync::Mutex<Option<Grant>>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn url_safe(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl Auth {
    pub fn new(home: PathBuf, http: reqwest::Client) -> Self {
        Self {
            home,
            http,
            cache: tokio::sync::Mutex::new(None),
        }
    }

    fn token_path(&self) -> PathBuf {
        self.home.join("token.json")
    }

    fn load(&self) -> Result<Grant> {
        let raw = std::fs::read(self.token_path()).map_err(|_| Error::auth())?;
        serde_json::from_slice(&raw).map_err(|_| Error::auth())
    }

    fn save(&self, grant: &Grant) -> Result<()> {
        config::write_private(&self.token_path(), &serde_json::to_vec_pretty(grant)?)
    }

    /// Serializes refreshes across processes; Google refresh tokens stay valid,
    /// so a second process simply reuses the token the first one stored.
    fn lock(&self) -> Result<std::fs::File> {
        config::private_dir(&self.home)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.home.join("token.lock"))?;
        file.lock()?;
        Ok(file)
    }

    pub async fn access_token(&self) -> Result<String> {
        let mut cache = self.cache.lock().await;
        if let Some(grant) = cache.as_ref().filter(|g| g.expires_at > now() + 60) {
            return Ok(grant.access_token.clone());
        }
        let _lock = self.lock()?;
        let mut grant = self.load()?;
        if grant.expires_at <= now() + 60 {
            grant = self.refresh(grant).await?;
            self.save(&grant)?;
        }
        let token = grant.access_token.clone();
        *cache = Some(grant);
        Ok(token)
    }

    /// Forget the cached access token after the service rejected it.
    pub async fn invalidate(&self) -> Result<()> {
        let mut cache = self.cache.lock().await;
        *cache = None;
        let _lock = self.lock()?;
        if let Ok(mut grant) = self.load() {
            grant.expires_at = 0;
            self.save(&grant)?;
        }
        Ok(())
    }

    async fn refresh(&self, mut grant: Grant) -> Result<Grant> {
        let response = self
            .http
            .post(config::token_url())
            .timeout(Duration::from_secs(30))
            .form(&[
                ("client_id", grant.client_id.as_str()),
                ("client_secret", grant.client_secret.as_str()),
                ("refresh_token", grant.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|_| Error::network("Token refresh"))?;
        if response.status().as_u16() == 400 || response.status().as_u16() == 401 {
            return Err(Error::new(
                "auth_required",
                "Google rejected the saved sign-in (it may have been revoked or expired).",
                "Run `colab auth login` again.",
            ));
        }
        if !response.status().is_success() {
            return Err(Error::network("Token refresh"));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| Error::network("Token refresh"))?;
        grant.access_token = body["access_token"]
            .as_str()
            .ok_or_else(Error::auth)?
            .to_owned();
        grant.expires_at = now() + body["expires_in"].as_i64().unwrap_or(3600);
        if let Some(refresh) = body["refresh_token"].as_str() {
            grant.refresh_token = refresh.to_owned();
        }
        Ok(grant)
    }

    pub fn status(&self) -> Result<Value> {
        match self.load() {
            Ok(grant) => Ok(json!({
                "signed_in": true,
                "email": grant.email,
                "subject": grant.subject,
                "access_token_valid_for_seconds": (grant.expires_at - now()).max(0),
                "token_file": self.token_path(),
            })),
            Err(_) => Ok(json!({
                "signed_in": false,
                "next_step": "Run `colab auth login --client CLIENT_SECRET.json` with a Google OAuth Desktop client, or `colab auth import FILE`.",
            })),
        }
    }

    /// The signed-in email, read locally (no network).
    pub fn email(&self) -> Option<String> {
        self.load().ok().map(|grant| grant.email)
    }

    pub fn logout(&self) -> Result<Value> {
        let path = self.token_path();
        let existed = path.exists();
        if existed {
            std::fs::remove_file(&path)?;
        }
        Ok(
            json!({"signed_out": existed, "note": "Only the local token was removed. Revoke access at https://myaccount.google.com/permissions if needed."}),
        )
    }

    /// Accepts a Hycli/colab-cli grant JSON with client and token fields.
    pub async fn import(&self, file: &std::path::Path) -> Result<Value> {
        let raw = std::fs::read(file)?;
        let mut grant: Grant = serde_json::from_slice(&raw).map_err(|_| {
            Error::input("The file must contain client_id, client_secret, access_token, refresh_token and expires_at.")
        })?;
        grant = self.refresh(grant).await?;
        self.identify(&mut grant).await?;
        let _lock = self.lock()?;
        self.save(&grant)?;
        *self.cache.lock().await = Some(grant.clone());
        Ok(json!({"signed_in": true, "email": grant.email}))
    }

    async fn identify(&self, grant: &mut Grant) -> Result<()> {
        let info: Value = self
            .http
            .get(format!("{}/oauth2/v3/userinfo", config::google_api_url()))
            .bearer_auth(&grant.access_token)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| Error::network("Account lookup"))?
            .json()
            .await
            .map_err(|_| Error::network("Account lookup"))?;
        grant.subject = info["sub"].as_str().unwrap_or_default().to_owned();
        grant.email = info["email"].as_str().unwrap_or_default().to_owned();
        if grant.subject.is_empty() {
            return Err(Error::auth());
        }
        Ok(())
    }

    /// Sign in with a Google OAuth "Desktop app" client through a loopback redirect.
    pub async fn login(
        &self,
        client_file: Option<&std::path::Path>,
        open_browser: bool,
    ) -> Result<Value> {
        let client_path = self.home.join("client.json");
        if let Some(file) = client_file {
            let raw: Value = serde_json::from_slice(&std::fs::read(file)?)
                .map_err(|_| Error::input("The client file is not JSON."))?;
            let client = raw.get("installed").unwrap_or(&raw);
            let id = client["client_id"].as_str().unwrap_or_default();
            let secret = client["client_secret"].as_str().unwrap_or_default();
            if id.is_empty() || secret.is_empty() {
                return Err(Error::input(
                    "The client file needs client_id and client_secret (a Google OAuth Desktop app client).",
                ));
            }
            config::write_private(
                &client_path,
                &serde_json::to_vec(&json!({"client_id": id, "client_secret": secret}))?,
            )?;
        }
        // Otherwise reuse a client already in use: the default account's, or
        // the one inside a saved (possibly imported) grant.
        let root = config::root();
        let client: Value = [
            client_path,
            root.join("client.json"),
            self.token_path(),
            root.join("token.json"),
        ]
        .iter()
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .find(|c| c["client_id"].is_string() && c["client_secret"].is_string())
        .ok_or_else(|| {
                Error::new(
                    "client_required",
                    "No Google OAuth client is configured.",
                    "Create an OAuth client of type Desktop app in Google Cloud Console (with the Drive API enabled), download its JSON, then run `colab auth login --client FILE`.",
                )
            })?;
        let client_id = client["client_id"].as_str().unwrap_or_default().to_owned();
        let client_secret = client["client_secret"]
            .as_str()
            .unwrap_or_default()
            .to_owned();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let redirect = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
        let verifier =
            url_safe(uuid::Uuid::new_v4().as_bytes()) + &url_safe(uuid::Uuid::new_v4().as_bytes());
        let challenge = url_safe(&sha2::Sha256::digest(verifier.as_bytes()));
        let state = url_safe(uuid::Uuid::new_v4().as_bytes());
        let mut url =
            url::Url::parse(&config::auth_url()).map_err(|_| Error::input("Invalid auth URL"))?;
        url.query_pairs_mut()
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("response_type", "code")
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("access_type", "offline")
            .append_pair("prompt", "select_account consent")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        eprintln!("Open this URL to sign in with Google:\n{url}\n");
        let _ = std::io::stderr().flush();
        if open_browser {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let _ = std::process::Command::new(opener).arg(url.as_str()).spawn();
        }
        let code = tokio::time::timeout(Duration::from_secs(600), receive_code(&listener, &state))
            .await
            .map_err(|_| {
                Error::new(
                    "login_timeout",
                    "No sign-in arrived within 10 minutes.",
                    "Run `colab auth login` again.",
                )
            })??;
        let response = self
            .http
            .post(config::token_url())
            .timeout(Duration::from_secs(30))
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("code", code.as_str()),
                ("code_verifier", verifier.as_str()),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect.as_str()),
            ])
            .send()
            .await
            .map_err(|_| Error::network("Sign-in"))?;
        let body: Value = response
            .json()
            .await
            .map_err(|_| Error::network("Sign-in"))?;
        let refresh_token = body["refresh_token"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if refresh_token.is_empty() {
            return Err(Error::new(
                "login_failed",
                "Google did not return a refresh token.",
                "Run `colab auth login` again and approve every requested permission.",
            ));
        }
        let mut grant = Grant {
            client_id,
            client_secret,
            access_token: body["access_token"].as_str().unwrap_or_default().to_owned(),
            refresh_token,
            expires_at: now() + body["expires_in"].as_i64().unwrap_or(3600),
            subject: String::new(),
            email: String::new(),
        };
        self.identify(&mut grant).await?;
        let _lock = self.lock()?;
        self.save(&grant)?;
        *self.cache.lock().await = Some(grant.clone());
        Ok(json!({"signed_in": true, "email": grant.email}))
    }
}

async fn receive_code(listener: &tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = vec![0u8; 8192];
        let read = stream.read(&mut buffer).await?;
        let head = String::from_utf8_lossy(&buffer[..read]).to_string();
        let target = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
        let query: std::collections::BTreeMap<String, String> =
            url::Url::parse(&format!("http://localhost{target}"))
                .map(|u| u.query_pairs().into_owned().collect())
                .unwrap_or_default();
        let (status, message) = if query.get("state").map(String::as_str) != Some(state) {
            (
                "400 Bad Request",
                "This sign-in response does not match the running colab-cli login.",
            )
        } else if query.contains_key("code") {
            (
                "200 OK",
                "Colab CLI sign-in received. You can close this tab.",
            )
        } else {
            ("400 Bad Request", "Sign-in was cancelled or denied.")
        };
        let page =
            format!("<!doctype html><meta charset=utf-8><title>Colab CLI</title><p>{message}</p>");
        let _ = stream
            .write_all(format!("HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}", page.len()).as_bytes())
            .await;
        if query.get("state").map(String::as_str) != Some(state) {
            continue;
        }
        return match query.get("code") {
            Some(code) => Ok(code.clone()),
            None => Err(Error::new(
                "login_failed",
                "Sign-in was cancelled or denied.",
                "Run `colab auth login` again.",
            )),
        };
    }
}

//! Authenticated calls to Google APIs and the Colab backend.

use crate::{
    auth::Auth,
    error::{Error, Result},
};
use serde_json::Value;
use std::time::Duration;

pub const XSSI_PREFIX: &str = ")]}'\n";

pub struct Api {
    pub auth: Auth,
    pub http: reqwest::Client,
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("colab-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("HTTP client")
}

/// A stable error for a non-success response, without echoing response bodies.
pub fn status_error(status: u16, context: &str) -> Error {
    match status {
        401 => Error::auth(),
        403 => Error::new(
            "forbidden",
            format!("{context}: Google denied access (HTTP 403)."),
            "Check that the signed-in account owns the resource and that the Drive API is enabled for the OAuth client's project.",
        ),
        404 => Error::new(
            "not_found",
            format!("{context}: not found (HTTP 404)."),
            "Check the ID or path.",
        ),
        429 => Error::new(
            "rate_limited",
            format!("{context}: too many requests (HTTP 429)."),
            "Wait a minute before retrying.",
        ),
        _ => Error::new(
            "request_failed",
            format!("{context}: the service answered HTTP {status}."),
            "Retry later; if it persists the service may be unavailable.",
        ),
    }
}

impl Api {
    pub fn new(home: std::path::PathBuf) -> Self {
        let http = client();
        Self {
            auth: Auth::new(home, http.clone()),
            http,
        }
    }

    /// Send with the current access token. A 401 means the request was not
    /// processed, so it is retried once with a refreshed token.
    pub async fn send(
        &self,
        context: &str,
        build: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let token = self.auth.access_token().await?;
            let response = build(&self.http, &token)
                .send()
                .await
                .map_err(|_| Error::network(context))?;
            if response.status().as_u16() == 401 && attempt == 0 {
                self.auth.invalidate().await?;
                continue;
            }
            return Ok(response);
        }
        Err(Error::auth())
    }

    /// Send and parse a JSON body (anti-XSSI prefix removed); bounded to 16 MiB.
    pub async fn json(
        &self,
        context: &str,
        build: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    ) -> Result<Value> {
        let response = self.send(context, build).await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(status_error(status, context));
        }
        let bytes = read_limited(response, 16 * 1024 * 1024, context).await?;
        parse_json(&bytes)
    }
}

pub async fn read_limited(
    response: reqwest::Response,
    limit: usize,
    context: &str,
) -> Result<Vec<u8>> {
    use futures_util::StreamExt;
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.map_err(|_| Error::network(context))?);
        if bytes.len() > limit {
            return Err(Error::new(
                "too_large",
                format!("{context}: the response exceeded {limit} bytes."),
                "Use a download command for large content.",
            ));
        }
    }
    Ok(bytes)
}

pub fn parse_json(bytes: &[u8]) -> Result<Value> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix(XSSI_PREFIX).unwrap_or(&text);
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    Ok(serde_json::from_str(text)?)
}

//! Authenticated calls to Google APIs and the Colab backend.

use crate::{
    auth::Auth,
    error::{Error, Result},
};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use std::time::Duration;

pub const XSSI_PREFIX: &str = ")]}'\n";
/// Answers that mean "try again shortly".
const TRANSIENT: [u16; 5] = [429, 500, 502, 503, 504];
const RETRIES: u32 = 3;
/// First backoff step; doubles per retry (0.5 s, 1 s, 2 s).
const BACKOFF: Duration = if cfg!(test) {
    Duration::from_millis(1)
} else {
    Duration::from_millis(500)
};
/// Retry-After waits are capped so a command never stalls for long.
const MAX_WAIT: Duration = Duration::from_secs(10);

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
    /// processed, so it is retried once with a refreshed token. Reads (GET,
    /// HEAD) are also repeated after a lost connection or a transient answer
    /// (429, 5xx), waiting as the service asks or backing off; other methods
    /// are never repeated, because they may have taken effect.
    pub async fn send(
        &self,
        context: &str,
        build: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut refreshed = false;
        let mut retries = 0;
        loop {
            let token = self.auth.access_token().await?;
            let request = build(&self.http, &token)
                .build()
                .map_err(|_| Error::network(context))?;
            let repeatable = matches!(*request.method(), Method::GET | Method::HEAD);
            let result = self.http.execute(request).await;
            let wait = match &result {
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED && !refreshed => {
                    refreshed = true;
                    self.auth.invalidate().await?;
                    continue;
                }
                Ok(response)
                    if repeatable
                        && retries < RETRIES
                        && TRANSIENT.contains(&response.status().as_u16()) =>
                {
                    retry_after(response)
                }
                Err(_) if repeatable && retries < RETRIES => None,
                _ => return result.map_err(|_| Error::network(context)),
            };
            retries += 1;
            tokio::time::sleep(wait.unwrap_or(BACKOFF * 2u32.pow(retries - 1))).await;
        }
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

/// The service's requested wait (Retry-After in seconds), bounded.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let seconds: u64 = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds).min(MAX_WAIT))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{respond, serve};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// An Api whose saved sign-in is valid, so no token refresh is attempted.
    fn api() -> Api {
        let home = std::env::temp_dir().join(format!("colab-cli-test-{}", uuid::Uuid::new_v4()));
        let grant = serde_json::json!({"client_id": "c", "client_secret": "s", "access_token": "access",
                                       "refresh_token": "r", "expires_at": i64::MAX / 2});
        crate::config::write_private(&home.join("token.json"), grant.to_string().as_bytes())
            .unwrap();
        Api::new(home)
    }

    /// A server that answers 503 to the first `failures` requests, then `{"ok": true}`.
    async fn flaky(failures: usize) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let base = serve(move |request, stream| {
            let hit = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(request.header("authorization"), Some("Bearer access"));
                if hit < failures {
                    respond(stream, 503, &[("retry-after", "0")], "").await;
                } else {
                    respond(stream, 200, &[], r#"{"ok": true}"#).await;
                }
            }
        })
        .await;
        (base, hits)
    }

    #[tokio::test]
    async fn reads_are_retried_after_transient_errors() {
        let (base, hits) = flaky(2).await;
        let value = api()
            .json("Test", |http, token| http.get(&base).bearer_auth(token))
            .await
            .unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_are_bounded() {
        let (base, hits) = flaky(usize::MAX).await;
        let error = api()
            .json("Test", |http, token| http.get(&base).bearer_auth(token))
            .await
            .unwrap_err();
        assert_eq!(error.code, "request_failed");
        assert_eq!(hits.load(Ordering::SeqCst), 1 + RETRIES as usize);
    }

    #[tokio::test]
    async fn writes_are_never_repeated() {
        let (base, hits) = flaky(1).await;
        let error = api()
            .json("Test", |http, token| http.post(&base).bearer_auth(token))
            .await
            .unwrap_err();
        assert_eq!(error.code, "request_failed");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lost_connections_are_retried_for_reads() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let base = serve(move |_, stream| {
            let hit = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if hit == 0 {
                    drop(stream);
                } else {
                    respond(stream, 200, &[], "{}").await;
                }
            }
        })
        .await;
        api()
            .json("Test", |http, token| http.get(&base).bearer_auth(token))
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}

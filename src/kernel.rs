//! The runtime's Jupyter server, reached through the Colab runtime proxy with
//! only the runtime's private proxy token (never the Google token).

use crate::{
    config,
    error::{Error, Result},
    google::Api,
    runtime::{self, Lease, Propagation},
};
use async_tungstenite::{
    WebSocketStream,
    tokio::TokioAdapter,
    tungstenite::{Message, handshake::derive_accept_key, protocol::Role},
};
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

const TOKEN_HEADER: &str = "x-colab-runtime-proxy-token";
const SESSION_PATH: &str = "colab-cli.ipynb";
const MESSAGE_LIMIT: usize = 64 * 1024 * 1024;
const OUTPUT_LIMIT: usize = 1024 * 1024;
const STREAM_HEAD: usize = 4 * 1024;
const STREAM_TAIL: usize = 60 * 1024;
const RICH_LIMIT: usize = 32 * 1024;
const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;
/// A long-lived process (the MCP server) reuses an idle kernel channel only
/// briefly, so a proxy-closed connection is not mistaken for a live one.
const CHANNEL_REUSE: Duration = Duration::from_secs(20);

type Socket = WebSocketStream<TokioAdapter<reqwest::Upgraded>>;

fn unavailable(context: &str) -> Error {
    Error::new(
        "runtime_unavailable",
        format!("{context}: the runtime did not respond as expected."),
        "Check `colab status`; the runtime may have been stopped or reclaimed. Run again to get a fresh runtime.",
    )
}

pub struct Kernel {
    http: reqwest::Client,
    base: url::Url,
    token: String,
    open: tokio::sync::Mutex<Option<Channel>>,
}

struct Channel {
    socket: Socket,
    session: String,
    kernel_id: String,
    last_used: Instant,
}

fn kernel_gone() -> Error {
    Error::new(
        "kernel_gone",
        "The kernel no longer exists on this runtime.",
        "Run again; a new kernel is started automatically.",
    )
}

/// Remote paths are relative to the runtime's filesystem root.
pub fn remote_parts(path: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = path
        .trim_start_matches('/')
        .split('/')
        .map(str::to_owned)
        .collect();
    if path.len() > 1024
        || path.chars().any(char::is_control)
        || parts.iter().any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(Error::input(
            "Remote paths look like content/out/result.json (no empty, '.' or '..' parts).",
        ));
    }
    Ok(parts)
}

impl Kernel {
    pub fn connect(lease: &Lease) -> Result<Self> {
        let mut base = url::Url::parse(&lease.url).map_err(|_| unavailable("Runtime address"))?;
        let host = base.host_str().unwrap_or_default().to_owned();
        let loopback =
            config::allow_loopback() && ["127.0.0.1", "localhost"].contains(&host.as_str());
        if !(base.scheme() == "https" || (base.scheme() == "http" && loopback))
            || !base.username().is_empty()
            || base.query().is_some()
        {
            return Err(Error::new(
                "unsafe_runtime",
                "The runtime address is not an HTTPS server.",
                "Do not continue; report this response.",
            ));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .http1_only()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| Error::network("Runtime client"))?;
        Ok(Self {
            http,
            base,
            token: lease.token.clone(),
            open: tokio::sync::Mutex::new(None),
        })
    }

    fn url(&self, path: &str) -> Result<url::Url> {
        self.base
            .join(path)
            .map_err(|_| Error::input("Invalid runtime path"))
    }

    fn file_url(&self, root: &str, parts: &[String]) -> Result<url::Url> {
        let mut url = self.url(root)?;
        url.path_segments_mut()
            .map_err(|_| Error::input("Invalid runtime path"))?
            .pop_if_empty()
            .extend(parts);
        Ok(url)
    }

    async fn json(
        &self,
        method: reqwest::Method,
        url: url::Url,
        body: Option<Value>,
        context: &str,
    ) -> Result<Value> {
        let mut request = self
            .http
            .request(method, url)
            .timeout(Duration::from_secs(120))
            .header(TOKEN_HEADER, &self.token)
            .header("accept", "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| unavailable(context))?;
        let status = response.status().as_u16();
        if status == 404 {
            return Err(Error::new(
                "not_found",
                format!("{context}: not found on the runtime."),
                "Check the path with `colab ls`.",
            ));
        }
        if !(200..300).contains(&status) {
            return Err(unavailable(context));
        }
        let bytes = crate::google::read_limited(response, 16 * 1024 * 1024, context).await?;
        crate::google::parse_json(&bytes)
    }

    /// Reuse this tool's kernel session so variables persist between runs.
    pub async fn session(&self) -> Result<String> {
        let sessions = self
            .json(
                reqwest::Method::GET,
                self.url("api/sessions")?,
                None,
                "Kernel sessions",
            )
            .await?;
        let existing = sessions
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["path"] == SESSION_PATH && s["kernel"]["name"] == "python3");
        let session = match existing {
            Some(session) => session.clone(),
            None => {
                self.json(
                    reqwest::Method::POST,
                    self.url("api/sessions")?,
                    Some(json!({"kernel": {"name": "python3"}, "name": "colab-cli", "path": SESSION_PATH, "type": "notebook"})),
                    "Kernel start",
                )
                .await?
            }
        };
        let id = session["kernel"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if id.is_empty()
            || id.len() > 64
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(unavailable("Kernel start"));
        }
        Ok(id)
    }

    async fn channel(&self, kernel_id: &str) -> Result<(Socket, String)> {
        let session = uuid::Uuid::new_v4().to_string();
        let mut url = self.url(&format!("api/kernels/{kernel_id}/channels"))?;
        url.query_pairs_mut().append_pair("session_id", &session);
        let key = base64::engine::general_purpose::STANDARD.encode(uuid::Uuid::new_v4().as_bytes());
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(30))
            .header(TOKEN_HEADER, &self.token)
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", &key)
            .send()
            .await
            .map_err(|_| unavailable("Kernel connection"))?;
        if response.status().as_u16() == 404 {
            return Err(kernel_gone());
        }
        let accepted = response
            .headers()
            .get("sec-websocket-accept")
            .is_some_and(|v| v.as_bytes() == derive_accept_key(key.as_bytes()).as_bytes());
        if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS || !accepted {
            return Err(unavailable("Kernel connection"));
        }
        let upgraded = response
            .upgrade()
            .await
            .map_err(|_| unavailable("Kernel connection"))?;
        let config = async_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MESSAGE_LIMIT))
            .max_frame_size(Some(MESSAGE_LIMIT));
        Ok((
            WebSocketStream::from_raw_socket(
                TokioAdapter::new(upgraded),
                Role::Client,
                Some(config),
            )
            .await,
            session,
        ))
    }

    /// Reuse a recent open channel, else the slot's known kernel, else look
    /// the kernel session up (starting one if needed).
    async fn channel_to(&self, known: Option<&str>) -> Result<Channel> {
        if let Some(open) = self.open.lock().await.take().filter(|c| {
            c.last_used.elapsed() < CHANNEL_REUSE && known.is_none_or(|id| id == c.kernel_id)
        }) {
            return Ok(open);
        }
        if let Some(id) = known {
            match self.channel(id).await {
                Ok((socket, session)) => {
                    return Ok(Channel {
                        socket,
                        session,
                        kernel_id: id.to_owned(),
                        last_used: Instant::now(),
                    });
                }
                Err(error) if error.code == "kernel_gone" => {}
                Err(error) => return Err(error),
            }
        }
        let started = Instant::now();
        let id = self.session().await?;
        config::trace("session", started);
        let (socket, session) = self.channel(&id).await?;
        Ok(Channel {
            socket,
            session,
            kernel_id: id,
            last_used: Instant::now(),
        })
    }

    /// Open the kernel channel ahead of a run, so it can overlap other work.
    pub async fn prepare(&self, known: Option<&str>) -> Result<()> {
        let channel = self.channel_to(known).await?;
        *self.open.lock().await = Some(channel);
        Ok(())
    }

    /// Run one cell and return the result and the kernel that ran it.
    /// Drive-mount credential requests are answered through the Colab backend
    /// with the user's Google sign-in, never inside the kernel.
    /// `full` keeps every output unshortened, for writing results into a notebook.
    pub async fn execute(
        &self,
        api: &Api,
        lease: &Lease,
        code: &str,
        timeout: Duration,
        full: bool,
    ) -> Result<(Value, String)> {
        let started = Instant::now();
        let mut channel = self.channel_to(lease.kernel_id.as_deref()).await?;
        config::trace("channel", started);
        let request = |session: &str| {
            let msg_id = uuid::Uuid::new_v4().to_string();
            let request = json!({
                "header": {"msg_id": msg_id, "msg_type": "execute_request", "session": session, "username": "colab-cli",
                           "version": "5.3", "date": chrono::Utc::now().to_rfc3339()},
                "parent_header": {}, "metadata": {}, "buffers": [], "channel": "shell",
                "content": {"code": code, "silent": false, "store_history": true, "user_expressions": {}, "allow_stdin": true, "stop_on_error": true},
            });
            (msg_id, request)
        };
        let (mut msg_id, mut payload) = request(&channel.session);
        // A failed send never reached the kernel, so one fresh channel is safe.
        if channel
            .socket
            .send(Message::Text(payload.to_string().into()))
            .await
            .is_err()
        {
            channel = self.channel_to(None).await?;
            (msg_id, payload) = request(&channel.session);
            channel
                .socket
                .send(Message::Text(payload.to_string().into()))
                .await
                .map_err(|_| unavailable("Kernel connection"))?;
        }
        let Channel {
            mut socket,
            session,
            kernel_id,
            ..
        } = channel;
        let message = |channel: &str, kind: &str, parent: Value, content: Value| {
            json!({
                "header": {"msg_id": uuid::Uuid::new_v4().to_string(), "msg_type": kind, "session": session,
                           "username": "colab-cli", "version": "5.3", "date": chrono::Utc::now().to_rfc3339()},
                "parent_header": parent, "metadata": {}, "buffers": [], "channel": channel, "content": content,
            })
        };
        let mut outputs = Outputs {
            full,
            ..Outputs::default()
        };
        let mut reply: Option<Value> = None;
        let mut idle = false;
        let deadline = tokio::time::Instant::now() + timeout;
        while reply.is_none() || !idle {
            let next = match tokio::time::timeout_at(deadline, socket.next()).await {
                Err(_) => break,
                Ok(None) => return Err(Error::outcome_unknown("Kernel connection")),
                Ok(Some(message)) => {
                    message.map_err(|_| Error::outcome_unknown("Kernel connection"))?
                }
            };
            let Message::Text(text) = next else { continue };
            let Ok(incoming) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            let kind = incoming["header"]["msg_type"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let answer = match kind.as_str() {
                "colab_request" => {
                    let requested = incoming["content"]["request"]["authType"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let result = if requested == "dfs_ephemeral" {
                        runtime::propagate(api, &lease.endpoint, &requested)
                            .await
                            .unwrap_or(Propagation::Failed)
                    } else {
                        Propagation::Failed
                    };
                    match result {
                        Propagation::Done => {}
                        Propagation::Consent(url) => outputs.consent = Some(url),
                        Propagation::Failed => outputs.declined.push(requested),
                    }
                    let id = incoming["metadata"]["colab_msg_id"].clone();
                    Some(json!({"value": {"type": "colab_reply", "colab_msg_id": id}}))
                }
                "input_request" => {
                    outputs.input_requested = true;
                    Some(json!({"value": ""}))
                }
                _ => None,
            };
            if let Some(content) = answer {
                let response = message("stdin", "input_reply", incoming["header"].clone(), content);
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .map_err(|_| Error::outcome_unknown("Kernel connection"))?;
                continue;
            }
            if incoming["parent_header"]["msg_id"].as_str() != Some(msg_id.as_str()) {
                continue;
            }
            let content = incoming.get("content").cloned().unwrap_or(Value::Null);
            match kind.as_str() {
                "execute_reply" => reply = Some(content),
                "status" => idle |= content["execution_state"] == "idle",
                other => outputs.push(other, content),
            }
        }
        config::trace("executed", started);
        *self.open.lock().await = Some(Channel {
            socket,
            session: session.clone(),
            kernel_id: kernel_id.clone(),
            last_used: Instant::now(),
        });
        Ok((outputs.finish(reply), kernel_id))
    }

    /// Close a kept channel (end of a CLI command).
    pub async fn close(&self) {
        if let Some(mut channel) = self.open.lock().await.take() {
            let _ = channel.socket.close(None).await;
        }
    }

    /// Kernel by known id, or the session's kernel.
    pub async fn kernel_id(&self, known: Option<&str>) -> Result<String> {
        if let Some(id) = known
            && self.state(id).await.is_ok()
        {
            return Ok(id.to_owned());
        }
        self.session().await
    }

    /// `idle`, `busy` or `starting`.
    pub async fn state(&self, kernel_id: &str) -> Result<String> {
        let kernel = self
            .json(
                reqwest::Method::GET,
                self.url(&format!("api/kernels/{kernel_id}"))?,
                None,
                "Kernel state",
            )
            .await
            .map_err(|error| {
                if error.code == "not_found" {
                    kernel_gone()
                } else {
                    error
                }
            })?;
        Ok(kernel["execution_state"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned())
    }

    /// Interrupt the running cell (like the stop button), or restart the kernel.
    pub async fn signal(&self, kernel_id: &str, action: &str) -> Result<()> {
        let url = self.url(&format!("api/kernels/{kernel_id}/{action}"))?;
        let response = self
            .http
            .post(url)
            .timeout(Duration::from_secs(60))
            .header(TOKEN_HEADER, &self.token)
            // The runtime proxy answers 411 to a POST without Content-Length,
            // and an empty body alone does not send one.
            .header("content-length", "0")
            .send()
            .await
            .map_err(|_| unavailable("Kernel control"))?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            404 => Err(kernel_gone()),
            status => Err(unavailable(&format!("Kernel control (HTTP {status})"))),
        }
    }

    /// Up to `limit` bytes from the end of a runtime file, plus its total size.
    pub async fn tail(&self, path: &str, limit: usize) -> Result<(Vec<u8>, Option<u64>)> {
        let url = self.file_url("files/", &remote_parts(path)?)?;
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(60))
            .header(TOKEN_HEADER, &self.token)
            .header("range", format!("bytes=-{limit}"))
            .send()
            .await
            .map_err(|_| unavailable("Runtime file"))?;
        let status = response.status().as_u16();
        if status == 404 {
            return Err(Error::new(
                "not_found",
                format!("{path} does not exist on the runtime."),
                "Check the path with `colab ls`.",
            ));
        }
        if status == 416 {
            return Ok((Vec::new(), Some(0)));
        }
        if !(200..300).contains(&status) {
            return Err(unavailable("Runtime file"));
        }
        let total = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse().ok())
            .or_else(|| (status == 200).then(|| response.content_length()).flatten());
        // Without range support, keep only the end while streaming.
        let mut kept: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            kept.extend(chunk.map_err(|_| unavailable("Runtime file"))?.iter());
            while kept.len() > limit {
                kept.pop_front();
            }
        }
        Ok((kept.into_iter().collect(), total))
    }

    pub async fn list(&self, path: &str) -> Result<Value> {
        let url = if path.trim_matches('/').is_empty() {
            self.url("api/contents")?
        } else {
            self.file_url("api/contents/", &remote_parts(path)?)?
        };
        let mut url = url;
        url.query_pairs_mut().append_pair("content", "1");
        let listing = self
            .json(reqwest::Method::GET, url, None, "Runtime listing")
            .await?;
        if listing["type"] != "directory" {
            return Ok(
                json!({"path": listing["path"], "type": listing["type"], "size": listing["size"], "last_modified": listing["last_modified"]}),
            );
        }
        let entries: Vec<Value> = listing["content"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| json!({"name": e["name"], "type": e["type"], "size": e["size"], "last_modified": e["last_modified"]}))
            .collect();
        Ok(json!({"path": listing["path"], "entries": entries}))
    }

    /// Stream a runtime file to a local path (no size limit, no buffering).
    pub async fn download(&self, path: &str, out: &Path, force: bool) -> Result<Value> {
        let url = self.file_url("files/", &remote_parts(path)?)?;
        let response = self
            .http
            .get(url)
            .header(TOKEN_HEADER, &self.token)
            .send()
            .await
            .map_err(|_| unavailable("Runtime download"))?;
        match response.status().as_u16() {
            200 => {}
            404 => {
                return Err(Error::new(
                    "not_found",
                    format!("{path} does not exist on the runtime."),
                    "Check the path with `colab ls`.",
                ));
            }
            _ => return Err(unavailable("Runtime download")),
        }
        let media_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = write_stream(response, out, force).await?;
        Ok(json!({"saved": out, "bytes": bytes, "media_type": media_type, "source": path}))
    }

    /// Upload through the Jupyter contents API, chunked for large files.
    pub async fn upload(&self, local: &Path, remote: &str) -> Result<Value> {
        let parts = remote_parts(remote)?;
        let mut file = tokio::fs::File::open(local).await?;
        let size = file.metadata().await?.len();
        let name = parts.last().cloned().unwrap_or_default();
        let url = self.file_url("api/contents/", &parts)?;
        let chunks = (size as usize).div_ceil(UPLOAD_CHUNK).max(1);
        for index in 1..=chunks {
            let mut buffer = vec![0u8; UPLOAD_CHUNK];
            let mut filled = 0;
            while filled < buffer.len() {
                use tokio::io::AsyncReadExt;
                let read = file.read(&mut buffer[filled..]).await?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            buffer.truncate(filled);
            let mut body = json!({
                "name": name, "path": parts.join("/"), "type": "file", "format": "base64",
                "content": base64::engine::general_purpose::STANDARD.encode(&buffer),
            });
            if chunks > 1 {
                body["chunk"] = json!(if index == chunks { -1 } else { index as i64 });
            }
            self.json(
                reqwest::Method::PUT,
                url.clone(),
                Some(body),
                "Runtime upload",
            )
            .await?;
        }
        Ok(json!({"uploaded": parts.join("/"), "bytes": size}))
    }
}

/// Write a response body to `out`, refusing to replace a file unless asked.
pub async fn write_stream(response: reqwest::Response, out: &Path, force: bool) -> Result<u64> {
    if out.exists() && !force {
        return Err(Error::new(
            "file_exists",
            format!("{} already exists.", out.display()),
            "Pass --force to replace it, or choose another --out path.",
        ));
    }
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    let partial = out.with_extension(format!("partial-{}", uuid::Uuid::new_v4().simple()));
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut stream = response.bytes_stream();
    let mut bytes = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(Error::network("Download"));
            }
        };
        file.write_all(&chunk).await?;
        bytes += chunk.len() as u64;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&partial, out).await?;
    Ok(bytes)
}

#[derive(Default)]
pub struct Outputs {
    items: Vec<Value>,
    size: usize,
    truncated: bool,
    omitted_rich: usize,
    consent: Option<String>,
    declined: Vec<String>,
    input_requested: bool,
    full: bool,
}

/// `text` already carries streams and tracebacks; keep only what it lacks.
fn compact(items: Vec<Value>) -> Value {
    Value::Array(
        items
            .into_iter()
            .filter(|item| item["output_type"] != "stream")
            .map(|mut item| {
                if let Some(object) = item.as_object_mut().filter(|o| o["output_type"] == "error") {
                    object.remove("traceback");
                }
                item
            })
            .filter(|item| {
                // A result whose only data is the plain text already in `text`.
                !(item["output_type"] == "execute_result"
                    && item["data"]
                        .as_object()
                        .is_some_and(|d| d.len() == 1 && d.contains_key("text/plain")))
            })
            .collect(),
    )
}

/// Keep the head and the most recent tail of a long text.
pub fn clip(text: &str) -> (String, bool) {
    if text.len() <= STREAM_HEAD + STREAM_TAIL {
        return (text.to_owned(), false);
    }
    let head = (0..=STREAM_HEAD)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    let tail = (text.len() - STREAM_TAIL..text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    let omitted = text[head..tail].chars().count();
    (
        format!(
            "{}\n...[{omitted} characters omitted]...\n{}",
            &text[..head],
            &text[tail..]
        ),
        true,
    )
}

impl Outputs {
    pub fn push(&mut self, kind: &str, content: Value) {
        let field = |name: &str| content.get(name).cloned().unwrap_or(Value::Null);
        let output = match kind {
            "stream" => {
                let name = field("name");
                let text = content["text"].as_str().unwrap_or_default();
                if let Some(Value::String(existing)) = self
                    .items
                    .last_mut()
                    .filter(|last| last["output_type"] == "stream" && last["name"] == name)
                    .and_then(|last| last.get_mut("text"))
                {
                    existing.push_str(text);
                    if !self.full && existing.len() > 4 * (STREAM_HEAD + STREAM_TAIL) {
                        *existing = clip(existing).0;
                        self.truncated = true;
                    }
                    return;
                }
                self.items
                    .push(json!({"output_type": "stream", "name": name, "text": text}));
                return;
            }
            "execute_result" => {
                json!({"output_type": "execute_result", "data": self.rich(field("data")),
                                       "metadata": field("metadata"), "execution_count": field("execution_count")})
            }
            "display_data" => {
                json!({"output_type": "display_data", "data": self.rich(field("data")), "metadata": field("metadata")})
            }
            "error" => {
                json!({"output_type": "error", "ename": field("ename"), "evalue": field("evalue"), "traceback": field("traceback")})
            }
            "clear_output" => {
                self.items.clear();
                self.size = 0;
                return;
            }
            _ => return,
        };
        let size = output.to_string().len();
        if self.size + size
            > if self.full {
                32 * OUTPUT_LIMIT
            } else {
                OUTPUT_LIMIT
            }
        {
            self.truncated = true;
            return;
        }
        self.size += size;
        self.items.push(output);
    }

    fn rich(&mut self, data: Value) -> Value {
        let Value::Object(bundle) = data else {
            return data;
        };
        let mut kept = Map::new();
        for (mime, value) in bundle {
            let size = value.to_string().len();
            if self.full
                || mime == "text/plain"
                || (!mime.starts_with("image/") && size <= RICH_LIMIT)
            {
                kept.insert(mime, value);
            } else {
                self.omitted_rich += 1;
                kept.insert(mime, format!("[omitted: {size} bytes]").into());
            }
        }
        Value::Object(kept)
    }

    pub fn finish(mut self, reply: Option<Value>) -> Value {
        let full = self.full;
        for output in self.items.iter_mut().filter(|_| !full) {
            if let Some(Value::String(text)) = output.get_mut("text") {
                let (clipped, cut) = clip(text);
                *text = clipped;
                self.truncated |= cut;
            }
        }
        let ansi = regex::Regex::new("\x1b\\[[0-9;]*[A-Za-z]").expect("valid pattern");
        let mut text = String::new();
        for output in &self.items {
            match output["output_type"].as_str() {
                Some("stream") => text.push_str(output["text"].as_str().unwrap_or_default()),
                Some("execute_result" | "display_data") => {
                    if let Some(plain) = output["data"]["text/plain"].as_str() {
                        text.push_str(plain);
                        text.push('\n');
                    }
                }
                Some("error") => {
                    for line in output["traceback"].as_array().into_iter().flatten() {
                        text.push_str(&ansi.replace_all(line.as_str().unwrap_or_default(), ""));
                        text.push('\n');
                    }
                }
                _ => {}
            }
        }
        let status = reply
            .as_ref()
            .map_or("timeout", |r| r["status"].as_str().unwrap_or("unknown"))
            .to_owned();
        let mut result = json!({
            "status": status,
            "execution_count": reply.as_ref().map_or(Value::Null, |r| r["execution_count"].clone()),
            "text": text,
            "outputs": if full { Value::Array(self.items) } else { compact(self.items) },
            "truncated": self.truncated,
        });
        let mut notes = Vec::new();
        if reply.is_none() {
            notes.push("The timeout elapsed; the code is still running on the runtime. Check on it with another short run.");
        }
        if let Some(url) = self.consent {
            result["authorization_url"] = url.into();
            notes.push("Drive access needs a browser approval for this runtime: open authorization_url while signed in to the same Google account, approve, then run again.");
        }
        if !self.declined.is_empty() {
            result["declined_requests"] = self.declined.into();
            notes.push("A runtime authorization request was declined or failed; code that needed it may have failed.");
        }
        if self.omitted_rich > 0 {
            notes.push("Images and large rich outputs were omitted; save them to files and fetch them with `colab download`.");
        }
        if self.truncated {
            notes.push("Long output was shortened to its beginning and most recent part.");
        }
        if self.input_requested {
            notes.push("The code asked for interactive input; an empty value was sent.");
        }
        if !notes.is_empty() {
            result["note"] = notes.join(" ").into();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_streams_keep_their_end_and_images_are_summarized() {
        let mut outputs = Outputs::default();
        for step in 0..40_000 {
            outputs.push(
                "stream",
                json!({"name": "stdout", "text": format!("step {step}\n")}),
            );
        }
        outputs.push(
            "display_data",
            json!({"data": {"image/png": "iVBOR", "text/plain": "<Figure>"}, "metadata": {}}),
        );
        let result = outputs.finish(Some(json!({"status": "ok", "execution_count": 1})));
        let text = result["text"].as_str().unwrap();
        assert!(text.starts_with("step 0\n") && text.contains("step 39999\n"));
        assert!(text.len() < 70 * 1024);
        // Compact results leave streams to `text`; rich data stays in outputs.
        assert_eq!(result["outputs"].as_array().unwrap().len(), 1);
        assert_eq!(result["outputs"][0]["data"]["text/plain"], "<Figure>");
        assert!(
            result["outputs"][0]["data"]["image/png"]
                .as_str()
                .unwrap()
                .starts_with("[omitted")
        );
        assert!(
            result["text"]
                .as_str()
                .unwrap()
                .ends_with("step 39999\n<Figure>\n")
        );
    }

    #[test]
    fn timeouts_and_errors_are_reported() {
        let mut outputs = Outputs::default();
        outputs.push("error", json!({"ename": "ValueError", "evalue": "bad", "traceback": ["\u{1b}[0;31mValueError\u{1b}[0m: bad"]}));
        let result = outputs.finish(Some(json!({"status": "error", "execution_count": 2})));
        assert_eq!(result["text"], "ValueError: bad\n");
        assert!(result["outputs"][0].get("traceback").is_none());
        let timed_out = Outputs::default().finish(None);
        assert_eq!(timed_out["status"], "timeout");
        assert!(
            timed_out["note"]
                .as_str()
                .unwrap()
                .contains("still running")
        );
    }

    #[test]
    fn remote_paths_reject_traversal() {
        assert_eq!(
            remote_parts("/content/out/a.json").unwrap(),
            ["content", "out", "a.json"]
        );
        for bad in ["content/../etc/passwd", "content//a", "", "a/./b"] {
            assert!(remote_parts(bad).is_err(), "{bad}");
        }
    }
}

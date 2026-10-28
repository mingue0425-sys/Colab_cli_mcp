//! Operations shared by the CLI and the MCP server.

use crate::{
    config, drive,
    error::{Error, Result},
    google::Api,
    jobs,
    kernel::{self, Kernel},
    runtime::{self, Hardware, Lease},
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

/// Errors that mean a cached runtime no longer answers, raised before any code
/// was sent, so the runtime can be looked up again once.
const STALE: [&str; 2] = ["runtime_unavailable", "kernel_gone"];

/// Use a running runtime; a cached one that stopped answering is looked up
/// again once. (A macro rather than an async closure, so futures stay `Send`.)
macro_rules! on_runtime {
    ($app:expr, $slot:expr, $endpoint:expr, |$kernel:ident, $lease:ident| $body:expr) => {{
        let mut outcome = Err(Error::network("Runtime"));
        for attempt in 0..2 {
            let $lease =
                runtime::existing(&$app.api, &$app.home, $slot, $endpoint, attempt == 0).await?;
            let $kernel = $app.kernel(&$lease).await?;
            let result: Result<_> = async { $body }.await;
            match result {
                Err(error) if attempt == 0 && $lease.cached && STALE.contains(&error.code) => {
                    $app.drop_kernel(&$lease).await;
                }
                other => {
                    $app.release(&$kernel).await;
                    let _ = runtime::touch(&$app.home, &$lease.endpoint);
                    outcome = other;
                    break;
                }
            }
        }
        outcome
    }};
}

/// Where `save_to_drive` puts a file: a folder path or folder ID, a name,
/// and optionally another signed-in account's Drive.
#[derive(Default)]
pub struct DriveTarget<'a> {
    pub folder: Option<&'a str>,
    pub parent: Option<&'a str>,
    pub name: Option<&'a str>,
    pub account: Option<&'a str>,
}

pub struct App {
    pub api: Api,
    /// The account in use and its state directory.
    pub account: String,
    pub home: std::path::PathBuf,
    kernels: tokio::sync::Mutex<HashMap<String, Arc<Kernel>>>,
    /// The MCP server keeps kernel channels open between calls; the CLI closes them.
    keep_open: bool,
}

pub fn timeout(seconds: u64) -> Result<Duration> {
    if !(1..=3600).contains(&seconds) {
        return Err(Error::input("timeout must be between 1 and 3600 seconds."));
    }
    Ok(Duration::from_secs(seconds))
}

fn runtime_info(slot: &str, lease: &Lease, allocated: bool) -> Value {
    json!({"slot": slot, "endpoint": lease.endpoint, "accelerator": lease.accelerator, "newly_allocated": allocated})
}

fn append_note(result: &mut Value, note: &str) {
    let combined = match result["note"].as_str() {
        Some(existing) => format!("{existing} {note}"),
        None => note.to_owned(),
    };
    result["note"] = combined.into();
}

/// The job supervisor's latest record, if it wrote one (jobs started by
/// colab-cli 0.3 and earlier have none).
async fn job_stats(kernel: &Kernel, dir: &str) -> Option<Value> {
    let (bytes, _) = kernel
        .tail(&format!("{dir}/stats.json"), 64 * 1024)
        .await
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

/// Stream one runtime file into Drive.
async fn save_file(
    kernel: &Kernel,
    drive_api: &Api,
    remote: &str,
    name: &str,
    parent: Option<&str>,
) -> Result<Value> {
    let response = kernel.open_file(remote).await?;
    let size = response.content_length().ok_or_else(|| {
        Error::new(
            "unsupported_file",
            "The runtime did not report the file size.",
            "Download it with `colab download`, then `colab drive upload`.",
        )
    })?;
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        // The runtime guesses types from extensions (".pt" becomes a
        // game-console format); pass only common ones, else Drive decides.
        .filter(|v| {
            ["text/", "image/", "audio/", "video/"]
                .iter()
                .any(|p| v.starts_with(p))
                || [
                    "application/json",
                    "application/pdf",
                    "application/zip",
                    "application/gzip",
                ]
                .contains(v)
        })
        .map(str::to_owned);
    let body = reqwest::Body::wrap_stream(response.bytes_stream());
    drive::upload_body(drive_api, name, parent, mime.as_deref(), size, body).await
}

/// Save a runtime folder into a Drive folder named `name` (under `parent`, or
/// My Drive), recreating its subfolders.
async fn save_tree(
    kernel: &Kernel,
    drive_api: &Api,
    remote: &str,
    name: &str,
    parent: Option<&str>,
) -> Result<Value> {
    let files = kernel.walk(remote).await?;
    let base = parent.unwrap_or("root");
    let top = drive::ensure_folder_in(drive_api, base, name).await?["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let mut folders = HashMap::from([(String::new(), top.clone())]);
    let mut bytes = 0u64;
    for rel in &files {
        let (dir, file) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
        let folder = match folders.get(dir) {
            Some(id) => id.clone(),
            None => {
                let id = drive::ensure_folder_in(drive_api, &top, dir).await?["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                folders.insert(dir.to_owned(), id.clone());
                id
            }
        };
        let saved = save_file(
            kernel,
            drive_api,
            &format!("{remote}/{rel}"),
            file,
            Some(&folder),
        )
        .await?;
        bytes += saved["size"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or(saved["size"].as_u64())
            .unwrap_or(0);
    }
    Ok(json!({"folder_id": top, "name": name, "files": files.len(), "bytes": bytes}))
}

/// Every file under a local directory (symlinks skipped), relative to it.
fn local_tree(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| Error::input("Bad local path."))?
                    .to_string_lossy()
                    .into_owned();
                files.push(rel);
            }
        }
        if files.len() > kernel::MAX_TREE_FILES {
            return Err(kernel::too_many_files());
        }
    }
    files.sort();
    Ok(files)
}

fn kernel_busy() -> Error {
    Error::new(
        "kernel_busy",
        "The previous run in this slot is still executing.",
        "Wait and retry, or stop it with `colab interrupt`. Use `colab job start` for long work so the kernel stays free.",
    )
}

impl App {
    pub fn new(keep_open: bool) -> Result<Self> {
        let account = config::account();
        let home = config::account_home(&account);
        config::private_dir(&home)?;
        Ok(Self {
            api: Api::new(home.clone()),
            account,
            home,
            kernels: tokio::sync::Mutex::new(HashMap::new()),
            keep_open,
        })
    }

    async fn kernel(&self, lease: &Lease) -> Result<Arc<Kernel>> {
        let key = format!("{}|{}", lease.endpoint, lease.token);
        let mut kernels = self.kernels.lock().await;
        if let Some(kernel) = kernels.get(&key) {
            return Ok(kernel.clone());
        }
        let kernel = Arc::new(Kernel::connect(lease)?);
        kernels.retain(|k, _| !k.starts_with(&format!("{}|", lease.endpoint)));
        kernels.insert(key, kernel.clone());
        Ok(kernel)
    }

    async fn release(&self, kernel: &Kernel) {
        if !self.keep_open {
            kernel.close().await;
        }
    }

    async fn drop_kernel(&self, lease: &Lease) {
        self.kernels
            .lock()
            .await
            .retain(|k, _| !k.starts_with(&format!("{}|", lease.endpoint)));
        if let Some(slot) = &lease.slot {
            let _ = runtime::forget(&self.home, slot);
        }
    }

    /// Lease the slot's runtime (cache first) and run one cell on it.
    async fn execute(
        &self,
        slot: &str,
        hardware: Option<Hardware>,
        code: &str,
        timeout: Duration,
        full: bool,
    ) -> Result<(Value, Lease, bool)> {
        for attempt in 0..2 {
            let started = std::time::Instant::now();
            let (lease, allocated) =
                runtime::lease(&self.api, &self.home, slot, hardware.clone(), attempt == 0).await?;
            config::trace("lease", started);
            let kernel = self.kernel(&lease).await?;
            let retry = |error: &Error| attempt == 0 && lease.cached && STALE.contains(&error.code);
            // Confirm an unconfirmed cached runtime while the kernel channel opens,
            // and send no code to a runtime Colab no longer lists.
            if !lease.confirmed {
                let (present, opened) = tokio::join!(
                    runtime::confirm(&self.api, &self.home, &lease),
                    kernel.prepare(lease.kernel_id.as_deref())
                );
                if !present? {
                    self.drop_kernel(&lease).await;
                    continue;
                }
                if let Err(error) = opened {
                    if retry(&error) {
                        self.drop_kernel(&lease).await;
                        continue;
                    }
                    return Err(error);
                }
            }
            // The last run timed out: refuse to queue behind it (the kernel id
            // may have been forgotten, so look the session up if needed).
            if runtime::slot_busy(&self.home, slot) {
                let id = match &lease.kernel_id {
                    Some(id) => id.clone(),
                    None => match kernel.session().await {
                        Ok(id) => id,
                        Err(error) if retry(&error) => {
                            self.drop_kernel(&lease).await;
                            continue;
                        }
                        Err(error) => return Err(error),
                    },
                };
                match kernel.state(&id).await {
                    Ok(state) if state == "busy" => return Err(kernel_busy()),
                    Err(error) if retry(&error) => {
                        self.drop_kernel(&lease).await;
                        continue;
                    }
                    _ => {}
                }
            }
            match kernel.execute(&self.api, &lease, code, timeout, full).await {
                Ok((result, kernel_id)) => {
                    let busy = result["status"] == "timeout";
                    runtime::remember_kernel(&self.home, slot, &lease.endpoint, &kernel_id, busy)?;
                    runtime::touch(&self.home, &lease.endpoint)?;
                    crate::reaper::ensure_running(&self.home, &self.account);
                    self.release(&kernel).await;
                    return Ok((result, lease, allocated));
                }
                Err(error) if retry(&error) => self.drop_kernel(&lease).await,
                Err(error) => {
                    self.drop_kernel(&lease).await;
                    return Err(error);
                }
            }
        }
        Err(Error::network("Runtime"))
    }

    pub async fn run(
        &self,
        code: &str,
        slot: &str,
        hardware: Option<Hardware>,
        seconds: u64,
        full_outputs: bool,
    ) -> Result<Value> {
        let timeout = timeout(seconds)?;
        if code.trim().is_empty() {
            return Err(Error::input("No code to run."));
        }
        let (mut result, lease, allocated) = self
            .execute(slot, hardware, code, timeout, full_outputs)
            .await?;
        result["runtime"] = runtime_info(slot, &lease, allocated);
        if allocated {
            append_note(
                &mut result,
                "A new runtime was started; stop it with `colab stop` when finished.",
            );
        }
        Ok(result)
    }

    pub async fn stop(
        &self,
        slot: Option<&str>,
        endpoint: Option<&str>,
        all: bool,
    ) -> Result<Value> {
        let endpoints: Vec<String> = if all {
            runtime::assignments(&self.api)
                .await?
                .iter()
                .filter_map(|a| a["endpoint"].as_str().map(str::to_owned))
                .collect()
        } else {
            let slot = if endpoint.is_none() {
                Some(slot.unwrap_or(runtime::DEFAULT_SLOT))
            } else {
                slot
            };
            vec![
                runtime::existing(&self.api, &self.home, slot, endpoint, false)
                    .await?
                    .endpoint,
            ]
        };
        for endpoint in &endpoints {
            runtime::stop(&self.api, &self.home, endpoint, "colab stop").await?;
            self.kernels
                .lock()
                .await
                .retain(|k, _| !k.starts_with(&format!("{endpoint}|")));
        }
        Ok(json!({"stopped": endpoints}))
    }

    /// Stop managed runtimes idle for longer than the configured minutes:
    /// no colab-cli use, no busy kernel, no kernel activity and no running job.
    /// Returns the stopped endpoints and how many managed runtimes remain.
    pub async fn reap_once(&self) -> Result<(Vec<String>, usize)> {
        let minutes = runtime::idle_stop_minutes(&self.home) as i64;
        if minutes == 0 {
            return Ok((Vec::new(), 0));
        }
        let mut watching = 0;
        let now = chrono::Utc::now().timestamp();
        let listed = runtime::assignments(&self.api).await?;
        // Checked every minute, so a runtime Colab ends is recorded promptly.
        runtime::reconcile(&self.home, &listed)?;
        let managed = runtime::managed(&self.home);
        let mut stopped = Vec::new();
        for assignment in listed {
            let Some((_, endpoint, last_used)) = managed
                .iter()
                .find(|(_, endpoint, _)| assignment["endpoint"] == endpoint.as_str())
            else {
                continue; // Started elsewhere (for example in the browser): not ours to stop.
            };
            watching += 1;
            if now - last_used < minutes * 60 {
                continue;
            }
            let Some(lease) = runtime::lease_for(&assignment) else {
                continue;
            };
            let kernel = Kernel::connect(&lease)?;
            // If the runtime cannot be inspected, leave it running.
            let Ok((busy, activity)) = kernel.activity().await else {
                continue;
            };
            if busy || activity.is_some_and(|t| now - t < minutes * 60) {
                continue;
            }
            let running_job = match kernel.list(jobs::ROOT).await {
                Ok(listing) => {
                    let mut running = false;
                    for entry in listing["entries"].as_array().into_iter().flatten() {
                        let id = entry["name"].as_str().unwrap_or_default();
                        if matches!(kernel.tail(&format!("{}/exit_code", jobs::dir(id)), 16).await, Err(ref e) if e.code == "not_found")
                        {
                            running = true;
                            break;
                        }
                    }
                    running
                }
                Err(error) if error.code == "not_found" => false,
                Err(_) => continue,
            };
            if running_job {
                continue;
            }
            runtime::stop(&self.api, &self.home, endpoint, "idle auto-stop").await?;
            stopped.push(endpoint.clone());
            watching -= 1;
        }
        Ok((stopped, watching))
    }

    /// Interrupt the running cell, or restart the kernel (clears variables).
    pub async fn signal(
        &self,
        action: &str,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        on_runtime!(self, slot, endpoint, |kernel, lease| {
            let id = kernel.kernel_id(lease.kernel_id.as_deref()).await?;
            kernel.signal(&id, action).await?;
            if let Some(slot) = &lease.slot {
                runtime::remember_kernel(&self.home, slot, &lease.endpoint, &id, false)?;
            }
            let state = kernel.state(&id).await.unwrap_or_else(|_| "unknown".into());
            let mut result =
                json!({"action": action, "kernel_state": state, "endpoint": lease.endpoint});
            if action == "restart" {
                result["note"] = "The kernel restarted; variables and imports are gone. Files and background jobs are kept.".into();
            }
            Ok(result)
        })
    }

    pub async fn list(
        &self,
        path: &str,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        on_runtime!(self, slot, endpoint, |kernel, _lease| kernel
            .list(path)
            .await)
    }

    pub async fn download(
        &self,
        remote: &str,
        out: Option<&Path>,
        force: bool,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        let parts = kernel::remote_parts(remote)?;
        let default = self.default_out(parts.last().map(String::as_str).unwrap_or("download.bin"));
        let out = out.map(Path::to_path_buf).unwrap_or(default);
        on_runtime!(self, slot, endpoint, |kernel, _lease| {
            if !kernel.is_dir(remote).await? {
                return kernel.download(remote, &out, force).await;
            }
            if out.is_file() {
                return Err(Error::input(format!(
                    "{} is a file; a folder needs a folder destination.",
                    out.display()
                )));
            }
            let files = kernel.walk(remote).await?;
            let mut bytes = 0;
            for rel in &files {
                let saved = kernel
                    .download(&format!("{remote}/{rel}"), &out.join(rel), force)
                    .await?;
                bytes += saved["bytes"].as_u64().unwrap_or(0);
            }
            Ok(json!({"saved": out, "files": files.len(), "bytes": bytes, "source": remote}))
        })
    }

    pub async fn upload(
        &self,
        local: &Path,
        remote: &str,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        if !local.is_dir() {
            return on_runtime!(self, slot, endpoint, |kernel, _lease| kernel
                .upload(local, remote)
                .await);
        }
        let files = local_tree(local)?;
        on_runtime!(self, slot, endpoint, |kernel, _lease| {
            kernel.mkdirs(remote).await?;
            let mut made = std::collections::HashSet::new();
            let mut bytes = 0;
            for rel in &files {
                if let Some((dir, _)) = rel.rsplit_once('/')
                    && made.insert(dir.to_owned())
                {
                    kernel.mkdirs(&format!("{remote}/{dir}")).await?;
                }
                let sent = kernel
                    .upload(&local.join(rel), &format!("{remote}/{rel}"))
                    .await?;
                bytes += sent["bytes"].as_u64().unwrap_or(0);
            }
            Ok(json!({"uploaded": remote, "files": files.len(), "bytes": bytes}))
        })
    }

    /// Copy a runtime file, or a folder with its layout, straight into Drive
    /// (no Drive mount or approval needed).
    pub async fn save_to_drive(
        &self,
        remote: &str,
        to: &DriveTarget<'_>,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        let parts = kernel::remote_parts(remote)?;
        let name = to
            .name
            .map(str::to_owned)
            .unwrap_or_else(|| parts.last().cloned().unwrap_or_default());
        // The runtime belongs to this account; the Drive may be another's.
        let other = match to.account.filter(|a| *a != self.account) {
            Some(account) => Some(crate::accounts::api(account)?),
            None => None,
        };
        let drive_api = other.as_ref().unwrap_or(&self.api);
        let parent = drive::destination(drive_api, to.parent, to.folder).await?;
        on_runtime!(self, slot, endpoint, |kernel, _lease| {
            let mut saved = if kernel.is_dir(remote).await? {
                save_tree(&kernel, drive_api, remote, &name, parent.as_deref()).await?
            } else {
                save_file(&kernel, drive_api, remote, &name, parent.as_deref()).await?
            };
            saved["source"] = remote.into();
            if let Some(account) = to.account.filter(|_| other.is_some()) {
                saved["drive_account"] = account.into();
                saved["drive_email"] = drive_api.auth.email().into();
            }
            Ok(saved)
        })
    }

    /// Wait until a job finishes (or the wait ends) and return its status.
    pub async fn job_wait(
        &self,
        id: &str,
        seconds: u64,
        lines: usize,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        let deadline = std::time::Instant::now() + timeout(seconds)?;
        loop {
            let status = self.job_status(id, lines, slot, endpoint).await?;
            if status["state"] != "running" {
                return Ok(status);
            }
            if std::time::Instant::now() + Duration::from_secs(5) > deadline {
                let mut status = status;
                status["note"] =
                    "Still running after the wait; call job wait or job status again.".into();
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// CLI downloads land in the working directory unless COLAB_CLI_DOWNLOADS is set.
    pub fn default_out(&self, name: &str) -> std::path::PathBuf {
        match std::env::var_os("COLAB_CLI_DOWNLOADS") {
            Some(dir) => Path::new(&dir).join(name),
            None => std::path::PathBuf::from(name),
        }
    }

    /// Start long work as a detached process on the slot's runtime.
    pub async fn job_start(
        &self,
        source: &str,
        shell: bool,
        name: Option<&str>,
        slot: &str,
        hardware: Option<Hardware>,
    ) -> Result<Value> {
        if source.trim().is_empty() {
            return Err(Error::input("No job code or command."));
        }
        let id = jobs::new_id(name)?;
        let snippet = jobs::start_snippet(&id, source, shell)?;
        let (result, lease, allocated) = self
            .execute(slot, hardware, &snippet, Duration::from_secs(60), false)
            .await?;
        let started = jobs::parse_line(&result)?;
        let mut out = json!({
            "job": id, "pid": started["pid"], "state": "running",
            "log": format!("{}/log", jobs::dir(&id).trim_start_matches('/')),
            "runtime": runtime_info(slot, &lease, allocated),
            "next_step": format!("Check progress with `colab job status {id}`; stop it with `colab job stop {id}`."),
        });
        if allocated {
            append_note(
                &mut out,
                "A new runtime was started; stop it with `colab stop` when the job is done.",
            );
        }
        Ok(out)
    }

    /// Job state and the end of its log, read from files (works while the kernel is busy).
    pub async fn job_status(
        &self,
        id: &str,
        lines: usize,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        jobs::check_id(id)?;
        let dir = jobs::dir(id);
        on_runtime!(self, slot, endpoint, |kernel, _lease| {
            let exit_code = match kernel.tail(&format!("{dir}/exit_code"), 64).await {
                Ok((bytes, _)) => String::from_utf8_lossy(&bytes).trim().parse::<i64>().ok(),
                Err(error) if error.code == "not_found" => None,
                Err(error) => return Err(error),
            };
            let limit = (lines.clamp(1, 2000) * 400).min(256 * 1024);
            let (log, total) = match kernel.tail(&format!("{dir}/log"), limit).await {
                Ok(found) => found,
                Err(error) if error.code == "not_found" => {
                    return Err(Error::new(
                        "not_found",
                        format!("No job {id} on this runtime."),
                        "List jobs with `colab job list`.",
                    ));
                }
                Err(error) => return Err(error),
            };
            let cut = total.is_some_and(|total| total > log.len() as u64);
            let stats = job_stats(&kernel, &dir).await;
            let mut status = jobs::describe(exit_code, stats.as_ref(), now());
            if let Ok(resources) = kernel.resources().await {
                if status["state"] == "running" && kernel::low_memory(&resources) {
                    status["warning"] = "The runtime has under 10% of its RAM free; the job (or the kernel) may soon be killed for lack of memory. Consider stopping parallel work.".into();
                }
                status["runtime_resources"] = resources;
            }
            status["job"] = id.into();
            status["log_bytes"] = total.into();
            status["log_tail"] = jobs::last_lines(&log, lines, cut).into();
            if stats.is_some() {
                status["memory_log"] =
                    format!("{}/memory.jsonl", dir.trim_start_matches('/')).into();
            }
            Ok(status)
        })
    }

    pub async fn job_list(&self, slot: Option<&str>, endpoint: Option<&str>) -> Result<Value> {
        on_runtime!(self, slot, endpoint, |kernel, _lease| {
            let listing = match kernel.list(jobs::ROOT).await {
                Ok(listing) => listing,
                Err(error) if error.code == "not_found" => return Ok(json!({"jobs": []})),
                Err(error) => return Err(error),
            };
            let mut entries: Vec<Value> = listing["entries"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|e| e["type"] == "directory")
                .collect();
            entries.sort_by(|a, b| {
                b["last_modified"]
                    .as_str()
                    .cmp(&a["last_modified"].as_str())
            });
            let mut jobs = Vec::new();
            for entry in entries.iter().take(20) {
                let id = entry["name"].as_str().unwrap_or_default();
                let exit_code = kernel
                    .tail(&format!("{}/exit_code", jobs::dir(id)), 64)
                    .await
                    .ok()
                    .and_then(|(bytes, _)| {
                        String::from_utf8_lossy(&bytes).trim().parse::<i64>().ok()
                    });
                // Stats only matter for telling running from lost, and why a job was killed.
                let stats = match exit_code {
                    None | Some(137) => job_stats(&kernel, &jobs::dir(id)).await,
                    Some(_) => None,
                };
                let described = jobs::describe(exit_code, stats.as_ref(), now());
                jobs.push(json!({"job": id, "state": described["state"], "exit_code": exit_code,
                                 "likely_cause": described["likely_cause"], "last_modified": entry["last_modified"]}));
            }
            Ok(json!({"jobs": jobs}))
        })
    }

    pub async fn job_stop(
        &self,
        id: &str,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        jobs::check_id(id)?;
        let snippet = jobs::stop_snippet(id)?;
        on_runtime!(self, slot, endpoint, |kernel, lease| {
            let kernel_id = kernel.kernel_id(lease.kernel_id.as_deref()).await?;
            if kernel.state(&kernel_id).await? == "busy" {
                return Err(kernel_busy());
            }
            let (result, _) = kernel
                .execute(&self.api, &lease, &snippet, Duration::from_secs(30), false)
                .await?;
            jobs::parse_line(&result)
        })
    }

    /// Run a Drive notebook's code cells in order on one runtime, optionally
    /// saving the outputs back into the notebook.
    pub async fn run_notebook(
        &self,
        id: &str,
        slot: &str,
        hardware: Option<Hardware>,
        seconds: u64,
        save: bool,
        continue_on_error: bool,
    ) -> Result<Value> {
        let timeout = timeout(seconds)?;
        let mut notebook = drive::read_notebook(&self.api, id).await?;
        let mut cells = Vec::new();
        let mut stopped_early = false;
        let mut runtime = None;
        let count = notebook["cells"].as_array().map_or(0, Vec::len);
        for index in 0..count {
            let cell = &notebook["cells"][index];
            if cell["cell_type"] != "code" {
                continue;
            }
            let source = match &cell["source"] {
                Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect::<String>(),
                Value::String(text) => text.clone(),
                _ => String::new(),
            };
            if source.trim().is_empty() {
                continue;
            }
            let hardware = if runtime.is_none() {
                hardware.clone()
            } else {
                None
            };
            let (result, lease, allocated) =
                self.execute(slot, hardware, &source, timeout, true).await?;
            if runtime.is_none() {
                runtime = Some(runtime_info(slot, &lease, allocated));
            }
            let status = result["status"].as_str().unwrap_or("unknown").to_owned();
            notebook["cells"][index]["outputs"] = result["outputs"].clone();
            notebook["cells"][index]["execution_count"] = result["execution_count"].clone();
            let (output, _) = kernel::clip(result["text"].as_str().unwrap_or_default());
            let tail: String = output
                .chars()
                .rev()
                .take(2000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let mut summary = json!({"index": index, "status": status, "output": tail});
            if let Some(url) = result.get("authorization_url") {
                summary["authorization_url"] = url.clone();
            }
            cells.push(summary);
            if status != "ok" && (!continue_on_error || status == "timeout") {
                stopped_early = true;
                break;
            }
        }
        let saved = if save {
            Some(drive::save_notebook(&self.api, id, &notebook).await?)
        } else {
            None
        };
        let mut result = json!({"cells": cells, "completed": !stopped_early, "saved": saved, "runtime": runtime});
        if stopped_early {
            append_note(
                &mut result,
                "Execution stopped at the first cell that did not finish with status ok.",
            );
        }
        if runtime
            .as_ref()
            .is_some_and(|r| r["newly_allocated"] == true)
        {
            append_note(
                &mut result,
                "A new runtime was started; stop it with `colab stop` when finished.",
            );
        }
        Ok(result)
    }
}

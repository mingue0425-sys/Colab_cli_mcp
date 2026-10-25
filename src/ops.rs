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
                    outcome = other;
                    break;
                }
            }
        }
        outcome
    }};
}

pub struct App {
    pub api: Api,
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

fn kernel_busy() -> Error {
    Error::new(
        "kernel_busy",
        "The previous run in this slot is still executing.",
        "Wait and retry, or stop it with `colab interrupt`. Use `colab job start` for long work so the kernel stays free.",
    )
}

impl App {
    pub fn new(keep_open: bool) -> Result<Self> {
        let home = config::home();
        config::private_dir(&home)?;
        Ok(Self {
            api: Api::new(home.clone()),
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
            runtime::stop(&self.api, &self.home, endpoint).await?;
            self.kernels
                .lock()
                .await
                .retain(|k, _| !k.starts_with(&format!("{endpoint}|")));
        }
        Ok(json!({"stopped": endpoints}))
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
        on_runtime!(self, slot, endpoint, |kernel, _lease| kernel
            .download(remote, &out, force)
            .await)
    }

    pub async fn upload(
        &self,
        local: &Path,
        remote: &str,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        on_runtime!(self, slot, endpoint, |kernel, _lease| kernel
            .upload(local, remote)
            .await)
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
            Ok(json!({
                "job": id, "state": jobs::state(exit_code), "exit_code": exit_code,
                "log_bytes": total, "log_tail": jobs::last_lines(&log, lines, cut),
            }))
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
                jobs.push(json!({"job": id, "state": jobs::state(exit_code), "exit_code": exit_code, "last_modified": entry["last_modified"]}));
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

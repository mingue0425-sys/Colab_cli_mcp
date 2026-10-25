//! Operations shared by the CLI and the MCP server.

use crate::{
    config, drive,
    error::{Error, Result},
    google::Api,
    kernel::{self, Kernel},
    runtime::{self, Hardware, Lease},
};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

pub struct App {
    pub api: Api,
    pub home: std::path::PathBuf,
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

impl App {
    pub fn new() -> Result<Self> {
        let home = config::home();
        config::private_dir(&home)?;
        Ok(Self {
            api: Api::new(home.clone()),
            home,
        })
    }

    pub async fn run(
        &self,
        code: &str,
        slot: &str,
        hardware: Option<Hardware>,
        seconds: u64,
    ) -> Result<Value> {
        let timeout = timeout(seconds)?;
        if code.trim().is_empty() {
            return Err(Error::input("No code to run."));
        }
        let (lease, allocated) = runtime::lease(&self.api, &self.home, slot, hardware).await?;
        let kernel = Kernel::connect(&lease)?;
        let mut result = kernel
            .execute(&self.api, &lease, code, timeout, false)
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
                runtime::existing(&self.api, &self.home, slot, endpoint)
                    .await?
                    .endpoint,
            ]
        };
        for endpoint in &endpoints {
            runtime::stop(&self.api, &self.home, endpoint).await?;
        }
        Ok(json!({"stopped": endpoints}))
    }

    async fn kernel(&self, slot: Option<&str>, endpoint: Option<&str>) -> Result<Kernel> {
        let lease = runtime::existing(&self.api, &self.home, slot, endpoint).await?;
        Kernel::connect(&lease)
    }

    pub async fn list(
        &self,
        path: &str,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        self.kernel(slot, endpoint).await?.list(path).await
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
        self.kernel(slot, endpoint)
            .await?
            .download(remote, &out, force)
            .await
    }

    pub async fn upload(
        &self,
        local: &Path,
        remote: &str,
        slot: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Value> {
        self.kernel(slot, endpoint)
            .await?
            .upload(local, remote)
            .await
    }

    /// CLI downloads land in the working directory; MCP sets COLAB_CLI_DOWNLOADS.
    pub fn default_out(&self, name: &str) -> std::path::PathBuf {
        match std::env::var_os("COLAB_CLI_DOWNLOADS") {
            Some(dir) => Path::new(&dir).join(name),
            None => std::path::PathBuf::from(name),
        }
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
        let (lease, allocated) = runtime::lease(&self.api, &self.home, slot, hardware).await?;
        let kernel = Kernel::connect(&lease)?;
        let mut cells = Vec::new();
        let mut stopped_early = false;
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
            let result = kernel
                .execute(&self.api, &lease, &source, timeout, true)
                .await?;
            let status = result["status"].as_str().unwrap_or("unknown").to_owned();
            notebook["cells"][index]["outputs"] = result["outputs"].clone();
            notebook["cells"][index]["execution_count"] = result["execution_count"].clone();
            let text = result["text"].as_str().unwrap_or_default();
            let tail: String = text
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
        let mut result = json!({
            "cells": cells, "completed": !stopped_early, "saved": saved,
            "runtime": runtime_info(slot, &lease, allocated),
        });
        if stopped_early {
            append_note(
                &mut result,
                "Execution stopped at the first cell that did not finish with status ok.",
            );
        }
        if allocated {
            append_note(
                &mut result,
                "A new runtime was started; stop it with `colab stop` when finished.",
            );
        }
        Ok(result)
    }
}

//! Colab runtime leases: allocate, find, stop, and authorize Drive on a runtime.
//! A "slot" names one runtime; the slot's last accelerator is remembered so
//! follow-up runs reach the same machine without repeating flags.

use crate::{
    config,
    error::{Error, Result},
    google::{Api, status_error},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

pub const DEFAULT_SLOT: &str = "default";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hardware {
    /// DEFAULT (CPU), GPU or TPU.
    pub variant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accelerator: Option<String>,
    #[serde(default)]
    pub high_ram: bool,
}

impl Hardware {
    pub fn cpu() -> Self {
        Self {
            variant: "DEFAULT".into(),
            accelerator: None,
            high_ram: false,
        }
    }

    pub fn from_flags(
        gpu: Option<&str>,
        tpu: Option<&str>,
        high_ram: bool,
    ) -> Result<Option<Self>> {
        let check = |name: &str| {
            if name.is_empty()
                || name.len() > 16
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(Error::input(
                    "Accelerator names look like T4, L4, A100, H100, V5E1 or V6E1.",
                ));
            }
            Ok(name.to_ascii_uppercase())
        };
        Ok(match (gpu, tpu) {
            (Some(_), Some(_)) => {
                return Err(Error::input("Choose either a GPU or a TPU, not both."));
            }
            (Some(gpu), None) => Some(Self {
                variant: "GPU".into(),
                accelerator: Some(check(gpu)?),
                high_ram,
            }),
            (None, Some(tpu)) => Some(Self {
                variant: "TPU".into(),
                accelerator: Some(check(tpu)?),
                high_ram,
            }),
            (None, None) if high_ram => Some(Self {
                high_ram,
                ..Self::cpu()
            }),
            (None, None) => None,
        })
    }

    fn query(&self) -> Vec<(&'static str, String)> {
        let mut query = vec![("variant", self.variant.clone())];
        if let Some(accelerator) = &self.accelerator {
            query.push(("accelerator", accelerator.clone()));
        }
        if self.high_ram {
            query.push(("shape", "hm".into()));
        }
        query
    }
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub endpoint: String,
    pub url: String,
    pub token: String,
    pub accelerator: String,
    pub expires_at: i64,
    /// The slot this runtime belongs to, when known.
    pub slot: Option<String>,
    /// The slot's last kernel, reused to skip a session lookup.
    pub kernel_id: Option<String>,
    /// True when served from the local cache without asking Colab.
    pub cached: bool,
    /// Colab listed this runtime recently. A runtime stopped elsewhere keeps
    /// answering for a while, so a cached one is re-confirmed before use.
    pub confirmed: bool,
}

/// How long a cached runtime is trusted without asking Colab again.
const CONFIRM_FOR: i64 = 60;

/// What a slot remembers. The proxy token is private (file mode 0600) and
/// only reused until shortly before Colab's stated expiry.
#[derive(Default, Clone, Serialize, Deserialize)]
struct SlotRecord {
    endpoint: String,
    hardware: Option<Hardware>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    token: String,
    #[serde(default)]
    expires_at: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    kernel_id: String,
    /// The last foreground run timed out, so the kernel may still be busy.
    #[serde(default)]
    busy: bool,
    /// When Colab last listed this runtime.
    #[serde(default)]
    confirmed_at: i64,
    /// When colab-cli last used this runtime; drives the idle auto-stop.
    #[serde(default)]
    last_used: i64,
    /// When colab-cli allocated this runtime (0 when it found one already running).
    #[serde(default)]
    started_at: i64,
}

fn events_path(home: &Path) -> std::path::PathBuf {
    home.join("events.jsonl")
}

/// Append a runtime event (started, stopped, gone) to the account's history.
pub fn record_event(home: &Path, mut event: Value) {
    use std::io::Write;
    event["time"] = chrono::Utc::now().to_rfc3339().into();
    event["at"] = now().into();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path(home))
    {
        let _ = writeln!(file, "{event}");
    }
}

/// Events from the last `seconds`, newest last (the file's tail only).
pub fn recent_events(home: &Path, seconds: i64) -> Vec<Value> {
    let text = std::fs::read_to_string(events_path(home)).unwrap_or_default();
    let mut events: Vec<Value> = text
        .lines()
        .rev()
        .take(200)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|e| e["at"].as_i64().is_some_and(|at| now() - at <= seconds))
        .collect();
    events.reverse();
    events
}

/// Record that Colab ended a runtime colab-cli did not stop, with how long it
/// ran and sat idle, which hints at the reason (idle reclaim, lifetime limit).
fn note_gone(home: &Path, slot: &str, record: &SlotRecord) {
    let minutes = |since: i64| (since > 0).then(|| (now() - since) / 60);
    record_event(
        home,
        json!({
            "event": "runtime_gone", "slot": slot, "endpoint": record.endpoint,
            "accelerator": record.hardware.as_ref().and_then(|h| h.accelerator.clone()),
            "age_minutes": minutes(record.started_at), "idle_minutes": minutes(record.last_used),
            "note": "Colab ended this runtime; colab-cli did not stop it. Long age suggests the runtime lifetime limit; long idle suggests an idle reclaim.",
        }),
    );
}

/// Forget managed runtimes Colab no longer lists, recording each as gone.
/// The slot keeps its hardware for the next run.
pub fn reconcile(home: &Path, listed: &[Value]) -> Result<()> {
    let mut slots = load_slots(home);
    let mut changed = false;
    for (slot, record) in slots.iter_mut() {
        if record.endpoint.is_empty()
            || listed
                .iter()
                .any(|a| a["endpoint"] == record.endpoint.as_str())
        {
            continue;
        }
        note_gone(home, slot, record);
        *record = SlotRecord {
            hardware: record.hardware.take(),
            ..SlotRecord::default()
        };
        changed = true;
    }
    if changed {
        save_slots(home, &slots)?;
    }
    Ok(())
}

/// The longest idle auto-stop (one week). Larger values are capped so the
/// seconds arithmetic can never wrap negative and stop runtimes at once.
pub const MAX_IDLE_STOP: u64 = 7 * 24 * 60;

/// Minutes without use after which colab-cli stops a runtime it manages
/// (0 disables). `COLAB_CLI_IDLE_STOP` overrides the saved setting.
pub fn idle_stop_minutes(home: &Path) -> u64 {
    let minutes = std::env::var("COLAB_CLI_IDLE_STOP")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| {
            std::fs::read(home.join("config.json"))
                .ok()
                .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                .and_then(|config| config["idle_stop_minutes"].as_u64())
        })
        .unwrap_or(30);
    minutes.min(MAX_IDLE_STOP)
}

pub fn set_idle_stop_minutes(home: &Path, minutes: u64) -> Result<()> {
    if minutes > MAX_IDLE_STOP {
        return Err(Error::input(format!(
            "Idle auto-stop takes 0 (off) to {MAX_IDLE_STOP} minutes (one week)."
        )));
    }
    config::write_private(
        &home.join("config.json"),
        &serde_json::to_vec_pretty(&json!({"idle_stop_minutes": minutes}))?,
    )
}

/// Mark a runtime as just used.
pub fn touch(home: &Path, endpoint: &str) -> Result<()> {
    let mut slots = load_slots(home);
    let mut changed = false;
    for record in slots
        .values_mut()
        .filter(|r| r.endpoint == endpoint && r.last_used < now() - 10)
    {
        record.last_used = now();
        changed = true;
    }
    if changed {
        save_slots(home, &slots)?;
    }
    Ok(())
}

/// Runtimes colab-cli manages, as (slot, endpoint, last used).
pub fn managed(home: &Path) -> Vec<(String, String, i64)> {
    load_slots(home)
        .into_iter()
        .filter(|(_, r)| !r.endpoint.is_empty())
        .map(|(slot, r)| (slot, r.endpoint, r.last_used))
        .collect()
}

/// A lease for a listed assignment, for maintenance work such as the idle check.
pub fn lease_for(assignment: &Value) -> Option<Lease> {
    lease_from(assignment)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl SlotRecord {
    fn cached_lease(&self, slot: &str) -> Option<Lease> {
        (!self.url.is_empty() && !self.token.is_empty() && self.expires_at > now() + 120).then(
            || Lease {
                endpoint: self.endpoint.clone(),
                url: self.url.clone(),
                token: self.token.clone(),
                accelerator: self
                    .hardware
                    .as_ref()
                    .and_then(|h| h.accelerator.clone())
                    .unwrap_or_else(|| "NONE".into()),
                expires_at: self.expires_at,
                slot: Some(slot.to_owned()),
                kernel_id: (!self.kernel_id.is_empty()).then(|| self.kernel_id.clone()),
                cached: true,
                confirmed: self.confirmed_at > now() - CONFIRM_FOR,
            },
        )
    }
}

fn slots_path(home: &Path) -> std::path::PathBuf {
    home.join("slots.json")
}

fn load_slots(home: &Path) -> BTreeMap<String, SlotRecord> {
    std::fs::read(slots_path(home))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

fn save_slots(home: &Path, slots: &BTreeMap<String, SlotRecord>) -> Result<()> {
    config::write_private(&slots_path(home), &serde_json::to_vec_pretty(slots)?)
}

pub fn check_slot(slot: &str) -> Result<()> {
    if slot.is_empty()
        || slot.len() > 32
        || !slot
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::input(
            "Slot names use 1-32 letters, digits, '-' or '_'.",
        ));
    }
    Ok(())
}

/// Colab identifies a runtime by a 44-character notebook hash.
pub fn notebook_hash(slot: &str) -> String {
    let base = format!("colab-cli-{slot}");
    format!("{base}{}", ".".repeat(44 - base.len()))
}

fn lease_from(value: &Value) -> Option<Lease> {
    let info = value.get("runtimeProxyInfo")?;
    Some(Lease {
        endpoint: value["endpoint"].as_str()?.to_owned(),
        url: info["url"].as_str()?.to_owned(),
        token: info["token"].as_str()?.to_owned(),
        accelerator: value["accelerator"].as_str().unwrap_or("NONE").to_owned(),
        expires_at: now() + info["tokenExpiresInSeconds"].as_i64().unwrap_or(3600),
        slot: None,
        kernel_id: None,
        cached: false,
        confirmed: true,
    })
}

/// Ask Colab whether a cached runtime is still assigned; remember a yes.
pub async fn confirm(api: &Api, home: &Path, lease: &Lease) -> Result<bool> {
    let listed = assignments(api).await?;
    let present = listed
        .iter()
        .any(|a| a["endpoint"] == lease.endpoint.as_str());
    reconcile(home, &listed)?;
    let mut slots = load_slots(home);
    let mut changed = false;
    for record in slots.values_mut().filter(|r| r.endpoint == lease.endpoint) {
        if present {
            record.confirmed_at = now();
        } else {
            record.url.clear();
            record.token.clear();
            record.kernel_id.clear();
            record.expires_at = 0;
        }
        changed = true;
    }
    if changed {
        save_slots(home, &slots)?;
    }
    Ok(present)
}

/// Remember a slot's kernel and whether its last run may still be executing.
pub fn remember_kernel(
    home: &Path,
    slot: &str,
    endpoint: &str,
    kernel_id: &str,
    busy: bool,
) -> Result<()> {
    let mut slots = load_slots(home);
    if let Some(record) = slots.get_mut(slot).filter(|r| r.endpoint == endpoint)
        && (record.kernel_id != kernel_id || record.busy != busy)
    {
        record.kernel_id = kernel_id.to_owned();
        record.busy = busy;
        save_slots(home, &slots)?;
    }
    Ok(())
}

pub fn slot_busy(home: &Path, slot: &str) -> bool {
    load_slots(home).get(slot).is_some_and(|r| r.busy)
}

/// Drop cached connection details after the runtime stopped answering.
pub fn forget(home: &Path, slot: &str) -> Result<()> {
    let mut slots = load_slots(home);
    if let Some(record) = slots.get_mut(slot) {
        record.url.clear();
        record.token.clear();
        record.kernel_id.clear();
        record.expires_at = 0;
        save_slots(home, &slots)?;
    }
    Ok(())
}

fn colab(path: &str) -> String {
    format!("{}/tun/m/{path}", crate::config::colab_url())
}

pub async fn assignments(api: &Api) -> Result<Vec<Value>> {
    let body = api
        .json("Runtime list", |http, token| {
            http.get(colab("assignments"))
                .bearer_auth(token)
                .query(&[("authuser", "0")])
                .header("accept", "application/json")
                .header("x-colab-client-agent", "colab-cli")
        })
        .await?;
    Ok(body["assignments"].as_array().cloned().unwrap_or_default())
}

pub async fn usage(api: &Api) -> Result<Value> {
    api.json("Compute usage", |http, token| {
        http.get(colab("ccu-info"))
            .bearer_auth(token)
            .query(&[("authuser", "0")])
            .header("accept", "application/json")
            .header("x-colab-client-agent", "colab-cli")
    })
    .await
}

/// Runtimes without their private proxy tokens, labelled with known slots.
pub async fn status(api: &Api, home: &Path) -> Result<Value> {
    let slots = load_slots(home);
    let idle_stop = idle_stop_minutes(home) as i64;
    let listed = assignments(api).await?;
    reconcile(home, &listed)?;
    // Resource use per runtime, fetched in parallel; a runtime that does not
    // answer simply has none.
    let resources = futures_util::future::join_all(listed.iter().map(|a| async move {
        let kernel = crate::kernel::Kernel::connect(&lease_for(a)?).ok()?;
        kernel.resources().await.ok()
    }))
    .await;
    let runtimes: Vec<Value> = listed
        .iter()
        .zip(resources)
        .map(|(a, resources)| {
            let endpoint = a["endpoint"].as_str().unwrap_or_default();
            json!({
                "endpoint": endpoint,
                "slot": slots.iter().find(|(_, r)| r.endpoint == endpoint).map(|(s, _)| s.clone()),
                "accelerator": a["accelerator"],
                "variant": match a["variant"].as_i64() { Some(1) => "GPU", Some(2) => "TPU", _ => "DEFAULT" },
                "high_ram": a["machineShape"].as_i64() == Some(1),
                "idle_minutes": slots.values().find(|r| r.endpoint == endpoint && r.last_used > 0).map(|r| (now() - r.last_used) / 60),
                "resources": resources,
            })
        })
        .collect();
    let gone: Vec<Value> = recent_events(home, 24 * 3600)
        .into_iter()
        .filter(|e| e["event"] == "runtime_gone")
        .collect();
    let mut out = json!({
        "runtimes": runtimes,
        "auto_stop": if idle_stop > 0 {
            json!(format!("Runtimes started by colab-cli stop after {idle_stop} idle minutes (no runs, no busy kernel, no running job). Change with `colab idle-stop MINUTES`; 0 disables."))
        } else {
            json!("Disabled; stop runtimes with `colab stop`.")
        },
    });
    if !gone.is_empty() {
        out["gone_recently"] = gone.into();
    }
    Ok(out)
}

/// Find or allocate the slot's runtime. Without explicit hardware, the slot's
/// previous hardware is reused while that runtime still exists.
pub async fn lease(
    api: &Api,
    home: &Path,
    slot: &str,
    hardware: Option<Hardware>,
    use_cache: bool,
) -> Result<(Lease, bool)> {
    check_slot(slot)?;
    let mut slots = load_slots(home);
    if use_cache
        && let Some(record) = slots.get(slot)
        && hardware
            .as_ref()
            .is_none_or(|h| record.hardware.as_ref() == Some(h))
        && let Some(lease) = record.cached_lease(slot)
    {
        return Ok((lease, false));
    }
    let hardware = match hardware {
        Some(hardware) => hardware,
        None => slots
            .get(slot)
            .and_then(|record| record.hardware.clone())
            .unwrap_or_else(Hardware::cpu),
    };
    let nbh = notebook_hash(slot);
    let mut query = hardware.query();
    query.push(("nbh", nbh));
    query.push(("authuser", "0".into()));
    let assign = |http: &reqwest::Client, token: &str, method: reqwest::Method| {
        http.request(method, colab("assign"))
            .bearer_auth(token)
            .query(&query)
            .header("accept", "application/json")
            .header("x-colab-client-agent", "colab-cli")
    };
    let first = api
        .json("Runtime lookup", |http, token| {
            assign(http, token, reqwest::Method::GET)
        })
        .await?;
    let (lease, allocated) = match lease_from(&first) {
        Some(lease) => (lease, false),
        None => {
            let xsrf = first["token"].as_str().ok_or_else(|| {
                Error::new(
                    "runtime_unavailable",
                    "Colab did not offer a runtime.",
                    "Check `colab usage` for eligible hardware and retry.",
                )
            })?;
            let response = api
                .send("Runtime allocation", |http, token| {
                    assign(http, token, reqwest::Method::POST).header("x-goog-colab-token", xsrf)
                })
                .await?;
            let status = response.status().as_u16();
            if status == 412 {
                return Err(Error::new(
                    "too_many_runtimes",
                    "The account already has as many runtimes as Colab allows.",
                    "List them with `colab status` and stop unused ones with `colab stop --endpoint ENDPOINT`.",
                ));
            }
            if !(200..300).contains(&status) {
                let mut error = status_error(status, "Runtime allocation");
                if status == 400 || status == 403 {
                    error.code = "runtime_unavailable";
                    error.next_step = "This hardware may not be available to the account now. Check `colab usage` (eligible GPUs/TPUs, compute units) or try CPU.".into();
                }
                return Err(error);
            }
            let body =
                crate::google::read_limited(response, 1024 * 1024, "Runtime allocation").await?;
            let value = crate::google::parse_json(&body)?;
            (
                lease_from(&value).ok_or_else(|| {
                    Error::new(
                        "runtime_unavailable",
                        "Colab allocated no usable runtime.",
                        "Retry, or choose other hardware.",
                    )
                })?,
                true,
            )
        }
    };
    let previous = slots.remove(slot).unwrap_or_default();
    let same_runtime = previous.endpoint == lease.endpoint;
    if allocated {
        record_event(
            home,
            json!({"event": "runtime_started", "slot": slot, "endpoint": lease.endpoint, "accelerator": hardware.accelerator}),
        );
    }
    slots.insert(
        slot.to_owned(),
        SlotRecord {
            endpoint: lease.endpoint.clone(),
            hardware: Some(hardware),
            url: lease.url.clone(),
            token: lease.token.clone(),
            expires_at: lease.expires_at,
            kernel_id: if same_runtime {
                previous.kernel_id
            } else {
                String::new()
            },
            busy: same_runtime && previous.busy,
            confirmed_at: now(),
            last_used: now(),
            started_at: if same_runtime {
                previous.started_at
            } else if allocated {
                now()
            } else {
                0
            },
        },
    );
    save_slots(home, &slots)?;
    let record = &slots[slot];
    let lease = Lease {
        slot: Some(slot.to_owned()),
        kernel_id: (!record.kernel_id.is_empty()).then(|| record.kernel_id.clone()),
        ..lease
    };
    Ok((lease, allocated))
}

/// Find a running runtime without allocating one. The cache answers when it
/// identifies exactly one runtime; callers retry with `use_cache = false`
/// when a cached runtime no longer answers.
pub async fn existing(
    api: &Api,
    home: &Path,
    slot: Option<&str>,
    endpoint: Option<&str>,
    use_cache: bool,
) -> Result<Lease> {
    let slots = load_slots(home);
    if use_cache {
        let cached: Vec<Lease> = slots
            .iter()
            .filter(|(name, record)| match (endpoint, slot) {
                (Some(endpoint), _) => record.endpoint == endpoint,
                (None, Some(slot)) => name.as_str() == slot,
                (None, None) => true,
            })
            .filter_map(|(name, record)| record.cached_lease(name))
            .collect();
        if let [one] = cached.as_slice()
            && (one.confirmed || confirm(api, home, one).await?)
        {
            return Ok(one.clone());
        }
    }
    let list = assignments(api).await?;
    let wanted = match (endpoint, slot) {
        (Some(endpoint), _) => Some(endpoint.to_owned()),
        (None, Some(slot)) => load_slots(home).get(slot).map(|r| r.endpoint.clone()),
        (None, None) => None,
    };
    let candidates: Vec<&Value> = match &wanted {
        Some(endpoint) => list
            .iter()
            .filter(|a| a["endpoint"] == endpoint.as_str())
            .collect(),
        None => list.iter().collect(),
    };
    match candidates.as_slice() {
        [one] => {
            let mut lease = lease_from(one).ok_or_else(|| Error::network("Runtime lookup"))?;
            let mut slots = slots;
            if let Some((name, record)) =
                slots.iter_mut().find(|(_, r)| r.endpoint == lease.endpoint)
            {
                lease.slot = Some(name.clone());
                lease.kernel_id = (!record.kernel_id.is_empty()).then(|| record.kernel_id.clone());
                // Refresh the cache so the next command skips this lookup.
                record.url = lease.url.clone();
                record.token = lease.token.clone();
                record.expires_at = lease.expires_at;
                record.confirmed_at = now();
                save_slots(home, &slots)?;
            }
            Ok(lease)
        }
        [] => Err(Error::new(
            "no_runtime",
            "No matching runtime is running.",
            "Start one with `colab run`, or check `colab status`.",
        )),
        _ => Err(Error::new(
            "ambiguous_runtime",
            format!(
                "Several runtimes are running: {}.",
                candidates
                    .iter()
                    .filter_map(|a| a["endpoint"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "Pass --slot or --endpoint.",
        )),
    }
}

/// Stop a runtime; `by` says who asked (recorded in the event history).
pub async fn stop(api: &Api, home: &Path, endpoint: &str, by: &str) -> Result<()> {
    if endpoint.is_empty()
        || !endpoint
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(Error::input("Invalid runtime endpoint."));
    }
    let url = colab(&format!("unassign/{endpoint}"));
    let first = api
        .json("Runtime stop", |http, token| {
            http.get(&url)
                .bearer_auth(token)
                .query(&[("authuser", "0")])
                .header("accept", "application/json")
                .header("x-colab-client-agent", "colab-cli")
        })
        .await?;
    let xsrf = first["token"].as_str().unwrap_or_default().to_owned();
    let response = api
        .send("Runtime stop", |http, token| {
            http.post(&url)
                .bearer_auth(token)
                .query(&[("authuser", "0")])
                .header("x-goog-colab-token", &xsrf)
                .header("x-colab-client-agent", "colab-cli")
        })
        .await?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(status_error(status, "Runtime stop"));
    }
    let mut slots = load_slots(home);
    slots.retain(|_, record| record.endpoint != endpoint);
    save_slots(home, &slots)?;
    record_event(
        home,
        json!({"event": "runtime_stopped", "endpoint": endpoint, "by": by}),
    );
    Ok(())
}

pub enum Propagation {
    Done,
    Consent(String),
    Failed,
}

/// Authorize a runtime credential (for example a Drive mount). The dry run only
/// reports whether a credential is already present, so the real exchange always
/// follows; each POST needs a fresh single-use anti-forgery token.
pub async fn propagate(api: &Api, endpoint: &str, auth_type: &str) -> Result<Propagation> {
    let url = colab(&format!("credentials-propagation/{endpoint}"));
    let mut consent = None;
    for dry_run in ["true", "false"] {
        let query = [
            ("authuser", "0"),
            ("authtype", auth_type),
            ("version", "2"),
            ("dryrun", dry_run),
            ("propagate", "true"),
            ("record", "false"),
        ];
        let first = api
            .json("Drive authorization", |http, token| {
                http.get(&url)
                    .bearer_auth(token)
                    .query(&query)
                    .header("x-colab-client-agent", "colab-cli")
            })
            .await?;
        let xsrf = first["token"].as_str().unwrap_or_default().to_owned();
        let response = api
            .send("Drive authorization", |http, token| {
                http.post(&url)
                    .bearer_auth(token)
                    .query(&query)
                    .header("x-goog-colab-token", &xsrf)
                    .header("x-colab-client-agent", "colab-cli")
                    .multipart(reqwest::multipart::Form::new().text("file_id", "empty.ipynb"))
            })
            .await?;
        let ok = response.status().is_success();
        let body =
            crate::google::read_limited(response, 1024 * 1024, "Drive authorization").await?;
        let value = crate::google::parse_json(&body).unwrap_or(Value::Null);
        if dry_run == "false" && ok && value["success"] == true {
            return Ok(Propagation::Done);
        }
        if consent.is_none() {
            consent = value["unauthorized_redirect_uri"]
                .as_str()
                .filter(|u| u.starts_with("https://"))
                .map(str::to_owned);
        }
    }
    Ok(consent.map_or(Propagation::Failed, Propagation::Consent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gone_runtimes_are_recorded_once_and_slots_keep_hardware() {
        let home = std::env::temp_dir().join(format!("colab-cli-test-{}", uuid::Uuid::new_v4()));
        let gpu = Hardware::from_flags(Some("T4"), None, false).unwrap();
        let mut slots = BTreeMap::new();
        for (slot, endpoint) in [("train", "gone-1"), ("live", "here-1")] {
            slots.insert(
                slot.to_owned(),
                SlotRecord {
                    endpoint: endpoint.into(),
                    hardware: gpu.clone(),
                    started_at: now() - 3 * 3600,
                    last_used: now() - 600,
                    ..SlotRecord::default()
                },
            );
        }
        save_slots(&home, &slots).unwrap();
        let listed = [json!({"endpoint": "here-1"})];
        reconcile(&home, &listed).unwrap();
        reconcile(&home, &listed).unwrap();
        let events = recent_events(&home, 60);
        assert_eq!(events.len(), 1);
        assert_eq!(
            (
                events[0]["slot"].as_str(),
                events[0]["age_minutes"].as_i64()
            ),
            (Some("train"), Some(180))
        );
        let slots = load_slots(&home);
        assert!(slots["train"].endpoint.is_empty() && slots["train"].hardware == gpu);
        assert_eq!(slots["live"].endpoint, "here-1");
    }

    #[test]
    fn idle_stop_minutes_stay_in_range() {
        let home = std::env::temp_dir().join(format!("colab-cli-test-{}", uuid::Uuid::new_v4()));
        assert_eq!(idle_stop_minutes(&home), 30);
        set_idle_stop_minutes(&home, MAX_IDLE_STOP).unwrap();
        assert_eq!(idle_stop_minutes(&home), MAX_IDLE_STOP);
        assert_eq!(
            set_idle_stop_minutes(&home, MAX_IDLE_STOP + 1)
                .unwrap_err()
                .code,
            "bad_input"
        );
        // A hand-edited huge value must not wrap negative and stop runtimes at once.
        std::fs::write(
            home.join("config.json"),
            r#"{"idle_stop_minutes": 9223372036854775808}"#,
        )
        .unwrap();
        assert_eq!(idle_stop_minutes(&home), MAX_IDLE_STOP);
    }

    #[test]
    fn notebook_hash_is_stable_and_44_chars() {
        assert_eq!(notebook_hash("default").len(), 44);
        assert_eq!(notebook_hash("x"), notebook_hash("x"));
        assert_ne!(notebook_hash("a"), notebook_hash("b"));
        assert!(check_slot("train-1").is_ok());
        assert!(check_slot("../x").is_err());
        assert!(check_slot(&"a".repeat(33)).is_err());
    }

    #[test]
    fn hardware_flags() {
        assert_eq!(Hardware::from_flags(None, None, false).unwrap(), None);
        let gpu = Hardware::from_flags(Some("t4"), None, false)
            .unwrap()
            .unwrap();
        assert_eq!(
            (gpu.variant.as_str(), gpu.accelerator.as_deref()),
            ("GPU", Some("T4"))
        );
        assert!(Hardware::from_flags(Some("T4"), Some("V5E1"), false).is_err());
        assert!(Hardware::from_flags(Some("T4&x=1"), None, false).is_err());
    }
}

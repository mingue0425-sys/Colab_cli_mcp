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
}

#[derive(Default, Serialize, Deserialize)]
struct SlotRecord {
    endpoint: String,
    hardware: Option<Hardware>,
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
    })
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
    let runtimes: Vec<Value> = assignments(api)
        .await?
        .iter()
        .map(|a| {
            let endpoint = a["endpoint"].as_str().unwrap_or_default();
            json!({
                "endpoint": endpoint,
                "slot": slots.iter().find(|(_, r)| r.endpoint == endpoint).map(|(s, _)| s.clone()),
                "accelerator": a["accelerator"],
                "variant": match a["variant"].as_i64() { Some(1) => "GPU", Some(2) => "TPU", _ => "DEFAULT" },
                "high_ram": a["machineShape"].as_i64() == Some(1),
            })
        })
        .collect();
    Ok(json!({"runtimes": runtimes}))
}

/// Find or allocate the slot's runtime. Without explicit hardware, the slot's
/// previous hardware is reused while that runtime still exists.
pub async fn lease(
    api: &Api,
    home: &Path,
    slot: &str,
    hardware: Option<Hardware>,
) -> Result<(Lease, bool)> {
    check_slot(slot)?;
    let mut slots = load_slots(home);
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
    slots.insert(
        slot.to_owned(),
        SlotRecord {
            endpoint: lease.endpoint.clone(),
            hardware: Some(hardware),
        },
    );
    save_slots(home, &slots)?;
    Ok((lease, allocated))
}

/// Find a running runtime without allocating one.
pub async fn existing(
    api: &Api,
    home: &Path,
    slot: Option<&str>,
    endpoint: Option<&str>,
) -> Result<Lease> {
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
        [one] => lease_from(one).ok_or_else(|| Error::network("Runtime lookup")),
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

pub async fn stop(api: &Api, home: &Path, endpoint: &str) -> Result<()> {
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

//! Where colab-cli keeps its state, and which service endpoints it talks to.
//! Endpoints are overridable only so tests can use local fixtures.

use crate::error::{Error, Result};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

/// The state root. The `default` account keeps its files here directly (as
/// before accounts existed); other accounts live in `accounts/<name>/`.
pub fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os("COLAB_CLI_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    base.join(".colab-cli")
}

pub const DEFAULT_ACCOUNT: &str = "default";

/// `--account` for this process, set once by main before anything runs.
static SELECTED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn select_account(name: &str) -> Result<()> {
    check_account(name)?;
    let _ = SELECTED.set(name.to_owned());
    Ok(())
}

pub fn check_account(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(Error::input(
            "Account names use lowercase letters, digits, '-' or '_' (up to 32), e.g. work.",
        ));
    }
    Ok(())
}

/// The account in use: `--account`, else `COLAB_CLI_ACCOUNT`, else the one
/// chosen with `colab auth use`, else `default`.
pub fn account() -> String {
    if let Some(name) = SELECTED.get() {
        return name.clone();
    }
    std::env::var("COLAB_CLI_ACCOUNT")
        .ok()
        .or_else(|| std::fs::read_to_string(root().join("account")).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| check_account(name).is_ok())
        .unwrap_or_else(|| DEFAULT_ACCOUNT.to_owned())
}

/// Where an account keeps its sign-in, slots, settings and idle watcher.
pub fn account_home(name: &str) -> PathBuf {
    if name == DEFAULT_ACCOUNT {
        root()
    } else {
        root().join("accounts").join(name)
    }
}

/// Every account with a directory (`default` always).
pub fn accounts() -> Vec<String> {
    let mut names = vec![DEFAULT_ACCOUNT.to_owned()];
    if let Ok(entries) = std::fs::read_dir(root().join("accounts")) {
        let mut more: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| check_account(name).is_ok() && name != DEFAULT_ACCOUNT)
            .collect();
        more.sort();
        names.extend(more);
    }
    names
}

/// Create a directory readable only by the current user.
pub fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Write a file atomically with owner-only permissions.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, bytes)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn endpoint(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

pub fn colab_url() -> String {
    endpoint("COLAB_CLI_COLAB_URL", "https://colab.research.google.com")
}

pub fn google_api_url() -> String {
    endpoint("COLAB_CLI_GOOGLE_API_URL", "https://www.googleapis.com")
}

pub fn token_url() -> String {
    endpoint("COLAB_CLI_TOKEN_URL", "https://oauth2.googleapis.com/token")
}

pub fn auth_url() -> String {
    endpoint(
        "COLAB_CLI_AUTH_URL",
        "https://accounts.google.com/o/oauth2/v2/auth",
    )
}

/// Tests may run a kernel server on plain-HTTP loopback; real servers need TLS.
pub fn allow_loopback() -> bool {
    std::env::var("COLAB_CLI_ALLOW_LOOPBACK").is_ok_and(|v| v == "1")
}

/// `COLAB_CLI_TRACE=1` prints phase timings to stderr, for diagnosing latency.
pub fn trace(phase: &str, started: std::time::Instant) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var("COLAB_CLI_TRACE").is_ok_and(|v| v == "1")) {
        eprintln!("[trace] {phase}: {} ms", started.elapsed().as_millis());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names_and_homes() {
        for good in ["default", "work", "team-2", "a_b"] {
            assert!(check_account(good).is_ok(), "{good}");
        }
        for bad in ["", "Work", "../x", "a/b", "a b", &"x".repeat(33)] {
            assert!(check_account(bad).is_err(), "{bad}");
        }
        // The default account keeps the pre-accounts layout.
        assert_eq!(account_home(DEFAULT_ACCOUNT), root());
        assert_eq!(account_home("work"), root().join("accounts").join("work"));
    }
}

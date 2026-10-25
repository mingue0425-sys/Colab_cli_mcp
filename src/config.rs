//! Where colab-cli keeps its state, and which service endpoints it talks to.
//! Endpoints are overridable only so tests can use local fixtures.

use crate::error::Result;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("COLAB_CLI_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    base.join(".colab-cli")
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

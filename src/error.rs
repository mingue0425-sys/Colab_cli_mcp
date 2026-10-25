//! One error shape for the CLI and MCP: a stable code, what happened, and what to do next.

use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub next_step: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(
        code: &'static str,
        message: impl Into<String>,
        next_step: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            next_step: next_step.into(),
        }
    }

    pub fn input(message: impl Into<String>) -> Self {
        Self::new(
            "bad_input",
            message,
            "Check the command's --help for valid inputs.",
        )
    }

    pub fn auth() -> Self {
        Self::new(
            "auth_required",
            "Google sign-in is missing or no longer valid.",
            "Run `colab auth login` (or `colab auth import FILE`), then retry.",
        )
    }

    pub fn network(context: &str) -> Self {
        Self::new(
            "request_failed",
            format!("{context}: the request did not complete."),
            "Check the network connection and retry.",
        )
    }

    /// Used after code may have started running; never retried automatically.
    pub fn outcome_unknown(context: &str) -> Self {
        Self::new(
            "outcome_unknown",
            format!("{context}: the connection ended after the code was sent."),
            "The code may have run. Inspect the runtime state (for example with `colab run`) before repeating side effects.",
        )
    }

    pub fn to_json(&self) -> Value {
        json!({"error": {"code": self.code, "message": self.message, "next_step": self.next_step}})
    }

    pub fn exit_code(&self) -> i32 {
        match self.code {
            "auth_required" => 3,
            "bad_input" => 2,
            _ => 1,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(
            "io_error",
            format!("Local file operation failed: {error}"),
            "Check the path and permissions.",
        )
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::new(
            "invalid_json",
            format!("Unexpected JSON: {error}"),
            "Retry; if it persists, the service may have changed.",
        )
    }
}

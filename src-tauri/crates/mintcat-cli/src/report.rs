//! Output handling: human progress goes to stderr, the command result goes to stdout
//! (a single JSON object with `--json`, readable text otherwise).

use serde_json::Value;
use std::fmt;

/// Error kinds map to exit codes so agents can branch on them.
#[derive(Debug)]
pub enum CliError {
    /// A safety check refused to proceed (game running, GUI running, foreign paks, ...). Exit 3.
    Refused(String),
    /// Bad selector / unknown name / invalid input. Exit 4.
    NotFound(String),
    /// Anything else. Exit 1.
    Failed(anyhow::Error),
}

impl CliError {
    pub fn code(&self) -> i32 {
        match self {
            CliError::Failed(_) => 1,
            CliError::Refused(_) => 3,
            CliError::NotFound(_) => 4,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            CliError::Failed(_) => "failed",
            CliError::Refused(_) => "refused",
            CliError::NotFound(_) => "not_found",
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Refused(m) | CliError::NotFound(m) => write!(f, "{m}"),
            CliError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for CliError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<CliError>() {
            Ok(c) => c,
            Err(e) => CliError::Failed(e),
        }
    }
}

impl From<rusqlite::Error> for CliError {
    fn from(e: rusqlite::Error) -> Self {
        CliError::Failed(e.into())
    }
}

impl std::error::Error for CliError {}

pub type CliResult<T> = std::result::Result<T, CliError>;

pub fn refused(msg: impl Into<String>) -> CliError {
    CliError::Refused(msg.into())
}
pub fn not_found(msg: impl Into<String>) -> CliError {
    CliError::NotFound(msg.into())
}

#[derive(Clone, Copy)]
pub struct Reporter {
    pub json: bool,
    pub quiet: bool,
}

impl Reporter {
    pub fn info(&self, msg: impl AsRef<str>) {
        if !self.quiet {
            eprintln!("[mintcat-cli] {}", msg.as_ref());
        }
    }
    pub fn warn(&self, msg: impl AsRef<str>) {
        eprintln!("[mintcat-cli] warning: {}", msg.as_ref());
    }

    /// Print the command result. `text` is the human rendering.
    pub fn done(&self, value: Value, text: String) {
        if self.json {
            let mut v = value;
            if let Value::Object(ref mut m) = v {
                m.insert("ok".into(), Value::Bool(true));
            }
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        } else if !text.is_empty() {
            println!("{}", text.trim_end());
        }
    }

    pub fn fail(&self, err: &CliError) {
        if self.json {
            let v = serde_json::json!({
                "ok": false,
                "error": { "kind": err.kind(), "message": err.to_string(), "exit_code": err.code() }
            });
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        } else {
            eprintln!("error: {err}");
        }
    }
}

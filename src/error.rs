//! Error type carrying a stable process exit code.
//!
//! Exit codes are part of the public interface (scripts branch on them), so
//! never renumber an existing variant; only append new ones.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// Unclassified failure (I/O, parse, unexpected state).
    General = 1,
    /// Bad command-line usage (clap also exits 2).
    Usage = 2,
    /// No live session matches the selector.
    NotFound = 3,
    /// Selector matches more than one live session.
    Ambiguous = 4,
    /// Session is not on the config allowlist, or is the caller's own session.
    NotAllowed = 5,
    /// `wait` / `ask` / confirmation deadline passed.
    Timeout = 6,
    /// Message could not be delivered (socket missing, refused, held/denied).
    Delivery = 7,
    /// Session has no resolvable tmux pane (needed by cmd/approve/deny/attach).
    NoTmux = 8,
    /// approve/deny: the session is not showing a permission prompt.
    NoPrompt = 9,
    /// Transcript file for the session does not exist / cannot be parsed.
    Transcript = 10,
    /// Discovery failed (`claude agents --json` and registry both unusable).
    Discovery = 11,
}

#[derive(Debug)]
pub struct CliError {
    pub code: Code,
    pub msg: String,
}

impl CliError {
    pub fn new(code: Code, msg: impl Into<String>) -> Self {
        CliError {
            code,
            msg: msg.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for CliError {}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::new(Code::General, e.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(e: serde_json::Error) -> Self {
        CliError::new(Code::General, format!("json: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, CliError>;

/// Shorthand constructor: `err(Code::NotFound, format!(..))`.
pub fn err<T>(code: Code, msg: impl Into<String>) -> Result<T> {
    Err(CliError::new(code, msg))
}

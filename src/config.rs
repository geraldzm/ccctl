//! `~/.config/ccctl/config.toml` (or `$CCCTL_CONFIG`).
//!
//! Every key is optional; defaults are chosen so a missing file works.

use crate::error::{CliError, Code, Result};
use crate::util::{expand_tilde, glob_match, home};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub claude: ClaudeCfg,
    pub sockets: SocketsCfg,
    pub timeouts: TimeoutsCfg,
    pub tmux: TmuxCfg,
    pub security: SecurityCfg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeCfg {
    /// Claude Code binary used for `claude agents --json`.
    pub bin: String,
    /// Directory holding `<pid>.json` session registry files.
    pub sessions_dir: String,
    /// Directory holding `<sanitized-cwd>/<sessionId>.jsonl` transcripts.
    pub projects_dir: String,
    /// "agents" (default: `claude agents --json`, registry for extra fields)
    /// or "registry" (skip spawning claude; read registry files directly).
    pub discovery: String,
}

impl Default for ClaudeCfg {
    fn default() -> Self {
        ClaudeCfg {
            bin: "claude".into(),
            sessions_dir: "~/.claude/sessions".into(),
            projects_dir: "~/.claude/projects".into(),
            discovery: "agents".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SocketsCfg {
    /// Directories an inbox socket may live in. The registry's
    /// `messagingSocketPath` is only trusted if it is inside one of these.
    /// Empty = built-in defaults ($XDG_RUNTIME_DIR/cc-socks, /tmp/cc-socks,
    /// /tmp/cc-socks-<uid>).
    pub roots: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutsCfg {
    /// Default `wait --timeout` (seconds).
    pub wait: f64,
    /// Default `ask --timeout` (seconds).
    pub ask: f64,
    /// How long `send` watches the transcript for the delivery record (seconds).
    pub send_confirm: f64,
    /// How long `send --peer-receipt` listens for a peer_message_status frame.
    pub peer_receipt: f64,
    /// Poll interval for wait loops (milliseconds).
    pub poll_ms: u64,
    /// After a send/cmd, an `idle` status only counts once the registry
    /// status was updated after the action, or this many ms passed with no
    /// change (covers local commands that never flip to busy).
    pub settle_ms: u64,
    /// Delay between typing text and pressing Enter in `cmd` (ms).
    pub enter_delay_ms: u64,
}

impl Default for TimeoutsCfg {
    fn default() -> Self {
        TimeoutsCfg {
            wait: 600.0,
            ask: 900.0,
            send_confirm: 10.0,
            peer_receipt: 3.0,
            poll_ms: 400,
            settle_ms: 3000,
            enter_delay_ms: 200,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TmuxCfg {
    /// tmux binary (default "tmux").
    pub bin: Option<String>,
    /// Explicit server socket (`tmux -S`). Default: tmux's own default.
    pub socket: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityCfg {
    /// Session-name globs that mutating commands (send, cmd, ask, approve,
    /// deny) may target. Absent = every session allowed. `[]` = none.
    pub allowlist: Option<Vec<String>>,
}

impl Config {
    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("CCCTL_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"));
        base.join("ccctl").join("config.toml")
    }

    /// Load from `path`; a missing file yields defaults, a malformed one errors.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => Config::parse(&s).map_err(|e| {
                CliError::new(Code::Usage, format!("config {}: {}", path.display(), e.msg))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(CliError::new(
                Code::General,
                format!("config {}: {e}", path.display()),
            )),
        }
    }

    pub fn parse(s: &str) -> Result<Config> {
        let c: Config = toml::from_str(s).map_err(|e| CliError::new(Code::Usage, e.to_string()))?;
        if !matches!(c.claude.discovery.as_str(), "agents" | "registry") {
            return Err(CliError::new(
                Code::Usage,
                "claude.discovery must be \"agents\" or \"registry\"",
            ));
        }
        Ok(c)
    }

    pub fn sessions_dir(&self) -> PathBuf {
        expand_tilde(&self.claude.sessions_dir)
    }

    pub fn projects_dir(&self) -> PathBuf {
        expand_tilde(&self.claude.projects_dir)
    }

    pub fn socket_roots(&self) -> Vec<PathBuf> {
        if !self.sockets.roots.is_empty() {
            return self.sockets.roots.iter().map(|r| expand_tilde(r)).collect();
        }
        // Mirrors Claude Code's accepted socket directories (2.1.x):
        // $XDG_RUNTIME_DIR/cc-socks, /tmp/cc-socks, /tmp/cc-socks-<uid>.
        let uid = unsafe { libc::getuid() };
        let mut v = Vec::new();
        if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR") {
            v.push(PathBuf::from(x).join("cc-socks"));
        }
        v.push(PathBuf::from(format!("/run/user/{uid}/cc-socks")));
        v.push(PathBuf::from("/tmp/cc-socks"));
        v.push(PathBuf::from(format!("/tmp/cc-socks-{uid}")));
        v.dedup();
        v
    }

    /// Whether automations may act on a session with this name.
    pub fn allowed(&self, name: &str) -> bool {
        match &self.security.allowlist {
            None => true,
            Some(list) => list.iter().any(|g| glob_match(g, name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.claude.bin, "claude");
        assert!(c.allowed("whatever"));
        assert_eq!(c.timeouts.poll_ms, 400);
    }

    #[test]
    fn allowlist_globs() {
        let c = Config::parse("[security]\nallowlist = [\"api-*\", \"exact\"]\n").unwrap();
        assert!(c.allowed("api-worker"));
        assert!(c.allowed("exact"));
        assert!(!c.allowed("docs-writer-2"));
        let none = Config::parse("[security]\nallowlist = []\n").unwrap();
        assert!(!none.allowed("x"));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("[security]\nallowlst = []\n").is_err());
        assert!(Config::parse("[claude]\ndiscovery = \"magic\"\n").is_err());
    }

    #[test]
    fn custom_socket_roots() {
        let c = Config::parse("[sockets]\nroots = [\"/x/cc-socks\"]\n").unwrap();
        assert_eq!(c.socket_roots(), vec![PathBuf::from("/x/cc-socks")]);
    }
}

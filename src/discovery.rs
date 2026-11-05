//! Session discovery.
//!
//! Source of truth is `claude agents --json` (pid, cwd, kind, startedAt,
//! sessionId, name, status). Fields it does not expose (inbox socket path,
//! tmux hint, procStart, statusUpdatedAt) come from the per-session registry
//! file `~/.claude/sessions/<pid>.json` that every Claude Code process writes.
//! Live status polling reads the registry file directly so `wait` does not
//! spawn a node/bun process every poll.

use crate::config::Config;
use crate::error::{CliError, Code, Result, err};
use crate::util::{pid_alive, proc_stat};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    Busy,
    WaitingPermission,
    Unknown,
}

impl Status {
    /// Map Claude Code's registry status enum (2.1.x: busy|shell|idle|waiting).
    /// `shell` = a `!` shell command is running in the prompt -> busy.
    /// `waiting` = a blocking dialog (permission prompt, question, elicitation)
    /// is on screen -> waiting_permission.
    pub fn from_raw(s: &str) -> Status {
        match s {
            "idle" => Status::Idle,
            "busy" | "shell" => Status::Busy,
            "waiting" => Status::WaitingPermission,
            _ => Status::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Busy => "busy",
            Status::WaitingPermission => "waiting_permission",
            Status::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub name: String,
    pub pid: i32,
    pub cwd: String,
    pub session_id: String,
    pub kind: String,
    pub status: Status,
    /// Status string exactly as Claude Code reported it.
    pub raw_status: String,
    pub started_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub status_updated_at: Option<i64>,
    pub socket: Option<PathBuf>,
    /// Registry `tmux` field (e.g. "0:@3.%7"); used only as a fallback hint.
    pub tmux_hint: Option<String>,
    pub version: Option<String>,
    /// Claude Code's own `waitingFor` (agent view: why a `waiting` session is
    /// blocked, e.g. a permission prompt vs. input needed). Only set while waiting.
    pub waiting_for: Option<String>,
}

/// Parsed registry file fields we care about.
#[derive(Debug, Clone, Default)]
pub struct RegEntry {
    pub pid: i32,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub started_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub status_updated_at: Option<i64>,
    pub socket: Option<String>,
    pub tmux: Option<String>,
    pub version: Option<String>,
    pub proc_start: Option<String>,
    pub waiting_for: Option<String>,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string)
}
fn n(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| x.as_i64())
}

pub fn parse_reg_entry(v: &Value) -> Option<RegEntry> {
    Some(RegEntry {
        pid: n(v, "pid")? as i32,
        session_id: s(v, "sessionId"),
        cwd: s(v, "cwd"),
        name: s(v, "name"),
        kind: s(v, "kind"),
        status: s(v, "status"),
        started_at: n(v, "startedAt"),
        updated_at: n(v, "updatedAt"),
        status_updated_at: n(v, "statusUpdatedAt"),
        socket: s(v, "messagingSocketPath"),
        tmux: s(v, "tmux"),
        version: s(v, "version"),
        proc_start: s(v, "procStart"),
        waiting_for: waiting_for_of(v),
    })
}

pub fn read_reg_file(path: &Path) -> Option<RegEntry> {
    let txt = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    parse_reg_entry(&v)
}

pub fn reg_path(cfg: &Config, pid: i32) -> PathBuf {
    cfg.sessions_dir().join(format!("{pid}.json"))
}

/// All registry entries (`<pid>.json`, canonical numeric names only).
pub fn read_registry(dir: &Path) -> Vec<RegEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let fname = e.file_name();
        let fname = fname.to_string_lossy();
        let Some(stem) = fname.strip_suffix(".json") else {
            continue;
        };
        let Ok(pid) = stem.parse::<i32>() else {
            continue;
        };
        if pid.to_string() != stem {
            continue;
        }
        if let Some(r) = read_reg_file(&e.path())
            && r.pid == pid
        {
            out.push(r);
        }
    }
    out
}

/// Liveness as Claude Code computes it: pid alive and, when recorded,
/// procStart equals the live process's start time (guards PID reuse).
pub fn entry_alive(r: &RegEntry) -> bool {
    if !pid_alive(r.pid) {
        return false;
    }
    match (&r.proc_start, proc_stat(r.pid)) {
        (Some(ps), Some(st)) => *ps == st.start_ticks,
        _ => true,
    }
}

/// `waitingFor` as a display string (Claude documents it as a field of
/// `claude agents --json`; tolerate non-string shapes).
pub fn waiting_for_of(v: &Value) -> Option<String> {
    match v.get("waitingFor")? {
        Value::Null => None,
        Value::String(x) if x.is_empty() => None,
        Value::String(x) => Some(x.clone()),
        other => Some(other.to_string()),
    }
}

fn session_from(agent: Option<&Value>, reg: Option<&RegEntry>, cfg: &Config) -> Option<Session> {
    let pid = agent
        .and_then(|a| n(a, "pid"))
        .map(|p| p as i32)
        .or(reg.map(|r| r.pid))?;
    let pick = |ak: &str, rv: Option<&String>| -> Option<String> {
        agent.and_then(|a| s(a, ak)).or_else(|| rv.cloned())
    };
    let raw_status = pick("status", reg.and_then(|r| r.status.as_ref())).unwrap_or_default();
    let session_id = pick("sessionId", reg.and_then(|r| r.session_id.as_ref())).unwrap_or_default();
    let cwd = pick("cwd", reg.and_then(|r| r.cwd.as_ref())).unwrap_or_default();
    let name = pick("name", reg.and_then(|r| r.name.as_ref())).unwrap_or_default();
    let kind = pick("kind", reg.and_then(|r| r.kind.as_ref())).unwrap_or_default();
    let socket = reg
        .and_then(|r| r.socket.as_ref())
        .map(PathBuf::from)
        .filter(|p| socket_in_roots(p, &cfg.socket_roots()));
    Some(Session {
        name,
        pid,
        cwd,
        session_id,
        kind,
        status: Status::from_raw(&raw_status),
        raw_status: raw_status.clone(),
        started_at: agent
            .and_then(|a| n(a, "startedAt"))
            .or(reg.and_then(|r| r.started_at)),
        updated_at: reg.and_then(|r| r.updated_at),
        status_updated_at: reg.and_then(|r| r.status_updated_at),
        socket,
        tmux_hint: reg.and_then(|r| r.tmux.clone()),
        version: reg.and_then(|r| r.version.clone()),
        waiting_for: if Status::from_raw(&raw_status) == Status::WaitingPermission {
            agent
                .and_then(waiting_for_of)
                .or_else(|| reg.and_then(|r| r.waiting_for.clone()))
        } else {
            None
        },
    })
}

/// Only trust socket paths inside a configured socket root.
pub fn socket_in_roots(p: &Path, roots: &[PathBuf]) -> bool {
    match p.parent() {
        Some(dir) => roots.iter().any(|r| r == dir),
        None => false,
    }
}

fn run_claude_agents(cfg: &Config) -> Result<Vec<Value>> {
    let mut child = Command::new(&cfg.claude.bin)
        .args(["agents", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            CliError::new(
                Code::Discovery,
                format!("spawn `{} agents --json`: {e}", cfg.claude.bin),
            )
        })?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                return err(
                    Code::Discovery,
                    "`claude agents --json` timed out after 15s",
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return err(
            Code::Discovery,
            format!(
                "`claude agents --json` failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        );
    }
    parse_agents_json(&out.stdout)
}

pub fn parse_agents_json(bytes: &[u8]) -> Result<Vec<Value>> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| {
        CliError::new(
            Code::Discovery,
            format!("`claude agents --json` output is not JSON: {e}"),
        )
    })?;
    match v {
        Value::Array(a) => Ok(a),
        _ => err(
            Code::Discovery,
            "`claude agents --json` did not return an array",
        ),
    }
}

/// Merge agents output with registry entries (keyed by pid).
pub fn merge(agents: &[Value], regs: Vec<RegEntry>, cfg: &Config) -> Vec<Session> {
    let mut by_pid: HashMap<i32, RegEntry> = regs.into_iter().map(|r| (r.pid, r)).collect();
    let mut out = Vec::new();
    for a in agents {
        let Some(pid) = n(a, "pid") else { continue };
        let mut reg = by_pid.remove(&(pid as i32));
        // Registry entry must describe the same session (pid reuse guard).
        if let (Some(r), Some(sid)) = (&reg, s(a, "sessionId"))
            && r.session_id.as_deref().is_some_and(|rs| rs != sid)
        {
            reg = None;
        }
        if let Some(sess) = session_from(Some(a), reg.as_ref(), cfg) {
            out.push(sess);
        }
    }
    out
}

/// Discover live sessions. `all_kinds=false` keeps only interactive ones.
pub fn discover(cfg: &Config, all_kinds: bool) -> Result<Vec<Session>> {
    let regs = read_registry(&cfg.sessions_dir());
    let mut sessions = if cfg.claude.discovery == "agents" {
        match run_claude_agents(cfg) {
            Ok(agents) => merge(&agents, regs, cfg),
            Err(e) => {
                eprintln!("ccctl: warning: {}; falling back to registry files", e.msg);
                registry_sessions(regs, cfg)
            }
        }
    } else {
        registry_sessions(regs, cfg)
    };
    if !all_kinds {
        sessions.retain(|s| s.kind == "interactive");
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    Ok(sessions)
}

fn registry_sessions(regs: Vec<RegEntry>, cfg: &Config) -> Vec<Session> {
    regs.iter()
        .filter(|r| entry_alive(r))
        .filter_map(|r| session_from(None, Some(r), cfg))
        .collect()
}

/// Re-read live status for an already-resolved session from its registry file.
/// Returns None if the session is gone (file missing or process dead/reused).
pub fn refresh(cfg: &Config, sess: &Session) -> Option<Session> {
    let r = read_reg_file(&reg_path(cfg, sess.pid))?;
    if r.session_id
        .as_deref()
        .is_some_and(|sid| sid != sess.session_id)
    {
        return None;
    }
    if !entry_alive(&r) {
        return None;
    }
    let mut out = sess.clone();
    if let Some(st) = &r.status {
        out.raw_status = st.clone();
        out.status = Status::from_raw(st);
    }
    if out.status != Status::WaitingPermission {
        out.waiting_for = None;
    } else if r.waiting_for.is_some() {
        out.waiting_for = r.waiting_for.clone();
    }
    out.updated_at = r.updated_at.or(out.updated_at);
    out.status_updated_at = r.status_updated_at.or(out.status_updated_at);
    if let Some(name) = r.name {
        out.name = name;
    }
    Some(out)
}

/// Resolve a selector: exact name, exact sessionId, pid, or a unique
/// sessionId / name prefix (>= 4 chars).
pub fn resolve<'a>(sessions: &'a [Session], sel: &str) -> Result<&'a Session> {
    let exact: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.name == sel || s.session_id == sel || s.pid.to_string() == sel)
        .collect();
    let cands = if exact.is_empty() && sel.len() >= 4 {
        sessions
            .iter()
            .filter(|s| s.session_id.starts_with(sel) || s.name.starts_with(sel))
            .collect()
    } else {
        exact
    };
    match cands.len() {
        0 => {
            let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
            err(
                Code::NotFound,
                format!(
                    "no live Claude session matches '{sel}' (known: {})",
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                ),
            )
        }
        1 => Ok(cands[0]),
        _ => {
            let list: Vec<String> = cands
                .iter()
                .map(|s| format!("{} (pid {}, id {}, {})", s.name, s.pid, s.session_id, s.cwd))
                .collect();
            err(
                Code::Ambiguous,
                format!(
                    "'{sel}' matches {} sessions; use the session id or pid:\n  {}",
                    cands.len(),
                    list.join("\n  ")
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::parse("[sockets]\nroots=[\"/run/user/1000/cc-socks\"]\n").unwrap()
    }

    const AGENTS: &str = r#"[
      {"pid":1234,"cwd":"/w/a","kind":"interactive","startedAt":1,"sessionId":"8c41f0d2-aaaa","name":"alpha","status":"idle"},
      {"pid":5678,"cwd":"/w/b","kind":"interactive","startedAt":2,"sessionId":"3b9e2c41-bbbb","name":"beta","status":"shell"},
      {"pid":5000,"cwd":"/w/c","kind":"bg","startedAt":3,"sessionId":"cccc","name":"gamma","status":"waiting","waitingFor":"permission"}
    ]"#;

    fn reg(pid: i32, sid: &str, sock: &str) -> RegEntry {
        RegEntry {
            pid,
            session_id: Some(sid.into()),
            socket: Some(sock.into()),
            tmux: Some("0:@0.%0".into()),
            status_updated_at: Some(99),
            ..Default::default()
        }
    }

    #[test]
    fn status_mapping() {
        assert_eq!(Status::from_raw("idle"), Status::Idle);
        assert_eq!(Status::from_raw("shell"), Status::Busy);
        assert_eq!(Status::from_raw("waiting"), Status::WaitingPermission);
        assert_eq!(Status::from_raw("zzz"), Status::Unknown);
        assert_eq!(Status::WaitingPermission.as_str(), "waiting_permission");
    }

    #[test]
    fn merge_attaches_registry_fields() {
        let agents = parse_agents_json(AGENTS.as_bytes()).unwrap();
        let regs = vec![
            reg(1234, "8c41f0d2-aaaa", "/run/user/1000/cc-socks/1234.sock"),
            // session id mismatch -> registry ignored (pid reuse)
            reg(5678, "other", "/run/user/1000/cc-socks/5678.sock"),
        ];
        let ss = merge(&agents, regs, &cfg());
        assert_eq!(ss.len(), 3);
        let a = &ss[0];
        assert_eq!(
            a.socket.as_deref(),
            Some(Path::new("/run/user/1000/cc-socks/1234.sock"))
        );
        assert_eq!(a.tmux_hint.as_deref(), Some("0:@0.%0"));
        assert_eq!(a.status_updated_at, Some(99));
        let b = &ss[1];
        assert!(b.socket.is_none());
        assert_eq!(b.status, Status::Busy);
        assert_eq!(ss[2].status, Status::WaitingPermission);
        assert_eq!(ss[2].waiting_for.as_deref(), Some("permission"));
        assert_eq!(ss[0].waiting_for, None);
    }

    #[test]
    fn untrusted_socket_dir_dropped() {
        let agents = parse_agents_json(AGENTS.as_bytes()).unwrap();
        let ss = merge(
            &agents,
            vec![reg(1234, "8c41f0d2-aaaa", "/home/evil/1234.sock")],
            &cfg(),
        );
        assert!(ss[0].socket.is_none());
    }

    #[test]
    fn resolve_rules() {
        let agents = parse_agents_json(AGENTS.as_bytes()).unwrap();
        let ss = merge(&agents, vec![], &cfg());
        assert_eq!(resolve(&ss, "alpha").unwrap().pid, 1234);
        assert_eq!(resolve(&ss, "5678").unwrap().name, "beta");
        assert_eq!(resolve(&ss, "8c41f0d2").unwrap().name, "alpha");
        assert_eq!(resolve(&ss, "nope").unwrap_err().code, Code::NotFound);
        let mut dup = ss.clone();
        dup[1].name = "alpha".into();
        assert_eq!(resolve(&dup, "alpha").unwrap_err().code, Code::Ambiguous);
    }

    #[test]
    fn registry_dir_scan() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("123.json"),
            r#"{"pid":123,"sessionId":"x","status":"idle"}"#,
        )
        .unwrap();
        std::fs::write(d.path().join("124.json"), r#"{"pid":999}"#).unwrap(); // pid mismatch
        std::fs::write(d.path().join("125.abc.key"), "{}").unwrap();
        std::fs::write(d.path().join("0125.json"), r#"{"pid":125}"#).unwrap(); // non-canonical
        let r = read_registry(d.path());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].pid, 123);
    }
}

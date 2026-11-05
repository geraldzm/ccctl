//! tmux integration: resolve the pane that hosts a Claude pid and drive it.
//!
//! Resolution never hardcodes pane indexes: we walk the Claude process's
//! parent chain until we hit a `pane_pid` reported by `tmux list-panes -a`.
//! The registry's `tmux` hint is a fallback, accepted only when the pane's
//! cwd equals the session cwd.

use crate::error::{CliError, Code, Result, err};
use crate::util::ancestors;
use serde::Serialize;
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Serialize)]
pub struct Pane {
    pub pane_id: String,
    #[serde(skip)]
    pub pane_pid: i32,
    pub session: String,
    pub window: String,
    pub pane: String,
    pub cwd: String,
    #[serde(skip)]
    pub in_mode: bool,
    pub current_command: String,
}

impl Pane {
    /// Human target, e.g. `0:3.1`.
    pub fn target(&self) -> String {
        format!("{}:{}.{}", self.session, self.window, self.pane)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PaneMatch {
    #[serde(flatten)]
    pub pane: Pane,
    pub target: String,
    /// "pid-ancestry" or "registry-hint+cwd".
    pub matched_by: &'static str,
}

#[derive(Debug, Clone)]
pub struct Tmux {
    bin: String,
    socket: Option<String>,
}

const FMT: &str = "#{pane_id}\t#{pane_pid}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_current_path}\t#{pane_in_mode}\t#{pane_current_command}";

impl Tmux {
    pub fn new(bin: Option<String>, socket: Option<String>) -> Tmux {
        Tmux {
            bin: bin.unwrap_or_else(|| "tmux".into()),
            socket,
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.bin);
        if let Some(s) = &self.socket {
            c.arg("-S").arg(s);
        }
        c
    }

    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| CliError::new(Code::NoTmux, format!("run {}: {e}", self.bin)))?;
        if !out.status.success() {
            return err(
                Code::NoTmux,
                format!(
                    "tmux {} failed: {}",
                    args.first().unwrap_or(&""),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// All panes on the server; empty if no server is running.
    pub fn list_panes(&self) -> Vec<Pane> {
        match self.run(&["list-panes", "-a", "-F", FMT]) {
            Ok(s) => parse_panes(&s),
            Err(_) => Vec::new(),
        }
    }

    /// Type literal text (no key-name interpretation).
    pub fn send_literal(&self, pane_id: &str, text: &str) -> Result<()> {
        self.run(&["send-keys", "-t", pane_id, "-l", "--", text])
            .map(|_| ())
    }

    /// Send tmux key names (Enter, C-c, Escape, Up, 1, ...).
    pub fn send_keys(&self, pane_id: &str, keys: &[String]) -> Result<()> {
        let mut args: Vec<&str> = vec!["send-keys", "-t", pane_id, "--"];
        args.extend(keys.iter().map(String::as_str));
        self.run(&args).map(|_| ())
    }

    /// Bracketed-paste multi-line text into the pane (no Enter).
    pub fn paste(&self, pane_id: &str, text: &str) -> Result<()> {
        let buf = format!("ccctl-{}", std::process::id());
        let mut child = self
            .command()
            .args(["load-buffer", "-b", &buf, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CliError::new(Code::NoTmux, format!("run tmux load-buffer: {e}")))?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(text.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return err(
                Code::NoTmux,
                format!(
                    "tmux load-buffer failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            );
        }
        self.run(&["paste-buffer", "-p", "-d", "-b", &buf, "-t", pane_id])
            .map(|_| ())
    }

    /// Visible screen of the pane (plus `history` lines of scrollback), joined.
    pub fn capture(&self, pane_id: &str, history: u32) -> Result<String> {
        let start = format!("-{history}");
        self.run(&["capture-pane", "-p", "-J", "-t", pane_id, "-S", &start])
    }

    pub fn cancel_copy_mode(&self, pane_id: &str) -> Result<()> {
        self.run(&["send-keys", "-t", pane_id, "-X", "cancel"])
            .map(|_| ())
    }

    /// Focus the pane: switch-client when inside tmux, else exec attach.
    pub fn attach(&self, pane: &Pane) -> Result<()> {
        self.run(&["select-window", "-t", &pane.pane_id])?;
        self.run(&["select-pane", "-t", &pane.pane_id])?;
        if std::env::var_os("TMUX").is_some() {
            self.run(&["switch-client", "-t", &pane.pane_id])
                .map(|_| ())
        } else {
            let status = self
                .command()
                .args(["attach-session", "-t", &pane.session])
                .status()
                .map_err(|e| CliError::new(Code::NoTmux, format!("tmux attach: {e}")))?;
            if status.success() {
                Ok(())
            } else {
                err(
                    Code::NoTmux,
                    format!("tmux attach-session exited with {status}"),
                )
            }
        }
    }
}

pub fn parse_panes(s: &str) -> Vec<Pane> {
    s.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 8 {
                return None;
            }
            Some(Pane {
                pane_id: f[0].into(),
                pane_pid: f[1].parse().ok()?,
                session: f[2].into(),
                window: f[3].into(),
                pane: f[4].into(),
                cwd: f[5].into(),
                in_mode: f[6] == "1",
                current_command: f[7..].join("\t"),
            })
        })
        .collect()
}

/// Find the pane hosting `pid`. `chain` is pid's ancestry (nearest first).
pub fn match_pane(
    panes: &[Pane],
    pid: i32,
    chain: &[i32],
    hint: Option<&str>,
    cwd: &str,
) -> Option<PaneMatch> {
    for p in std::iter::once(&pid).chain(chain.iter()) {
        if let Some(pane) = panes.iter().find(|x| x.pane_pid == *p) {
            return Some(PaneMatch {
                target: pane.target(),
                pane: pane.clone(),
                matched_by: "pid-ancestry",
            });
        }
    }
    // Registry hint looks like "0:@3.%7"; take the trailing %id.
    let id = hint?.rsplit('.').next().filter(|s| s.starts_with('%'))?;
    let pane = panes.iter().find(|x| x.pane_id == id && x.cwd == cwd)?;
    Some(PaneMatch {
        target: pane.target(),
        pane: pane.clone(),
        matched_by: "registry-hint+cwd",
    })
}

pub fn resolve(t: &Tmux, pid: i32, hint: Option<&str>, cwd: &str) -> Option<PaneMatch> {
    let panes = t.list_panes();
    if panes.is_empty() {
        return None;
    }
    match_pane(&panes, pid, &ancestors(pid), hint, cwd)
}

/// Like `resolve`, but sharing one `list-panes` across many sessions.
pub fn resolve_with(panes: &[Pane], pid: i32, hint: Option<&str>, cwd: &str) -> Option<PaneMatch> {
    if panes.is_empty() {
        return None;
    }
    match_pane(panes, pid, &ancestors(pid), hint, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "%0\t2001\t0\t1\t1\t/w/a\t0\tclaude\n%1\t2002\t0\t1\t2\t/w/a/x\t0\tbash\n%7\t2003\t0\t3\t1\t/w/c\t1\tclaude\n";

    #[test]
    fn parse() {
        let p = parse_panes(LIST);
        assert_eq!(p.len(), 3);
        assert_eq!(p[2].target(), "0:3.1");
        assert!(p[2].in_mode);
        assert_eq!(p[0].current_command, "claude");
    }

    #[test]
    fn ancestry_match() {
        let p = parse_panes(LIST);
        let m = match_pane(&p, 1234, &[2001, 321], None, "/w/a").unwrap();
        assert_eq!(m.pane.pane_id, "%0");
        assert_eq!(m.matched_by, "pid-ancestry");
    }

    #[test]
    fn hint_requires_cwd() {
        let p = parse_panes(LIST);
        assert!(match_pane(&p, 1, &[], Some("0:@3.%7"), "/elsewhere").is_none());
        let m = match_pane(&p, 1, &[], Some("0:@3.%7"), "/w/c").unwrap();
        assert_eq!(m.matched_by, "registry-hint+cwd");
        assert!(match_pane(&p, 1, &[], None, "/w/c").is_none());
    }
}

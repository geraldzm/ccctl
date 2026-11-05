//! Command implementations. Data goes to stdout (JSON with --json, otherwise
//! human tables/text); progress and diagnostics go to stderr.

use crate::cli::*;
use crate::config::Config;
use crate::discovery::{self, Session, Status};
use crate::error::{CliError, Code, Result, err};
use crate::inbox;
use crate::pane::{self, Choice, Prompt};
use crate::state;
use crate::tmux::{self, PaneMatch, Tmux};
use crate::transcript::{self, Turn};
use crate::util::{ago, ancestors, file_len, now_ms, read_from, rfc3339_ms};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Ctx {
    pub cfg: Config,
    pub config_path: PathBuf,
    pub json: bool,
    pub tmux: Tmux,
}

impl Ctx {
    pub fn new(cfg: Config, config_path: PathBuf, json: bool) -> Ctx {
        let tmux = Tmux::new(cfg.tmux.bin.clone(), cfg.tmux.socket.clone());
        Ctx {
            cfg,
            config_path,
            json,
            tmux,
        }
    }

    fn poll(&self) -> Duration {
        Duration::from_millis(self.cfg.timeouts.poll_ms.max(50))
    }
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn find(ctx: &Ctx, sel: &str) -> Result<Session> {
    let all = discovery::discover(&ctx.cfg, true)?;
    // Prefer interactive sessions when the selector is ambiguous across kinds.
    let inter: Vec<Session> = all
        .iter()
        .filter(|s| s.kind == "interactive")
        .cloned()
        .collect();
    match discovery::resolve(&inter, sel) {
        Ok(s) => Ok(s.clone()),
        Err(e) if e.code == Code::NotFound => discovery::resolve(&all, sel).cloned(),
        Err(e) => Err(e),
    }
}

/// Is `pid` one of ccctl's own ancestors (i.e. ccctl runs inside that session)?
fn is_self(pid: i32) -> bool {
    ancestors(std::process::id() as i32).contains(&pid)
}

fn guard(ctx: &Ctx, s: &Session, allow_self: bool) -> Result<()> {
    if !ctx.cfg.allowed(&s.name) {
        return err(
            Code::NotAllowed,
            format!(
                "session '{}' is not on the allowlist in {} ([security].allowlist)",
                s.name,
                ctx.config_path.display()
            ),
        );
    }
    if !allow_self && is_self(s.pid) {
        return err(
            Code::NotAllowed,
            format!(
                "'{}' is the Claude session ccctl is running inside; pass --allow-self to target it anyway",
                s.name
            ),
        );
    }
    Ok(())
}

fn pane_of(ctx: &Ctx, s: &Session) -> Option<PaneMatch> {
    tmux::resolve(&ctx.tmux, s.pid, s.tmux_hint.as_deref(), &s.cwd)
}

fn require_pane(ctx: &Ctx, s: &Session) -> Result<PaneMatch> {
    pane_of(ctx, s).ok_or_else(|| {
        CliError::new(
            Code::NoTmux,
            format!(
                "no tmux pane found for '{}' (pid {}): not running under this tmux server{}",
                s.name,
                s.pid,
                ctx.cfg
                    .tmux
                    .socket
                    .as_ref()
                    .map(|x| format!(" ({x})"))
                    .unwrap_or_default()
            ),
        )
    })
}

fn transcript_of(ctx: &Ctx, s: &Session) -> Result<PathBuf> {
    transcript::transcript_path(&ctx.cfg.projects_dir(), &s.cwd, &s.session_id)
}

#[derive(Serialize)]
struct SessionView<'a> {
    name: &'a str,
    status: Status,
    raw_status: &'a str,
    pid: i32,
    cwd: &'a str,
    session_id: &'a str,
    kind: &'a str,
    tmux: Option<PaneMatch>,
    socket: Option<&'a PathBuf>,
    version: Option<&'a str>,
    started_at: Option<String>,
    status_updated_at: Option<String>,
    /// Claude Code's `waitingFor`, verbatim (only while waiting).
    waiting_for: Option<&'a str>,
}

fn view<'a>(s: &'a Session, pane: Option<PaneMatch>) -> SessionView<'a> {
    SessionView {
        name: &s.name,
        status: s.status,
        raw_status: &s.raw_status,
        pid: s.pid,
        cwd: &s.cwd,
        session_id: &s.session_id,
        kind: &s.kind,
        tmux: pane,
        socket: s.socket.as_ref(),
        version: s.version.as_deref(),
        started_at: s.started_at.map(rfc3339_ms),
        status_updated_at: s.status_updated_at.map(rfc3339_ms),
        waiting_for: s.waiting_for.as_deref(),
    }
}

// ---------------------------------------------------------------- list

pub fn list(ctx: &Ctx, all: bool) -> Result<()> {
    let sessions = discovery::discover(&ctx.cfg, all)?;
    let panes = ctx.tmux.list_panes();
    let rows: Vec<SessionView> = sessions
        .iter()
        .map(|s| {
            view(
                s,
                tmux::resolve_with(&panes, s.pid, s.tmux_hint.as_deref(), &s.cwd),
            )
        })
        .collect();
    if ctx.json {
        return print_json(&rows);
    }
    if rows.is_empty() {
        eprintln!("no Claude sessions found");
        return Ok(());
    }
    let w = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    println!(
        "{:<w$}  {:<18}  {:>8}  {:<12}  {:<36}  CWD",
        "NAME", "STATUS", "PID", "TMUX", "SESSION ID"
    );
    for r in &rows {
        let t = r
            .tmux
            .as_ref()
            .map(|p| format!("{} {}", p.target, p.pane.pane_id))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<w$}  {:<18}  {:>8}  {:<12}  {:<36}  {}",
            r.name,
            r.status.as_str(),
            r.pid,
            t,
            r.session_id,
            r.cwd
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- status

/// Why a waiting session is blocked. Claude Code's own `waitingFor`
/// (from `claude agents --json`) wins; "…permission…" is normalised to
/// "permission". Without it, fall back to the pane scrape's classification.
fn waiting_kind(s: &Session, prompt: Option<&Prompt>) -> Option<String> {
    if s.status != Status::WaitingPermission {
        return None;
    }
    match &s.waiting_for {
        Some(w) if w.to_lowercase().contains("permission") => Some("permission".into()),
        Some(w) => Some(w.clone()),
        None => prompt.map(|p| p.kind.clone()),
    }
}

pub fn status(ctx: &Ctx, sel: &str) -> Result<()> {
    let s = find(ctx, sel)?;
    let pane = pane_of(ctx, &s);
    let tpath = transcript_of(ctx, &s).ok();
    let records = tpath.as_ref().and_then(|p| transcript::load(p).ok());
    let last_record = records.as_ref().and_then(|r| transcript::last_timestamp(r));
    let last_turn = records.as_ref().and_then(|r| transcript::turns(r).pop());
    // Last activity = newest of registry update and last transcript record.
    let reg_ms = s.updated_at.max(s.status_updated_at);
    let last_activity = match (&last_record, reg_ms) {
        (Some(ts), Some(ms)) if ts.as_str() > rfc3339_ms(ms).as_str() => Some(ts.clone()),
        (_, Some(ms)) => Some(rfc3339_ms(ms)),
        (Some(ts), None) => Some(ts.clone()),
        _ => None,
    };
    let prompt = if s.status == Status::WaitingPermission {
        pane.as_ref()
            .and_then(|p| ctx.tmux.capture(&p.pane.pane_id, 0).ok())
            .and_then(|scr| pane::parse_prompt(&scr))
    } else {
        None
    };
    let is_me = is_self(s.pid);
    if ctx.json {
        let mut v = serde_json::to_value(view(&s, pane))?;
        v["last_activity"] = json!(last_activity);
        v["transcript"] = json!(tpath);
        v["last_msg_id"] = json!(last_turn.as_ref().and_then(|t| t.msg_id.clone()));
        v["waiting_for"] = json!(waiting_kind(&s, prompt.as_ref()));
        v["prompt"] = json!(prompt);
        v["allowed"] = json!(ctx.cfg.allowed(&s.name));
        v["is_self"] = json!(is_me);
        return print_json(&v);
    }
    let kv = |k: &str, v: String| println!("{k:<14} {v}");
    kv("name", s.name.clone());
    kv(
        "status",
        format!("{} (raw: {})", s.status.as_str(), s.raw_status),
    );
    if let Some(w) = waiting_kind(&s, prompt.as_ref()) {
        kv("waiting for", w);
    }
    kv("pid", s.pid.to_string());
    kv("session id", s.session_id.clone());
    kv("cwd", s.cwd.clone());
    kv("kind", s.kind.clone());
    kv(
        "tmux",
        pane.map(|p| format!("{} ({}, via {})", p.target, p.pane.pane_id, p.matched_by))
            .unwrap_or_else(|| "-".into()),
    );
    kv(
        "socket",
        s.socket
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".into()),
    );
    kv(
        "transcript",
        tpath
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".into()),
    );
    kv("version", s.version.clone().unwrap_or_else(|| "-".into()));
    kv(
        "last activity",
        last_activity.unwrap_or_else(|| "-".into())
            + &s.status_updated_at
                .map(|m| format!(" (status changed {})", ago(m)))
                .unwrap_or_default(),
    );
    if let Some(t) = last_turn.and_then(|t| t.msg_id) {
        kv("last msg id", t);
    }
    kv(
        "allowed",
        format!(
            "{}{}",
            ctx.cfg.allowed(&s.name),
            if is_me {
                " (this is ccctl's own session)"
            } else {
                ""
            }
        ),
    );
    if let Some(p) = prompt {
        println!("prompt:");
        print_prompt(&p);
    }
    Ok(())
}

fn print_prompt(p: &Prompt) {
    for c in &p.context {
        println!("    {c}");
    }
    if let Some(q) = &p.question {
        println!("  {q}");
    }
    for o in &p.options {
        println!(
            "  {} {}. {}",
            if o.selected { "❯" } else { " " },
            o.index,
            o.label
        );
    }
}

// ---------------------------------------------------------------- send / ask

fn read_body(b: &Body) -> Result<String> {
    let text = if let Some(t) = &b.text {
        t.clone()
    } else if let Some(f) = &b.file {
        std::fs::read_to_string(f)
            .map_err(|e| CliError::new(Code::Usage, format!("--file {}: {e}", f.display())))?
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    };
    let trimmed = text.trim_end_matches(['\n', '\r']).to_string();
    if trimmed.trim().is_empty() {
        return err(Code::Usage, "message body is empty");
    }
    Ok(trimmed)
}

#[derive(Serialize)]
struct SendOutcome {
    session: String,
    via: &'static str,
    /// Inbox msg_id (socket delivery only); also usable with `read --since`.
    msg_id: Option<String>,
    /// "delivered" | "queued" | "sent" (unconfirmed) | "held" | "denied" | "expired"
    state: String,
    confirmed: bool,
    /// Transcript record uuid of the delivered prompt, when confirmed.
    record_uuid: Option<String>,
    receipt: Option<inbox::Receipt>,
    #[serde(skip)]
    offset: u64,
    #[serde(skip)]
    transcript: Option<PathBuf>,
}

fn deliver(ctx: &Ctx, s: &Session, text: &str, d: &DeliveryOpts) -> Result<SendOutcome> {
    let via = match d.via {
        Via::Socket => "socket",
        Via::Tmux => "tmux",
        Via::Auto if s.socket.is_some() => "socket",
        Via::Auto => {
            eprintln!(
                "ccctl: '{}' has no inbox socket; falling back to tmux paste",
                s.name
            );
            "tmux"
        }
    };
    let tpath = transcript_of(ctx, s).ok();
    let offset = tpath.as_deref().map(file_len).unwrap_or(0);
    let mut out = SendOutcome {
        session: s.name.clone(),
        via,
        msg_id: None,
        state: "sent".into(),
        confirmed: false,
        record_uuid: None,
        receipt: None,
        offset,
        transcript: tpath.clone(),
    };
    state::record(&s.session_id);
    if via == "socket" {
        let sock = s.socket.as_ref().ok_or_else(|| {
            CliError::new(Code::Delivery, format!("'{}' has no inbox socket (registry has no messagingSocketPath in an allowed root)", s.name))
        })?;
        // PID-reuse guard right before connecting.
        if discovery::refresh(&ctx.cfg, s).is_none() {
            return err(
                Code::Delivery,
                format!("session '{}' (pid {}) is gone", s.name, s.pid),
            );
        }
        let pr = d
            .peer_receipt
            .then(|| Duration::from_secs_f64(ctx.cfg.timeouts.peer_receipt));
        let sent = inbox::send(sock, text, d.priority.as_str(), pr)?;
        out.msg_id = Some(sent.msg_id);
        if let Some(r) = sent.receipt {
            out.state = r.status.clone();
            out.receipt = Some(r);
            if matches!(out.state.as_str(), "denied" | "expired") {
                return Err(CliError::new(
                    Code::Delivery,
                    format!("message {} by '{}'", out.state, s.name),
                ));
            }
        }
    } else {
        if text.trim_start().starts_with('/') {
            return err(
                Code::Usage,
                "text starts with '/', which the TUI would run as a slash command; use `ccctl cmd` for commands",
            );
        }
        let p = require_pane(ctx, s)?;
        tmux_ready(ctx, s, &p, d.force, false)?;
        if text.contains('\n') {
            ctx.tmux.paste(&p.pane.pane_id, text)?;
        } else {
            ctx.tmux.send_literal(&p.pane.pane_id, text)?;
        }
        std::thread::sleep(Duration::from_millis(ctx.cfg.timeouts.enter_delay_ms));
        ctx.tmux
            .send_keys(&p.pane.pane_id, &["Enter".to_string()])?;
    }
    let ct = d.confirm_timeout.unwrap_or(ctx.cfg.timeouts.send_confirm);
    if ct > 0.0
        && out.state != "held"
        && let Some(tp) = &tpath
    {
        let deadline = Instant::now() + Duration::from_secs_f64(ct);
        loop {
            let recs = transcript::parse_lines(&read_from(tp, offset).unwrap_or_default());
            if let Some(dv) = transcript::find_delivery(&recs, out.msg_id.as_deref(), text) {
                out.confirmed = dv.state == "delivered";
                out.state = dv.state;
                out.record_uuid = dv.uuid;
                if out.confirmed {
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(ctx.poll());
        }
    }
    if d.require_confirm && !out.confirmed {
        return Err(CliError::new(
            Code::Delivery,
            format!(
                "delivery to '{}' not confirmed (state: {})",
                s.name, out.state
            ),
        ));
    }
    Ok(out)
}

/// Refuse to type into a pane that is in copy mode or showing a dialog.
fn tmux_ready(ctx: &Ctx, s: &Session, p: &PaneMatch, force: bool, exit_copy: bool) -> Result<()> {
    if p.pane.in_mode {
        if exit_copy {
            ctx.tmux.cancel_copy_mode(&p.pane.pane_id)?;
        } else {
            return err(
                Code::NoTmux,
                format!(
                    "pane {} is in copy/view mode; keys would not reach Claude (use --exit-copy-mode)",
                    p.target
                ),
            );
        }
    }
    if !force
        && let Some(cur) = discovery::refresh(&ctx.cfg, s)
        && cur.status == Status::WaitingPermission
    {
        return err(
            Code::NotAllowed,
            format!(
                "'{}' is showing a dialog; typing would answer it. Use approve/deny, `cmd --keys`, or --force",
                s.name
            ),
        );
    }
    Ok(())
}

pub fn send(ctx: &Ctx, a: &SendArgs) -> Result<()> {
    let s = find(ctx, &a.guard.session)?;
    guard(ctx, &s, a.guard.allow_self)?;
    let text = read_body(&a.body)?;
    let out = deliver(ctx, &s, &text, &a.delivery)?;
    report_send(ctx, &out)
}

fn report_send(ctx: &Ctx, out: &SendOutcome) -> Result<()> {
    if ctx.json {
        return print_json(out);
    }
    let id = out
        .msg_id
        .as_deref()
        .map(|m| format!(" msg_id={m}"))
        .unwrap_or_default();
    match out.state.as_str() {
        "delivered" => eprintln!("ccctl: delivered to '{}' via {}{id}", out.session, out.via),
        "queued" => eprintln!(
            "ccctl: queued in '{}' via {} (session busy; delivered at next tool boundary){id}",
            out.session, out.via
        ),
        "held" => eprintln!(
            "ccctl: '{}' is holding the message for approval in its pane{id}",
            out.session
        ),
        st => eprintln!(
            "ccctl: sent to '{}' via {} ({st}, not confirmed in transcript){id}",
            out.session, out.via
        ),
    }
    if let Some(m) = &out.msg_id {
        println!("{m}");
    }
    Ok(())
}

pub fn ask(ctx: &Ctx, a: &AskArgs) -> Result<()> {
    let s = find(ctx, &a.guard.session)?;
    guard(ctx, &s, a.guard.allow_self)?;
    let text = read_body(&a.body)?;
    let timeout = Duration::from_secs_f64(a.timeout.unwrap_or(ctx.cfg.timeouts.ask));
    let start = Instant::now();
    let out = deliver(ctx, &s, &text, &a.delivery)?;
    if out.state == "held" {
        return err(
            Code::Delivery,
            format!(
                "'{}' is holding the message for approval; approve it in the pane",
                s.name
            ),
        );
    }
    let tp = out
        .transcript
        .clone()
        .ok_or_else(|| CliError::new(Code::Transcript, "no transcript to read the reply from"))?;
    // Wait for: our prompt recorded, then status idle (settled), then a
    // complete turn answering it.
    let mut saw_prompt = out.record_uuid.clone();
    let mut warned = false;
    let t: Turn = loop {
        let recs = transcript::parse_lines(&read_from(&tp, out.offset).unwrap_or_default());
        if saw_prompt.is_none() {
            saw_prompt = transcript::find_delivery(&recs, out.msg_id.as_deref(), &text)
                .filter(|d| d.state == "delivered")
                .and_then(|d| d.uuid);
        }
        let cur = discovery::refresh(&ctx.cfg, &s)
            .ok_or_else(|| CliError::new(Code::NotFound, format!("session '{}' exited", s.name)))?;
        let settled = state::idle_settled(
            cur.status_updated_at,
            state::last(&s.session_id),
            ctx.cfg.timeouts.settle_ms,
            now_ms(),
        );
        if let Some(pu) = &saw_prompt {
            let ts = transcript::turns(&recs);
            let mine = ts
                .iter()
                .position(|t| t.prompt_uuid.as_deref() == Some(pu.as_str()));
            if let Some(i) = mine {
                // Our turn plus any continuation turns (e.g. stop-hook) merged.
                let t = ts[i..].last().cloned();
                if cur.status == Status::Idle
                    && settled
                    && let Some(t) = t.filter(|t| t.complete)
                {
                    break t;
                }
            }
        }
        if cur.status == Status::WaitingPermission && !warned {
            warned = true;
            eprintln!(
                "ccctl: '{}' is waiting on a permission prompt (see `ccctl pending`)",
                s.name
            );
        }
        if start.elapsed() >= timeout {
            return err(
                Code::Timeout,
                format!(
                    "no reply from '{}' within {:.0}s",
                    s.name,
                    timeout.as_secs_f64()
                ),
            );
        }
        std::thread::sleep(ctx.poll().max(Duration::from_millis(500)));
    };
    if ctx.json {
        return print_json(&json!({"session": s.name, "msg_id": out.msg_id, "turn": t}));
    }
    println!("{}", t.text);
    Ok(())
}

// ---------------------------------------------------------------- cmd

pub fn cmd(ctx: &Ctx, a: &CmdArgs) -> Result<()> {
    let s = find(ctx, &a.guard.session)?;
    guard(ctx, &s, a.guard.allow_self)?;
    let p = require_pane(ctx, &s)?;
    let keys_mode = !a.keys.is_empty();
    tmux_ready(ctx, &s, &p, a.force || keys_mode, a.exit_copy_mode)?;
    let id = &p.pane.pane_id;

    let typed = if keys_mode {
        let keys: Vec<String> = a
            .keys
            .iter()
            .map(|k| normalize_key(k))
            .collect::<Result<_>>()?;
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(80));
            }
            ctx.tmux.send_keys(id, std::slice::from_ref(k))?;
        }
        keys.join(" ")
    } else {
        let text = a.input.join(" ");
        if let Some(c) = text.chars().find(|c| c.is_control()) {
            return err(
                Code::Usage,
                format!(
                    "input contains control character U+{:04X}; use --keys for keys or `send` for multi-line text",
                    c as u32
                ),
            );
        }
        ctx.tmux.send_literal(id, &text)?;
        if !a.no_enter {
            std::thread::sleep(Duration::from_millis(ctx.cfg.timeouts.enter_delay_ms));
            ctx.tmux.send_keys(id, &["Enter".to_string()])?;
        }
        text
    };
    state::record(&s.session_id);
    if ctx.json {
        return print_json(&json!({
            "session": s.name, "tmux": p, "keys": keys_mode, "input": typed,
            "enter": !keys_mode && !a.no_enter,
        }));
    }
    eprintln!("ccctl: typed into {} ({}): {typed}", p.target, s.name);
    Ok(())
}

/// Map friendly key names onto tmux key names; reject anything that is not
/// a plausible single key so text never sneaks through `--keys`.
pub fn normalize_key(k: &str) -> Result<String> {
    let l = k.trim().to_ascii_lowercase();
    let named = match l.as_str() {
        "enter" | "return" | "cr" => Some("Enter"),
        "esc" | "escape" => Some("Escape"),
        "tab" => Some("Tab"),
        "btab" | "shift-tab" | "shift+tab" | "s-tab" => Some("BTab"),
        "up" => Some("Up"),
        "down" => Some("Down"),
        "left" => Some("Left"),
        "right" => Some("Right"),
        "space" => Some("Space"),
        "bspace" | "backspace" => Some("BSpace"),
        "pageup" | "ppage" => Some("PageUp"),
        "pagedown" | "npage" => Some("PageDown"),
        "home" => Some("Home"),
        "end" => Some("End"),
        _ => None,
    };
    if let Some(n) = named {
        return Ok(n.to_string());
    }
    for pre in ["ctrl-", "ctrl+", "c-"] {
        if let Some(rest) = l.strip_prefix(pre)
            && rest.chars().count() == 1
        {
            return Ok(format!("C-{rest}"));
        }
    }
    for pre in ["alt-", "alt+", "m-", "meta-"] {
        if let Some(rest) = l.strip_prefix(pre)
            && rest.chars().count() == 1
        {
            return Ok(format!("M-{rest}"));
        }
    }
    if k.chars().count() == 1 {
        return Ok(k.to_string());
    }
    if l.len() >= 2
        && l.starts_with('f')
        && l[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n))
    {
        return Ok(l.to_uppercase());
    }
    err(
        Code::Usage,
        format!("unknown key '{k}' (use Enter, Escape, C-c, Up, BTab, ... or put text after --)"),
    )
}

// ---------------------------------------------------------------- wait

pub fn wait(ctx: &Ctx, a: &WaitArgs) -> Result<()> {
    let s = find(ctx, &a.session)?;
    let target = match a.for_ {
        WaitFor::Idle => Status::Idle,
        WaitFor::Busy => Status::Busy,
        WaitFor::WaitingPermission => Status::WaitingPermission,
    };
    let timeout = Duration::from_secs_f64(a.timeout.unwrap_or(ctx.cfg.timeouts.wait));
    let settle = a.settle_ms.unwrap_or(ctx.cfg.timeouts.settle_ms);
    let start = Instant::now();
    let last_action = state::last(&s.session_id);
    loop {
        let cur = discovery::refresh(&ctx.cfg, &s)
            .ok_or_else(|| CliError::new(Code::NotFound, format!("session '{}' exited", s.name)))?;
        let ok = cur.status == target
            && (target != Status::Idle
                || state::idle_settled(cur.status_updated_at, last_action, settle, now_ms()));
        if ok {
            if ctx.json {
                return print_json(&json!({
                    "session": cur.name, "status": cur.status, "raw_status": cur.raw_status,
                    "waited_ms": start.elapsed().as_millis() as u64,
                }));
            }
            eprintln!(
                "ccctl: '{}' is {} (waited {:.1}s)",
                cur.name,
                cur.status.as_str(),
                start.elapsed().as_secs_f64()
            );
            return Ok(());
        }
        if start.elapsed() >= timeout {
            if ctx.json {
                print_json(&json!({"session": cur.name, "status": cur.status, "timeout": true}))?;
            }
            return err(
                Code::Timeout,
                format!(
                    "timed out after {:.0}s waiting for '{}' to be {} (now {})",
                    timeout.as_secs_f64(),
                    s.name,
                    target.as_str(),
                    cur.status.as_str()
                ),
            );
        }
        std::thread::sleep(ctx.poll());
    }
}

// ---------------------------------------------------------------- read

pub fn read(ctx: &Ctx, a: &ReadArgs) -> Result<()> {
    let s = find(ctx, &a.session)?;
    let tp = transcript_of(ctx, &s)?;
    let recs = transcript::load(&tp)?;
    let all = transcript::turns(&recs);
    let pool: &[Turn] = match &a.since {
        Some(id) => transcript::since(&all, id)?,
        None => &all,
    };
    let n = a.last.max(1);
    let picked = &pool[pool.len().saturating_sub(n)..];
    if ctx.json {
        return print_json(&json!({
            "session": s.name, "session_id": s.session_id, "status": s.status,
            "transcript": tp, "turns": picked,
        }));
    }
    if picked.is_empty() {
        eprintln!(
            "ccctl: no assistant turns{}",
            if a.since.is_some() {
                " after that id"
            } else {
                ""
            }
        );
        return Ok(());
    }
    for (i, t) in picked.iter().enumerate() {
        if picked.len() > 1 {
            if i > 0 {
                println!();
            }
            println!(
                "── {} · {} · {}",
                t.msg_id.as_deref().unwrap_or("-"),
                t.timestamp.as_deref().unwrap_or("-"),
                if t.complete {
                    "complete"
                } else {
                    "in progress"
                }
            );
        } else if !t.complete {
            eprintln!(
                "ccctl: turn still in progress ({} tool calls so far)",
                t.tool_uses
            );
        }
        println!("{}", t.text);
    }
    Ok(())
}

// ---------------------------------------------------------------- approvals

pub fn pending(ctx: &Ctx) -> Result<()> {
    let sessions = discovery::discover(&ctx.cfg, true)?;
    let panes = ctx.tmux.list_panes();
    let mut rows = Vec::new();
    for s in sessions
        .iter()
        .filter(|s| s.status == Status::WaitingPermission)
    {
        let pm = tmux::resolve_with(&panes, s.pid, s.tmux_hint.as_deref(), &s.cwd);
        let prompt = pm
            .as_ref()
            .and_then(|p| ctx.tmux.capture(&p.pane.pane_id, 0).ok())
            .and_then(|scr| pane::parse_prompt(&scr));
        rows.push((s, pm, prompt));
    }
    if ctx.json {
        let v: Vec<Value> = rows
            .iter()
            .map(|(s, pm, pr)| {
                json!({"name": s.name, "pid": s.pid, "session_id": s.session_id, "cwd": s.cwd,
                       "tmux": pm, "waiting_for": waiting_kind(s, pr.as_ref()),
                       "waiting_for_raw": s.waiting_for, "prompt": pr,
                       "since": s.status_updated_at.map(rfc3339_ms)})
            })
            .collect();
        return print_json(&v);
    }
    if rows.is_empty() {
        eprintln!("no sessions are waiting on a prompt");
        return Ok(());
    }
    for (s, pm, pr) in &rows {
        println!(
            "{} (pid {}, {}){}",
            s.name,
            s.pid,
            pm.as_ref()
                .map(|p| p.target.clone())
                .unwrap_or_else(|| "no tmux pane".into()),
            s.status_updated_at
                .map(|m| format!(" waiting {}", ago(m)))
                .unwrap_or_default()
        );
        if let Some(w) = waiting_kind(s, pr.as_ref()) {
            println!("    waiting for: {w}");
        }
        match pr {
            Some(p) => print_prompt(p),
            None => println!("    (dialog not readable from the pane)"),
        }
    }
    Ok(())
}

fn answer(ctx: &Ctx, g: &GuardArgs, choice: Choice) -> Result<()> {
    let s = find(ctx, &g.session)?;
    guard(ctx, &s, g.allow_self)?;
    if s.status != Status::WaitingPermission {
        return err(
            Code::NoPrompt,
            format!(
                "'{}' is {}, not waiting on a prompt",
                s.name,
                s.status.as_str()
            ),
        );
    }
    let p = require_pane(ctx, &s)?;
    if p.pane.in_mode {
        return err(
            Code::NoTmux,
            format!("pane {} is in copy/view mode; leave it first", p.target),
        );
    }
    let id = p.pane.pane_id.clone();
    let screen = ctx.tmux.capture(&id, 0)?;
    let prompt = pane::parse_prompt(&screen).ok_or_else(|| {
        CliError::new(
            Code::NoPrompt,
            format!("could not find a dialog on {}'s screen", s.name),
        )
    })?;
    if prompt.kind != "permission" {
        return err(
            Code::NoPrompt,
            format!(
                "the dialog in '{}' is not a permission prompt ({:?}); answer it with `ccctl cmd --keys`",
                s.name, prompt.question
            ),
        );
    }
    let opt = prompt.pick(choice).cloned();
    let keys = match (&opt, choice) {
        (Some(o), _) => pane::nav_keys(prompt.selected_index(), o.index),
        (None, Choice::Deny) => vec!["Escape".to_string()],
        (None, _) => {
            let labels: Vec<String> = prompt
                .options
                .iter()
                .map(|o| format!("{}. {}", o.index, o.label))
                .collect();
            return err(
                Code::NoPrompt,
                format!(
                    "no option for {:?} in this prompt; options are:\n  {}",
                    choice,
                    labels.join("\n  ")
                ),
            );
        }
    };
    // Re-check the screen right before acting so we never answer a different dialog.
    let again = ctx.tmux.capture(&id, 0)?;
    let still = pane::parse_prompt(&again);
    if still.as_ref().map(|p| &p.options) != Some(&prompt.options) {
        return err(
            Code::NoPrompt,
            "the dialog changed while reading it; run the command again",
        );
    }
    // One key per send-keys with a short gap: TUIs that parse input per
    // read() (and Ink's select) handle discrete keypresses most reliably.
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(80));
        }
        ctx.tmux.send_keys(&id, std::slice::from_ref(k))?;
    }
    state::record(&s.session_id);
    // Give the TUI a moment and report whether the dialog went away.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut cleared = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        let scr = ctx.tmux.capture(&id, 0).unwrap_or_default();
        if pane::parse_prompt(&scr).map(|p| p.options) != Some(prompt.options.clone()) {
            cleared = true;
            break;
        }
    }
    let label = opt
        .as_ref()
        .map(|o| o.label.clone())
        .unwrap_or_else(|| "Escape".into());
    if ctx.json {
        return print_json(&json!({
            "session": s.name, "choice": format!("{choice:?}").to_lowercase(),
            "option": opt, "keys": keys, "dialog_cleared": cleared, "prompt": prompt,
        }));
    }
    eprintln!(
        "ccctl: {} '{}': {}{}",
        if choice == Choice::Deny {
            "denied"
        } else {
            "approved"
        },
        s.name,
        label,
        if cleared {
            ""
        } else {
            " (dialog still visible; check the pane)"
        }
    );
    Ok(())
}

pub fn approve(ctx: &Ctx, a: &ApproveArgs) -> Result<()> {
    let choice = if a.always {
        Choice::Always
    } else if a.session_scope {
        Choice::Session
    } else {
        Choice::Once
    };
    answer(ctx, &a.guard, choice)
}

pub fn deny(ctx: &Ctx, g: &GuardArgs) -> Result<()> {
    answer(ctx, g, Choice::Deny)
}

// ---------------------------------------------------------------- attach / doctor

pub fn attach(ctx: &Ctx, sel: &str) -> Result<()> {
    let s = find(ctx, sel)?;
    let p = require_pane(ctx, &s)?;
    eprintln!(
        "ccctl: attaching to {} ({}) for '{}'",
        p.target, p.pane.pane_id, s.name
    );
    ctx.tmux.attach(&p.pane)
}

pub fn doctor(ctx: &Ctx) -> Result<()> {
    let ver = std::process::Command::new(&ctx.cfg.claude.bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let reg = discovery::read_registry(&ctx.cfg.sessions_dir());
    let live = reg.iter().filter(|r| discovery::entry_alive(r)).count();
    let disc = discovery::discover(&ctx.cfg, true);
    let panes = ctx.tmux.list_panes();
    let roots: Vec<Value> = ctx
        .cfg
        .socket_roots()
        .iter()
        .map(|r| json!({"path": r, "exists": r.is_dir()}))
        .collect();
    let v = json!({
        "ccctl_version": env!("CARGO_PKG_VERSION"),
        "config": {"path": ctx.config_path, "exists": ctx.config_path.is_file(),
                    "allowlist": ctx.cfg.security.allowlist},
        "claude": {"bin": ctx.cfg.claude.bin, "version": ver, "tested_with": crate::TESTED_CLAUDE_VERSION,
                    "discovery": ctx.cfg.claude.discovery},
        "registry": {"dir": ctx.cfg.sessions_dir(), "entries": reg.len(), "live": live},
        "projects_dir": {"path": ctx.cfg.projects_dir(), "exists": ctx.cfg.projects_dir().is_dir()},
        "socket_roots": roots,
        "discovered_sessions": disc.as_ref().map(|d| d.len()).ok(),
        "discovery_error": disc.as_ref().err().map(|e| e.msg.clone()),
        "tmux": {"panes": panes.len(), "socket": ctx.cfg.tmux.socket},
    });
    if ctx.json {
        return print_json(&v);
    }
    println!("ccctl {}", env!("CARGO_PKG_VERSION"));
    println!(
        "config        {} ({})",
        ctx.config_path.display(),
        if ctx.config_path.is_file() {
            "loaded"
        } else {
            "absent, using defaults"
        }
    );
    println!(
        "claude        {} -> {}",
        ctx.cfg.claude.bin,
        ver.as_deref().unwrap_or("NOT FOUND")
    );
    println!("tested with   {}", crate::TESTED_CLAUDE_VERSION);
    println!(
        "registry      {} ({} entries, {} live)",
        ctx.cfg.sessions_dir().display(),
        reg.len(),
        live
    );
    println!("projects      {}", ctx.cfg.projects_dir().display());
    for r in ctx.cfg.socket_roots() {
        println!(
            "socket root   {} {}",
            r.display(),
            if r.is_dir() { "(exists)" } else { "(missing)" }
        );
    }
    match &disc {
        Ok(d) => println!(
            "discovery     {} sessions via {}",
            d.len(),
            ctx.cfg.claude.discovery
        ),
        Err(e) => println!("discovery     ERROR {}", e.msg),
    }
    println!("tmux          {} panes visible", panes.len());
    match &ctx.cfg.security.allowlist {
        None => println!("allowlist     (unset: every session may be targeted)"),
        Some(l) => println!("allowlist     {l:?}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::normalize_key;

    #[test]
    fn keys() {
        assert_eq!(normalize_key("ctrl-c").unwrap(), "C-c");
        assert_eq!(normalize_key("C-c").unwrap(), "C-c");
        assert_eq!(normalize_key("esc").unwrap(), "Escape");
        assert_eq!(normalize_key("Enter").unwrap(), "Enter");
        assert_eq!(normalize_key("shift-tab").unwrap(), "BTab");
        assert_eq!(normalize_key("2").unwrap(), "2");
        assert_eq!(normalize_key("F5").unwrap(), "F5");
        assert!(normalize_key("hello").is_err());
    }
}

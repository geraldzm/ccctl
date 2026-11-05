//! End-to-end test against a FAKE Claude session (tests/fake_claude.py)
//! running in a private tmux server with its own registry, inbox socket and
//! transcript. Never touches real Claude sessions or your tmux server.
//! Skipped automatically when tmux or python3 is unavailable.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Env {
    root: tempfile::TempDir,
    tmux_sock: PathBuf,
}

impl Env {
    fn p(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.tmux_sock)
            .arg("kill-server")
            .output();
    }
}

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-V").output().is_ok()
        || Command::new(bin).arg("--version").output().is_ok()
}

fn write_config(env: &Env, file: &str, allow: &str) -> PathBuf {
    let cfg = format!(
        r#"[claude]
discovery = "registry"
sessions_dir = "{s}"
projects_dir = "{p}"
[sockets]
roots = ["{k}"]
[tmux]
socket = "{t}"
[timeouts]
poll_ms = 100
settle_ms = 800
send_confirm = 5
enter_delay_ms = 100
[security]
allowlist = [{allow}]
"#,
        s = env.p("sessions").display(),
        p = env.p("projects").display(),
        k = env.p("socks").display(),
        t = env.tmux_sock.display(),
    );
    let path = env.p(file);
    std::fs::write(&path, cfg).unwrap();
    path
}

fn setup() -> Option<Env> {
    if !have("tmux") || !have("python3") {
        eprintln!("skipping e2e: tmux/python3 not available");
        return None;
    }
    let root = tempfile::Builder::new()
        .prefix("ccctl-e2e")
        .tempdir_in("/tmp")
        .unwrap();
    let env = Env {
        tmux_sock: root.path().join("tmux.sock"),
        root,
    };
    for d in ["sessions", "projects", "socks", "state", "work", "xdg"] {
        std::fs::create_dir_all(env.p(d)).unwrap();
    }
    write_config(&env, "config.toml", "\"fake-*\"");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_claude.py");
    let cmd = format!(
        "exec python3 {} {} {} {} {} fake-one {}",
        script.display(),
        env.p("sessions").display(),
        env.p("projects").display(),
        env.p("socks").display(),
        env.p("work").display(),
        env.p("state").display()
    );
    let st = Command::new("tmux")
        .arg("-S")
        .arg(&env.tmux_sock)
        .args([
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "t",
            "-x",
            "200",
            "-y",
            "50",
            "-c",
        ])
        .arg(env.p("work"))
        .arg(cmd)
        .status()
        .unwrap();
    assert!(st.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let n = std::fs::read_dir(env.p("sessions"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .count();
        let socks = std::fs::read_dir(env.p("socks")).unwrap().count();
        if n == 1 && socks == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "fake session did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    Some(env)
}

fn run_cfg(env: &Env, cfg: &str, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ccctl"))
        .args(args)
        .env("CCCTL_CONFIG", env.p(cfg))
        .env("XDG_STATE_HOME", env.p("xdg"))
        .env_remove("TMUX")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run(env: &Env, args: &[&str]) -> (i32, String, String) {
    run_cfg(env, "config.toml", args)
}

fn ok(env: &Env, args: &[&str]) -> String {
    let (c, o, e) = run(env, args);
    assert_eq!(c, 0, "ccctl {args:?} failed: {e}\n{o}");
    o
}

fn json(env: &Env, args: &[&str]) -> Value {
    serde_json::from_str(&ok(env, args)).unwrap()
}

fn wait_file_contains(path: &Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(path)
            .unwrap_or_default()
            .contains(needle)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} never contained {needle:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn fake_session_end_to_end() {
    let Some(env) = setup() else { return };

    // Discovery + tmux resolution from pid.
    let list = json(&env, &["list", "--json"]);
    let s = &list.as_array().unwrap()[0];
    assert_eq!(s["name"], "fake-one");
    assert_eq!(s["status"], "idle");
    assert_eq!(s["tmux"]["matched_by"], "pid-ancestry");
    assert_eq!(s["tmux"]["target"], "t:0.0");

    // Socket send with transcript-confirmed delivery.
    let sent = json(
        &env,
        &["send", "fake-one", "--text", "hello\nworld", "--json"],
    );
    assert_eq!(sent["via"], "socket");
    assert_eq!(sent["state"], "delivered", "{sent}");
    let msg_id = sent["msg_id"].as_str().unwrap().to_string();
    ok(
        &env,
        &["wait", "fake-one", "--for", "idle", "--timeout", "10"],
    );
    let r = json(&env, &["read", "fake-one", "--json"]);
    let turn = &r["turns"][0];
    assert_eq!(turn["text"], "echo: hello\nworld");
    assert_eq!(turn["prompt_msg_id"], msg_id.as_str());
    assert_eq!(turn["complete"], true);
    // --since the send's msg id: nothing newer yet.
    let r2 = json(&env, &["read", "fake-one", "--since", &msg_id, "--json"]);
    assert_eq!(r2["turns"].as_array().unwrap().len(), 0);

    // --file body
    std::fs::write(env.p("goal.txt"), "goal line 1\ngoal line 2\n").unwrap();
    let sent = json(
        &env,
        &[
            "send",
            "fake-one",
            "--file",
            env.p("goal.txt").to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(sent["state"], "delivered");
    ok(
        &env,
        &["wait", "fake-one", "--for", "idle", "--timeout", "10"],
    );

    // ask = send + wait + read
    let reply = ok(
        &env,
        &[
            "ask",
            "fake-one",
            "--text",
            "what is 2+2",
            "--timeout",
            "20",
        ],
    );
    assert_eq!(reply.trim(), "echo: what is 2+2");

    // cmd: slash command typed into the pane.
    ok(&env, &["cmd", "fake-one", "--", "/goal"]);
    wait_file_contains(&env.p("state/typed.log"), "/goal\n");
    ok(&env, &["cmd", "fake-one", "--", "go"]);
    wait_file_contains(&env.p("state/typed.log"), "go\n");
    ok(
        &env,
        &["wait", "fake-one", "--for", "idle", "--timeout", "10"],
    );
    let r = json(&env, &["read", "fake-one", "--json"]);
    assert_eq!(r["turns"][0]["text"], "typed: go");

    // --keys
    ok(&env, &["cmd", "fake-one", "--keys", "ctrl-c"]);
    wait_file_contains(&env.p("state/typed.log"), "<C-c>");

    // send via tmux paste (multi-line), confirmed by transcript text match.
    let sent = json(
        &env,
        &[
            "send",
            "fake-one",
            "--via",
            "tmux",
            "--text",
            "line a\nline b",
            "--json",
        ],
    );
    assert_eq!(sent["via"], "tmux");
    assert_eq!(sent["state"], "delivered", "{sent}");
    ok(
        &env,
        &["wait", "fake-one", "--for", "idle", "--timeout", "10"],
    );
    // tmux path refuses slash-leading prose
    assert_eq!(
        run(&env, &["send", "fake-one", "--via", "tmux", "--text", "/x"]).0,
        2
    );

    // Permission prompt flow.
    ok(&env, &["cmd", "fake-one", "--", "/perm"]);
    ok(
        &env,
        &[
            "wait",
            "fake-one",
            "--for",
            "waiting_permission",
            "--timeout",
            "5",
        ],
    );
    let p = json(&env, &["pending", "--json"]);
    let p0 = &p.as_array().unwrap()[0];
    assert_eq!(p0["waiting_for"], "permission");
    assert_eq!(p0["waiting_for_raw"], "permission prompt");
    let st = json(&env, &["status", "fake-one", "--json"]);
    assert_eq!(st["waiting_for"], "permission");
    assert_eq!(p0["prompt"]["options"].as_array().unwrap().len(), 3);
    assert_eq!(p0["prompt"]["question"], "Do you want to proceed?");
    // typing while a dialog is up is refused
    assert_eq!(run(&env, &["cmd", "fake-one", "--", "oops"]).0, 5);
    // no "session" option in this dialog
    assert_eq!(run(&env, &["approve", "fake-one", "--session"]).0, 9);
    let a = json(&env, &["approve", "fake-one", "--always", "--json"]);
    assert_eq!(a["option"]["index"], 2);
    assert_eq!(a["dialog_cleared"], true);
    wait_file_contains(&env.p("state/choice.txt"), "don't ask again");
    ok(
        &env,
        &["wait", "fake-one", "--for", "idle", "--timeout", "5"],
    );
    // approve when nothing is pending
    assert_eq!(run(&env, &["approve", "fake-one"]).0, 9);
    ok(&env, &["cmd", "fake-one", "--", "/perm"]);
    ok(
        &env,
        &[
            "wait",
            "fake-one",
            "--for",
            "waiting_permission",
            "--timeout",
            "5",
        ],
    );
    ok(&env, &["deny", "fake-one"]);
    wait_file_contains(&env.p("state/choice.txt"), "No, and tell Claude");
    ok(&env, &["cmd", "fake-one", "--", "/perm"]);
    ok(
        &env,
        &[
            "wait",
            "fake-one",
            "--for",
            "waiting_permission",
            "--timeout",
            "5",
        ],
    );
    ok(&env, &["approve", "fake-one"]);
    wait_file_contains(&env.p("state/choice.txt"), "Yes");

    // Exit codes.
    assert_eq!(
        run(
            &env,
            &["wait", "fake-one", "--for", "busy", "--timeout", "1"]
        )
        .0,
        6
    );
    assert_eq!(run(&env, &["status", "nope"]).0, 3);
    let st = json(&env, &["status", "fake-one", "--json"]);
    assert!(st["last_activity"].is_string());
    assert!(st["transcript"].as_str().unwrap().ends_with(".jsonl"));

    // Allowlist enforcement.
    write_config(&env, "deny.toml", "\"other-*\"");
    let (c, _, e) = run_cfg(&env, "deny.toml", &["send", "fake-one", "--text", "x"]);
    assert_eq!(c, 5, "{e}");
    assert!(e.contains("allowlist"));
    // read-only commands still work for non-allowlisted sessions
    assert_eq!(run_cfg(&env, "deny.toml", &["read", "fake-one"]).0, 0);
}

//! Claude Code transcript (`~/.claude/projects/<sanitized cwd>/<sessionId>.jsonl`).
//!
//! The path is derived, never searched for: Claude Code sanitizes the cwd with
//! `cwd.replace(/[^a-zA-Z0-9]/g, "-")` (per UTF-16 code unit) and, when that is
//! longer than 200 chars, keeps the first 200 and appends `-<base36 hash>`.
//! For that long-path case we list only the projects dir entries that start
//! with the 200-char prefix and pick the one containing `<sessionId>.jsonl`.

use crate::error::{Code, Result, err};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

const MAX_SANITIZED: usize = 200;

pub fn sanitize_cwd(cwd: &str) -> String {
    let mut out = String::with_capacity(cwd.len());
    for c in cwd.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                out.push('-');
            }
        }
    }
    out
}

pub fn transcript_path(projects: &Path, cwd: &str, session_id: &str) -> Result<PathBuf> {
    if session_id.is_empty() || session_id.contains('/') {
        return err(
            Code::Transcript,
            format!("invalid session id '{session_id}'"),
        );
    }
    let file = format!("{session_id}.jsonl");
    let san = sanitize_cwd(cwd);
    if san.len() <= MAX_SANITIZED {
        let p = projects.join(&san).join(&file);
        if p.is_file() {
            return Ok(p);
        }
        return err(
            Code::Transcript,
            format!(
                "transcript not found at {} (has the session written anything yet?)",
                p.display()
            ),
        );
    }
    let prefix = format!("{}-", &san[..MAX_SANITIZED]);
    if let Ok(rd) = std::fs::read_dir(projects) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                let p = e.path().join(&file);
                if p.is_file() {
                    return Ok(p);
                }
            }
        }
    }
    err(
        Code::Transcript,
        format!(
            "transcript for {session_id} not found under {}/{prefix}*",
            projects.display()
        ),
    )
}

pub fn parse_lines(bytes: &[u8]) -> Vec<Value> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect()
}

pub fn load(path: &Path) -> Result<Vec<Value>> {
    let b = std::fs::read(path).map_err(|e| {
        crate::error::CliError::new(Code::Transcript, format!("{}: {e}", path.display()))
    })?;
    Ok(parse_lines(&b))
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Text of a user/assistant `message.content` (string or block array).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| str_of(b, "type") == Some("text"))
            .filter_map(|b| str_of(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn is_tool_result(content: &Value) -> bool {
    content
        .as_array()
        .is_some_and(|a| a.iter().any(|b| str_of(b, "type") == Some("tool_result")))
}

/// Does this record start a new conversational turn?
/// Yes for: human prompts, slash-command invocations, peer (inbox) messages,
/// background task notifications. No for: tool results, meta records (skill
/// bodies, hook feedback, caveats), local command output, sidechains.
pub fn is_turn_boundary(r: &Value) -> bool {
    if str_of(r, "type") != Some("user") || r.get("isSidechain") == Some(&Value::Bool(true)) {
        return false;
    }
    let Some(content) = r.get("message").and_then(|m| m.get("content")) else {
        return false;
    };
    if is_tool_result(content) {
        return false;
    }
    let origin_kind = r.get("origin").and_then(|o| str_of(o, "kind"));
    if matches!(
        origin_kind,
        Some("peer") | Some("human") | Some("task-notification")
    ) {
        return true;
    }
    if r.get("isMeta") == Some(&Value::Bool(true)) {
        return false;
    }
    let t = content_text(content);
    let t = t.trim_start();
    !(t.starts_with("<local-command-stdout>")
        || t.starts_with("<local-command-stderr>")
        || t.is_empty())
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Turn {
    /// API message id of the last assistant message in the turn (`msg_...`).
    pub msg_id: Option<String>,
    /// Transcript record uuid of the last assistant record.
    pub uuid: Option<String>,
    pub timestamp: Option<String>,
    pub text: String,
    pub stop_reason: Option<String>,
    /// True when the turn's last assistant message ended (not mid tool-use).
    pub complete: bool,
    pub tool_uses: usize,
    pub prompt: Option<String>,
    pub prompt_uuid: Option<String>,
    pub prompt_origin: Option<String>,
    /// For inbox deliveries: the msg_id ccctl (or a peer) sent.
    pub prompt_msg_id: Option<String>,
    #[serde(skip)]
    pub message_ids: Vec<String>,
}

pub fn turns(records: &[Value]) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    let mut cur = Turn::default();
    let mut has_assistant = false;
    let mut last_msg: Option<String> = None;
    for r in records {
        if is_turn_boundary(r) {
            if has_assistant {
                out.push(std::mem::take(&mut cur));
            }
            cur = Turn::default();
            has_assistant = false;
            last_msg = None;
            let content = &r["message"]["content"];
            cur.prompt = Some(content_text(content));
            cur.prompt_uuid = str_of(r, "uuid").map(str::to_string);
            cur.prompt_origin = r
                .get("origin")
                .and_then(|o| str_of(o, "kind"))
                .map(str::to_string);
            cur.prompt_msg_id = r
                .get("origin")
                .and_then(|o| str_of(o, "msg_id"))
                .map(str::to_string);
            continue;
        }
        if str_of(r, "type") != Some("assistant")
            || r.get("isSidechain") == Some(&Value::Bool(true))
        {
            continue;
        }
        let Some(msg) = r.get("message") else {
            continue;
        };
        has_assistant = true;
        let mid = str_of(msg, "id").map(str::to_string);
        if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
            for b in blocks {
                match str_of(b, "type") {
                    Some("text") => {
                        let t = str_of(b, "text").unwrap_or("");
                        if t.trim().is_empty() {
                            continue;
                        }
                        if !cur.text.is_empty() {
                            cur.text
                                .push_str(if mid == last_msg { "\n" } else { "\n\n" });
                        }
                        cur.text.push_str(t);
                        last_msg = mid.clone();
                    }
                    Some("tool_use") => cur.tool_uses += 1,
                    _ => {}
                }
            }
        }
        if let Some(m) = &mid
            && cur.message_ids.last() != Some(m)
        {
            cur.message_ids.push(m.clone());
        }
        cur.msg_id = mid.or(cur.msg_id.take());
        cur.uuid = str_of(r, "uuid").map(str::to_string).or(cur.uuid.take());
        cur.timestamp = str_of(r, "timestamp")
            .map(str::to_string)
            .or(cur.timestamp.take());
        cur.stop_reason = str_of(msg, "stop_reason").map(str::to_string);
        cur.complete = matches!(
            cur.stop_reason.as_deref(),
            Some("end_turn") | Some("stop_sequence") | Some("max_tokens") | Some("refusal")
        );
    }
    if has_assistant {
        out.push(cur);
    }
    out
}

/// Turns strictly after the turn containing `id` (msg id, record uuid, or
/// prompt uuid / prompt msg id).
pub fn since<'a>(all: &'a [Turn], id: &str) -> Result<&'a [Turn]> {
    let pos = all.iter().position(|t| {
        t.msg_id.as_deref() == Some(id)
            || t.uuid.as_deref() == Some(id)
            || t.prompt_uuid.as_deref() == Some(id)
            || t.prompt_msg_id.as_deref() == Some(id)
            || t.message_ids.iter().any(|m| m == id)
    });
    match pos {
        Some(i) => Ok(&all[i + 1..]),
        None => err(
            Code::Transcript,
            format!("--since: id '{id}' not found in transcript"),
        ),
    }
}

/// How a sent message showed up in the transcript.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Delivery {
    /// "delivered" (user record exists) or "queued" (enqueued, session busy).
    pub state: String,
    pub uuid: Option<String>,
}

/// Look for evidence that our message arrived, in records appended after the
/// send. Prefers exact `origin.msg_id` match; falls back to text match.
pub fn find_delivery(records: &[Value], msg_id: Option<&str>, text: &str) -> Option<Delivery> {
    let needle: String = text.trim().chars().take(200).collect();
    let mut queued = None;
    for r in records {
        let ty = str_of(r, "type");
        if ty == Some("user") {
            let origin_id = r.get("origin").and_then(|o| str_of(o, "msg_id"));
            let id_hit = msg_id.is_some() && origin_id == msg_id;
            let text_hit = !needle.is_empty()
                && r.get("message")
                    .and_then(|m| m.get("content"))
                    .is_some_and(|c| content_text(c).contains(&needle));
            // With a msg id, only an exact origin.msg_id match counts, unless
            // the record carries no origin id at all (older builds) and the
            // text matches. Without a msg id (tmux delivery) the text decides.
            let hit = match msg_id {
                Some(_) => id_hit || (origin_id.is_none() && text_hit),
                None => text_hit,
            };
            if hit {
                return Some(Delivery {
                    state: "delivered".into(),
                    uuid: str_of(r, "uuid").map(str::to_string),
                });
            }
        } else if ty == Some("queue-operation")
            && str_of(r, "operation") == Some("enqueue")
            && !needle.is_empty()
            && str_of(r, "content").is_some_and(|c| c.contains(&needle))
        {
            queued = Some(Delivery {
                state: "queued".into(),
                uuid: None,
            });
        }
    }
    queued
}

/// Last activity timestamp (RFC 3339 string) of any record with a timestamp.
pub fn last_timestamp(records: &[Value]) -> Option<String> {
    records
        .iter()
        .rev()
        .find_map(|r| str_of(r, "timestamp").map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize() {
        assert_eq!(
            sanitize_cwd("/home/me/src/my-project"),
            "-home-me-src-my-project"
        );
        assert_eq!(sanitize_cwd("/home/g/.config/nvim"), "-home-g--config-nvim");
        assert_eq!(sanitize_cwd("/a/é"), "-a--");
        assert_eq!(sanitize_cwd("/a/😀"), "-a---"); // surrogate pair = 2 units
    }

    #[test]
    fn path_derivation() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("-w-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("abc.jsonl"), "").unwrap();
        assert_eq!(
            transcript_path(d.path(), "/w/proj", "abc").unwrap(),
            dir.join("abc.jsonl")
        );
        assert_eq!(
            transcript_path(d.path(), "/w/other", "abc")
                .unwrap_err()
                .code,
            Code::Transcript
        );
        assert!(transcript_path(d.path(), "/w/proj", "../x").is_err());

        let long = format!("/{}", "x".repeat(250));
        let san = sanitize_cwd(&long);
        let ldir = d.path().join(format!("{}-abc123", &san[..200]));
        std::fs::create_dir_all(&ldir).unwrap();
        std::fs::write(ldir.join("sid.jsonl"), "").unwrap();
        assert_eq!(
            transcript_path(d.path(), &long, "sid").unwrap(),
            ldir.join("sid.jsonl")
        );
    }

    fn user(uuid: &str, content: Value, origin: Option<Value>) -> Value {
        let mut v = json!({"type":"user","uuid":uuid,"isSidechain":false,"message":{"role":"user","content":content}});
        if let Some(o) = origin {
            v["origin"] = o;
        }
        v
    }
    fn asst(uuid: &str, id: &str, blocks: Value, stop: &str) -> Value {
        json!({"type":"assistant","uuid":uuid,"isSidechain":false,"timestamp":"2026-11-04T00:00:00Z",
               "message":{"id":id,"role":"assistant","content":blocks,"stop_reason":stop}})
    }

    fn sample() -> Vec<Value> {
        vec![
            json!({"type":"mode"}),
            user("u1", json!("hello"), Some(json!({"kind":"human"}))),
            asst(
                "a1",
                "msg_1",
                json!([{"type":"thinking","thinking":"..."}]),
                "tool_use",
            ),
            asst(
                "a2",
                "msg_1",
                json!([{"type":"text","text":"Looking."},{"type":"tool_use","id":"t","name":"Bash","input":{}}]),
                "tool_use",
            ),
            user(
                "r1",
                json!([{"type":"tool_result","tool_use_id":"t","content":"ok"}]),
                None,
            ),
            asst(
                "a3",
                "msg_2",
                json!([{"type":"text","text":"Done: 42"}]),
                "end_turn",
            ),
            user("c1", json!("<command-name>/goal</command-name>"), None),
            user(
                "c2",
                json!("<local-command-stdout>Goal set</local-command-stdout>"),
                None,
            ),
            user("m1", json!("Stop hook feedback: keep going"), None).tap_meta(),
            user(
                "p1",
                json!("Another Claude session sent a message:\nhi"),
                Some(json!({"kind":"peer","msg_id":"11111111-1111-4111-8111-111111111111"})),
            ),
            asst(
                "a4",
                "msg_3",
                json!([{"type":"text","text":"Got it"}]),
                "end_turn",
            ),
            asst(
                "s1",
                "msg_x",
                json!([{"type":"text","text":"sidechain"}]),
                "end_turn",
            )
            .tap_side(),
            user("u2", json!("next"), Some(json!({"kind":"human"}))),
            asst(
                "a5",
                "msg_4",
                json!([{"type":"tool_use","id":"t2","name":"Bash","input":{}}]),
                "tool_use",
            ),
        ]
    }

    trait Tap {
        fn tap_meta(self) -> Self;
        fn tap_side(self) -> Self;
    }
    impl Tap for Value {
        fn tap_meta(mut self) -> Self {
            self["isMeta"] = json!(true);
            self
        }
        fn tap_side(mut self) -> Self {
            self["isSidechain"] = json!(true);
            self
        }
    }

    #[test]
    fn segmentation() {
        let t = turns(&sample());
        assert_eq!(t.len(), 3, "{t:#?}");
        assert_eq!(t[0].prompt.as_deref(), Some("hello"));
        assert_eq!(t[0].text, "Looking.\n\nDone: 42");
        assert_eq!(t[0].msg_id.as_deref(), Some("msg_2"));
        assert!(t[0].complete);
        assert_eq!(t[0].tool_uses, 1);
        assert_eq!(t[1].prompt_origin.as_deref(), Some("peer"));
        assert_eq!(t[1].text, "Got it");
        assert_eq!(
            t[1].prompt_msg_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
        assert!(!t[2].complete);
        assert_eq!(t[2].text, "");
    }

    #[test]
    fn since_ids() {
        let t = turns(&sample());
        assert_eq!(since(&t, "msg_2").unwrap().len(), 2);
        assert_eq!(since(&t, "msg_1").unwrap().len(), 2);
        assert_eq!(since(&t, "a4").unwrap().len(), 1);
        assert_eq!(since(&t, "u2").unwrap().len(), 0);
        assert!(since(&t, "zzz").is_err());
    }

    #[test]
    fn delivery_detection() {
        let id = "22222222-2222-4222-8222-222222222222";
        let recs = vec![
            json!({"type":"queue-operation","operation":"enqueue","content":"line one\nline two"}),
        ];
        assert_eq!(
            find_delivery(&recs, Some(id), "line one\nline two")
                .unwrap()
                .state,
            "queued"
        );
        let mut recs2 = recs.clone();
        recs2.push(user(
            "x",
            json!("Another Claude session sent a message:\nline one\nline two"),
            Some(json!({"kind":"peer","msg_id":id})),
        ));
        let d = find_delivery(&recs2, Some(id), "line one\nline two").unwrap();
        assert_eq!(d.state, "delivered");
        assert_eq!(d.uuid.as_deref(), Some("x"));
        // a different peer msg with same text but another id is not ours
        let recs3 = vec![user(
            "y",
            json!("line one"),
            Some(json!({"kind":"peer","msg_id":"other"})),
        )];
        assert!(find_delivery(&recs3, Some(id), "line one").is_none());
        // tmux path: no msg id, text match on human prompt
        let recs4 = vec![user(
            "z",
            json!("hello there"),
            Some(json!({"kind":"human"})),
        )];
        assert_eq!(
            find_delivery(&recs4, None, "hello there")
                .unwrap()
                .uuid
                .as_deref(),
            Some("z")
        );
        assert!(find_delivery(&[], Some(id), "x").is_none());
    }
}

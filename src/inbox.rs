//! Claude Code inbox-socket client (cross-session messaging, peerProtocol 1).
//!
//! Wire format (observed in Claude Code 2.1.318, matches the documented
//! `{"type":"user","message":{"role":"user","content":"..."}}` injection
//! example the binary prints in its debug log):
//!   - AF_UNIX / SOCK_STREAM, NDJSON (one JSON object per line).
//!   - Optional first line `{"type":"auth","token":"<hex>"}` (Linux/macOS).
//!   - User frame: {"type":"user","message":{"role":"user","content":<string>},
//!     "msgV":1,"msg_id":"<uuid v4>","priority":"next","from"?:"uds:<path>"}
//!     `content` MUST be a string (non-string content is dropped).
//!   - The receiver never runs slash commands from this channel
//!     (`skipSlashCommands: true`), so `/goal` arrives as plain text.
//!   - Receipts: the receiver sends `{"type":"control","action":
//!     "peer_message_status","status":"held|delivered|denied|expired",
//!     "orig_msg_id":...}` to the `from` address ONLY for messages it holds
//!     for approval. Ordinary deliveries produce no receipt, so ccctl confirms
//!     delivery by watching the target transcript instead (see transcript.rs).

use crate::error::{CliError, Code, Result, err};
use crate::util::random_hex;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const MAX_BYTES: usize = 900_000; // Claude caps serialized messages near 1M chars.

/// RFC 4122 v4 UUID. Claude Code 2.1.318 validates `msg_id` against a UUID
/// regex and records it as `origin.msg_id` on the delivered transcript entry,
/// which is how `send` confirms delivery. (Older community notes describe
/// `cc-msg-<hex>`; the current receiver silently drops such ids.)
pub fn new_msg_id() -> String {
    let h = random_hex(16);
    let mut b: Vec<char> = h.chars().collect();
    b[12] = '4';
    b[16] = ['8', '9', 'a', 'b'][(b[16].to_digit(16).unwrap_or(0) & 3) as usize];
    let s: String = b.into_iter().collect();
    format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )
}

/// `uds:` + path, percent-encoding bytes outside `[A-Za-z0-9:_/.-]`.
pub fn reply_address(path: &Path) -> String {
    let mut s = String::from("uds:");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b":_/.-".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

pub fn build_frame(text: &str, msg_id: &str, priority: &str, from: Option<&str>) -> String {
    let mut v = json!({
        "type": "user",
        "message": {"role": "user", "content": text},
        "msgV": 1,
        "msg_id": msg_id,
        "priority": priority,
    });
    if let Some(f) = from {
        v["from"] = Value::String(f.to_string());
    }
    let mut line = v.to_string();
    line.push('\n');
    line
}

/// Refuse anything that is not a socket owned by us.
pub fn check_socket(path: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(path).map_err(|e| {
        CliError::new(
            Code::Delivery,
            format!("inbox socket {}: {e}", path.display()),
        )
    })?;
    if !md.file_type().is_socket() {
        return err(
            Code::Delivery,
            format!("{} is not a unix socket", path.display()),
        );
    }
    let uid = unsafe { libc::getuid() };
    if md.uid() != uid {
        return err(
            Code::Delivery,
            format!("{} is owned by uid {}, not {uid}", path.display(), md.uid()),
        );
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Receipt {
    pub status: String,
    pub raw: Value,
}

/// A temporary reply socket in the target's socket directory, so the
/// receiver accepts it as a reply target. Removed on drop.
struct ReplySock {
    path: PathBuf,
    listener: UnixListener,
}

impl ReplySock {
    fn bind(dir: &Path) -> Result<ReplySock> {
        // Name must match Claude's accepted pattern ^\d+(-[0-9a-f]{8})?\.sock$;
        // the `-<hex>` suffix keeps us from ever shadowing a live session's pid socket.
        let path = dir.join(format!("{}-{}.sock", std::process::id(), random_hex(4)));
        if path.exists() {
            return err(Code::Delivery, format!("{} already exists", path.display()));
        }
        let listener = UnixListener::bind(&path)
            .map_err(|e| CliError::new(Code::Delivery, format!("bind {}: {e}", path.display())))?;
        listener.set_nonblocking(true)?;
        Ok(ReplySock { path, listener })
    }

    fn wait_receipt(&self, msg_id: &str, timeout: Duration) -> Option<Receipt> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                    for line in BufReader::new(stream).lines().map_while(|l| l.ok()) {
                        if let Some(r) = parse_receipt(&line, msg_id) {
                            return Some(r);
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for ReplySock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn parse_receipt(line: &str, msg_id: &str) -> Option<Receipt> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type")?.as_str()? != "control"
        || v.get("action")?.as_str()? != "peer_message_status"
        || v.get("orig_msg_id")?.as_str()? != msg_id
    {
        return None;
    }
    Some(Receipt {
        status: v.get("status")?.as_str()?.to_string(),
        raw: v,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Sent {
    pub msg_id: String,
    pub socket: PathBuf,
    pub receipt: Option<Receipt>,
}

/// Deliver `text` to the inbox at `socket`.
/// `peer_receipt`: bind a reply socket and wait up to this long for a
/// peer_message_status frame (only arrives for held messages).
pub fn send(
    socket: &Path,
    text: &str,
    priority: &str,
    peer_receipt: Option<Duration>,
) -> Result<Sent> {
    if text.is_empty() {
        return err(Code::Usage, "refusing to send an empty message");
    }
    if text.len() > MAX_BYTES {
        return err(
            Code::Usage,
            format!(
                "message is {} bytes; Claude Code refuses messages near 1M chars",
                text.len()
            ),
        );
    }
    check_socket(socket)?;
    let msg_id = new_msg_id();
    let reply = match (peer_receipt, socket.parent()) {
        (Some(_), Some(dir)) => Some(ReplySock::bind(dir)?),
        _ => None,
    };
    let from = reply.as_ref().map(|r| reply_address(&r.path));
    let frame = build_frame(text, &msg_id, priority, from.as_deref());

    let mut stream = UnixStream::connect(socket).map_err(|e| {
        CliError::new(
            Code::Delivery,
            format!(
                "connect {}: {e} (is the session still running?)",
                socket.display()
            ),
        )
    })?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream
        .write_all(frame.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|e| CliError::new(Code::Delivery, format!("write {}: {e}", socket.display())))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    // The inbox does not answer on the same connection in protocol 1, but
    // drain briefly so an unexpected error line is not lost silently.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let mut sink = Vec::new();
    let _ = stream.take(64 * 1024).read_to_end(&mut sink);
    if !sink.is_empty() {
        eprintln!(
            "ccctl: inbox replied: {}",
            String::from_utf8_lossy(&sink).trim()
        );
    }

    let receipt = match (&reply, peer_receipt) {
        (Some(r), Some(t)) => r.wait_receipt(&msg_id, t),
        _ => None,
    };
    Ok(Sent {
        msg_id,
        socket: socket.to_path_buf(),
        receipt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_shape() {
        let f = build_frame("hi\nthere", "cc-msg-00", "next", None);
        assert!(f.ends_with('\n'));
        let v: Value = serde_json::from_str(f.trim()).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "hi\nthere");
        assert_eq!(v["msgV"], 1);
        assert!(v.get("from").is_none());
        let g = build_frame("x", "cc-msg-00", "now", Some("uds:/a.sock"));
        let v: Value = serde_json::from_str(g.trim()).unwrap();
        assert_eq!(v["from"], "uds:/a.sock");
    }

    #[test]
    fn msg_id_shape() {
        let id = new_msg_id();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!("89ab".contains(&parts[3][..1]));
    }

    #[test]
    fn addr_encoding() {
        assert_eq!(
            reply_address(Path::new("/run/user/1000/cc-socks/1.sock")),
            "uds:/run/user/1000/cc-socks/1.sock"
        );
        assert_eq!(reply_address(Path::new("/a b/x.sock")), "uds:/a%20b/x.sock");
    }

    #[test]
    fn receipt_parse() {
        let l = r#"{"type":"control","action":"peer_message_status","status":"held","orig_msg_id":"cc-msg-1"}"#;
        assert_eq!(parse_receipt(l, "cc-msg-1").unwrap().status, "held");
        assert!(parse_receipt(l, "cc-msg-2").is_none());
        assert!(parse_receipt("garbage", "cc-msg-1").is_none());
    }

    /// End-to-end against a fake inbox: frame arrives as one NDJSON line.
    #[test]
    fn send_to_fake_inbox() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("4242.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).unwrap();
            line
        });
        let sent = send(&sock, "hello\nworld", "next", None).unwrap();
        let line = h.join().unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["message"]["content"], "hello\nworld");
        assert_eq!(v["msg_id"], sent.msg_id.as_str());
    }

    /// Fake inbox that holds the message and sends a receipt back.
    #[test]
    fn peer_receipt_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("4243.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            let from = v["from"]
                .as_str()
                .unwrap()
                .strip_prefix("uds:")
                .unwrap()
                .to_string();
            let mut back = UnixStream::connect(from).unwrap();
            let r = json!({"type":"control","action":"peer_message_status","status":"held",
                           "orig_msg_id": v["msg_id"], "msgV":1});
            back.write_all(format!("{r}\n").as_bytes()).unwrap();
        });
        let sent = send(&sock, "x", "next", Some(Duration::from_secs(3))).unwrap();
        h.join().unwrap();
        assert_eq!(sent.receipt.unwrap().status, "held");
        // reply socket cleaned up
        let left: Vec<_> = std::fs::read_dir(d.path()).unwrap().flatten().collect();
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn refuses_non_socket() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("1.sock");
        std::fs::write(&f, "").unwrap();
        assert_eq!(
            send(&f, "x", "next", None).unwrap_err().code,
            Code::Delivery
        );
    }
}

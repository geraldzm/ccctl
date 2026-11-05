//! Small dependency-free helpers: time, globbing, randomness, /proc access.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Format epoch milliseconds as RFC 3339 UTC (`2026-11-04T15:53:00.123Z`).
pub fn rfc3339_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Howard Hinnant's days -> civil date algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Human "3m ago" style relative time.
pub fn ago(ms: i64) -> String {
    let d = (now_ms() - ms).max(0) / 1000;
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86_400),
    }
}

/// Minimal glob: `*` matches any run of characters, `?` one character.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// `n` random bytes as lowercase hex (from /dev/urandom).
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        // Extremely unlikely on Linux/macOS; fall back to time+pid mixing.
        let mut x = (now_ms() as u64) ^ ((std::process::id() as u64) << 32);
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Expand a leading `~` / `~/` using $HOME.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(p)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Whether a pid is alive (kill(pid, 0) succeeds or fails with EPERM).
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only existence/permission checks.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Parsed subset of /proc/<pid>/stat.
#[derive(Debug, Clone)]
pub struct ProcStat {
    pub ppid: i32,
    /// Field 22: process start time in clock ticks since boot. Claude Code
    /// records the same value as `procStart` in its session registry (Linux).
    pub start_ticks: String,
}

pub fn proc_stat(pid: i32) -> Option<ProcStat> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_proc_stat(&s)
}

pub fn parse_proc_stat(s: &str) -> Option<ProcStat> {
    // comm (field 2) may contain spaces/parens; split after the LAST ')'.
    let close = s.rfind(')')?;
    let rest: Vec<&str> = s[close + 1..].split_whitespace().collect();
    // rest[0] = state (field 3), rest[1] = ppid (field 4), field 22 = rest[19]
    Some(ProcStat {
        ppid: rest.get(1)?.parse().ok()?,
        start_ticks: rest.get(19)?.to_string(),
    })
}

/// Ancestor chain of `pid` (excluding pid itself), nearest first.
pub fn ancestors(pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut cur = pid;
    for _ in 0..64 {
        match proc_stat(cur) {
            Some(st) if st.ppid > 1 => {
                out.push(st.ppid);
                cur = st.ppid;
            }
            _ => break,
        }
    }
    out
}

/// Read a file's bytes from `offset` to EOF.
pub fn read_from(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut f = fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_793_800_000_000), "2026-11-04T13:46:40.000Z");
    }

    #[test]
    fn globbing() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("api-*", "api-worker"));
        assert!(!glob_match("api-*", "docs-writer-2"));
        assert!(glob_match("docs-writer-?", "docs-writer-2"));
        assert!(glob_match("*-worker", "api-worker"));
        assert!(!glob_match("exact", "exact2"));
    }

    #[test]
    fn proc_stat_with_spaces_in_comm() {
        let s = "1234 (tmux: server) S 1 321 321 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 424242 1 1";
        let st = parse_proc_stat(s).unwrap();
        assert_eq!(st.ppid, 1);
        assert_eq!(st.start_ticks, "424242");
    }

    #[test]
    fn hex_len() {
        let h = random_hex(16);
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

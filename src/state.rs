//! Tiny persistent state: when ccctl last acted on a session.
//!
//! `wait --for idle` right after `send`/`cmd` must not return on the stale
//! `idle` the session reported *before* it picked the input up. We record the
//! action time here; `wait` then only accepts an idle whose statusUpdatedAt is
//! newer, or one that stayed put for `settle_ms` (local commands such as
//! `/goal` with no args may never flip the session to busy).

use crate::util::{home, now_ms};
use std::path::PathBuf;

fn dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("state"))
        .join("ccctl")
        .join("last-action")
}

pub fn record(session_id: &str) {
    if session_id.is_empty() || session_id.contains('/') {
        return;
    }
    let d = dir();
    if std::fs::create_dir_all(&d).is_ok() {
        let _ = std::fs::write(d.join(session_id), now_ms().to_string());
    }
}

pub fn last(session_id: &str) -> Option<i64> {
    if session_id.is_empty() || session_id.contains('/') {
        return None;
    }
    std::fs::read_to_string(dir().join(session_id))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Is an `idle` observation trustworthy given the last action time?
pub fn idle_settled(
    status_updated_at: Option<i64>,
    last_action: Option<i64>,
    settle_ms: u64,
    now: i64,
) -> bool {
    match last_action {
        None => true,
        Some(t) => status_updated_at.is_some_and(|u| u > t) || now - t >= settle_ms as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settle_rules() {
        assert!(idle_settled(Some(5), None, 3000, 10));
        assert!(idle_settled(Some(1001), Some(1000), 3000, 1500));
        assert!(!idle_settled(Some(900), Some(1000), 3000, 1500));
        assert!(idle_settled(Some(900), Some(1000), 3000, 4000));
        assert!(!idle_settled(None, Some(1000), 3000, 2000));
    }
}

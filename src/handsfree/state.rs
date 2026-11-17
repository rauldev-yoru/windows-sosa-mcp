//! Hands-free session state file (`<root>\handsfree\state.json`).
//!
//! One writer (the session process) and many readers (every
//! `windows_handsfree` / `windows_input` / `ui_action activate` call).
//! The writer retries on sharing
//! violations instead of crashing, and readers open with
//! `FILE_SHARE_READ|WRITE|DELETE` so a reader can never block the
//! heartbeat. The writer truncates then writes, so a reader can catch an
//! empty or partial document: readers retry the parse too (D26).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

/// Heartbeat age above which a session counts as dead.
pub const STALE_AFTER_S: f64 = 6.0;

pub fn dir() -> PathBuf {
    crate::state::root().join("handsfree")
}

pub fn state_path() -> PathBuf {
    dir().join("state.json")
}

pub fn stop_path() -> PathBuf {
    dir().join("stop.json")
}

/// .NET round-trip (`'o'`) UTC timestamp: 7 fractional digits and `Z`.
pub fn fmt_o(t: DateTime<Utc>) -> String {
    use chrono::Timelike;
    format!(
        "{}.{:07}Z",
        t.format("%Y-%m-%dT%H:%M:%S"),
        (t.nanosecond() % 1_000_000_000) / 100
    )
}

pub fn now_o() -> String {
    fmt_o(Utc::now())
}

/// Parse any RFC 3339 timestamp (the session writes 7 fractional digits).
pub fn parse_utc(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(windows)]
fn open_shared(p: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x7)
        .open(p)
}

#[cfg(not(windows))]
fn open_shared(p: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(p)
}

/// Read a small JSON file written by another process: shared read, up to
/// 20 tries 15 ms apart on I/O errors or a half-written document.
/// `None` when the file is missing or never parses.
pub fn read_shared(p: &std::path::Path) -> Option<Value> {
    for _ in 0..20 {
        match open_shared(p) {
            Ok(mut f) => {
                let mut buf = Vec::new();
                if f.read_to_end(&mut buf).is_ok() {
                    let b = buf.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&buf);
                    if let Ok(v) = serde_json::from_slice::<Value>(b) {
                        return Some(v);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    None
}

pub fn read_state() -> Option<Value> {
    read_shared(&state_path())
}

#[cfg(windows)]
fn create_for_write(p: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ | FILE_SHARE_DELETE: readers may stay open.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(0x1 | 0x4)
        .open(p)
}

#[cfg(not(windows))]
fn create_for_write(p: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(p)
}

/// `WriteState`: up to 25 tries 15 ms apart; `false` when every try failed
/// (the caller counts `state_write_failures`, never crashes).
pub fn write_retrying(p: &std::path::Path, v: &Value) -> bool {
    let body = v.to_string();
    for _ in 0..25 {
        if let Ok(mut f) = create_for_write(p)
            && f.write_all(body.as_bytes()).is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

/// Seconds since `heartbeat_utc`, rounded to 0.1 (half to even).
pub fn heartbeat_age(state: Option<&Value>, now: DateTime<Utc>) -> Option<f64> {
    let hb = parse_utc(state?.get("heartbeat_utc")?.as_str()?)?;
    let s = (now - hb).num_microseconds()? as f64 / 1e6;
    Some((s * 10.0).round_ties_even() / 10.0)
}

/// Active and heartbeat at most 6 s old.
pub fn is_live(state: Option<&Value>, now: DateTime<Utc>) -> bool {
    let active = state.is_some_and(|s| s["active"].as_bool() == Some(true));
    active && heartbeat_age(state, now).is_some_and(|a| a <= STALE_AFTER_S)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timestamps_round_trip_with_seven_digits() {
        let t = parse_utc("2026-11-16T22:01:02.1234567Z").unwrap();
        assert_eq!(fmt_o(t), "2026-11-16T22:01:02.1234567Z");
        assert!(parse_utc("2026-11-16T22:01:02.123456+00:00").is_some());
        assert!(parse_utc("nope").is_none());
    }

    #[test]
    fn liveness_follows_the_heartbeat() {
        let now = parse_utc("2026-11-16T22:00:10Z").unwrap();
        let s = json!({"active": true, "heartbeat_utc": "2026-11-16T22:00:04.0000000Z"});
        assert_eq!(heartbeat_age(Some(&s), now), Some(6.0));
        assert!(is_live(Some(&s), now));
        let s = json!({"active": true, "heartbeat_utc": "2026-11-16T22:00:03.9000000Z"});
        assert_eq!(heartbeat_age(Some(&s), now), Some(6.1));
        assert!(!is_live(Some(&s), now));
        let s = json!({"active": false, "heartbeat_utc": "2026-11-16T22:00:09Z"});
        assert!(!is_live(Some(&s), now));
        assert!(!is_live(None, now));
        assert_eq!(heartbeat_age(None, now), None);
    }

    #[test]
    fn shared_reader_and_retrying_writer() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.json");
        assert!(read_shared(&p).is_none());
        assert!(write_retrying(&p, &json!({"a": "ñ"})));
        assert_eq!(read_shared(&p).unwrap()["a"], "ñ");
        std::fs::write(&p, b"\xef\xbb\xbf{\"b\":1}").unwrap();
        assert_eq!(read_shared(&p).unwrap()["b"], 1);
        std::fs::write(&p, b"{\"half").unwrap();
        assert!(read_shared(&p).is_none());
    }
}

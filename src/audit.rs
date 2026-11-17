//! Metadata-only audit log.
//!
//! JSON lines in `<state>\audit\events.jsonl`, rotated at 2 MiB keeping 3
//! backups (`events.jsonl.1` .. `.3`). Phases: `session` (server start/stop),
//! `started`, `finished`, `rejected`. Never stores scripts, file contents,
//! edit fragments, search patterns or outputs. Write failures never deny the
//! operation: the caller attaches `audit_warning`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub const MAX_BYTES: u64 = 2 * 1_048_576;
pub const BACKUPS: u32 = 3;
/// Env var naming the remote caller, if a launcher provides one.
pub const PEER_ENV: &str = "ENISHI_PEER";

pub static SESSION_ID: LazyLock<String> = LazyLock::new(|| uuid::Uuid::new_v4().to_string());
static WRITE: Mutex<()> = Mutex::new(());

const PLAIN_FIELDS: &[&str] = &[
    "path",
    "cwd",
    "mode",
    "timeout_seconds",
    "max_output_bytes",
    "max_bytes",
    "max_lines",
    "start_line",
    "include_sha256",
    "expected_sha256",
    "dry_run",
    "encoding",
    "source_encoding",
    "bom",
    "newline",
    "name",
    "kind",
    "max_depth",
    "limit",
    "offset",
    "scan_limit",
    "budget_ms",
    "include_hidden",
    "respect_ignore",
    "case_sensitive",
    "context_lines",
    "include",
    "exclude",
    "job_id",
    "label",
    "tail_bytes",
    "state",
    "wait_seconds",
    "prune_finished_older_than_hours",
    "id",
    "version",
    "scope",
    "source",
    "allow_upgrade",
    "include_available_versions",
    "additional_directories",
    "request_id",
    "snapshot_id",
    "ref",
    "element",
    "action",
    "max_nodes",
    "detail",
    "monitor",
    "max_width",
    "redact",
    "restore_foreground",
    "include_untitled",
    "include_offscreen",
    "reason",
    "capture_id",
    "expect_foreground_hwnd",
    "abort_on_foreground_change",
    "capture_after",
    "condition",
    "idle_ms",
    "poll_ms",
    "glow_px",
    "arm_delay_ms",
];
const HASHED_FIELDS: &[&str] = &[
    "script",
    "content",
    "pattern",
    "value",
    "title_contains",
    "name_contains",
];

pub fn log_path() -> PathBuf {
    crate::state::audit_dir().join("events.jsonl")
}

pub fn digest_text(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Redacted view of tool arguments: whitelisted scalars, hashes for content fields.
pub fn summarize_arguments(arguments: &Value) -> Map<String, Value> {
    let mut summary = Map::new();
    let Some(args) = arguments.as_object() else {
        return summary;
    };
    for (key, value) in args {
        let k = key.as_str();
        if HASHED_FIELDS.contains(&k) && value.is_string() {
            let s = value.as_str().unwrap_or_default();
            summary.insert(format!("{k}_sha256"), json!(digest_text(s)));
            summary.insert(format!("{k}_chars"), json!(chars(s)));
        } else if k == "edits" && value.is_array() {
            let list = value.as_array().map(Vec::as_slice).unwrap_or_default();
            let total: usize = list
                .iter()
                .filter_map(Value::as_object)
                .map(|e| field_len(e.get("old_text")) + field_len(e.get("new_text")))
                .sum();
            summary.insert("edits_count".into(), json!(list.len()));
            summary.insert("edits_chars".into(), json!(total));
        } else if k == "actions" && value.is_array() {
            let list = value.as_array().map(Vec::as_slice).unwrap_or_default();
            let objs: Vec<&Map<String, Value>> = list.iter().filter_map(Value::as_object).collect();
            let types: Vec<Value> = list
                .iter()
                .take(50)
                .filter_map(Value::as_object)
                .map(|a| json!(display_str(a.get("type"))))
                .collect();
            let points: usize = objs
                .iter()
                .map(|a| a.get("path").and_then(Value::as_array).map_or(0, Vec::len))
                .sum();
            let text: usize = objs.iter().map(|a| field_len(a.get("text"))).sum();
            summary.insert("actions_count".into(), json!(list.len()));
            summary.insert("actions_types".into(), Value::Array(types));
            summary.insert("actions_points".into(), json!(points));
            summary.insert("actions_text_chars".into(), json!(text));
        } else if k == "window" && value.is_object() {
            let w = value.as_object().cloned().unwrap_or_default();
            let mut out = Map::new();
            for (wk, wv) in &w {
                if wk == "hwnd" || wk == "process" {
                    out.insert(wk.clone(), wv.clone());
                }
            }
            if let Some(t) = w.get("title_contains") {
                out.insert(
                    "title_contains_sha256".into(),
                    json!(digest_text(&display_str(Some(t)))),
                );
            }
            summary.insert("window".into(), Value::Object(out));
        } else if PLAIN_FIELDS.contains(&k) {
            match value {
                Value::String(s) => {
                    summary.insert(k.into(), json!(truncate_chars(s, 512)));
                }
                Value::Number(_) | Value::Bool(_) | Value::Null => {
                    summary.insert(k.into(), value.clone());
                }
                Value::Array(a) if a.iter().all(Value::is_string) => {
                    let v: Vec<Value> = a
                        .iter()
                        .take(20)
                        .map(|s| json!(truncate_chars(s.as_str().unwrap_or_default(), 256)))
                        .collect();
                    summary.insert(k.into(), Value::Array(v));
                }
                _ => {}
            }
        } else {
            summary.insert(format!("{k}_present"), json!(true));
        }
    }
    summary
}

fn display_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn field_len(v: Option<&Value>) -> usize {
    match v {
        None => 0,
        Some(Value::String(s)) => chars(s),
        Some(other) => chars(&display_str(Some(other))),
    }
}

/// Caller identity: `ENISHI_PEER` when a launcher sets it, else `local`.
pub fn caller() -> String {
    std::env::var(PEER_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local".into())
}

fn user() -> Option<String> {
    let u = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()?;
    match std::env::var("USERDOMAIN") {
        Ok(d) if cfg!(windows) && !d.is_empty() => Some(format!("{d}\\{u}")),
        _ => Some(u),
    }
}

fn rotate(path: &PathBuf) -> std::io::Result<()> {
    for i in (1..BACKUPS).rev() {
        let src = path.with_extension(format!("jsonl.{i}"));
        if src.exists() {
            let dst = path.with_extension(format!("jsonl.{}", i + 1));
            let _ = fs::remove_file(&dst);
            fs::rename(&src, &dst)?;
        }
    }
    let first = path.with_extension("jsonl.1");
    let _ = fs::remove_file(&first);
    fs::rename(path, first)
}

/// Append one event. Errors only if the log cannot be opened/written.
pub fn record(phase: &str, fields: Map<String, Value>) -> std::io::Result<()> {
    let mut event = Map::new();
    event.insert("ts".into(), json!(crate::envelope::now_iso()));
    event.insert("phase".into(), json!(phase));
    event.insert("session_id".into(), json!(*SESSION_ID));
    event.insert("pid".into(), json!(std::process::id()));
    event.insert("user".into(), json!(user()));
    event.insert("caller".into(), json!(caller()));
    for (k, v) in fields {
        if !v.is_null() {
            event.insert(k, v);
        }
    }
    let line = serde_json::to_string(&Value::Object(event))? + "\n";
    let path = log_path();
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if let Ok(meta) = fs::metadata(&path)
        && meta.len() + line.len() as u64 > MAX_BYTES
        && meta.len() > 0
    {
        rotate(&path)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(line.as_bytes())
}

const RESULT_FIELDS: &[&str] = &[
    "request_id",
    "started_at",
    "operation",
    "status",
    "exit_code",
    "duration_ms",
    "truncated",
    "tool_name",
    "server_version",
    "error_kind",
    "may_have_changed",
    "script_sha256",
    "script_chars",
    "job_id",
    "timed_out",
    "snapshot_id",
    "capture_sha256",
    "session_id",
];

/// Finished/rejected event from a result object (metadata subset only).
pub fn record_result(
    result: &Value,
    phase: &str,
    extra: Map<String, Value>,
) -> std::io::Result<()> {
    let mut payload = Map::new();
    for k in RESULT_FIELDS {
        payload.insert((*k).into(), result.get(*k).cloned().unwrap_or(Value::Null));
    }
    if payload.get("error_kind").is_none_or(Value::is_null)
        && let Some(kind) = result.get("error").and_then(|e| e.get("kind"))
    {
        payload.insert("error_kind".into(), kind.clone());
    }
    payload.extend(extra);
    record(phase, payload)
}

pub fn session_event(event: &str) -> std::io::Result<()> {
    let mut f = Map::new();
    f.insert("event".into(), json!(event));
    f.insert("server_version".into(), json!(env!("CARGO_PKG_VERSION")));
    record("session", f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_redacts_content_and_keeps_plain_fields() {
        let s = summarize_arguments(&json!({
            "script": "Get-Date", "path": "C:\\x", "edits": [{"old_text": "ab", "new_text": "cde"}],
            "secret_blob": {"a": 1}, "include": ["*.rs"], "timeout_seconds": 5
        }));
        assert_eq!(s["script_chars"], json!(8));
        assert_eq!(s["script_sha256"], json!(digest_text("Get-Date")));
        assert!(s.get("script").is_none());
        assert_eq!(s["path"], json!("C:\\x"));
        assert_eq!(s["edits_count"], json!(1));
        assert_eq!(s["edits_chars"], json!(5));
        assert_eq!(s["secret_blob_present"], json!(true));
        assert_eq!(s["include"], json!(["*.rs"]));
        assert_eq!(s["timeout_seconds"], json!(5));
    }

    #[test]
    fn long_plain_strings_are_cut() {
        let s = summarize_arguments(&json!({"path": "a".repeat(600)}));
        assert_eq!(s["path"].as_str().unwrap().len(), 512);
    }
}

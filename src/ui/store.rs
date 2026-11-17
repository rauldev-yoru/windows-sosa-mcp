//! Snapshot and capture records: JSON files under the state root, refs valid 120 s,
//! capture metadata 600 s, files older than 600 s pruned on write.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::UiError;
use super::render::{Node, control_type_id};

pub const SNAPSHOT_TTL_SECONDS: f64 = 120.0;
pub const CAPTURE_TTL_SECONDS: u64 = 600;
const PRUNE_SECONDS: u64 = 600;

pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn snapshot_dir() -> PathBuf {
    crate::state::root().join("ui_snapshots")
}

pub fn capture_dir() -> PathBuf {
    crate::state::root().join("ui_captures")
}

/// Ids are 32 hex chars (uuid4().hex); anything else cannot name a file.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn write_record(dir: &Path, id: &str, rec: &Value) -> Result<(), UiError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| UiError::new("io_error", format!("cannot create {}: {e}", dir.display())))?;
    let path = dir.join(format!("{id}.json"));
    std::fs::write(&path, rec.to_string())
        .map_err(|e| UiError::new("io_error", format!("cannot write {}: {e}", path.display())))?;
    prune(dir);
    Ok(())
}

fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json")
            && let Ok(m) = e.metadata().and_then(|m| m.modified())
            && now.duration_since(m).map(|d| d.as_secs()).unwrap_or(0) > PRUNE_SECONDS
        {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Persist a snapshot's refs; returns `(snapshot_id, expires_at)`.
pub fn save_snapshot(
    window: &Value,
    nodes: &[Node],
    scope: &str,
) -> Result<(String, f64), UiError> {
    save_snapshot_in(&snapshot_dir(), window, nodes, scope)
}

fn save_snapshot_in(
    dir: &Path,
    window: &Value,
    nodes: &[Node],
    scope: &str,
) -> Result<(String, f64), UiError> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let now = now_epoch();
    let expires = now + SNAPSHOT_TTL_SECONDS;
    let mut refs = Map::new();
    let mut order = Vec::new();
    for n in nodes {
        if let Some(r) = &n.ref_ {
            refs.insert(
                r.clone(),
                json!({"runtime_id": n.runtime_id, "role": n.role, "name": n.name,
                       "automation_id": n.automation_id, "patterns": n.patterns}),
            );
            order.push(r.clone());
        }
    }
    // The ref of the scope root (`e1` of a window snapshot): an action on it
    // targets the window itself, whose identity (hwnd + pid + process
    // start) is verified independently of its title (D35).
    let root_ref = nodes.first().and_then(|n| n.ref_.clone());
    let rec = json!({"snapshot_id": id, "created_at": now, "expires_at": expires,
                     "window": window, "refs": refs, "ref_order": order, "scope": scope,
                     "root_ref": root_ref});
    write_record(dir, &id, &rec)?;
    Ok((id, expires))
}

/// `load_snapshot`: snapshot_not_found / snapshot_expired.
pub fn load_snapshot(id: &str) -> Result<Value, UiError> {
    load_snapshot_in(&snapshot_dir(), id)
}

fn load_snapshot_in(dir: &Path, id: &str) -> Result<Value, UiError> {
    let not_found = || {
        UiError::new(
            "snapshot_not_found",
            "unknown snapshot_id; take windows_ui_snapshot first",
        )
    };
    if !valid_id(id) {
        return Err(not_found());
    }
    let path = dir.join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path).map_err(|_| not_found())?;
    let rec: Value = serde_json::from_str(&text).map_err(|_| not_found())?;
    if now_epoch() > rec["expires_at"].as_f64().unwrap_or(0.0) {
        return Err(UiError::new(
            "snapshot_expired",
            format!(
                "snapshot older than {} s; take a new one",
                SNAPSHOT_TTL_SECONDS as u64
            ),
        ));
    }
    Ok(rec)
}

/// A ref resolved against its snapshot record.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub runtime_id: Vec<i32>,
    pub control_type_id: i32,
    pub automation_id: Option<String>,
    pub name: Option<String>,
    pub role: String,
    /// The ref is the root of a `scope=window` snapshot, i.e. the window
    /// element itself (D35).
    pub window_root: bool,
}

pub fn anchor_for(rec: &Value, r: &str) -> Result<Anchor, UiError> {
    let Some(node) = rec["refs"].get(r) else {
        let order: Vec<String> = match rec.get("ref_order").and_then(Value::as_array) {
            Some(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => rec["refs"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default(),
        };
        let shown: Vec<&str> = order.iter().take(20).map(String::as_str).collect();
        return Err(UiError::new(
            "ref_not_found",
            format!(
                "ref {r} is not in snapshot {} (refs: {})",
                rec["snapshot_id"].as_str().unwrap_or(""),
                shown.join(", ")
            ),
        ));
    };
    let text = |k: &str| {
        node.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let role = node["role"].as_str().unwrap_or("Custom").to_string();
    Ok(Anchor {
        runtime_id: node["runtime_id"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_i64().map(|x| x as i32))
                    .collect()
            })
            .unwrap_or_default(),
        control_type_id: control_type_id(&role),
        automation_id: text("automation_id"),
        name: text("name"),
        role,
        window_root: rec["scope"].as_str() == Some("window") && rec["root_ref"].as_str() == Some(r),
    })
}

/// `window_selector(record)`: the snapshot's window identity.
pub fn window_selector(rec: &Value) -> Value {
    let w = &rec["window"];
    json!({"hwnd": w["hwnd"], "pid": w["pid"], "process_start_utc": w.get("process_start_utc").cloned().unwrap_or(Value::Null)})
}

/// `save_capture`: persist origin/scale for `windows_input`; returns the id.
pub fn save_capture(
    capture: &Value,
    kind: &str,
    window_hwnd: Option<i64>,
    monitor: Option<&str>,
) -> Result<String, UiError> {
    save_capture_in(&capture_dir(), capture, kind, window_hwnd, monitor)
}

fn save_capture_in(
    dir: &Path,
    capture: &Value,
    kind: &str,
    window_hwnd: Option<i64>,
    monitor: Option<&str>,
) -> Result<String, UiError> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let rec = json!({"capture_id": id, "created_at": now_epoch(), "kind": kind,
        "origin_x": capture["origin_x"], "origin_y": capture["origin_y"], "scale": capture["scale"],
        "width": capture["width"], "height": capture["height"], "sha256": capture["sha256"],
        "window_hwnd": window_hwnd, "monitor": monitor});
    write_record(dir, &id, &rec)?;
    Ok(id)
}

/// Load a capture record (used by `windows_input` for `capture_id`).
pub fn load_capture(id: &str) -> Result<Value, UiError> {
    load_capture_in(&capture_dir(), id)
}

fn load_capture_in(dir: &Path, id: &str) -> Result<Value, UiError> {
    let nf = || {
        UiError::new(
            "capture_not_found",
            format!("unknown or expired capture_id (captures are kept {CAPTURE_TTL_SECONDS} s)"),
        )
    };
    if !valid_id(id) {
        return Err(nf());
    }
    let text = std::fs::read_to_string(dir.join(format!("{id}.json"))).map_err(|_| nf())?;
    serde_json::from_str(&text).map_err(|_| nf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render::{Classifier, RawNode};

    fn nodes() -> Vec<Node> {
        let mut c = Classifier::new(None);
        (0..12)
            .map(|i| {
                c.node(
                    RawNode {
                        role: if i == 0 { "Window" } else { "Button" }.into(),
                        name: format!("b{i}"),
                        patterns: vec!["invoke"],
                        runtime_id: vec![42, i],
                        ..RawNode::default()
                    },
                    usize::from(i != 0),
                )
            })
            .collect()
    }

    #[test]
    fn snapshot_roundtrip_anchor_and_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        {
            let win = json!({"hwnd": 10, "pid": 20, "process_start_utc": "2026-01-01T00:00:00.0000000Z", "title": "t"});
            let (id, exp) = save_snapshot_in(d, &win, &nodes(), "window").unwrap();
            assert_eq!(id.len(), 32);
            assert!(exp > now_epoch() + 100.0);
            let rec = load_snapshot_in(d, &id).unwrap();
            let a = anchor_for(&rec, "e3").unwrap();
            assert_eq!(a.runtime_id, vec![42, 2]);
            assert_eq!(a.control_type_id, 50000);
            assert_eq!(a.name.as_deref(), Some("b2"));
            assert_eq!(a.automation_id, None);
            assert!(!a.window_root);
            assert!(anchor_for(&rec, "e1").unwrap().window_root);
            let e = anchor_for(&rec, "e99").unwrap_err();
            assert_eq!(e.kind, "ref_not_found");
            assert!(
                e.message
                    .ends_with("(refs: e1, e2, e3, e4, e5, e6, e7, e8, e9, e10, e11, e12)"),
                "{}",
                e.message
            );
            assert_eq!(
                window_selector(&rec),
                json!({"hwnd": 10, "pid": 20, "process_start_utc": "2026-01-01T00:00:00.0000000Z"})
            );
            assert_eq!(
                load_snapshot_in(d, "deadbeef").unwrap_err().kind,
                "snapshot_not_found"
            );
            assert_eq!(
                load_snapshot_in(d, "../x").unwrap_err().kind,
                "snapshot_not_found"
            );
            // Expire it.
            let path = d.join(format!("{id}.json"));
            let mut rec = rec;
            rec["expires_at"] = json!(now_epoch() - 1.0);
            std::fs::write(&path, rec.to_string()).unwrap();
            assert_eq!(
                load_snapshot_in(d, &id).unwrap_err().kind,
                "snapshot_expired"
            );
        }
    }

    #[test]
    fn capture_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        {
            let cap = json!({"origin_x": -2560, "origin_y": -1440, "scale": 0.5, "width": 1280, "height": 720, "sha256": "ab"});
            let id = save_capture_in(d, &cap, "monitor", None, Some("\\\\.\\DISPLAY1")).unwrap();
            let rec = load_capture_in(d, &id).unwrap();
            assert_eq!(rec["origin_x"], -2560);
            assert_eq!(rec["kind"], "monitor");
            assert_eq!(
                load_capture_in(d, "nope").unwrap_err().kind,
                "capture_not_found"
            );
        }
    }

    #[test]
    fn window_root_only_for_root_of_window_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let win = json!({"hwnd": 10, "pid": 20, "title": "t"});
        let (id, _) = save_snapshot_in(d, &win, &nodes(), "ref").unwrap();
        let rec = load_snapshot_in(d, &id).unwrap();
        assert_eq!(rec["root_ref"], "e1");
        assert!(!anchor_for(&rec, "e1").unwrap().window_root);
        let (id, _) = save_snapshot_in(d, &win, &nodes(), "window").unwrap();
        let mut rec = load_snapshot_in(d, &id).unwrap();
        assert!(anchor_for(&rec, "e1").unwrap().window_root);
        assert!(!anchor_for(&rec, "e2").unwrap().window_root);
        // Records written before D35 have no root_ref: no fallback.
        rec.as_object_mut().unwrap().remove("root_ref");
        assert!(!anchor_for(&rec, "e1").unwrap().window_root);
    }
}

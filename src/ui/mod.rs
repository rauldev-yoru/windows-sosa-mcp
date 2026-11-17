//! UI tools: `windows_list_windows`, `windows_ui_snapshot`,
//! `windows_ui_action`, `windows_capture_window`, `windows_capture_monitor`.
//!
//! Argument defaults, snapshot/capture stores, result shaping and warnings
//! live here, on top of the native backend (`win`: UI Automation through
//! COM, GDI capture).

pub mod frame;
pub mod policy;
pub mod render;
pub mod store;
#[cfg(windows)]
pub mod win;

use std::time::Instant;

use serde_json::{Value, json};

use crate::envelope::{self, Envelope};

/// A typed tool failure (`error.kind` + message).
#[derive(Debug, Clone, PartialEq)]
pub struct UiError {
    pub kind: String,
    pub message: String,
    pub may_have_changed: bool,
}

impl UiError {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            kind: kind.into(),
            message: if message.is_empty() {
                kind.into()
            } else {
                message
            },
            may_have_changed: false,
        }
    }

    pub fn changed(mut self, v: bool) -> Self {
        self.may_have_changed = self.may_have_changed || v;
        self
    }
}

/// The five UI tools.
pub const TOOLS: [&str; 5] = [
    "windows_list_windows",
    "windows_ui_snapshot",
    "windows_ui_action",
    "windows_capture_window",
    "windows_capture_monitor",
];

pub fn is_ui_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// `validate`: schema defaults filled in (arguments were already checked
/// against the contract schema by the server).
pub fn with_defaults(name: &str, a: &Value) -> Value {
    let defaults = match name {
        "windows_list_windows" => json!({"include_untitled": false, "limit": 50}),
        "windows_ui_snapshot" => json!({"scope": "window", "max_nodes": 300, "max_depth": 25,
            "budget_ms": 6000, "include_offscreen": false, "detail": "compact"}),
        "windows_ui_action" => json!({"restore_foreground": true, "settle_ms": 300}),
        "windows_capture_window" => json!({"max_width": 1280}),
        "windows_capture_monitor" => {
            json!({"monitor": "foreground", "max_width": 1280, "redact": true})
        }
        _ => json!({}),
    };
    let mut out = defaults.as_object().cloned().unwrap_or_default();
    if let Some(m) = a.as_object() {
        out.extend(m.clone());
    }
    if name == "windows_ui_action"
        && let Some(s) = out.get("scroll").and_then(Value::as_object)
    {
        let mut sc = json!({"amount": "line", "times": 1})
            .as_object()
            .cloned()
            .unwrap_or_default();
        sc.extend(s.clone());
        out.insert("scroll".into(), Value::Object(sc));
    }
    Value::Object(out)
}

/// ISO 8601 UTC of a Unix timestamp (microseconds only when non-zero).
pub fn iso_from_epoch(t: f64) -> String {
    let secs = t.floor() as i64;
    let micros = ((t - t.floor()) * 1e6).round() as u32;
    let (secs, micros) = if micros >= 1_000_000 {
        (secs + 1, 0)
    } else {
        (secs, micros)
    };
    let dt = chrono::DateTime::from_timestamp(secs, micros * 1000).unwrap_or_default();
    if micros == 0 {
        dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        dt.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

/// `build_snapshot_result` minus the store write (done by the caller).
#[allow(clippy::too_many_arguments)]
pub fn snapshot_result(
    snapshot_id: &str,
    expires: f64,
    raw_window: &Value,
    scope: &str,
    scope_root: &Value,
    nodes: &[render::Node],
    detail: &str,
    stats: &Value,
    foreground: &Value,
    user_idle_ms: i64,
) -> Value {
    let elements: Vec<Value> = nodes
        .iter()
        .filter(|n| n.ref_.is_some())
        .map(render::element)
        .collect();
    let hint = if stats["truncated"].as_bool() == Some(true) {
        json!(format!(
            "Snapshot truncated ({}). Narrow with scope=web_document / scope=ref / name_contains or raise max_nodes.",
            stats["stop_reason"].as_str().unwrap_or("")
        ))
    } else if elements.is_empty() {
        json!(
            "No actionable elements found. The app may not expose UI Automation (try include_offscreen, or windows_capture_window to see it)."
        )
    } else {
        Value::Null
    };
    json!({
        "snapshot_id": snapshot_id,
        "expires_at_utc": iso_from_epoch(expires),
        "window": raw_window,
        "scope": scope,
        "scope_root": scope_root,
        "tree": render::render_tree(nodes, detail),
        "elements": elements,
        "stats": stats,
        "foreground": foreground,
        "user_idle_ms": user_idle_ms,
        "hint": hint,
    })
}

/// Warnings added by `call` to an action result.
pub fn action_warnings(raw: &Value) -> Vec<String> {
    let mut w = Vec::new();
    if raw["foreground_taken_by_target"].as_bool() == Some(true) {
        if raw["foreground_restored"].as_bool() == Some(true) {
            w.push("The target window took the foreground (UWP Invoke and Win32 edit SetValue do this); it was given back to the user's previous window.".to_string());
        } else {
            w.push("The target window took the foreground (UWP Invoke and Win32 edit SetValue do this) and it could not be given back (user active or Windows policy).".to_string());
        }
    } else if raw["foreground_changed"].as_bool() == Some(true) {
        w.push("Foreground changed to another window during the action (user activity or a popup); not caused by the target.".to_string());
    }
    let cands = raw["candidates_with_same_identity"].as_i64().unwrap_or(1);
    if cands > 1 {
        w.push(format!(
            "{cands} elements share this role/name; RuntimeId matched exactly one."
        ));
    }
    if raw["after"]["exists"].as_bool() == Some(false) {
        w.push("Element no longer exists after the action (expected for close/menus); take a new snapshot.".to_string());
    }
    w
}

/// Result of one UI tool call: data plus an optional PNG (base64).
pub(crate) type Outcome = Result<(Value, Option<String>), UiError>;

/// Run one UI tool; returns the result envelope.
pub fn execute(name: &str, args: &Value) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base(name);
    let args = with_defaults(name, args);
    match call(name, &args) {
        Ok((data, image)) => {
            if let Some(img) = image {
                r.insert("image_png_base64".into(), json!(img));
                r.insert("capture_sha256".into(), data["capture"]["sha256"].clone());
            }
            if name == "windows_ui_snapshot" {
                r.insert("snapshot_id".into(), data["snapshot_id"].clone());
            }
            envelope::succeed(&mut r, data);
        }
        Err(e) => {
            envelope::fail(&mut r, &e.kind, &e.message);
            let changed =
                e.may_have_changed || (name == "windows_ui_action" && e.kind == "timeout");
            r.insert("may_have_changed".into(), json!(changed));
            if e.kind == "timeout" {
                r.insert("timed_out".into(), json!(true));
                r.insert("windows_termination_confirmed".into(), json!(false));
            }
        }
    }
    envelope::stamp(&mut r, start);
    r
}

#[cfg(windows)]
fn add_capture_id(data: &mut Value, kind: &str) -> Result<(), UiError> {
    let hwnd = data["window"]["hwnd"].as_i64();
    let monitor = data["monitor"]["device"].as_str().map(String::from);
    let id = store::save_capture(&data["capture"], kind, hwnd, monitor.as_deref())?;
    if let Some(c) = data.get_mut("capture").and_then(Value::as_object_mut) {
        c.insert("capture_id".into(), json!(id));
    }
    Ok(())
}

#[cfg(windows)]
fn str_arg<'a>(a: &'a Value, k: &str) -> &'a str {
    a[k].as_str().unwrap_or("")
}

#[cfg(windows)]
fn u64_arg(a: &Value, k: &str) -> u64 {
    a[k].as_u64().unwrap_or(0)
}

#[cfg(not(windows))]
pub(crate) fn call(_name: &str, _args: &Value) -> Outcome {
    // Policy and stores still load so their errors surface first, as there.
    policy::Policy::load()?;
    Err(UiError::new(
        "unsupported_platform",
        "UI tools need Windows (UI Automation and GDI)",
    ))
}

#[cfg(windows)]
pub(crate) fn call(name: &str, a: &Value) -> Outcome {
    use std::time::Duration;
    use win::{ActReq, Selector, SnapReq};

    let policy = policy::Policy::load()?;
    match name {
        "windows_list_windows" => {
            let process = a["process"].as_str().map(String::from);
            let title = a["title_contains"].as_str().map(String::from);
            let untitled = a["include_untitled"].as_bool().unwrap_or(false);
            let limit = u64_arg(a, "limit") as usize;
            let data = win::run_detached(Duration::from_secs(25), move || {
                Ok(win::list_windows(
                    &policy,
                    process.as_deref(),
                    title.as_deref(),
                    untitled,
                    limit,
                ))
            })?;
            Ok((data, None))
        }
        "windows_ui_snapshot" => {
            let scope = str_arg(a, "scope").to_string();
            let budget_ms = u64_arg(a, "budget_ms");
            let mut req = SnapReq {
                window: Selector::from_json(&a["window"]),
                scope: scope.clone(),
                anchor: None,
                max_nodes: u64_arg(a, "max_nodes") as usize,
                max_depth: u64_arg(a, "max_depth") as usize,
                budget_ms,
                include_offscreen: a["include_offscreen"].as_bool().unwrap_or(false),
                name_contains: a["name_contains"].as_str().map(String::from),
            };
            if scope == "ref" {
                let rec = store::load_snapshot(str_arg(a, "snapshot_id"))?;
                req.window = Selector::from_json(&store::window_selector(&rec));
                req.anchor = Some(store::anchor_for(&rec, str_arg(a, "ref"))?);
            }
            let timeout = Duration::from_secs((budget_ms / 1000 + 20).max(30));
            let raw = win::run_uia(timeout, move |u| win::snapshot(u, &policy, &req))?;
            let (id, expires) = store::save_snapshot(&raw.window, &raw.nodes, &scope)?;
            let data = snapshot_result(
                &id,
                expires,
                &raw.window,
                &scope,
                &raw.scope_root,
                &raw.nodes,
                str_arg(a, "detail"),
                &raw.stats,
                &raw.foreground,
                raw.user_idle_ms,
            );
            Ok((data, None))
        }
        "windows_ui_action" => {
            let sid = str_arg(a, "snapshot_id").to_string();
            let rref = str_arg(a, "ref").to_string();
            let act = str_arg(a, "action").to_string();
            // Gate first: without a session `activate` is refused whatever
            // the snapshot id (D35).
            if act == "activate"
                && !crate::handsfree::state::is_live(
                    crate::handsfree::state::read_state().as_ref(),
                    chrono::Utc::now(),
                )
            {
                return Err(UiError::new(
                    "handsfree_required",
                    "activate needs hands-free mode (windows_handsfree start); in background mode use invoke/set_value which do not need the foreground",
                ));
            }
            let rec = store::load_snapshot(&sid)?;
            let req = ActReq {
                window: Selector::from_json(&store::window_selector(&rec)),
                anchor: store::anchor_for(&rec, &rref)?,
                action: act,
                value: a.get("value").cloned(),
                scroll: a.get("scroll").cloned(),
                restore_foreground: a["restore_foreground"].as_bool().unwrap_or(true),
                settle_ms: u64_arg(a, "settle_ms"),
            };
            let raw = win::run_uia(Duration::from_secs(40), move |u| {
                win::action(u, &policy, &req)
            })
            .map_err(|e| e.changed(true))?;
            let mut data = serde_json::Map::new();
            data.insert("snapshot_id".into(), json!(sid));
            data.insert("ref".into(), json!(rref));
            data.insert("element".into(), a["element"].clone());
            for k in [
                "applied",
                "executor",
                "action",
                "activation_steps",
                "before",
                "after",
                "foreground_before",
                "foreground_after",
                "foreground_changed",
                "foreground_taken_by_target",
                "foreground_restored",
                "user_idle_ms",
                "candidates_with_same_identity",
            ] {
                data.insert(k.into(), raw.get(k).cloned().unwrap_or(Value::Null));
            }
            data.insert("warnings".into(), json!(action_warnings(&raw)));
            Ok((Value::Object(data), None))
        }
        "windows_capture_window" => {
            let sel = Selector::from_json(&a["window"]);
            let mw = u64_arg(a, "max_width") as u32;
            let (mut data, png) = win::run_detached(Duration::from_secs(40), move || {
                win::capture_window(&policy, &sel, mw)
            })?;
            add_capture_id(&mut data, "window")?;
            Ok((data, Some(png)))
        }
        "windows_capture_monitor" => {
            if !policy.capture_monitor_enabled {
                return Err(UiError::new(
                    "capture_disabled_by_policy",
                    format!("capture_monitor_enabled=false in {}", policy.source),
                ));
            }
            let redact = a["redact"].as_bool().unwrap_or(true);
            if !redact && policy.has_exclusions() {
                return Err(UiError::new(
                    "redaction_required",
                    "redact=false is not allowed while the policy excludes windows",
                ));
            }
            let monitor = str_arg(a, "monitor").to_string();
            let mw = u64_arg(a, "max_width") as u32;
            let (mut data, png) = win::run_detached(Duration::from_secs(60), move || {
                win::capture_monitor(&policy, &monitor, mw, redact)
            })?;
            add_capture_id(&mut data, "monitor")?;
            Ok((data, Some(png)))
        }
        _ => Err(UiError::new("unknown_tool", "")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_scroll_merge() {
        let a = with_defaults(
            "windows_ui_action",
            &json!({"action": "scroll", "scroll": {"direction": "down"}}),
        );
        assert_eq!(a["settle_ms"], 300);
        assert_eq!(a["restore_foreground"], true);
        assert_eq!(
            a["scroll"],
            json!({"amount": "line", "times": 1, "direction": "down"})
        );
        let a = with_defaults(
            "windows_ui_snapshot",
            &json!({"window": {"hwnd": 1}, "max_nodes": 50}),
        );
        assert_eq!(a["max_nodes"], 50);
        assert_eq!(a["detail"], "compact");
        assert_eq!(a["budget_ms"], 6000);
    }

    #[test]
    fn iso_from_epoch_format() {
        assert_eq!(iso_from_epoch(0.0), "1970-01-01T00:00:00+00:00");
        assert_eq!(iso_from_epoch(1.5), "1970-01-01T00:00:01.500000+00:00");
    }

    #[test]
    fn action_warnings_cases() {
        let w = action_warnings(
            &json!({"foreground_taken_by_target": true, "foreground_restored": true,
            "candidates_with_same_identity": 2, "after": {"exists": false}}),
        );
        assert_eq!(w.len(), 3);
        assert!(w[0].contains("given back"));
        assert!(w[1].starts_with("2 elements"));
        let w = action_warnings(&json!({"foreground_changed": true, "after": {"exists": true}}));
        assert_eq!(w, vec!["Foreground changed to another window during the action (user activity or a popup); not caused by the target.".to_string()]);
        assert!(action_warnings(&json!({"after": {}})).is_empty());
    }

    #[test]
    fn snapshot_hint() {
        let stats = json!({"truncated": true, "stop_reason": "node_limit"});
        let v = snapshot_result(
            "a",
            0.0,
            &json!({}),
            "window",
            &json!({}),
            &[],
            "compact",
            &stats,
            &json!({}),
            0,
        );
        assert!(v["hint"].as_str().unwrap().contains("(node_limit)"));
        let stats = json!({"truncated": false});
        let v = snapshot_result(
            "a",
            0.0,
            &json!({}),
            "window",
            &json!({}),
            &[],
            "compact",
            &stats,
            &json!({}),
            0,
        );
        assert!(v["hint"].as_str().unwrap().starts_with("No actionable"));
    }
}

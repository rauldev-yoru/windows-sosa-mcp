//! The hands-free tools: `windows_handsfree`, `windows_input` and
//! `windows_wait_until`, wrapped in the result envelope.

use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value, json};

use super::keys::parse_hotkey;
use super::state::{heartbeat_age, is_live, read_state};
use crate::envelope::{self, Envelope};
use crate::ui::UiError;

/// The hands-free tools.
pub const TOOLS: [&str; 3] = ["windows_handsfree", "windows_input", "windows_wait_until"];

/// `windows_input` error kinds raised before the first SendInput.
pub const PREFLIGHT_KINDS: [&str; 9] = [
    "handsfree_required",
    "handsfree_stale",
    "handsfree_session_mismatch",
    "session_locked",
    "target_elevated",
    "foreground_mismatch",
    "capture_not_found",
    "invalid_arguments",
    "busy",
];

pub fn is_handsfree_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// Schema defaults (arguments were validated against the contract).
pub fn with_defaults(name: &str, a: &Value) -> Value {
    let d = match name {
        "windows_handsfree" => json!({"timeout_seconds": 900, "glow_px": 44, "arm_delay_ms": 2500,
            "guard": "block", "block_keyboard": true, "release_hotkey": "ctrl+alt+shift+h"}),
        "windows_input" => json!({"abort_on_foreground_change": true, "capture_after": "none"}),
        "windows_wait_until" => json!({"timeout_seconds": 15, "poll_ms": 400}),
        _ => json!({}),
    };
    let mut out = d.as_object().cloned().unwrap_or_default();
    if let Some(m) = a.as_object() {
        for (k, v) in m {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// Extra argument checks after the inputSchema.
pub fn validate(name: &str, a: &mut Value) -> Result<(), UiError> {
    if name == "windows_handsfree" && a["action"] == "start" {
        let hk = parse_hotkey(a["release_hotkey"].as_str().unwrap_or(""))?;
        a["release_hotkey_label"] = json!(hk.label);
    }
    if name == "windows_wait_until" {
        let cond = a["condition"].as_str().unwrap_or("");
        let need: &[&str] = match cond {
            "window_appears" | "window_disappears" => &["process", "title_contains", "hwnd"],
            "foreground_is" => &["process", "hwnd"],
            "element_appears" | "element_disappears" => &["window"],
            "user_idle_above" => &["idle_ms"],
            _ => &[],
        };
        if !need.is_empty() && !need.iter().any(|k| a.get(*k).is_some()) {
            return Err(UiError::new(
                "invalid_arguments",
                format!("condition {cond} needs one of {}", need.join(", ")),
            ));
        }
        if cond.starts_with("element")
            && !(a["name_contains"].as_str().is_some_and(|s| !s.is_empty())
                || a["automation_id"].as_str().is_some_and(|s| !s.is_empty()))
        {
            return Err(UiError::new(
                "invalid_arguments",
                "element conditions need name_contains or automation_id",
            ));
        }
    }
    Ok(())
}

/// Public view of the session state (`windows_handsfree` data).
pub fn state_view(action: &str, state: Option<&Value>, message: &str) -> Value {
    let now = Utc::now();
    let live = is_live(state, now);
    let empty = json!({});
    let st = state.unwrap_or(&empty);
    let g = |k: &str| st.get(k).cloned().unwrap_or(Value::Null);
    let age = heartbeat_age(state, now).map(|a| (a * 10.0).round() / 10.0);
    json!({
        "action": action, "active": live, "session_id": g("session_id"), "job_id": g("job_id"),
        "reason": g("reason"), "started_utc": g("started_utc"), "expires_utc": g("expires_utc"),
        "ended_utc": g("ended_utc"), "ended_by": if live { Value::Null } else { g("ended_by") },
        "end_detail": g("end_detail"), "last_real_input_utc": g("last_real_input_utc"),
        "hooks_installed": g("hooks_installed"), "monitors": g("monitors"), "heartbeat_age_s": age,
        "guard": g("guard"), "block_keyboard": g("block_keyboard"), "release_hotkey": g("release_hotkey"),
        "armed": g("armed"), "blocked_events": g("blocked_events"), "real_input_events": g("real_input_events"),
        "hook_reinstalls": g("hook_reinstalls"), "state_write_failures": g("state_write_failures"),
        "message": message,
    })
}

/// Shortest float formatting (always with a decimal point) for messages.
fn float_text(v: Option<f64>) -> String {
    match v {
        None => "None".into(),
        Some(f) if f.fract() == 0.0 => format!("{f:.1}"),
        Some(f) => format!("{f}"),
    }
}

fn handsfree(a: &Value) -> Result<Value, UiError> {
    let state = read_state();
    match a["action"].as_str().unwrap_or("") {
        "status" => {
            let live = is_live(state.as_ref(), Utc::now());
            if state.as_ref().is_some_and(|s| s["active"] == true) && !live {
                let age =
                    heartbeat_age(state.as_ref(), Utc::now()).map(|a| (a * 10.0).round() / 10.0);
                return Ok(state_view(
                    "status",
                    state.as_ref(),
                    &format!(
                        "state.json says active but the heartbeat is stale ({} s): the session process died; treat as inactive",
                        float_text(age)
                    ),
                ));
            }
            Ok(state_view(
                "status",
                state.as_ref(),
                if live {
                    "hands-free mode is active"
                } else {
                    "hands-free mode is not active"
                },
            ))
        }
        "stop" => {
            if !is_live(state.as_ref(), Utc::now()) {
                return Ok(state_view("stop", state.as_ref(), "nothing to stop"));
            }
            let _ = std::fs::create_dir_all(super::state::dir());
            std::fs::write(super::state::stop_path(), r#"{"by":"windows_handsfree"}"#)
                .map_err(|e| UiError::new("io_error", e.to_string()))?;
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut st = state;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(500));
                st = read_state();
                if st.as_ref().is_some_and(|s| s["active"] != true) {
                    return Ok(state_view("stop", st.as_ref(), "hands-free mode ended"));
                }
            }
            Err(UiError::new(
                "stop_unconfirmed",
                format!(
                    "stop marker written but the session did not confirm within 8 s; check windows_job_status({})",
                    st.as_ref()
                        .and_then(|s| s["job_id"].as_str())
                        .unwrap_or("None")
                ),
            ))
        }
        _ => start(a, state),
    }
}

fn start(a: &Value, state: Option<Value>) -> Result<Value, UiError> {
    if let Some(s) = state.as_ref().filter(|s| is_live(Some(s), Utc::now())) {
        return Err(UiError::new(
            "handsfree_already_active",
            format!(
                "session {} is active (started {}, expires {}); stop it first or reuse it",
                s["session_id"].as_str().unwrap_or(""),
                s["started_utc"].as_str().unwrap_or(""),
                s["expires_utc"].as_str().unwrap_or("")
            ),
        ));
    }
    let session_id: String = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
    let job_id = uuid::Uuid::new_v4().to_string();
    let reason = a["reason"].as_str().unwrap_or("").to_string();
    let timeout = a["timeout_seconds"].as_i64().unwrap_or(900);
    let cfg = json!({
        "session_id": session_id, "reason": reason, "timeout_seconds": timeout,
        "glow_px": a["glow_px"], "arm_delay_ms": a["arm_delay_ms"], "guard": a["guard"],
        "block_keyboard": a["block_keyboard"], "release_hotkey": a["release_hotkey"],
        "job_id": job_id, "dry_run": super::session::dry_run_env(),
    });
    let args = crate::tools::jobs::StartArgs {
        script: format!(
            "# windows-sosa-mcp native hands-free session ({})\n",
            super::session::SESSION_ARG
        ),
        cwd: None,
        timeout_seconds: timeout + 120,
        max_output_bytes: 1_048_576,
        label: Some(format!(
            "hands-free: {}",
            reason.chars().take(60).collect::<String>()
        )),
    };
    let started = crate::tools::jobs::start(
        &args,
        super::session::JOB_KIND,
        &[("handsfree.json".to_string(), cfg.to_string())],
        Some(job_id.clone()),
    )
    .map_err(|e| UiError::new(&e.kind, e.message).changed(e.may_have_changed))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(700));
        let st = read_state();
        let Some(s) = st
            .as_ref()
            .filter(|s| s["session_id"] == session_id.as_str())
        else {
            continue;
        };
        if s["active"] == true {
            let msg = if a["guard"] == "block" {
                let what = if a["block_keyboard"] == true {
                    "mouse and keyboard"
                } else {
                    "mouse"
                };
                format!(
                    "hands-free mode active: border shown on {} monitor(s); after {} ms the user's {what} input is blocked (accidental touches neither disturb you nor end the session); ends ONLY by {}, timeout, lock screen or stop",
                    display_val(&s["monitors"]),
                    a["arm_delay_ms"].as_i64().unwrap_or(2500),
                    a["release_hotkey_label"].as_str().unwrap_or("")
                )
            } else {
                format!(
                    "hands-free mode active: border shown on {} monitor(s); ends at first real user input, timeout, lock or stop",
                    display_val(&s["monitors"])
                )
            };
            return Ok(state_view("start", Some(s), &msg));
        }
        return Err(UiError::new(
            "handsfree_start_failed",
            format!(
                "session ended immediately ({} {})",
                display_val(&s["ended_by"]),
                display_val(&s["end_detail"])
            ),
        ));
    }
    let st = crate::tools::jobs::status(started["job_id"].as_str().unwrap_or(&job_id), 2000)
        .unwrap_or_else(|_| json!({"state": "unknown", "stderr_tail": ""}));
    let tail = st["stderr_tail"].as_str().unwrap_or("");
    let tail: String = {
        let c: Vec<char> = tail.chars().collect();
        c[c.len().saturating_sub(300)..].iter().collect()
    };
    Err(UiError::new(
        "handsfree_start_failed",
        format!(
            "session did not report active within 15 s; job state={} stderr={tail}",
            st["state"].as_str().unwrap_or("unknown")
        ),
    ))
}

fn display_val(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(windows)]
fn input_batch(a: &Value) -> Result<(Value, Option<String>), UiError> {
    use super::input::{Request, batch_timeout_s, remap, run_batch};
    let mut actions: Vec<Value> = a["actions"].as_array().cloned().unwrap_or_default();
    let mut space = "virtual_desktop".to_string();
    if let Some(id) = a["capture_id"].as_str() {
        let cap = crate::ui::store::load_capture(id)?;
        actions = remap(&actions, &cap);
        space = format!(
            "capture:{} (origin {},{} scale {:.4})",
            &id[..8],
            cap["origin_x"].as_f64().unwrap_or(0.0) as i64,
            cap["origin_y"].as_f64().unwrap_or(0.0) as i64,
            cap["scale"].as_f64().unwrap_or(1.0)
        );
    }
    let timeout_ms = batch_timeout_s(&actions) as f64 * 1000.0;
    let req = Request {
        actions: &actions,
        session_id: a["session_id"].as_str(),
        expect_foreground_hwnd: a["expect_foreground_hwnd"].as_i64(),
        abort_on_foreground_change: a["abort_on_foreground_change"].as_bool().unwrap_or(true),
        timeout_ms,
    };
    let mut inj = super::input_win::WinInjector::new();
    let mut data = run_batch(&mut inj, &req)?;
    data["coordinate_space"] = json!(space);
    data["capture_after"] = Value::Null;
    let mut image = None;
    let mode = a["capture_after"].as_str().unwrap_or("none");
    if mode != "none" {
        let r = if mode == "foreground_window" {
            let h = data["foreground_after"]["root_hwnd"].clone();
            let args =
                crate::ui::with_defaults("windows_capture_window", &json!({"window": {"hwnd": h}}));
            crate::ui::call("windows_capture_window", &args)
        } else {
            let args = crate::ui::with_defaults(
                "windows_capture_monitor",
                &json!({"monitor": "foreground"}),
            );
            crate::ui::call("windows_capture_monitor", &args)
        };
        match r {
            Ok((mut cap, img)) => {
                if let Some(m) = cap.as_object_mut() {
                    m.remove("windows");
                }
                data["capture_after"] = cap;
                image = img;
            }
            Err(e) => data["capture_after"] = json!({"error": e.kind, "message": e.message}),
        }
    }
    Ok((data, image))
}

#[cfg(not(windows))]
fn input_batch(a: &Value) -> Result<(Value, Option<String>), UiError> {
    if let Some(id) = a["capture_id"].as_str() {
        crate::ui::store::load_capture(id)?;
    }
    super::input::require_session(read_state().as_ref(), a["session_id"].as_str(), Utc::now())?;
    Err(UiError::new(
        "unsupported_platform",
        "windows_input needs Windows (SendInput)",
    ))
}

#[cfg(windows)]
fn wait_until(a: &Value) -> Result<Value, UiError> {
    let policy = crate::ui::policy::Policy::load()?;
    let secs = a["timeout_seconds"].as_u64().unwrap_or(15);
    let mut req = a.clone();
    req["timeout_ms"] = json!(secs * 1000);
    crate::ui::win::run_uia(Duration::from_secs(secs + 20), move |u| {
        crate::ui::win::wait_until(u, &policy, &req)
    })
}

#[cfg(not(windows))]
fn wait_until(_a: &Value) -> Result<Value, UiError> {
    crate::ui::policy::Policy::load()?;
    Err(UiError::new(
        "unsupported_platform",
        "windows_wait_until needs Windows (UI Automation)",
    ))
}

/// Run one hands-free tool; returns the result envelope.
pub fn execute(name: &str, args: &Value) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base(name);
    let mut a = with_defaults(name, args);
    let validated = validate(name, &mut a);
    let out: Result<(Value, Option<String>), UiError> = validated.and_then(|()| match name {
        "windows_handsfree" => handsfree(&a).map(|d| (d, None)),
        "windows_input" => input_batch(&a),
        _ => wait_until(&a).map(|d| (d, None)),
    });
    match out {
        Ok((data, image)) => {
            let mut failure: Option<(String, String, bool)> = None;
            if name == "windows_handsfree" || name == "windows_input" {
                r.insert("session_id".into(), data["session_id"].clone());
            }
            if name == "windows_input" {
                if let Some(img) = image {
                    r.insert("image_png_base64".into(), json!(img));
                    r.insert(
                        "capture_sha256".into(),
                        data["capture_after"]["capture"]["sha256"].clone(),
                    );
                }
                if data["failed"].is_object() {
                    let f = &data["failed"];
                    failure = Some((
                        f["kind"].as_str().unwrap_or("").into(),
                        format!(
                            "action {} ({}) failed: {}; {} executed, {} skipped",
                            f["index"],
                            f["type"].as_str().unwrap_or(""),
                            f["message"].as_str().unwrap_or(""),
                            data["executed_count"],
                            data["skipped"].as_array().map_or(0, Vec::len)
                        ),
                        data["any_input"] == true
                            || data["executed_count"].as_u64().unwrap_or(0) > 0,
                    ));
                }
            }
            if name == "windows_wait_until" && data["met"] != true {
                let obs: String = json_spaced(&data["observed"]).chars().take(300).collect();
                failure = Some((
                    "condition_not_met".into(),
                    format!(
                        "{} not met within {} s; observed={obs}",
                        data["condition"].as_str().unwrap_or(""),
                        a["timeout_seconds"]
                    ),
                    false,
                ));
            }
            envelope::succeed(&mut r, data);
            if let Some((kind, message, changed)) = failure {
                envelope::fail(&mut r, &kind, &message);
                r.insert("exit_code".into(), Value::Null);
                if name == "windows_input" {
                    r.insert("may_have_changed".into(), json!(changed));
                }
            }
        }
        Err(e) => {
            let preflight = PREFLIGHT_KINDS.contains(&e.kind.as_str());
            envelope::fail(&mut r, &e.kind, &e.message);
            if e.kind == "timeout" {
                r.insert("status".into(), json!("timed_out"));
                r.insert("timed_out".into(), json!(true));
                r.insert("windows_termination_confirmed".into(), json!(false));
            }
            r.insert(
                "may_have_changed".into(),
                json!(e.may_have_changed || (name == "windows_input" && !preflight)),
            );
        }
    }
    envelope::stamp(&mut r, start);
    r
}

/// Compact JSON with `", "` and `": "` separators, non-ASCII kept.
pub fn json_spaced(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{}: {}", Value::String(k.clone()), json_spaced(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(json_spaced).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_hotkey_label() {
        let mut a = with_defaults(
            "windows_handsfree",
            &json!({"action": "start", "reason": "test"}),
        );
        validate("windows_handsfree", &mut a).unwrap();
        assert_eq!(a["release_hotkey_label"], "Ctrl+Alt+Shift+H");
        assert_eq!(a["guard"], "block");
        let mut bad = with_defaults(
            "windows_handsfree",
            &json!({"action": "start", "reason": "x", "release_hotkey": "ctrl+h"}),
        );
        assert_eq!(
            validate("windows_handsfree", &mut bad).unwrap_err().kind,
            "invalid_arguments"
        );
    }

    #[test]
    fn wait_needs() {
        let mut a = with_defaults("windows_wait_until", &json!({"condition": "foreground_is"}));
        assert_eq!(
            validate("windows_wait_until", &mut a).unwrap_err().message,
            "condition foreground_is needs one of process, hwnd"
        );
        let mut a = with_defaults(
            "windows_wait_until",
            &json!({"condition": "element_appears", "window": {"process": "x"}}),
        );
        assert_eq!(
            validate("windows_wait_until", &mut a).unwrap_err().message,
            "element conditions need name_contains or automation_id"
        );
        let mut a = with_defaults(
            "windows_wait_until",
            &json!({"condition": "time", "timeout_seconds": 1}),
        );
        assert!(validate("windows_wait_until", &mut a).is_ok());
    }

    #[test]
    fn state_view_hides_ended_by_while_live() {
        let s = json!({"active": true, "session_id": "s", "ended_by": "x",
                       "heartbeat_utc": super::super::state::fmt_o(Utc::now())});
        let v = state_view("status", Some(&s), "m");
        assert_eq!(v["active"], true);
        assert_eq!(v["ended_by"], Value::Null);
        let v = state_view("status", None, "hands-free mode is not active");
        assert_eq!(v["active"], false);
        assert_eq!(v["heartbeat_age_s"], Value::Null);
        assert_eq!(v.as_object().unwrap().len(), 23);
    }

    #[test]
    fn json_spaced_separators() {
        assert_eq!(
            json_spaced(&json!({"a": [1, "é"], "b": {}})),
            r#"{"a": [1, "é"], "b": {}}"#
        );
        assert_eq!(float_text(Some(7.0)), "7.0");
        assert_eq!(float_text(Some(12.3)), "12.3");
    }
}

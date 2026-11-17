//! Native `windows_health` (no PowerShell round-trip).
//!
//! The envelope's `verification` object describes the host and the server
//! (docs/design.md, D6).

use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::envelope::{Envelope, SCHEMA_VERSION};
use crate::platform;

/// Constant echoed to prove UTF-8 end to end.
pub const UNICODE_PROBE: &str = "UTF-8 check: áéíóú ñ 日本 ✓";

/// Build the health envelope.
pub fn run(audit_session_id: &str) -> Envelope {
    let start = Instant::now();
    let started_at = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6f+00:00")
        .to_string();
    let host = platform::host_info();

    let mut v = Map::new();
    v.insert(
        "administrator_in_current_token".into(),
        json!(host.administrator_in_current_token),
    );
    v.insert("user_profile".into(), json!(host.user_profile));
    v.insert("temp_directory".into(), json!(host.temp_directory));
    v.insert("computer".into(), json!(host.computer));
    v.insert("user".into(), json!(host.user));
    v.insert("cwd".into(), json!(host.cwd));
    v.insert("powershell_version".into(), json!(host.powershell_version));
    v.insert("unicode".into(), json!(UNICODE_PROBE));
    v.insert("audit_session_id".into(), json!(audit_session_id));
    v.insert(
        "audit_path".into(),
        Value::String(crate::audit::log_path().to_string_lossy().into_owned()),
    );
    v.insert("server_name".into(), json!(crate::SERVER_NAME));
    v.insert("server_version".into(), json!(crate::SERVER_VERSION));
    v.insert("os_version".into(), json!(host.os_version));
    v.insert("os_product_name".into(), json!(host.os_product_name));
    v.insert("os_display_version".into(), json!(host.os_display_version));
    v.insert("process_id".into(), json!(std::process::id()));
    v.insert("process_session_id".into(), json!(host.process_session_id));
    v.insert("elevated".into(), json!(host.elevated));
    v.insert(
        "powershell_paths".into(),
        json!({"windows_powershell": host.windows_powershell, "pwsh": host.pwsh}),
    );

    let (status, error) = if cfg!(windows) {
        ("succeeded", Value::Null)
    } else {
        (
            "failed",
            json!({"kind": "unsupported_platform",
                   "message": "windows-sosa-mcp only reports Windows state when running on Windows"}),
        )
    };

    let mut r = Map::new();
    r.insert("schema_version".into(), json!(SCHEMA_VERSION));
    r.insert("server_version".into(), json!(crate::SERVER_VERSION));
    r.insert("request_id".into(), json!(uuid::Uuid::new_v4().to_string()));
    r.insert("started_at".into(), json!(started_at));
    r.insert("status".into(), json!(status));
    r.insert("exit_code".into(), Value::Null);
    r.insert("stdout".into(), json!(""));
    r.insert("stderr".into(), json!(""));
    r.insert("truncated".into(), json!(false));
    r.insert("error".into(), error);
    r.insert("verification".into(), Value::Object(v));
    r.insert("operation".into(), json!("health"));
    r.insert("timed_out".into(), json!(false));
    r.insert("encoding_replaced".into(), json!(false));
    r.insert(
        "duration_ms".into(),
        json!(start.elapsed().as_millis() as u64),
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_has_verification_keys() {
        let r = run("sid");
        let v = r["verification"].as_object().unwrap();
        for key in [
            "administrator_in_current_token",
            "user_profile",
            "temp_directory",
            "computer",
            "user",
            "cwd",
            "powershell_version",
            "unicode",
            "audit_session_id",
            "audit_path",
            "server_name",
            "server_version",
        ] {
            assert!(v.contains_key(key), "missing {key}");
        }
        assert_eq!(v["unicode"], UNICODE_PROBE);
        assert_eq!(v["server_version"], crate::SERVER_VERSION);
        assert_eq!(r["server_version"], crate::SERVER_VERSION);
        if cfg!(windows) {
            assert_eq!(r["status"], "succeeded");
            assert!(v["computer"].is_string());
            assert!(v["user"].as_str().unwrap().contains('\\'));
            assert!(v["process_session_id"].is_u64());
            assert!(v["elevated"].is_boolean());
            assert!(v["os_version"].as_str().unwrap().starts_with("10."));
        } else {
            assert_eq!(r["status"], "failed");
            assert_eq!(r["error"]["kind"], "unsupported_platform");
        }
    }
}

//! Result envelope shared by every tool (schema version 1).
//!
//! Every `tools/call` answers with the same object twice: as
//! `structuredContent` and as JSON text in `content[0]`, with
//! `isError = status != "succeeded"`. `server_version`, `tool_name` and
//! `error_kind` are always present (see docs/design.md, "Result envelope").

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value, json};

/// Object type used for results.
pub type Envelope = Map<String, Value>;

/// Envelope schema version.
pub const SCHEMA_VERSION: u64 = 1;

/// Current UTC time as ISO 8601 with microseconds and a `+00:00` offset.
pub fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6f+00:00")
        .to_string()
}

/// The common result skeleton of the typed tools: failed until the tool
/// says otherwise.
pub fn base(operation: &str) -> Envelope {
    let mut r = Map::new();
    r.insert("schema_version".into(), json!(SCHEMA_VERSION));
    r.insert("server_version".into(), json!(crate::SERVER_VERSION));
    r.insert("request_id".into(), json!(uuid::Uuid::new_v4().to_string()));
    r.insert("started_at".into(), json!(now_iso()));
    r.insert("operation".into(), json!(operation));
    r.insert("status".into(), json!("failed"));
    r.insert("exit_code".into(), Value::Null);
    r.insert("stdout".into(), json!(""));
    r.insert("stderr".into(), json!(""));
    r.insert("truncated".into(), json!(false));
    r.insert("error".into(), Value::Null);
    r.insert("verification".into(), Value::Null);
    r
}

/// Set `status=failed` and `error={kind,message}` (message capped at 2000 chars).
pub fn fail(r: &mut Envelope, kind: &str, message: impl AsRef<str>) {
    let m: String = message.as_ref().chars().take(2000).collect();
    r.insert(
        "status".into(),
        json!(if kind == "timeout" {
            "timed_out"
        } else {
            "failed"
        }),
    );
    r.insert("error".into(), json!({"kind": kind, "message": m}));
}

/// Set `status=succeeded`, `exit_code=0` and `data`.
pub fn succeed(r: &mut Envelope, data: Value) {
    r.insert("status".into(), json!("succeeded"));
    r.insert("exit_code".into(), json!(0));
    r.insert("data".into(), data);
}

/// Stamp `duration_ms` from a start instant.
pub fn stamp(r: &mut Envelope, start: std::time::Instant) {
    r.insert(
        "duration_ms".into(),
        json!(start.elapsed().as_millis() as u64),
    );
}

/// A failure produced before or instead of execution.
pub fn failure(tool_name: &str, kind: &str, message: impl Into<String>) -> Envelope {
    let mut map = Map::new();
    map.insert("schema_version".into(), json!(SCHEMA_VERSION));
    map.insert("status".into(), json!("failed"));
    map.insert("exit_code".into(), Value::Null);
    map.insert("tool_name".into(), json!(tool_name));
    map.insert(
        "error".into(),
        json!({"kind": kind, "message": message.into()}),
    );
    map.insert("error_kind".into(), json!(kind));
    map
}

/// Fill the always-present keys and wrap the envelope as an MCP result.
pub fn finish(tool_name: &str, mut result: Envelope) -> CallToolResult {
    let kind = result
        .get("error")
        .and_then(|e| e.get("kind"))
        .cloned()
        .unwrap_or(Value::Null);
    result.insert("server_version".into(), json!(crate::SERVER_VERSION));
    result.insert("tool_name".into(), json!(tool_name));
    result.insert("error_kind".into(), kind);
    let succeeded = result.get("status").and_then(Value::as_str) == Some("succeeded");
    // Captures travel as ImageContent; structuredContent keeps only the
    // metadata and sha256.
    let image = result
        .remove("image_png_base64")
        .and_then(|v| v.as_str().map(String::from));
    let value = Value::Object(result);
    let text = value.to_string();
    let mut content = vec![ContentBlock::text(text)];
    if let Some(img) = image {
        content.push(ContentBlock::image(img, "image/png"));
    }
    let mut out = if succeeded {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    };
    out.structured_content = Some(value);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_has_envelope_keys() {
        let r = finish("windows_x", failure("windows_x", "not_implemented", "m"));
        assert_eq!(r.is_error, Some(true));
        let sc = r.structured_content.unwrap();
        for key in [
            "schema_version",
            "status",
            "exit_code",
            "tool_name",
            "server_version",
            "error",
            "error_kind",
        ] {
            assert!(sc.get(key).is_some(), "missing {key}");
        }
        assert_eq!(sc["error"]["kind"], "not_implemented");
        assert_eq!(sc["error_kind"], "not_implemented");
        assert_eq!(sc["exit_code"], Value::Null);
        assert_eq!(sc["server_version"], crate::SERVER_VERSION);
    }

    #[test]
    fn success_text_equals_structured() {
        let mut m = Map::new();
        m.insert("status".into(), json!("succeeded"));
        m.insert("error".into(), Value::Null);
        let r = finish("windows_health", m);
        assert_eq!(r.is_error, Some(false));
        let text = match &r.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("unexpected content {other:?}"),
        };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(Some(parsed), r.structured_content);
        assert_eq!(r.structured_content.unwrap()["error_kind"], Value::Null);
    }
}

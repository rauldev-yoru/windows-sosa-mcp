//! MCP server handler: serves the tool contract and dispatches calls.

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};

use crate::contract::Contract;
use crate::envelope::{self, Envelope};
use crate::{SERVER_NAME, SERVER_VERSION, audit, health, lock, state, tools};

/// The stdio MCP server.
#[derive(Clone)]
pub struct SosaServer {
    contract: &'static Contract,
    audit_session_id: String,
}

impl SosaServer {
    /// Build the server from the embedded contract.
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            contract: Contract::embedded()?,
            audit_session_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    /// Run one tool call and return its result envelope (before wrapping).
    /// Order: unknown tool, invalid arguments, then execution.
    pub async fn dispatch(&self, name: &str, arguments: Value) -> Envelope {
        if !self.contract.contains(name) {
            let mut r = envelope::failure(name, "unknown_tool", "Unknown tool");
            safe_audit(&mut r, "rejected", Map::new());
            return r;
        }
        if let Err(message) = self.contract.validate(name, &arguments) {
            let mut r = envelope::failure(name, "invalid_arguments", message);
            safe_audit(&mut r, "rejected", summary(&arguments));
            return r;
        }
        match name {
            "windows_health" => {
                let sid = self.audit_session_id.clone();
                tokio::task::spawn_blocking(move || health::run(&sid))
                    .await
                    .unwrap_or_else(|e| {
                        envelope::failure(
                            name,
                            "internal_error",
                            format!("health task failed: {e}"),
                        )
                    })
            }
            _ if tools::is_native(name) => {
                let prepared = match tools::prepare(name, &arguments) {
                    Ok(p) => p,
                    Err(e) => {
                        let mut r = envelope::failure(
                            name,
                            &e.kind,
                            e.message.chars().take(2000).collect::<String>(),
                        );
                        safe_audit(&mut r, "rejected", summary(&arguments));
                        return r;
                    }
                };
                let tool = name.to_string();
                tokio::task::spawn_blocking(move || locked_execute(&tool, &arguments, prepared))
                    .await
                    .unwrap_or_else(|e| {
                        envelope::failure(name, "internal_error", format!("tool task failed: {e}"))
                    })
            }
            _ => envelope::failure(
                name,
                "not_implemented",
                format!(
                    "{name} has no handler in {SERVER_NAME} {SERVER_VERSION}; nothing was executed."
                ),
            ),
        }
    }
}

fn summary(arguments: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "args_summary".into(),
        Value::Object(audit::summarize_arguments(arguments)),
    );
    m
}

/// `safe_audit`: audit failures never fail the call, they add a warning.
fn safe_audit(r: &mut Envelope, phase: &str, extra: Map<String, Value>) {
    let mut view = r.clone();
    view.entry("server_version")
        .or_insert(json!(SERVER_VERSION));
    if audit::record_result(&Value::Object(view), phase, extra).is_err() {
        r.insert(
            "audit_warning".into(),
            json!("Could not write metadata audit log"),
        );
    }
}

/// Run under the host-wide busy lock with started/finished audit events
/// (`started` before execution, `finished` after).
fn locked_execute(name: &str, arguments: &Value, prepared: tools::Prepared) -> Envelope {
    let _guard = match lock::try_acquire(&state::lock_path()) {
        lock::Acquire::Held(g) => g,
        lock::Acquire::Busy => {
            let mut r = envelope::failure(
                name,
                "busy",
                "Another MCP operation is running; this request was not executed.",
            );
            safe_audit(&mut r, "rejected", summary(arguments));
            return r;
        }
        lock::Acquire::Error(e) => {
            return envelope::failure(name, "internal_error", format!("cannot open mcp.lock: {e}"));
        }
    };
    let call_id = uuid::Uuid::new_v4().to_string();
    let mut started = summary(arguments);
    started.insert("call_id".into(), json!(call_id));
    started.insert("tool_name".into(), json!(name));
    started.insert("server_version".into(), json!(SERVER_VERSION));
    if let Some(s) = arguments.get("script").and_then(Value::as_str) {
        started.insert("script_sha256".into(), json!(audit::digest_text(s)));
        started.insert("script_chars".into(), json!(s.encode_utf16().count()));
    }
    let _ = audit::record("started", started);
    let mut r = tools::execute(prepared);
    r.insert("server_version".into(), json!(SERVER_VERSION));
    r.insert("tool_name".into(), json!(name));
    let kind = r
        .get("error")
        .and_then(|e| e.get("kind"))
        .cloned()
        .unwrap_or(Value::Null);
    r.insert("error_kind".into(), kind);
    let mut extra = Map::new();
    extra.insert("call_id".into(), json!(call_id));
    safe_audit(&mut r, "finished", extra);
    r
}

impl ServerHandler for SosaServer {
    fn get_info(&self) -> ServerConfig {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, SERVER_VERSION))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.contract.tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let result = self.dispatch(&name, arguments).await;
        Ok(envelope::finish(&name, result).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server() -> SosaServer {
        SosaServer::new().expect("contract parses")
    }

    #[test]
    fn contract_has_24_tools() {
        let c = Contract::embedded().unwrap();
        assert_eq!(c.tools.len(), 24);
        for t in &c.tools {
            assert!(t.description.is_some());
            assert!(t.annotations.is_some());
        }
    }

    #[tokio::test]
    async fn unknown_tool() {
        let r = server().dispatch("windows_nope", json!({})).await;
        assert_eq!(r["error"]["kind"], "unknown_tool");
    }

    #[tokio::test]
    async fn invalid_arguments_before_execution() {
        let r = server().dispatch("windows_run_powershell", json!({})).await;
        assert_eq!(r["error"]["kind"], "invalid_arguments");
        assert!(r["error"]["message"].as_str().unwrap().contains("script"));
        let r = server()
            .dispatch("windows_health", json!({"extra": 1}))
            .await;
        assert_eq!(r["error"]["kind"], "invalid_arguments");
    }

    #[tokio::test]
    async fn every_contract_tool_is_native() {
        let s = server();
        for t in &s.contract.tools {
            assert!(
                t.name == "windows_health" || tools::is_native(&t.name),
                "{} is not native",
                t.name
            );
        }
        // No session file in a fresh state dir: status is a plain inactive view.
        let r = s
            .dispatch("windows_handsfree", json!({"action": "nope"}))
            .await;
        assert_eq!(r["error"]["kind"], "invalid_arguments");
    }
}

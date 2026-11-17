//! windows-sosa-mcp: native Windows stdio MCP server (see `docs/design.md`).

pub mod audit;
pub mod contract;
pub mod envelope;
pub mod handsfree;
pub mod health;
pub mod lock;
pub mod paths;
pub mod platform;
pub mod server;
pub mod state;
pub mod tools;
pub mod ui;

/// Name reported in `initialize.serverInfo` and in `windows_health`.
pub const SERVER_NAME: &str = "windows-sosa-mcp";
/// Version of this implementation (Cargo package version).
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

//! Per-user state directory (`%LOCALAPPDATA%\windows-sosa-mcp`).
//!
//! `WINDOWS_SOSA_MCP_HOME` overrides the root (tests, portable installs).
//! Elsewhere than Windows the fallback is `$XDG_STATE_HOME/windows-sosa-mcp`
//! or `~/.local/state/windows-sosa-mcp`, so the portable parts are testable.

use std::path::PathBuf;

pub const HOME_ENV: &str = "WINDOWS_SOSA_MCP_HOME";

/// Root of all persistent state: `audit/`, `jobs/`, `mcp.lock`.
pub fn root() -> PathBuf {
    if let Some(p) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if cfg!(windows)
        && let Some(l) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())
    {
        return PathBuf::from(l).join("windows-sosa-mcp");
    }
    if let Some(x) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(x).join("windows-sosa-mcp");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".local").join("state").join("windows-sosa-mcp")
}

pub fn audit_dir() -> PathBuf {
    root().join("audit")
}

pub fn jobs_dir() -> PathBuf {
    root().join("jobs")
}

pub fn lock_path() -> PathBuf {
    root().join("mcp.lock")
}

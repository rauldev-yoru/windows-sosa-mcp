# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-16

First release.

### Added

- Native Windows MCP server over stdio (`rmcp`), shipped as a single
  `windows-sosa-mcp.exe`.
- 24 tools with JSON Schema inputs/outputs (`contracts/tools.json`):
  - core: `windows_health`, `windows_run_powershell`;
  - files: `windows_list_directory`, `windows_find_files`,
    `windows_stat_path`, `windows_read_text_file`, `windows_search_text`
    (pinned ripgrep), `windows_write_text_file`, `windows_edit_text_file`
    (compare-and-commit), `windows_resolve_executable`;
  - detached jobs: `windows_job_start`, `windows_job_status`,
    `windows_job_list`, `windows_job_cancel`;
  - WinGet: `windows_package_query`, `windows_package_install`;
  - UI Automation: `windows_list_windows`, `windows_ui_snapshot`,
    `windows_ui_action`;
  - capture: `windows_capture_window`, `windows_capture_monitor` (PNG image
    content);
  - hands-free input: `windows_handsfree`, `windows_input`,
    `windows_wait_until`.
- Common result envelope with `schema_version`, `server_version`,
  `tool_name`, `status`, `error`/`error_kind`.
- Guarded hands-free mode: separate fail-open session process with
  low-level hooks, glowing per-monitor overlay, arming delay, TTL and the
  Ctrl+Alt+Shift+H release hotkey.
- UI privacy policy (`ui_policy.json`), cross-process busy lock and a
  metadata-only audit log.
- Release workflow: zip with the exe and pinned ripgrep, standalone exe and
  `SHA256SUMS`.

[Unreleased]: https://github.com/rauldev-yoru/windows-sosa-mcp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/rauldev-yoru/windows-sosa-mcp/releases/tag/v0.1.0

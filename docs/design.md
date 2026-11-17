# windows-sosa-mcp — design

## Goals

1. **One native Windows executable** (`windows-sosa-mcp.exe`) that gives an
   MCP client typed, bounded control of a Windows desktop: shell, files,
   detached jobs, WinGet packages, UI Automation, screen capture and a
   guarded hands-free input mode.
2. **A stable, explicit contract.** 24 tools whose names, descriptions,
   `inputSchema`, `outputSchema` and annotations live in one JSON file
   (`contracts/tools.json`), plus one result envelope shared by every tool.
3. **Fast calls.** Native Win32/COM wherever an API exists; PowerShell is
   only started when the tool *is* PowerShell (`windows_run_powershell`,
   jobs, package installs).
4. **Safe by construction.** Every tool bounds its work (time, bytes,
   entries), reports failures as typed results, and the only tools that
   move the real mouse/keyboard require an explicit, visible, time-limited
   hands-free session the user can always end.
5. **Buildable and testable everywhere.** Windows APIs live behind
   `cfg(windows)`; the crate builds, lints and tests on Linux too.

Non-goals: an HTTP/SSE transport (remote use goes through
[enishi](https://github.com/rauldev-yoru/enishi) or any stdio tunnel),
multi-user/multi-session servers, elevation (UAC) handling.

## Architecture

```
MCP client ──stdio JSON-RPC──▶ windows-sosa-mcp.exe
                                 │ server.rs   tools/list (contract), tools/call
                                 │ contract.rs inputSchema validation
                                 │ lock.rs     cross-process busy lock
                                 │ audit.rs    metadata-only JSONL audit
                                 ├─ tools/     shell, files/text, jobs, packages
                                 ├─ ui/        UIA (COM MTA thread), capture, policy
                                 └─ handsfree/ session control, SendInput, waits
     detached children (same exe, hidden arguments):
       __job-supervisor <job_dir>     one per windows_job_start / package install
       __handsfree-session <job_dir>  LL hooks + overlay while hands-free is on
```

### Tool groups

| Group | Tools | Implementation |
|---|---|---|
| core | `windows_health`, `windows_run_powershell` | Win32 queries; `powershell.exe` 5.1 child with a job object |
| files | `windows_list_directory`, `windows_find_files`, `windows_stat_path`, `windows_read_text_file`, `windows_resolve_executable`, `windows_search_text`, `windows_write_text_file`, `windows_edit_text_file` | Rust std + Win32; `windows_search_text` runs a pinned `rg.exe` |
| jobs | `windows_job_start`, `windows_job_status`, `windows_job_list`, `windows_job_cancel` | the exe re-launched as its own supervisor |
| packages | `windows_package_query`, `windows_package_install` | `winget.exe` directly; installs run as a job |
| UI | `windows_list_windows`, `windows_ui_snapshot`, `windows_ui_action` | native UI Automation on a resident COM MTA thread |
| capture | `windows_capture_window`, `windows_capture_monitor` | GDI (`PrintWindow` / `BitBlt`), PNG as MCP `ImageContent` |
| hands-free | `windows_handsfree`, `windows_input`, `windows_wait_until` | separate session process (hooks + overlay), `SendInput` |

### Call pipeline

`tools/call` → unknown tool? (`unknown_tool`) → inputSchema validation
(`invalid_arguments`) → tool-specific semantic checks → busy lock
(`busy`) → audit `started` → execution (blocking work on
`spawn_blocking` / dedicated threads) → audit `finished` → envelope.

### Processes and threads

- **UIA worker thread** (D19): one resident thread initialised with
  `COINIT_MULTITHREADED` owns `IUIAutomation` and its `CacheRequest`.
  Requests are closures with a deadline; a provider that hangs past it
  gets the call a `timeout` and the thread is abandoned and replaced.
- **Job supervisor** (D11): `windows_job_start` writes a job directory and
  launches `windows-sosa-mcp.exe __job-supervisor <job_dir>` detached
  (direct → `CREATE_BREAKAWAY_FROM_JOB` → WMI `Win32_Process.Create`). The
  supervisor owns the PowerShell child in a kill-on-close job object and
  writes `result.json`, so jobs outlive the MCP connection.
- **Hands-free session process** (D25): `windows-sosa-mcp.exe
  __handsfree-session <job_dir>`, launched through the job launcher. It
  installs the low-level mouse/keyboard hooks on a dedicated thread (D27),
  paints the overlay (D28) and writes `state.json`. If it dies, Windows
  removes its hooks: the session fails open.
- **Injected-input marker** (D27, D30): `windows_input` stamps every
  `SendInput` event with a magic `dwExtraInfo`; the hooks let marked
  events through and block (or count) real ones. A separate probe marker
  verifies every 10 s that the hooks are still installed.

### State directories

| Path | Content |
|---|---|
| `%LOCALAPPDATA%\windows-sosa-mcp\` (`WINDOWS_SOSA_MCP_HOME`) | state root |
| `…\mcp.lock` | busy lock (D14) |
| `…\audit\events.jsonl` (+ `.1`..`.3`) | audit log, rotated at 2 MiB (D13) |
| `…\jobs\<uuid>\` | `meta.json`, `launch.json`, `user.ps1`, `wrapper.ps1`, `stdout.log`, `stderr.log`, `result.json`, `cancel.json` (D11) |
| `…\handsfree\` | `state.json`, `stop.json` (D26) |
| `…\ui_snapshots\`, `…\ui_captures\` | snapshot (120 s) and capture (600 s) records (D17) |
| `%APPDATA%\windows-sosa-mcp\ui_policy.json` (`WINDOWS_SOSA_MCP_UI_POLICY`) | UI privacy policy (D18) |
| `<exe dir>\vendor\rg.exe` + `rg.sha256` (`WINDOWS_SOSA_MCP_VENDOR`) | pinned ripgrep (D15) |

Off Windows the state root falls back to `$XDG_STATE_HOME/windows-sosa-mcp`.

## Safety model

- **Read-mostly by default.** Mutating tools are explicit
  (`write`/`edit` with compare-and-commit hashes, `job_start`,
  `package_install`, `ui_action`, `windows_input`) and annotated as such in
  the contract.
- **Bounded work.** Every tool has hard limits (timeouts ≤ 300 s for
  PowerShell, output caps, entry/depth/byte limits, ≤ 5 running jobs, ≤ 50
  input actions per batch) and reports why it stopped.
- **One call at a time.** A cross-process lock serialises calls from every
  server instance of the user; a concurrent call gets `busy` immediately.
- **Privacy policy for UI.** Windows of password managers, UAC/credential
  prompts and secret-looking titles are excluded from listings, snapshots
  and captures, and painted black in monitor captures (D18).
- **Real input only in hands-free mode.** `windows_input` and
  `ui_action activate` refuse to run without a live session
  (`handsfree_required`). A session is visible (glowing border and label on
  every monitor), time-limited (TTL), blocks the user's mouse (and keyboard
  unless disabled) only after an arming delay, and ends on the release
  hotkey **Ctrl+Alt+Shift+H**, on `stop`, on TTL expiry, on workstation lock
  or when its process dies. Ctrl+Alt+Del always works (Windows never routes
  it through hooks).
- **Preflight before input.** Lock screen, elevated foreground window,
  foreground mismatch, stale session and invalid batches are refused
  before the first event is sent (D30).
- **Metadata-only audit.** The audit log records tool, status, durations
  and argument *summaries* (lengths, hashes) — never file contents,
  scripts or typed text (D13).
- **No elevation.** The server runs with the user's token; it never asks
  for UAC and refuses to inject input into elevated windows.

## Result envelope

Every `tools/call` returns one JSON object, both as `structuredContent` and
as compact UTF-8 JSON text in `content[0]`; captures add an
`ImageContent` (`image/png`) after it. `isError = status != "succeeded"`.
Tool failures are results, never JSON-RPC errors.

| Key | Type | Meaning |
|---|---|---|
| `schema_version` | int | envelope version, `1` |
| `server_version` | string | crate version of the server that answered |
| `tool_name` | string | tool that was called |
| `status` | string | `succeeded`, `failed` or `timed_out` |
| `error` | object\|null | `{kind, message}` (message ≤ 2000 chars) |
| `error_kind` | string\|null | copy of `error.kind` for easy filtering |
| `exit_code` | int\|null | process exit code where a process ran, `0` on native success |
| `data` | object | tool-specific result (see each tool's `outputSchema`) |
| `request_id`, `started_at`, `duration_ms` | | correlation and timing |
| `operation` | string | internal operation name |
| `stdout`, `stderr`, `truncated`, `timed_out`, `encoding_replaced` | | process output (empty for native tools) |
| `verification` | object\|null | `windows_health` facts (identity, OS, PowerShell, audit session, `server_name`, `server_version`, …) |
| `may_have_changed` | bool | UI/input tools: whether the desktop may have been modified despite a failure |

`server_version` is the only identity key; the protocol-level identity is
`initialize.serverInfo = {name: "windows-sosa-mcp", version}`.

### Error kinds

Kinds are stable identifiers; messages are human text and may change.
Common kinds:

| Kind | When |
|---|---|
| `unknown_tool` | tool name not in the contract |
| `invalid_arguments` | inputSchema or semantic validation failed; nothing ran |
| `busy` | another call holds the lock |
| `timeout` | time budget exceeded (`status = timed_out`) |
| `not_found`, `already_exists`, `access_denied`, `io_error` | filesystem/OS outcomes |
| `unsupported_path`, `reparse_point_not_supported` | UNC, device or reparse-point targets where not allowed |
| `version_conflict`, `file_changed`, `text_not_found`, `ambiguous_match` | compare-and-commit edits |
| `dependency_missing`, `dependency_changed` | pinned `rg.exe` absent or hash mismatch |
| `execution_failed`, `launch_failed` | PowerShell/job process outcome |
| `window_ambiguous`, `window_gone`, `window_excluded`, `snapshot_not_found`, `ref_not_found`, `ref_stale`, `pattern_unavailable`, `element_offscreen`, `activation_denied` | UI Automation |
| `handsfree_required`, `handsfree_stale`, `handsfree_ended`, `handsfree_session_mismatch`, `session_locked`, `target_elevated`, `foreground_mismatch`, `foreground_changed_abort`, `cursor_placement_blocked`, `capture_not_found` | hands-free and input |
| `condition_not_met` | `windows_wait_until` timed out without the condition |
| `unsupported_platform` | the tool needs Windows |

## Configuration

| Setting | Default | Purpose |
|---|---|---|
| `WINDOWS_SOSA_MCP_HOME` | `%LOCALAPPDATA%\windows-sosa-mcp` | state root |
| `WINDOWS_SOSA_MCP_UI_POLICY` | `%APPDATA%\windows-sosa-mcp\ui_policy.json` | UI privacy policy file |
| `WINDOWS_SOSA_MCP_VENDOR` | `<exe dir>\vendor` | directory of `rg.exe` + `rg.sha256` |
| `WINDOWS_SOSA_MCP_POWERSHELL` | `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe` | PowerShell used by shell and jobs |
| `WINDOWS_SOSA_MCP_WINGET` | `winget.exe` on `PATH` | WinGet executable |
| `WINDOWS_SOSA_MCP_FORCE_WMI` | unset | `1` forces the WMI job launch path (tests) |
| `WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN` | unset | `1` = hands-free without hooks, overlay or `SendInput` (tests only, D29) |
| `ENISHI_PEER` | unset | recorded as `caller` in the audit log when set |

`ui_policy.json` (all keys optional; a missing file means the defaults, a
`null` value disables that key):

```json
{
  "exclude_processes": ["KeePass", "KeePassXC", "1Password", "Bitwarden", "LastPass", "consent", "..."],
  "exclude_title_regex": "(?i)password|passcode|passphrase|\\b2fa\\b|verification code|private key|secret key|recovery code",
  "allow_processes": [],
  "capture_monitor_enabled": true
}
```

`allow_processes`, when non-empty, restricts UI tools to those processes.

## Platform notes and limitations

- Windows 10/11 x64. Windows PowerShell 5.1 is required for
  `windows_run_powershell`, jobs and package installs; WinGet for the
  package tools.
- UI Automation, capture and input need an **interactive desktop session**
  (session ≠ 0). A server started by a service in session 0 can run the
  shell/file/job tools only (D8).
- The server runs with the user's token and is **not elevated**: it cannot
  automate or inject into elevated windows, and machine-scope package
  installs that need UAC block until their job times out.
- Monitors are enumerated once per hands-free session; a monitor plugged in
  mid-session has no overlay.
- `type` input above ~60 characters per second is unreliable in WinUI/TSF
  apps; use `paste` or UIA `set_value` for exact text.
- `windows_search_text` requires the pinned `rg.exe` next to the exe
  (bundled in the release zip).
- Typical client-observed latency: tens of milliseconds or less for
  health, file and window-listing calls; ~0.1–0.4 s for snapshots of a few
  hundred nodes and full-monitor captures; `windows_run_powershell` is
  dominated by `powershell.exe` start-up (~1.3 s).

## Testing

- `cargo test` — unit tests (rendering, policy, paths, encodings, key
  parsing, session state machine, input mapping…) and `tests/stdio.rs`,
  which drives the real binary over stdio: `tools/list` equals
  `contracts/tools.json`, envelope keys, validation order, busy lock, audit,
  file/job/package tools (with a fake `winget`), and the hands-free tools
  end to end in dry-run mode. Runs on Linux and Windows CI.
- `cargo test -- --ignored` on an interactive Windows desktop — opt-in e2e
  tests that open a self-made test window (`tests/acceptance/hf_test_app.ps1`).
- `tests/acceptance/handsfree_acceptance.py` — attended real-desktop
  acceptance of hands-free mode (S0–S7: lifecycle, overlay, input blocking,
  Unicode typing/paste/chords, drawing, capture-space clicks, safety
  aborts, release hotkey, fail-open, TTL). It only touches its own test
  windows (D37) and has a `--dry-run` mode.
- `tests/acceptance/ui_test_app.ps1` — deterministic WinForms window for
  manual UI Automation checks.

## Decisions

| ID | Decision |
|---|---|
| D1 | Rust + rmcp, stdio only |
| D2 | The contract is a JSON file served verbatim |
| D3 | One result envelope for every tool |
| D4 | `server_version` is the only identity key |
| D5 | Validation order: unknown tool → schema → execution |
| D6 | Native `windows_health` |
| D7 | Builds everywhere; Windows code behind `cfg(windows)` |
| D8 | Runs in the user's interactive session |
| D9 | Native tools omit process-only envelope keys |
| D10 | `windows_run_powershell`: PowerShell 5.1 child, 32-bit exit code |
| D11 | Detached jobs: the server binary is its own supervisor |
| D12 | Per-user state root |
| D13 | Metadata-only audit log |
| D14 | Cross-process busy lock |
| D15 | `windows_search_text` uses a pinned ripgrep |
| D16 | Packages: `winget.exe` directly, positional parsing |
| D17 | UI module layout and stable render format |
| D18 | UI privacy policy file |
| D19 | One resident COM MTA thread for UIA |
| D20 | Per-Monitor-V2 DPI awareness |
| D21 | Native UIA core and its built-in proxies |
| D22 | `AutoSetFocus = false` |
| D23 | `activate` requires hands-free mode |
| D24 | Capture algorithm |
| D25 | The hands-free session is a separate process |
| D26 | Session state files |
| D27 | LL hooks on a dedicated thread; callbacks only touch atomics |
| D28 | Overlay: one layered window per monitor, painted in Rust |
| D29 | Dry-run mode for desktop-free tests |
| D30 | `windows_input` preflight and in-batch checks |
| D31 | Native `ui_action activate` |
| D32 | `meta.json` reads retry transient errors |
| D33 | Dry-run injector |
| D34 | `windows_wait_until` runs on the UIA thread |
| D35 | The window root resolves by hwnd |
| D36 | Dry-run cursor |
| D37 | Acceptance uses only self-made windows |

**D1 — Rust + rmcp, stdio only.** The official Rust SDK (`rmcp`, features
`server`, `transport-io`) on tokio. One self-contained `.exe`, no runtime.
Remote access is a transport concern (e.g. an enishi `--mcp` service), not
the server's.

**D2 — The contract is a JSON file served verbatim.** `contracts/tools.json`
holds every tool's name, description, inputSchema, outputSchema and
annotations. It is embedded at build time and deserialized straight into
`rmcp::model::Tool`, so `tools/list` cannot drift from the file;
`tests/stdio.rs` asserts equality. `outputSchema: null` is emitted as an
absent key. Changing a tool means editing that file, the implementation
and the tests together.

**D3 — One result envelope for every tool.** See "Result envelope". Text
content is compact UTF-8 JSON (non-ASCII is not escaped).

**D4 — `server_version` is the only identity key.** Every envelope carries
`server_version` (the crate version); `windows_health.verification` adds
`server_name` and `server_version`; `initialize.serverInfo` reports the
same. Clients that need a capability check should rely on `tools/list`.

**D5 — Validation order.** `unknown_tool` → `invalid_arguments`
(inputSchema validated with the `jsonschema` crate, draft 2020-12, first
error only, ≤ 2000 chars) → tool-specific semantic checks → execution.
Invalid calls never take the busy lock and never run anything.

**D6 — Native `windows_health`.** No PowerShell launch: Win32 calls
(`GetComputerNameExW`, `GetUserNameExW(NameSamCompatible)`,
`CheckTokenMembership(Administrators)`, `GetTokenInformation(TokenElevation)`,
`ProcessIdToSessionId`, registry `CurrentVersion` and
`PowerShellEngine\PowerShellVersion`). `verification` reports `computer`,
`user`, `user_profile`, `temp_directory`, `cwd`, `powershell_version`,
`powershell_paths{windows_powershell,pwsh}`, `administrator_in_current_token`,
`elevated`, `os_version`, `os_product_name`, `os_display_version`,
`process_id`, `process_session_id`, `unicode` (a fixed non-ASCII string
that proves UTF-8 end to end), `audit_session_id`, `audit_path`,
`server_name`, `server_version`. `exit_code` is `null` (no child process).
`os_product_name` maps the registry's stale "Windows 10" to "Windows 11"
for builds ≥ 22000. `elevated` (token elevation) differs from
`administrator_in_current_token` (Administrators membership in the current,
possibly UAC-filtered token).

**D7 — Builds everywhere.** Win32 via `windows-sys` (FFI) and `windows`
(COM/UIA) behind `cfg(windows)`; elsewhere `platform::other` returns the
portable subset and Windows-only tools answer `status=failed`,
`error.kind=unsupported_platform` (typed, never a panic). CI runs fmt,
clippy `-D warnings` and tests on ubuntu-latest and windows-latest.

**D8 — Runs in the user's interactive session.** UIA, capture and
`SendInput` need the interactive desktop (`process_session_id ≠ 0`).
Started by an MCP client in the user's logon session — or by an enishi
daemon running as that user — the server runs non-elevated in that
session; `windows_health` reports `process_session_id` so clients can check.

**D9 — Native tools omit process-only envelope keys.** The shell, file, job
and package tools run in-process (Rust std + Win32). Keys that only
describe a PowerShell launch (`engine`, `cwd_requested`, `script_chars`,
`script_sha256`, `timeout_seconds`, `max_output_bytes`,
`output_bytes_observed`) appear only where a script actually ran;
`stdout`/`stderr`/`exit_code`/`timed_out`/`encoding_replaced`/`truncated`
are always present. Error messages may carry OS text; kinds are stable.
UNC paths are rejected for writes and job cwd; reads of UNC paths fail with
`not_found`. Off Windows, absolute POSIX paths are accepted so the same
code runs in Linux tests.

**D10 — `windows_run_powershell`: PowerShell 5.1 child, 32-bit exit
code.** `powershell.exe -NoLogo -NoProfile -NonInteractive
-EncodedCommand` (UTF-16LE base64, script ≤ 24000 encoded chars),
stdout/stderr read concurrently into bounded buffers
(`max_output_bytes`), timeout ≤ 300 s kills the whole tree (job object with
KILL_ON_JOB_CLOSE on Windows, process group elsewhere) and reports
`status=timed_out`. `exit_code` is the full 32-bit Windows code
(`exit 70000` → 70000).

**D11 — Detached jobs: the server binary is its own supervisor.**
`windows_job_start` writes `meta.json`, `launch.json`, `user.ps1` and
`wrapper.ps1` under `<state>\jobs\<uuid>` and launches
`windows-sosa-mcp.exe __job-supervisor <job_dir>`. The supervisor puts the
PowerShell child in its own kill-on-close job object, enforces timeout /
output limit / cancel (polls `cancel.json` every 250 ms) and writes
`result.json` with the 32-bit exit code. Launch order: not in a job →
plain spawn; in a job (e.g. a daemon or Task Scheduler) →
`CREATE_BREAKAWAY_FROM_JOB`; breakaway denied → WMI `Win32_Process.Create`
(parent WmiPrvSE, outside every caller job; forced with
`WINDOWS_SOSA_MCP_FORCE_WMI=1` for tests). `meta.launch_method` records
which one ran. `windows_job_cancel` asks the supervisor first, then kills
the tree itself and verifies (`kill_verified`). At most 5 jobs run at once.

**D12 — Per-user state root.** `%LOCALAPPDATA%\windows-sosa-mcp`
(override `WINDOWS_SOSA_MCP_HOME`; XDG state dir off Windows) holds
`audit\`, `jobs\`, `handsfree\`, UI stores and `mcp.lock`.
`job_list`/`job_status` see the jobs of that root only.

**D13 — Metadata-only audit log.** JSONL at `<state>\audit\events.jsonl`,
rotated at 2 MiB keeping 3 backups. Fields: `ts`, `phase`
(`session`/`started`/`finished`/`rejected`), `session_id`, `call_id`,
`tool_name`, argument *summary* (lengths/hashes, never content or
scripts), status, error kind, duration, pid, user, `caller`
(`ENISHI_PEER` if set, else `local`). `windows_health` reports the path.

**D14 — Cross-process busy lock.** Non-blocking exclusive OS file lock
(`LockFileEx` / `flock` through std `File::try_lock`) on
`<state>\mcp.lock`, held for the whole call; a concurrent call from any
server process gets `busy` immediately (audited as `rejected`). Clients
should not issue parallel tool calls to this server.

**D15 — `windows_search_text` uses a pinned ripgrep.** `rg.exe` 15.2.0,
verified against `vendor\rg.sha256` on every search, gives proven
encoding, binary-file and `.gitignore` handling without re-implementing
them. It lives in `vendor\` next to the exe (override
`WINDOWS_SOSA_MCP_VENDOR`); `scripts\fetch-rg.ps1 -Dest <dir>` downloads
the pinned release and checks archive and exe hashes; the release zip
bundles it. Missing binary → `dependency_missing`, changed binary →
`dependency_changed`; there is no fallback engine.

**D16 — Packages: `winget.exe` directly, positional parsing.**
`windows_package_query` runs `winget list`/`show --versions` with an exact
id and parses the table by column positions of the header, so it works
with any display language. `windows_package_install` re-queries, then runs
the embedded `assets/package_install.ps1` as a detached job, waits a
bounded time, re-queries and maps the WinGet return code via
`winget_codes.rs` (official return-code table). Scope is passed only when
given (`--scope user|machine`). `WINDOWS_SOSA_MCP_WINGET` overrides the
executable (tests use a fake).

**D17 — UI module layout and stable render format.** `src/ui/mod.rs`
(defaults, stores, result shaping, warnings), `src/ui/win.rs` (window
enumeration, resolution, UIA walk, patterns, GDI capture) and
platform-free, unit-tested pieces: `render.rs` (text tree:
`[Role] "name" id=… {invoke value="…" toggle=on scroll_v=12%} [focused]
[ref=eN]`, numbers in `%g`), `store.rs` (snapshots 120 s, captures 600 s,
files pruned after 600 s), `policy.rs`, `frame.rs` (BGRA frames,
non-black ratio, downscale, PNG). Window identity is hwnd + pid + process
start time, so a reused hwnd is `window_gone`. Error kinds:
`window_ambiguous` (with candidates), `window_gone`, `window_excluded`,
`snapshot_not_found`, `ref_not_found`, `ref_stale`,
`pattern_unavailable`, `session_locked`, `target_elevated`,
`redaction_required`…

**D18 — UI privacy policy file.** `%APPDATA%\windows-sosa-mcp\ui_policy.json`
(override `WINDOWS_SOSA_MCP_UI_POLICY`). Defaults exclude password
managers (KeePass, KeePassXC, 1Password, Bitwarden, LastPass…), the UAC
`consent` process and titles matching a password/2FA/secret regex
(English and Spanish); `capture_monitor_enabled=true`. A `null` value
disables that key. The policy applies to `list_windows` (`excluded`),
snapshots and window captures (`window_excluded`) and monitor captures
(excluded windows painted black; `redact=false` refused while any
exclusion exists).

**D19 — One resident COM MTA thread for UIA.** `IUIAutomation`
(`CUIAutomation8`, fallback `CUIAutomation`) and its `CacheRequest` live on
a dedicated thread initialised with `COINIT_MULTITHREADED` (UIA client
calls are free-threaded; MTA avoids pumping messages). Requests are
closures sent over a channel with a deadline; if a provider hangs past it
the call returns `timeout` and that thread is abandoned (a fresh one
serves the next call), so a frozen app cannot wedge the server. Window
enumeration and GDI captures run on short-lived threads with the same
deadline rule. Properties are fetched with `FindAllBuildCache` (children
scope, one call per walked node, reported as `find_calls`).

**D20 — Per-Monitor-V2 DPI awareness.**
`SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` at start-up (before
any window/GDI/UIA call), so element bounds, window rects, captures and
input coordinates all use physical virtual-desktop pixels, including
negative coordinates of monitors left of or above the primary.

**D21 — Native UIA core and its built-in proxies.** The server talks to the
native UIA core (no CLR). For apps that implement UIA themselves
(browsers, WinUI, WPF) that is the provider; for classic Win32/WinForms
controls the core's MSAA/Win32 proxies are used, which expose e.g. `Edit`
with its value, `ComboBox` with `value`, `invoke` on check/radio/list
items, and localized system names. Hosting the CLR for the managed
client-side proxies was rejected (runtime dependency, start-up cost).

**D22 — `AutoSetFocus = false`.** The native client focuses the target
element before pattern calls by default, which pulls windows to the
foreground on toggle/select. `IUIAutomation6::SetAutoSetFocus(false)`
avoids that. Consequence: a WinForms `RadioButton` may not change with
`SelectionItem.Select` without focus; `invoke` works. Foreground-restore
logic and warnings remain for providers that still take the foreground.

**D23 — `activate` requires hands-free mode.** Taking the foreground is a
visible desktop change, so outside a live hands-free session
`ui_action activate` is refused with `handsfree_required` (checked before
the snapshot lookup, so any snapshot id gets the documented error).

**D24 — Capture algorithm.** Windows: `PrintWindow(PW_RENDERFULLCONTENT)`
into a DIB, falling back to a screen `BitBlt` of the window rect when it
fails or the non-black ratio is < 0.02. Monitors: screen `BitBlt` of the
monitor rect with excluded windows blacked out. Downscale to `max_width`
with a triangle filter, encode PNG 8-bit RGB, return as MCP `ImageContent`
after the JSON text; `structuredContent` keeps metadata, `capture_sha256`,
origin/scale and `capture_id` (600 s, usable by `windows_input`).

**D25 — The hands-free session is a separate process.** The same exe with
a hidden `__handsfree-session <job_dir>` argument, launched through the
job launcher (direct → breakaway → WMI). Hooks and overlay live only in
that process, so (a) it survives MCP restarts like any job, and (b) when
it dies for any reason Windows removes its LL hooks and the user gets the
mouse/keyboard back (fail-open). `windows_job_cancel` / `stop` kill it
like any job. The session derives the state root from its job dir
(`<root>/jobs/<id>`), because a WMI launch does not inherit the server's
environment.

**D26 — Session state files.** `state.json` / `stop.json` under
`<root>/handsfree/` (`active, armed, ended_by, end_detail, expires_utc,
heartbeat_utc, blocked_events, last_real_input_utc, state_write_failures,
hooks_installed, monitors, guard, block_keyboard, release_hotkey…`). The
writer goes temp + rename with 25 × 15 ms retries; a failed write is
counted in `state_write_failures`, never fatal (a reader holding the file
must not kill the session). Readers open shared and retry the parse
20 × 15 ms. A heartbeat older than 6 s means `handsfree_stale`.

**D27 — LL hooks on a dedicated thread; callbacks only touch atomics.**
Windows silently drops LL hooks whose callback exceeds
`LowLevelHooksTimeout` (~300 ms), so state writes, overlay painting and the
self-probe live on other threads and the hook thread runs a bare
`GetMessage` loop. Injected events pass (our `SendInput` carries
`INPUT_MAGIC` in `dwExtraInfo`; UIA/accessibility injection also passes).
The self-probe (`PROBE_MAGIC`, every 10 s; a miss reinstalls the hooks
once, a second miss = `hook_lost`) is swallowed. Real input is counted
and, once armed (`arm_delay_ms`, default 2500), blocked (keyboard only with
`block_keyboard`), except the release of keys/buttons already down at
arming time. The release hotkey (default Ctrl+Alt+Shift+H, ≥ 2 modifiers)
ends the session even before arming. Past `expires + 5 s` nothing is
blocked even if the main loop is wedged. Ctrl+Alt+Del never reaches LL
hooks, and locking the workstation ends the session with
`session_locked`. With `guard=end_on_input` nothing is blocked: the first
real input ends the session.

**D28 — Overlay: one layered window per monitor, painted in Rust.**
`WS_EX_LAYERED|TRANSPARENT|TOPMOST|TOOLWINDOW|NOACTIVATE` (click-through,
never activates), Per-Monitor-V2 aware; monitors are enumerated once at
start. `paint.rs` renders premultiplied BGRA for `UpdateLayeredWindow`:
an edge gradient (#E64F8CFF → #E6B05CFF → #E62ED6C8) with an opacity
falloff over `glow_px` (default 44, scaled by monitor DPI) and a label
pill with the reason, minutes left and how to take control back
(GDI `DrawTextW`). No WPF/CLR dependency.

**D29 — Dry-run mode for desktop-free tests.**
`WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN=1` (or any non-Windows host) runs the
full session state machine (arming, TTL, stop, stale detection, state
files) without hooks or overlay; the state reports `dry_run: true`. Used
by `tests/stdio.rs` on Linux and Windows CI. It is a test switch: never
set it in a real deployment.

**D30 — `windows_input` preflight and in-batch checks.** Preflight (no
input sent, `may_have_changed: false`): `handsfree_required` (with
`(ended_by=X)`), `handsfree_stale`, `handsfree_session_mismatch`,
`session_locked`, `target_elevated` (integrity ≥ high),
`foreground_mismatch`, `capture_not_found`, `invalid_arguments` (the whole
batch is validated before the first event). During the batch the session
is re-read every 5 steps (`handsfree_ended`; a dead session process stops
the batch too), the foreground is compared after every action when
`abort_on_foreground_change` (`foreground_changed_abort`) and the cursor is
verified after moves (`cursor_placement_blocked`); on failure all held
keys/buttons are released and `failed{index,kind,message}` + `skipped[]`
are reported. `SendInput` in batches of ≤ 10; absolute coordinates over
the virtual desktop (`floor((x-VL)*65536/VW)+1`, clamped); `capture_id`
maps capture pixels back through origin/scale (round half to even);
typing ≤ 120 cps with `KEYEVENTF_UNICODE` (surrogate pairs); chords use
layout-independent virtual-key codes.

**D31 — Native `ui_action activate`.** Restore if minimized, `SetFocus` on
the window, then on the first focusable/enabled/on-screen child, then a
plain `SetForegroundWindow`; each step is verified and reported in
`activation_steps`; otherwise `activation_denied`. No
`AttachThreadInput`/fake-key tricks. Foreground restore is skipped for
`activate`.

**D32 — `meta.json` reads retry transient errors.** On Windows, `rename`
over a file someone is reading fails with a sharing violation and readers
can see a transient error, which made `job_cancel` occasionally report a
running job as unknown. `read_json` treats `NotFound` as final but retries
other errors and unparsable content 14 × 15 ms.

**D33 — Dry-run injector.** With D29 active (or `dry_run` in the session
state) the injector counts events instead of calling `SendInput`, and
`paste` uses an in-memory clipboard, so the stdio tests exercise the whole
`windows_input` path on a real Windows host without touching the desktop.

**D34 — `windows_wait_until` runs on the UIA thread.** Conditions:
`window_appears/disappears` (filters `hwnd`, `process`, `title_contains`,
`exclude_hwnds`; sample of 3), `foreground_is` (`hwnd`/`root_hwnd`/
`process`), `element_appears/disappears` (`automation_id`/`name_contains`,
hits capped at 50, sample 5), `user_idle_above`, `time`; `poll_ms ≥ 100`
(default 400), timeout default 15 s; result `{condition, met, elapsed_ms,
polls, observed}`, not met = `condition_not_met` with the data. Read-only;
needs no session.

**D35 — The window root resolves by hwnd.** Refs are normally re-found by
`ControlType AND (AutomationId | Name)` and matched by RuntimeId. For the
window root, Name is the title, which apps change while they initialize
(restored tabs, "(loading)" suffixes), so a name search would miss the
same window and `activate` on `e1` would fail with `ref_stale`. The
snapshot record stores `root_ref` (only `scope=window` snapshots root at
the window element) and `resolve_ref` returns the window element itself
for it (or for any anchor whose RuntimeId equals the window's). Identity
stays enforced by hwnd + pid + process start. `activate` on the root also
skips the `element_offscreen` refusal (a minimized window is offscreen;
activate restores it first).

**D36 — Dry-run cursor.** With the dry-run injector the real cursor never
moves, so the injector remembers where the counted absolute move would
have left it (exact inverse of the 0..65535 mapping) and reports that for
the post-move placement check. Off-desktop targets still clamp to the
virtual-screen edge and are still `cursor_placement_blocked`.

**D37 — Acceptance uses only self-made windows.** Real applications can
restore private content (e.g. an editor reopening unsaved tabs), so the
acceptance script never opens or reads them. It launches
`tests/acceptance/hf_test_app.ps1` (WinForms, shown without activation,
title `SosaAcceptance-<hex>`): multiline TextBox (UIA ValuePattern), a
canvas that records every stroke in screen pixels, a click target that
records the click position, buttons that make a label appear/disappear and
open an owned popup after a delay, and a "Save changes?" dialog on close
with unsaved text. Effects are verified through UIA and the app's JSON
state file (the clipboard text present at start is restored on close if
the test changed it). The script finds the hwnd with `wait_until
window_appears {title_contains, exclude_hwnds}`, acts only on that hwnd and
its own dialog, and cleans up via UIA close, `job_cancel` and its PID.
`--dry-run` (server with D29) runs the whole flow without takeover, marks
effect checks that need real input as SKIP and refuses to run if the flag
and the server's environment disagree.

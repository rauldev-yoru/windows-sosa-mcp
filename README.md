# windows-sosa-mcp (操作)

A native Windows [MCP](https://modelcontextprotocol.io) server written in
Rust: shell, files, detached jobs, WinGet packages, UI Automation, screen
capture and a guarded hands-free input mode — one small `.exe`, stdio
transport, no runtime to install.

*Sōsa* (操作) means "operation / to operate" in Japanese.

## Features

- **24 typed tools** with JSON Schemas for inputs and outputs
  ([contracts/tools.json](contracts/tools.json)) and one
  [result envelope](docs/design.md#result-envelope) for all of them.
- **Native and fast**: Win32 and UI Automation from Rust; PowerShell only
  runs when you ask for PowerShell.
- **Bounded by design**: timeouts, output caps and size limits on every
  tool; one call at a time; metadata-only audit log.
- **Guarded hands-free mode**: real mouse/keyboard input only inside a
  visible, time-limited session that the user can end at any moment.
- **Privacy policy for UI tools**: password managers, UAC prompts and
  secret-looking windows are excluded from snapshots and captures.

| Group | Tools |
|---|---|
| Core | `windows_health`, `windows_run_powershell` |
| Files | `windows_list_directory`, `windows_find_files`, `windows_stat_path`, `windows_read_text_file`, `windows_search_text`, `windows_write_text_file`, `windows_edit_text_file`, `windows_resolve_executable` |
| Jobs | `windows_job_start`, `windows_job_status`, `windows_job_list`, `windows_job_cancel` |
| Packages (WinGet) | `windows_package_query`, `windows_package_install` |
| UI Automation | `windows_list_windows`, `windows_ui_snapshot`, `windows_ui_action` |
| Capture | `windows_capture_window`, `windows_capture_monitor` |
| Hands-free input | `windows_handsfree`, `windows_input`, `windows_wait_until` |

Highlights: `windows_ui_snapshot` returns a compact text tree with `eN`
refs that `windows_ui_action` (invoke, toggle, set value, select, scroll,
expand, activate…) acts on; captures return PNG images as MCP image
content; `windows_input` clicks, drags, types Unicode, pastes and sends
chords, optionally in screenshot pixel space; `windows_wait_until` waits
for windows, elements, focus or user idleness.

## Requirements

- Windows 10 or 11, x64.
- Windows PowerShell 5.1 (built in) for `windows_run_powershell`, jobs and
  package installs; WinGet for the package tools.
- UI, capture and input tools need an interactive desktop session; run the
  server as the logged-in user (not as a service in session 0).

## Install

Pick one. All three give you the same self-contained `windows-sosa-mcp.exe`
(Windows x64, ~10 MB, no runtime or helper binaries).

### 1. npx (Node.js 18+)

Nothing to install up front: point your MCP client at

```sh
npx -y windows-sosa-mcp
```

npm downloads the package once (~4 MB) and runs the bundled exe through a
tiny launcher. Each start goes through `npx`, so it takes about a second
longer than starting the exe directly.

### 2. One-line installer (PowerShell, no admin)

```powershell
irm https://raw.githubusercontent.com/rauldev-yoru/windows-sosa-mcp/main/install.ps1 | iex
```

It downloads the latest release, verifies it against `SHA256SUMS`, installs
it to `%LOCALAPPDATA%\Programs\windows-sosa-mcp\windows-sosa-mcp.exe`, adds
that folder to your user `PATH` and prints a config snippet. Run it again to
upgrade (works while clients still run the old exe). Options, as
environment variables: `WINDOWS_SOSA_MCP_VERSION` (`v0.1.1`, default latest),
`WINDOWS_SOSA_MCP_INSTALL_DIR`, `WINDOWS_SOSA_MCP_NO_MODIFY_PATH=1`.
Uninstall:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/rauldev-yoru/windows-sosa-mcp/main/install.ps1))) -Uninstall
```

### 3. Manual download

Each [GitHub release](https://github.com/rauldev-yoru/windows-sosa-mcp/releases)
has `windows-sosa-mcp.exe` and `SHA256SUMS`:

```powershell
$v = 'v0.1.1'
$base = "https://github.com/rauldev-yoru/windows-sosa-mcp/releases/download/$v"
$dir = "$env:LOCALAPPDATA\Programs\windows-sosa-mcp"
New-Item -ItemType Directory -Force $dir | Out-Null
Invoke-WebRequest "$base/windows-sosa-mcp.exe" -OutFile "$dir\windows-sosa-mcp.exe"
Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$dir\SHA256SUMS"
(Get-FileHash "$dir\windows-sosa-mcp.exe" -Algorithm SHA256).Hash.ToLower()
Get-Content "$dir\SHA256SUMS"   # must list the same hash
```

### Build from source

Needs a Rust toolchain (stable, MSVC target) on Windows:

```powershell
git clone https://github.com/rauldev-yoru/windows-sosa-mcp
cd windows-sosa-mcp
cargo build --release --locked   # target\release\windows-sosa-mcp.exe
```

The crate also builds and tests on Linux; Windows-only tools then answer
`unsupported_platform`.

## Configure an MCP client

The server speaks MCP over stdio and takes no arguments. The examples use
`npx`; with the installer, use `windows-sosa-mcp` (or the full exe path
it printed) as the command and drop the args.

**Claude Desktop** (`%APPDATA%\Claude\claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "windows-sosa": {
      "command": "npx",
      "args": ["-y", "windows-sosa-mcp"]
    }
  }
}
```

Installed exe instead: `"command": "windows-sosa-mcp"` (or
`"C:\\Users\\<you>\\AppData\\Local\\Programs\\windows-sosa-mcp\\windows-sosa-mcp.exe"`).

**VS Code** (`.vscode/mcp.json` or the user `mcp.json`):

```json
{
  "servers": {
    "windows-sosa": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "windows-sosa-mcp"]
    }
  }
}
```

**OpenCode** (`opencode.json`):

```json
{
  "mcp": {
    "windows-sosa": {
      "type": "local",
      "command": ["npx", "-y", "windows-sosa-mcp"],
      "enabled": true
    }
  }
}
```

**Codex** (`~/.codex/config.toml`):

```toml
[mcp_servers.windows-sosa]
command = "npx"
args = ["-y", "windows-sosa-mcp"]
```

**Hermes Agent and other generic stdio clients** (YAML):

```yaml
mcp_servers:
  windows-sosa:
    command: windows-sosa-mcp
    args: []
```

If a client can't start `npx` on Windows (it is a `.cmd` shim), use
`"command": "cmd", "args": ["/c", "npx", "-y", "windows-sosa-mcp"]` or the
installed exe.

Clients should issue one tool call at a time: a call made while another is
running gets a `busy` result immediately.

## Use it remotely with enishi

[enishi](https://github.com/rauldev-yoru/enishi) can publish the server
from the Windows machine and give clients on other devices a stdio
endpoint:

```sh
# on the Windows machine (the enishi daemon must run as the desktop user)
enishi service add sosa --mcp -- npx -y windows-sosa-mcp
# or, with the installed exe: enishi service add sosa --mcp -- windows-sosa-mcp

# on the client machine: use this as the MCP server command
enishi mcp <device>/sosa
```

For example, with a generic stdio client:

```yaml
mcp_servers:
  windows-sosa:
    command: enishi
    args: ["mcp", "<device>/sosa"]
```

When `ENISHI_PEER` is set by the transport, the audit log records it as
the caller.

## Safety

- **Hands-free mode is the only way to send real input.** `windows_input`
  and `windows_ui_action activate` fail with `handsfree_required` unless a
  session started with `windows_handsfree` is live. A session:
  - shows a glowing border and a label on every monitor;
  - blocks the user's mouse (and keyboard unless disabled) only after an
    arming delay, letting the server's own injected input through;
  - ends on **Ctrl+Alt+Shift+H**, on `windows_handsfree stop`, when its
    TTL expires, when the workstation is locked, or when its process dies
    (the hooks are removed by Windows: fail-open). Ctrl+Alt+Del always
    works.
- **Input preflight**: no input is sent to a locked session, an elevated
  window or a window other than the expected foreground.
- **UI privacy policy**: `%APPDATA%\windows-sosa-mcp\ui_policy.json`
  (override with `WINDOWS_SOSA_MCP_UI_POLICY`):

  ```json
  {
    "exclude_processes": ["KeePass", "KeePassXC", "1Password", "Bitwarden", "LastPass", "consent"],
    "exclude_title_regex": "(?i)password|passphrase|\\b2fa\\b|verification code|secret key|recovery code",
    "allow_processes": [],
    "capture_monitor_enabled": true
  }
  ```

  Every key is optional (missing file = built-in defaults, `null` disables
  a key). Excluded windows are hidden from listings and snapshots, refused
  for window captures and painted black in monitor captures.
- **No elevation**: the server runs with the user's token and never asks
  for UAC.
- **Audit**: `%LOCALAPPDATA%\windows-sosa-mcp\audit\events.jsonl` records
  tool, status, durations and argument summaries — never file contents,
  scripts or typed text.
- `windows_run_powershell` and `windows_job_start` run arbitrary
  PowerShell as the user: only connect clients you trust.

## Configuration

| Variable | Purpose |
|---|---|
| `WINDOWS_SOSA_MCP_HOME` | state root (default `%LOCALAPPDATA%\windows-sosa-mcp`) |
| `WINDOWS_SOSA_MCP_UI_POLICY` | UI policy file |
| `WINDOWS_SOSA_MCP_POWERSHELL` | PowerShell executable (default Windows PowerShell 5.1) |
| `WINDOWS_SOSA_MCP_WINGET` | WinGet executable |
| `WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN` | `1` = hands-free without hooks/overlay/real input — **tests only** |

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

`tests/stdio.rs` drives the real binary over stdio (contract, envelope,
files, jobs, packages with a fake WinGet, hands-free in dry-run mode).
`tests/acceptance/` holds the attended real-desktop acceptance run for
hands-free mode and WinForms test windows:

```sh
# local, on the Windows machine
python tests/acceptance/handsfree_acceptance.py --server-cmd "C:\path\windows-sosa-mcp.exe"
# remote, through enishi
python3 tests/acceptance/handsfree_acceptance.py --server-cmd "enishi mcp <device>/sosa"
# without taking over the desktop (server started with WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN=1)
python3 tests/acceptance/handsfree_acceptance.py --server-cmd "..." --dry-run --yes
```

Design, safety model, envelope, error kinds and decisions:
[docs/design.md](docs/design.md). Changes: [CHANGELOG.md](CHANGELOG.md).

## Status

v0.1.1 — single self-contained exe; npm and one-line installer
distribution. All 24 tools are implemented natively and
validated on Windows 11 with multiple monitors, including real input
blocking, overlay, release hotkey, fail-open and TTL. The tool contract
may still change before 1.0; breaking changes are listed in the changelog.

## License

Licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. `windows_search_text` is
built on [ripgrep](https://github.com/BurntSushi/ripgrep)'s library crates
(MIT / Unlicense).

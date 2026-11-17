# windows-sosa-mcp

Native Windows [MCP](https://modelcontextprotocol.io) server over stdio:
shell, files, detached jobs, WinGet packages, UI Automation, screen capture
and a guarded hands-free input mode. This package ships the self-contained
`windows-sosa-mcp.exe` (Windows 10/11 x64) and a tiny launcher.

```json
{
  "mcpServers": {
    "windows-sosa": { "command": "npx", "args": ["-y", "windows-sosa-mcp"] }
  }
}
```

Documentation, safety model and the other install options (one-line
PowerShell installer, release download):
https://github.com/rauldev-yoru/windows-sosa-mcp

Licensed under MIT or Apache-2.0, at your option.

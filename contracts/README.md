# Tool contract

`tools.json` is the tool contract of `windows-sosa-mcp`: for each of the 24
tools its `name`, `description`, `inputSchema`, `outputSchema` and
`annotations`. It is the single source of truth (docs/design.md, D2):

- The file is embedded in the binary at build time and served verbatim as the
  `tools/list` answer; `inputSchema` is also what `tools/call` validates
  arguments against (`invalid_arguments`).
- `tests/stdio.rs` asserts that the running server's `tools/list` equals this
  file, so a change here must ship with the matching implementation and tests.
- `outputSchema` describes the tool-specific `data` object. Every result is
  wrapped in the common envelope (`schema_version`, `server_version`,
  `tool_name`, `status`, `error`, `error_kind`, …) documented in
  docs/design.md, "Result envelope".
- Tool names and argument shapes are a public API: prefer additive changes
  (new optional arguments, new output fields) and record breaking ones in
  CHANGELOG.md.

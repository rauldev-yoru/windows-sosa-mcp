#!/usr/bin/env node
'use strict';
// Launcher for the bundled windows-sosa-mcp.exe: stdio is inherited (it is
// the MCP channel, so this file never writes to stdout), the exit code is
// forwarded and termination signals are passed on to the server.
const { spawn } = require('node:child_process');
const path = require('node:path');

if (process.platform !== 'win32' || process.arch !== 'x64') {
  console.error(
    `windows-sosa-mcp: only Windows x64 is supported (this is ${process.platform}-${process.arch})`,
  );
  process.exit(1);
}

const exe = path.join(__dirname, 'windows-sosa-mcp.exe');
const child = spawn(exe, process.argv.slice(2), { stdio: 'inherit', windowsHide: true });

for (const signal of ['SIGINT', 'SIGTERM', 'SIGBREAK', 'SIGHUP']) {
  process.on(signal, () => {
    if (child.exitCode === null && child.signalCode === null) child.kill(signal);
  });
}

child.on('error', (err) => {
  console.error(`windows-sosa-mcp: cannot start ${exe}: ${err.message}`);
  process.exit(1);
});

child.on('exit', (code, signal) => {
  process.exit(code ?? (signal ? 1 : 0));
});

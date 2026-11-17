// Smoke test of a packed npm tarball: install it into a scratch project,
// start the server through `npx windows-sosa-mcp` and check initialize,
// tools/list (24 tools) and one windows_search_text call.
//   node npm/smoke.mjs <windows-sosa-mcp-x.y.z.tgz>
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const tgz = process.argv[2];
if (!tgz) {
  console.error('usage: node npm/smoke.mjs <tarball>');
  process.exit(2);
}
const dir = mkdtempSync(join(tmpdir(), 'sosa-npm-smoke-'));
const fail = (msg) => {
  console.error(`smoke: ${msg}`);
  process.exit(1);
};

try {
  writeFileSync(join(dir, 'package.json'), '{"name":"sosa-smoke","private":true}\n');
  writeFileSync(join(dir, 'needle.txt'), 'npm smoke needle\n');
  // npm/npx are .cmd shims on Windows: run them through the shell as one command line.
  const inst = spawnSync(`npm install --no-audit --no-fund "${resolve(tgz)}"`, {
    cwd: dir,
    shell: true,
    stdio: 'inherit',
  });
  if (inst.status !== 0) fail(`npm install exited ${inst.status}`);

  const started = Date.now();
  const child = spawn('npx --no-install windows-sosa-mcp', {
    cwd: dir,
    shell: true,
    stdio: ['pipe', 'pipe', 'inherit'],
    env: { ...process.env, WINDOWS_SOSA_MCP_HOME: join(dir, 'state') },
  });
  const replies = new Map();
  let buf = '';
  let raw = '';
  child.stdout.on('data', (d) => {
    raw += d;
    buf += d;
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (!line) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        fail(`non JSON-RPC line on stdout: ${line.slice(0, 200)}`);
      }
      if (msg.id !== undefined) replies.get(msg.id)?.(msg);
    }
  });
  const send = (m) => child.stdin.write(`${JSON.stringify(m)}\n`);
  const call = (id, method, params) =>
    new Promise((res, rej) => {
      const t = setTimeout(() => rej(new Error(`${method} timed out`)), 60_000);
      replies.set(id, (m) => {
        clearTimeout(t);
        res(m);
      });
      send({ jsonrpc: '2.0', id, method, params });
    });

  const init = await call(1, 'initialize', {
    protocolVersion: '2025-06-18',
    capabilities: {},
    clientInfo: { name: 'npm-smoke', version: '0' },
  });
  send({ jsonrpc: '2.0', method: 'notifications/initialized' });
  const firstReplyMs = Date.now() - started;
  const list = await call(2, 'tools/list', {});
  const n = list.result?.tools?.length;
  if (n !== 24) fail(`expected 24 tools, got ${n}`);
  const search = await call(3, 'tools/call', {
    name: 'windows_search_text',
    arguments: { path: dir, pattern: 'smoke needle', max_depth: 0 },
  });
  const sr = search.result?.structuredContent ?? JSON.parse(search.result?.content?.[0]?.text ?? 'null');
  if (sr?.status !== 'succeeded' || sr.data.count !== 1) {
    fail(`windows_search_text: ${JSON.stringify(search).slice(0, 500)}`);
  }
  child.stdin.end();
  const code = await new Promise((res) => child.on('exit', res));
  if (code !== 0) fail(`server exited ${code} after stdin EOF`);
  console.log(
    `smoke ok: ${init.result.serverInfo.name} ${init.result.serverInfo.version}, ${n} tools, ` +
      `search ok, first reply ${firstReplyMs} ms, clean exit, ${raw.length} bytes of pure JSON-RPC on stdout`,
  );
} finally {
  rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
}

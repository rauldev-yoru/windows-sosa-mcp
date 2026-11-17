// Stage the npm package: copy the release exe and the licenses into
// npm/windows-sosa-mcp and check that package.json matches the crate version.
//   node npm/stage.mjs <path to windows-sosa-mcp.exe>
import { copyFileSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const pkgDir = join(here, 'windows-sosa-mcp');

const exe = process.argv[2];
if (!exe) {
  console.error('usage: node npm/stage.mjs <windows-sosa-mcp.exe>');
  process.exit(2);
}

const cargo = readFileSync(join(root, 'Cargo.toml'), 'utf8');
const crateVersion = /^\[package\][^[]*?^version\s*=\s*"([^"]+)"/ms.exec(cargo)?.[1];
const pkg = JSON.parse(readFileSync(join(pkgDir, 'package.json'), 'utf8'));
if (!crateVersion || pkg.version !== crateVersion) {
  console.error(`npm package version ${pkg.version} != crate version ${crateVersion}`);
  process.exit(1);
}

copyFileSync(exe, join(pkgDir, 'bin', 'windows-sosa-mcp.exe'));
for (const f of ['LICENSE-MIT', 'LICENSE-APACHE']) copyFileSync(join(root, f), join(pkgDir, f));
console.log(`staged ${pkg.name}@${pkg.version} in ${pkgDir}`);

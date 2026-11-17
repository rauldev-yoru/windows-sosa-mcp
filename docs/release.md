# Releasing

A release is one tag. `.github/workflows/release.yml` does the rest.

## Steps

1. Bump the version in `Cargo.toml` **and** `npm/windows-sosa-mcp/package.json`
   (they must be equal; `npm/stage.mjs` refuses to stage otherwise), add a
   `## [x.y.z] - date` section to `CHANGELOG.md`, run the checks
   (`cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings`,
   `cargo test --all-targets --locked`) and push to `main`.
2. Tag and push: `git tag -a vX.Y.Z -m "windows-sosa-mcp vX.Y.Z" && git push origin vX.Y.Z`.
   A tag with a suffix (`v0.2.0-rc.1`) becomes a GitHub pre-release and is
   published to npm under the `next` dist-tag instead of `latest`.

The workflow then runs three jobs:

| Job | Runner | What it does |
|---|---|---|
| `build` | Windows | checks tag = crate version, `cargo test`, release build, stdio smoke test (`initialize` + `tools/list` = 24 tools) |
| `release` | Linux | `SHA256SUMS`, notes from the `CHANGELOG.md` section, GitHub Release with `windows-sosa-mcp.exe` + `SHA256SUMS` |
| `npm` | Windows | stages the exe into `npm/windows-sosa-mcp`, `npm pack`, installs the tarball into a scratch project and runs `npx windows-sosa-mcp` (24 tools + one `windows_search_text` call + clean exit on stdin EOF), then publishes if configured |

`install.ps1` (the one-line installer) downloads the GitHub Release assets,
so it picks up a new version as soon as the `release` job is done.

## npm package

`npm/windows-sosa-mcp/` is a single package, `windows-sosa-mcp`, restricted
to `"os": ["win32"], "cpu": ["x64"]`. It contains:

```
bin/windows-sosa-mcp.js    launcher (package "bin"): spawns the exe with stdio inherited,
                           forwards the exit code and termination signals, never writes stdout
bin/windows-sosa-mcp.exe   the release exe (copied by npm/stage.mjs, not in git)
LICENSE-MIT, LICENSE-APACHE (copied by npm/stage.mjs)
README.md, package.json
```

Only one platform is supported, so there are no per-platform optional
packages: `npx -y windows-sosa-mcp` downloads one tarball (~4 MB packed) and
runs the exe.

Local dry run on Windows (after `cargo build --release --locked`):

```powershell
node npm/stage.mjs target\release\windows-sosa-mcp.exe
npm pack ./npm/windows-sosa-mcp
node npm/smoke.mjs .\windows-sosa-mcp-<version>.tgz
```

### Publishing

The `npm` job always packs and smoke-tests; it publishes only when one of
these is configured, otherwise it ends with a notice and the release is
still complete (GitHub assets + installer).

**First publish — token.** The package name must exist on npm before
trusted publishing can be enabled, so the first release uses a token:

1. Create an npm account (with 2FA) and a **granular access token** with
   read/write on packages (it can't be scoped to a package that doesn't
   exist yet; restrict it to `windows-sosa-mcp` after the first publish, or
   delete it once trusted publishing works).
2. Add it as the repository secret `NPM_TOKEN`
   (Settings → Secrets and variables → Actions).
3. Push the tag. The job runs `npm publish --provenance --access public`;
   provenance uses the job's `id-token: write` permission.

Alternatively publish the first version by hand from the smoke-tested
tarball: `npm publish windows-sosa-mcp-<version>.tgz --access public`
(no provenance).

**Target — trusted publishing (OIDC, no long-lived token).** Once the
package exists:

1. On npmjs.com → package `windows-sosa-mcp` → Settings → Trusted publishing,
   add a GitHub Actions publisher: owner `rauldev-yoru`, repository
   `windows-sosa-mcp`, workflow `release.yml` (no environment).
2. Set the repository **variable** `NPM_TRUSTED_PUBLISHING` = `true`.
3. Delete the `NPM_TOKEN` secret (it takes precedence when present) and
   revoke the token on npm. Optionally set the package to "require 2FA and
   disallow tokens".

The job then upgrades npm (trusted publishing needs npm ≥ 11.5.1) and
`npm publish` authenticates with the workflow's OIDC token; provenance is
attached automatically.

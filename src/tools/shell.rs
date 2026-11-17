//! `windows_run_powershell`: Windows PowerShell 5.1 with `-EncodedCommand`,
//! bounded capture of stdout/stderr and a job-object tree kill on timeout.
//! The process exit code is reported as a 32-bit integer.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

use super::{ToolError, opt_i64, opt_str};
use crate::envelope::{self, Envelope};

pub const MAX_ENCODED: usize = 24_000;

pub struct RunArgs {
    pub script: String,
    pub cwd: Option<String>,
    pub timeout_seconds: u64,
    pub max_output_bytes: usize,
    pub request_id: Option<String>,
}

pub fn prepare(a: &Value) -> Result<RunArgs, ToolError> {
    Ok(RunArgs {
        script: opt_str(a, "script").unwrap_or_default(),
        cwd: opt_str(a, "cwd"),
        timeout_seconds: opt_i64(a, "timeout_seconds").unwrap_or(30) as u64,
        max_output_bytes: opt_i64(a, "max_output_bytes").unwrap_or(65536) as usize,
        request_id: opt_str(a, "request_id"),
    })
}

const WRAPPER: &str = r#"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$utf8 = New-Object System.Text.UTF8Encoding
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8
try {
    $request = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__PAYLOAD__')) | ConvertFrom-Json
    $dir = $request.cwd
    if ($null -eq $dir) { $dir = $env:USERPROFILE }
    if ($dir -notmatch '^(?:[A-Za-z]:[\\/]|\\\\[^\\]+\\[^\\]+(?:\\|$))') { throw 'cwd must be an absolute Windows path' }
    Set-Location -LiteralPath $dir
    if ((Get-Location).Provider.Name -ne 'FileSystem') { throw 'cwd must be a filesystem directory' }
    $global:LASTEXITCODE = 0
    . ([ScriptBlock]::Create($request.script))
    exit ([int]$LASTEXITCODE)
} catch {
    [Console]::Error.WriteLine($_.ToString())
    exit 1
}
"#;

/// Base64 of the UTF-16LE wrapper with the request embedded as base64 JSON
/// data (never interpolated as source).
pub fn encoded_script(script: &str, cwd: Option<&str>) -> Result<String, String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let payload = b64
        .encode(serde_json::to_string(&json!({"script": script, "cwd": cwd})).unwrap_or_default());
    let source = WRAPPER.replace("__PAYLOAD__", &payload);
    let utf16: Vec<u8> = source.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = b64.encode(utf16);
    if encoded.len() > MAX_ENCODED {
        return Err("Script/cwd too large for v0.1 command transport; split the task".into());
    }
    Ok(encoded)
}

/// Result of a bounded capture.
pub struct Captured {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
    pub observed: (u64, u64),
    pub encoding_replaced: bool,
    pub termination_confirmed: bool,
}

/// Drain both pipes with a shared retention budget; on timeout kill the
/// whole process tree (job object on Windows).
pub fn capture(mut cmd: Command, timeout: Duration, limit: usize) -> std::io::Result<Captured> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::platform::no_window(&mut cmd);
    crate::platform::prepare_tree(&mut cmd);
    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let tree = crate::platform::KillTree::attach(&child);
    let (tx, rx) = mpsc::channel::<(usize, Option<Vec<u8>>)>();
    let pipes: [Box<dyn Read + Send>; 2] = [
        Box::new(child.stdout.take().expect("stdout")),
        Box::new(child.stderr.take().expect("stderr")),
    ];
    for (i, mut p) in pipes.into_iter().enumerate() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            loop {
                match p.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send((i, Some(buf[..n].to_vec()))).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send((i, None));
        });
    }
    drop(tx);
    let mut out: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    let mut totals = [0u64; 2];
    let (mut retained, mut open, mut timed_out) = (0usize, 2, false);
    let mut status = None;
    loop {
        if open == 0 {
            if status.is_none() {
                status = child.try_wait()?;
            }
            if status.is_some() {
                break;
            }
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        if open == 0 {
            std::thread::sleep(remaining.min(Duration::from_millis(20)));
            continue;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok((_, None)) => open -= 1,
            Ok((i, Some(chunk))) => {
                totals[i] += chunk.len() as u64;
                let keep = chunk.len().min(limit.saturating_sub(retained));
                out[i].extend_from_slice(&chunk[..keep]);
                retained += keep;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => open = 0,
        }
    }
    let mut confirmed = true;
    if timed_out || status.is_none() {
        confirmed = tree.kill(&mut child);
    }
    if status.is_none() {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(s)) = child.try_wait() {
                status = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let mut replaced = false;
    let mut dec = |b: &[u8]| match String::from_utf8(b.to_vec()) {
        Ok(s) => s,
        Err(_) => {
            replaced = true;
            String::from_utf8_lossy(b).into_owned()
        }
    };
    let (stdout, stderr) = (dec(&out[0]), dec(&out[1]));
    Ok(Captured {
        stdout,
        stderr,
        exit_code: if timed_out {
            None
        } else {
            status.and_then(|s| s.code())
        },
        timed_out,
        truncated: totals[0] + totals[1] > retained as u64,
        observed: (totals[0], totals[1]),
        encoding_replaced: replaced,
        termination_confirmed: confirmed,
    })
}

pub fn powershell_exe() -> String {
    if let Ok(p) = std::env::var("WINDOWS_SOSA_MCP_POWERSHELL") {
        return p;
    }
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe")
}

pub fn run(args: RunArgs) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base("powershell.run");
    if let Some(id) = &args.request_id {
        r.insert("request_id".into(), json!(id));
    }
    r.insert("timeout_seconds".into(), json!(args.timeout_seconds));
    r.insert("max_output_bytes".into(), json!(args.max_output_bytes));
    r.insert("cwd_requested".into(), json!(args.cwd));
    r.insert(
        "script_sha256".into(),
        json!(super::text::digest(args.script.as_bytes())),
    );
    r.insert("script_chars".into(), json!(args.script.chars().count()));
    let invalid = if args.script.trim().is_empty() || args.script.contains('\0') {
        Some("script must be nonempty text without NUL".to_string())
    } else if args
        .cwd
        .as_deref()
        .is_some_and(|c| c.is_empty() || c.contains('\0'))
    {
        Some("cwd must be null or nonempty text without NUL".to_string())
    } else if args
        .request_id
        .as_deref()
        .is_some_and(|id| id.chars().any(|c| (c as u32) < 32))
    {
        Some("request_id contains control characters".to_string())
    } else {
        None
    };
    let encoded =
        match invalid.map_or_else(|| encoded_script(&args.script, args.cwd.as_deref()), Err) {
            Ok(e) => e,
            Err(m) => {
                envelope::fail(&mut r, "invalid_request", m);
                envelope::stamp(&mut r, start);
                return r;
            }
        };
    let engine = powershell_exe();
    r.insert("engine".into(), json!(engine));
    let mut cmd = Command::new(&engine);
    cmd.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-OutputFormat",
        "Text",
        "-EncodedCommand",
        &encoded,
    ]);
    match capture(
        cmd,
        Duration::from_secs(args.timeout_seconds),
        args.max_output_bytes,
    ) {
        Err(e) => envelope::fail(&mut r, "launch_failed", e.to_string()),
        Ok(c) => {
            r.insert("stdout".into(), json!(c.stdout));
            r.insert("stderr".into(), json!(c.stderr));
            r.insert("exit_code".into(), json!(c.exit_code));
            r.insert("timed_out".into(), json!(c.timed_out));
            r.insert("truncated".into(), json!(c.truncated));
            r.insert(
                "output_bytes_observed".into(),
                json!({"stdout": c.observed.0, "stderr": c.observed.1}),
            );
            r.insert("encoding_replaced".into(), json!(c.encoding_replaced));
            if c.timed_out {
                envelope::fail(
                    &mut r,
                    "timeout",
                    if c.termination_confirmed {
                        "Timed out; the PowerShell process tree was terminated (job object). Check effects before retrying."
                    } else {
                        "Timed out; Windows process termination is NOT confirmed. Check effects before retrying."
                    },
                );
                r.insert(
                    "windows_termination_confirmed".into(),
                    json!(c.termination_confirmed),
                );
            } else if c.exit_code == Some(0) {
                r.insert("status".into(), json!("succeeded"));
            } else {
                envelope::fail(
                    &mut r,
                    "execution_failed",
                    "PowerShell returned a nonzero exit code",
                );
            }
        }
    }
    envelope::stamp(&mut r, start);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_limit() {
        assert!(encoded_script("1", None).is_ok());
        let big = "x".repeat(20_000);
        assert!(
            encoded_script(&big, None)
                .unwrap_err()
                .contains("too large")
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_truncates_and_times_out() {
        let mut c = Command::new("sh");
        c.args(["-c", "printf 'aaaaaaaaaa'; printf 'bbbbb' >&2; exit 3"]);
        let r = capture(c, Duration::from_secs(5), 12).unwrap();
        assert_eq!(r.exit_code, Some(3));
        assert!(r.truncated);
        assert_eq!(r.stdout.len() + r.stderr.len(), 12);
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 5"]);
        let t = Instant::now();
        let r = capture(c, Duration::from_millis(300), 100).unwrap();
        assert!(r.timed_out && r.exit_code.is_none());
        assert!(t.elapsed() < Duration::from_secs(3));
    }
}

//! Persistent detached jobs: `windows_job_start/status/list/cancel`.
//!
//! The supervisor is this executable re-launched as
//! `windows-sosa-mcp.exe __job-supervisor <job_dir>` outside the caller's
//! job object (design D11); it runs the user script with a fixed
//! PowerShell wrapper, redirects output to files, enforces timeout/output
//! limits/cancel with a job-object tree kill and records `result.json`.
//!
//! Per-job layout: meta.json, launch.json,
//! result.json, cancel.json, user.ps1, wrapper.ps1, stdout.log, stderr.log
//! (+ server-authored data files such as package.json).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use super::text::{TextError, digest};
use super::{ToolError, opt_i64, opt_str};
use crate::envelope::{self, Envelope};
use crate::paths::{self, fmt_o};
use crate::platform;

pub const MAX_RUNNING: usize = 5;
pub const SUPERVISOR_ARG: &str = "__job-supervisor";
const RESERVED: [&str; 8] = [
    "meta.json",
    "launch.json",
    "result.json",
    "cancel.json",
    "user.ps1",
    "wrapper.ps1",
    "stdout.log",
    "stderr.log",
];

pub const WRAPPER: &str = r#"param([string]$JobDir)
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$utf8=New-Object Text.UTF8Encoding($false)
[Console]::OutputEncoding=$utf8; $OutputEncoding=$utf8
try {
 $meta=[IO.File]::ReadAllText((Join-Path $JobDir 'meta.json'),[Text.Encoding]::UTF8) | ConvertFrom-Json
 Set-Location -LiteralPath $meta.cwd
 $global:LASTEXITCODE=0
 . ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $JobDir 'user.ps1'),[Text.Encoding]::UTF8)))
 exit $LASTEXITCODE
} catch { [Console]::Error.WriteLine($_.ToString()); exit 1 }
"#;

fn now() -> String {
    fmt_o(SystemTime::now())
}

fn err(kind: &str) -> TextError {
    TextError::new(kind)
}

fn io(e: std::io::Error) -> TextError {
    TextError::msg(super::fs::io_kind(&e), e.to_string())
}

pub fn valid_job_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}

// ------------------------------------------------------------ json files

/// Read a JSON file written by [`write_json`] from another process.
///
/// On Windows the copy-overwrite writer can hold the file exclusively or
/// leave it truncated for a few milliseconds; a reader that gave up at once
/// made the supervisor fail with `meta.json unreadable` (state `unknown`,
/// the flaky `jobs_survive_server_exit_and_cancel_kills_tree`, D32). So a
/// missing file returns `None` immediately, while sharing violations and
/// half-written content are retried for ~200 ms.
pub fn read_json(p: &Path) -> Option<Value> {
    for attempt in 0..14 {
        match std::fs::read(p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => {}
            Ok(b) => {
                let s = b.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&b);
                if let Ok(v) = serde_json::from_slice(s) {
                    return Some(v);
                }
            }
        }
        if attempt < 13 {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
    None
}

fn tmp_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    PathBuf::from(s)
}

/// `Job-WriteJson`: temp file then copy-overwrite, retried while a reader
/// holds the destination (sharing violation on Windows, D32).
pub fn write_json(p: &Path, v: &Value) -> std::io::Result<()> {
    let tmp = tmp_of(p);
    std::fs::write(&tmp, v.to_string())?;
    let mut r = Ok(());
    for attempt in 0..25 {
        r = std::fs::copy(&tmp, p).map(|_| ());
        if r.is_ok() {
            break;
        }
        if attempt < 24 {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
    let _ = std::fs::remove_file(&tmp);
    r
}

/// `Job-WriteJsonIfAbsent`: first writer wins (hard link fails if present).
pub fn write_json_if_absent(p: &Path, v: &Value) -> bool {
    let tmp = tmp_of(p);
    if std::fs::write(&tmp, v.to_string()).is_err() {
        return false;
    }
    let ok = std::fs::hard_link(&tmp, p).is_ok();
    let _ = std::fs::remove_file(&tmp);
    ok
}

#[derive(Default, Debug)]
pub struct JobFiles {
    pub meta: Option<Value>,
    pub launch: Option<Value>,
    pub result: Option<Value>,
    pub cancel: Option<Value>,
}

pub fn job_read(dir: &Path) -> JobFiles {
    JobFiles {
        meta: read_json(&dir.join("meta.json")),
        launch: read_json(&dir.join("launch.json")),
        result: read_json(&dir.join("result.json")),
        cancel: read_json(&dir.join("cancel.json")),
    }
}

fn get<'a>(v: &'a Option<Value>, k: &str) -> &'a Value {
    v.as_ref().and_then(|o| o.get(k)).unwrap_or(&Value::Null)
}

fn pid_of(v: &Value) -> u32 {
    v.as_i64().unwrap_or(0).clamp(0, u32::MAX as i64) as u32
}

/// `Job-Alive`: PID exists and its start time matches the recorded one.
pub fn job_alive(pid: u32, expected: &Value) -> bool {
    match platform::proc_start(pid) {
        None => false,
        Some(st) => match expected.as_str() {
            None | Some("") => true,
            Some(_) if st == "unknown" => true,
            Some(e) => st == e,
        },
    }
}

pub struct JobState {
    pub state: String,
    pub supervisor_alive: bool,
    pub worker_alive: bool,
}

pub fn job_state(j: &JobFiles) -> JobState {
    let (mut sup, mut work) = (false, false);
    if j.launch.is_some() {
        sup = job_alive(
            pid_of(get(&j.launch, "supervisor_pid")),
            get(&j.launch, "supervisor_start_utc"),
        );
        work = job_alive(
            pid_of(get(&j.launch, "worker_pid")),
            get(&j.launch, "worker_start_utc"),
        );
    }
    let state = if let Some(r) = &j.result {
        r.get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string()
    } else if j.launch.is_none() {
        let launcher = job_alive(
            pid_of(get(&j.meta, "launcher_supervisor_pid")),
            get(&j.meta, "launcher_supervisor_start_utc"),
        );
        if sup || launcher {
            "starting"
        } else {
            "unknown"
        }
        .into()
    } else if work || sup {
        "running".into()
    } else {
        "unknown".into()
    };
    JobState {
        state,
        supervisor_alive: sup,
        worker_alive: work,
    }
}

pub struct Tail {
    pub bytes: u64,
    pub tail: String,
    pub replaced: bool,
    pub truncated: bool,
}

/// `Job-Tail`: last N bytes, strict UTF-8 else replacement decoding.
pub fn job_tail(path: &Path, n: usize) -> Tail {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return Tail {
            bytes: 0,
            tail: String::new(),
            replaced: false,
            truncated: false,
        };
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let k = (n as u64).min(len);
    let mut buf = vec![0u8; k as usize];
    if k > 0 && f.seek(SeekFrom::Start(len - k)).is_ok() {
        let mut i = 0;
        while i < buf.len() {
            match f.read(&mut buf[i..]) {
                Ok(0) | Err(_) => break,
                Ok(r) => i += r,
            }
        }
        buf.truncate(i);
    }
    let (tail, replaced) = match String::from_utf8(buf) {
        Ok(s) => (s, false),
        Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
    };
    Tail {
        bytes: len,
        tail,
        replaced,
        truncated: len > k,
    }
}

/// `Job-Bootstrap`: -EncodedCommand that loads `<job_dir>\<script>` through
/// ScriptBlock::Create (no -File, so the execution policy does not apply).
pub fn bootstrap(script: &str, job_dir: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let p = b64.encode(json!({"job_dir": job_dir, "script": script}).to_string());
    let src = format!(
        "$ProgressPreference='SilentlyContinue'; $p=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{p}'))|ConvertFrom-Json; & ([ScriptBlock]::Create([IO.File]::ReadAllText((Join-Path $p.job_dir $p.script),[Text.Encoding]::UTF8))) -JobDir $p.job_dir"
    );
    b64.encode(
        src.encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<u8>>(),
    )
}

fn sorted_dirs(root: &Path) -> Vec<(PathBuf, String, SystemTime)> {
    let mut out: Vec<(PathBuf, String, SystemTime)> = std::fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| {
                    let m = e.metadata().ok();
                    let t = m
                        .and_then(|m| m.created().or_else(|_| m.modified()).ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    (e.path(), e.file_name().to_string_lossy().into_owned(), t)
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|a| std::cmp::Reverse(a.2));
    out
}

// ------------------------------------------------------------ operations

pub struct StartArgs {
    pub script: String,
    pub cwd: Option<String>,
    pub timeout_seconds: i64,
    pub max_output_bytes: i64,
    pub label: Option<String>,
}

pub fn prepare_start(a: &Value) -> Result<StartArgs, ToolError> {
    let cwd = match opt_str(a, "cwd") {
        Some(c) => {
            let c = paths::valid_path(&c).map_err(ToolError::invalid)?;
            if !paths::is_local_drive(&c) {
                return Err(ToolError::kind(
                    "unsupported_path",
                    "Jobs require a local drive path",
                ));
            }
            Some(c)
        }
        None => None,
    };
    let script = opt_str(a, "script").unwrap_or_default();
    if script.contains('\0') {
        return Err(ToolError::kind(
            "invalid_arguments",
            "script must not contain NUL",
        ));
    }
    Ok(StartArgs {
        script,
        cwd,
        timeout_seconds: opt_i64(a, "timeout_seconds").unwrap_or(3600),
        max_output_bytes: opt_i64(a, "max_output_bytes").unwrap_or(16_777_216),
        label: opt_str(a, "label"),
    })
}

fn home() -> String {
    std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default()
}

/// The `job_start` operation; `files` are package-authored data files.
pub fn start(
    args: &StartArgs,
    kind: &str,
    files: &[(String, String)],
    job_id: Option<String>,
) -> Result<Value, TextError> {
    let job_id = job_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if !valid_job_id(&job_id) {
        return Err(err("invalid_job_id"));
    }
    let root = crate::state::jobs_dir();
    std::fs::create_dir_all(&root).map_err(io)?;
    let running = sorted_dirs(&root)
        .into_iter()
        .take(50)
        .filter(|(d, ..)| !d.join("result.json").exists())
        .filter(|(d, ..)| {
            matches!(
                job_state(&job_read(d)).state.as_str(),
                "running" | "starting"
            )
        })
        .count();
    if running >= MAX_RUNNING {
        return Err(err("too_many_running_jobs"));
    }
    let cwd = args
        .cwd
        .clone()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(home);
    if !paths::is_local_drive(&cwd) {
        return Err(err("unsupported_path"));
    }
    if !Path::new(&cwd).is_dir() {
        return Err(err("cwd_not_found"));
    }
    let dir = root.join(&job_id);
    if dir.exists() {
        return Err(err("already_exists"));
    }
    for (name, _) in files {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
            && !RESERVED.contains(&name.as_str());
        if !ok {
            return Err(err("invalid_file_name"));
        }
    }
    std::fs::create_dir_all(&dir).map_err(io)?;
    let bom = |s: &str| [b"\xef\xbb\xbf".as_slice(), s.as_bytes()].concat();
    std::fs::write(dir.join("user.ps1"), bom(&args.script)).map_err(io)?;
    std::fs::write(dir.join("wrapper.ps1"), bom(WRAPPER)).map_err(io)?;
    for (name, content) in files {
        std::fs::write(dir.join(name), bom(content)).map_err(io)?;
    }
    let mut meta = json!({
        "job_id": job_id, "label": args.label, "kind": kind, "cwd": cwd, "timeout_seconds": args.timeout_seconds,
        "max_output_bytes": args.max_output_bytes, "script_sha256": digest(args.script.as_bytes()),
        "script_chars": args.script.encode_utf16().count(),
        "created_at_utc": now(), "launcher_pid": std::process::id(), "launcher_supervisor_pid": null,
        "launcher_supervisor_start_utc": null, "launch_method": null, "server_version": crate::SERVER_VERSION,
    });
    write_json(&dir.join("meta.json"), &meta).map_err(io)?;
    let exe = std::env::current_exe().map_err(io)?;
    let dir_s = dir.to_string_lossy().into_owned();
    let (sup_pid, method) = platform::spawn_detached(
        &exe.to_string_lossy(),
        &[SUPERVISOR_ARG.into(), dir_s.clone()],
        &cwd,
    )
    .map_err(|e| TextError::msg("launch_failed", e.to_string()).changed_flag(true))?;
    meta["launcher_supervisor_pid"] = json!(sup_pid);
    meta["launcher_supervisor_start_utc"] = json!(platform::proc_start(sup_pid));
    meta["launch_method"] = json!(method.as_str());
    write_json(&dir.join("meta.json"), &meta).map_err(io)?;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut launch = None;
    while Instant::now() < deadline {
        if let Some(l) = read_json(&dir.join("launch.json")) {
            launch = Some(l);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let st = job_state(&job_read(&dir));
    Ok(json!({
        "job_id": job_id, "job_dir": dir_s, "state": st.state, "cwd": cwd, "created_at_utc": meta["created_at_utc"],
        "supervisor_pid": sup_pid, "supervisor_start_utc": meta["launcher_supervisor_start_utc"],
        "worker_pid": launch.as_ref().map(|l| l["worker_pid"].clone()).unwrap_or(Value::Null),
        "worker_start_utc": launch.as_ref().map(|l| l["worker_start_utc"].clone()).unwrap_or(Value::Null),
        "launch_confirmed": launch.is_some(),
    }))
}

fn job_dir(id: &str) -> Result<PathBuf, TextError> {
    if !valid_job_id(id) {
        return Err(err("invalid_job_id"));
    }
    let d = crate::state::jobs_dir().join(id);
    if !d.is_dir() {
        return Err(err("not_found"));
    }
    Ok(d)
}

pub fn status(id: &str, tail_bytes: usize) -> Result<Value, TextError> {
    let dir = job_dir(id)?;
    let j = job_read(&dir);
    let st = job_state(&j);
    let out = job_tail(&dir.join("stdout.log"), tail_bytes);
    let errt = job_tail(&dir.join("stderr.log"), tail_bytes);
    let int = |v: &Value| v.as_i64().map_or(Value::Null, |n| json!(n));
    let message = get(&j.result, "message")
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    Ok(json!({
        "job_id": id, "job_dir": dir.to_string_lossy(), "label": get(&j.meta, "label"), "kind": get(&j.meta, "kind"),
        "state": st.state, "supervisor_alive": st.supervisor_alive, "worker_alive": st.worker_alive,
        "supervisor_pid": int(get(&j.launch, "supervisor_pid")), "worker_pid": int(get(&j.launch, "worker_pid")),
        "worker_start_utc": get(&j.launch, "worker_start_utc"), "created_at_utc": get(&j.meta, "created_at_utc"),
        "started_at_utc": get(&j.launch, "started_at_utc"), "finished_at_utc": get(&j.result, "finished_at_utc"),
        "duration_ms": int(get(&j.result, "duration_ms")), "exit_code": int(get(&j.result, "exit_code")),
        "reason": get(&j.result, "reason"), "finished_by": get(&j.result, "finished_by"),
        "kill_verified": get(&j.result, "kill_verified").as_bool().map_or(Value::Null, Value::Bool),
        "cancel_requested": j.cancel.is_some(), "timeout_seconds": int(get(&j.meta, "timeout_seconds")),
        "stdout_bytes": out.bytes, "stdout_tail": out.tail, "stdout_tail_truncated": out.truncated,
        "stderr_bytes": errt.bytes, "stderr_tail": errt.tail, "stderr_tail_truncated": errt.truncated,
        "tail_decoding_replaced": out.replaced || errt.replaced, "message": message,
    }))
}

fn pids(v: &Value) -> Vec<i64> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

pub fn cancel(id: &str, wait_seconds: u64) -> Result<Value, TextError> {
    let dir = job_dir(id)?;
    let j = job_read(&dir);
    if let Some(r) = &j.result {
        return Ok(json!({
            "job_id": id, "state": r.get("state").and_then(Value::as_str).unwrap_or("unknown"), "action": "none",
            "already_finished": true, "kill_verified": r.get("kill_verified").and_then(Value::as_bool),
            "finished_by": r.get("finished_by").cloned().unwrap_or(Value::Null),
            "killed_pids": pids(r.get("killed_pids").unwrap_or(&Value::Null)),
            "descendants_killed": r.get("descendants_killed").and_then(Value::as_i64).unwrap_or(0),
        }));
    }
    if j.cancel.is_none() {
        write_json(
            &dir.join("cancel.json"),
            &json!({"requested_at_utc": now(), "requested_by": "windows_job_cancel"}),
        )
        .map_err(io)?;
    }
    let st = job_state(&j);
    let mut action = "supervisor";
    let mut fin = None;
    if st.supervisor_alive {
        let deadline = Instant::now() + Duration::from_secs(wait_seconds);
        while Instant::now() < deadline {
            if let Some(r) = read_json(&dir.join("result.json")) {
                fin = Some(r);
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    let fin = match fin {
        Some(f) => f,
        None => {
            action = "direct";
            let mut kill = platform::KillReport {
                kill_verified: true,
                ..Default::default()
            };
            let (wp, ws) = (
                pid_of(get(&j.launch, "worker_pid")),
                get(&j.launch, "worker_start_utc"),
            );
            if j.launch.is_some() && job_alive(wp, ws) {
                kill = platform::kill_tree(wp, ws.as_str());
            }
            let (sp, ss) = (
                pid_of(get(&j.launch, "supervisor_pid")),
                get(&j.launch, "supervisor_start_utc"),
            );
            if j.launch.is_some() && job_alive(sp, ss) {
                let _ = platform::kill_tree(sp, ss.as_str());
            }
            let mine = json!({
                "job_id": id, "state": "cancelled", "exit_code": null, "reason": "cancel_requested",
                "finished_at_utc": now(), "duration_ms": null, "finished_by": "windows_job_cancel",
                "kill_verified": kill.kill_verified, "killed_pids": kill.killed_pids, "descendants_killed": kill.descendants,
            });
            write_json_if_absent(&dir.join("result.json"), &mine);
            read_json(&dir.join("result.json")).unwrap_or(mine)
        }
    };
    Ok(json!({
        "job_id": id, "state": fin.get("state").and_then(Value::as_str).unwrap_or("unknown"), "action": action,
        "already_finished": false, "kill_verified": fin.get("kill_verified").and_then(Value::as_bool),
        "finished_by": fin.get("finished_by").cloned().unwrap_or(Value::Null),
        "killed_pids": pids(fin.get("killed_pids").unwrap_or(&Value::Null)),
        "descendants_killed": fin.get("descendants_killed").and_then(Value::as_i64).unwrap_or(0),
    }))
}

pub fn list(limit: usize, state: &str, prune_hours: Option<i64>) -> Result<Value, TextError> {
    let root = crate::state::jobs_dir();
    let (mut jobs, mut pruned, mut total) = (Vec::new(), 0usize, 0usize);
    if root.is_dir() {
        let dirs: Vec<_> = sorted_dirs(&root)
            .into_iter()
            .filter(|(_, n, _)| {
                n.len() == 36
                    && n.bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || c == b'-')
            })
            .collect();
        total = dirs.len();
        let cut = prune_hours.map(|h| chrono::Utc::now() - chrono::Duration::hours(h));
        for (d, name, _) in dirs {
            let j = job_read(&d);
            let st = job_state(&j);
            let finished = j.result.is_some();
            if let (Some(cut), true) = (cut, finished) {
                let fin = get(&j.result, "finished_at_utc")
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
                if fin.is_some_and(|f| f < cut) && std::fs::remove_dir_all(&d).is_ok() {
                    pruned += 1;
                    total -= 1;
                    continue;
                }
            }
            if state == "running" && !matches!(st.state.as_str(), "running" | "starting") {
                continue;
            }
            if state == "finished" && !finished {
                continue;
            }
            if jobs.len() < limit {
                jobs.push(json!({
                    "job_id": name, "label": get(&j.meta, "label"), "kind": get(&j.meta, "kind"), "state": st.state,
                    "created_at_utc": get(&j.meta, "created_at_utc"), "finished_at_utc": get(&j.result, "finished_at_utc"),
                    "exit_code": get(&j.result, "exit_code").as_i64(), "worker_alive": st.worker_alive,
                }));
            }
        }
    }
    Ok(
        json!({"jobs_root": root.to_string_lossy(), "count": jobs.len(), "jobs": jobs, "total": total, "pruned": pruned}),
    )
}

// ------------------------------------------------------------ supervisor

fn worker_command(
    dir: &Path,
    kind: &str,
    cwd: &str,
    out: std::fs::File,
    errf: std::fs::File,
) -> std::process::Command {
    if kind == crate::handsfree::session::JOB_KIND {
        // The hands-free session is this same executable (D25): its LL hooks
        // and overlay die with it, and the supervisor's job object kills it
        // if the supervisor dies, so input is always given back (fail-open).
        let exe = std::env::current_exe().unwrap_or_else(|_| "windows-sosa-mcp".into());
        let mut c = std::process::Command::new(exe);
        c.arg(crate::handsfree::session::SESSION_ARG).arg(dir);
        c.current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(out)
            .stderr(errf);
        return c;
    }
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new(super::shell::powershell_exe());
        c.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-OutputFormat",
            "Text",
            "-EncodedCommand",
        ])
        .arg(bootstrap("wrapper.ps1", &dir.to_string_lossy()));
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        // Test portability only: the user script runs under sh.
        let mut c = std::process::Command::new("sh");
        c.arg(dir.join("user.ps1"));
        c
    };
    cmd.current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(errf);
    cmd
}

/// Entry point of `windows-sosa-mcp __job-supervisor <job_dir>`.
pub fn supervise(dir: &Path) -> i32 {
    let mut meta_job_id = Value::Null;
    let run = || -> Result<(), String> {
        let meta = read_json(&dir.join("meta.json")).ok_or("meta.json unreadable")?;
        let job_id = meta["job_id"].clone();
        let cwd = meta["cwd"].as_str().unwrap_or(".").to_string();
        let (out_p, err_p) = (dir.join("stdout.log"), dir.join("stderr.log"));
        let out = std::fs::File::create(&out_p).map_err(|e| e.to_string())?;
        let errf = std::fs::File::create(&err_p).map_err(|e| e.to_string())?;
        let started = SystemTime::now();
        let t0 = Instant::now();
        let mut c = platform::spawn_contained(worker_command(
            dir,
            meta["kind"].as_str().unwrap_or(""),
            &cwd,
            out,
            errf,
        ))
        .map_err(|e| e.to_string())?;
        let wpid = c.child.id();
        let wstart = platform::proc_start(wpid);
        let me = std::process::id();
        write_json(
            &dir.join("launch.json"),
            &json!({
                "job_id": job_id, "supervisor_pid": me, "supervisor_start_utc": platform::proc_start(me),
                "worker_pid": wpid, "worker_start_utc": wstart, "started_at_utc": fmt_o(started),
            }),
        )
        .map_err(|e| e.to_string())?;
        let limit = meta["max_output_bytes"].as_i64().unwrap_or(16_777_216) as u64;
        let timeout = Duration::from_secs(meta["timeout_seconds"].as_u64().unwrap_or(3600));
        let mut status = None;
        let (state, reason): (&str, Value) = loop {
            if let Ok(Some(s)) = c.child.try_wait() {
                status = Some(s);
                break (
                    if platform::exit_code(s) == Some(0) {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    Value::Null,
                );
            }
            if dir.join("cancel.json").exists() {
                break ("cancelled", json!("cancel_requested"));
            }
            if t0.elapsed() >= timeout {
                break ("timed_out", json!("timeout"));
            }
            let size: u64 = [&out_p, &err_p]
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .sum();
            if size > limit {
                break ("output_limit", json!("output_limit"));
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        let kill = (status.is_none()).then(|| c.tree.kill_report(&mut c.child));
        if status.is_none() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(Some(s)) = c.child.try_wait() {
                    status = Some(s);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let result = json!({
            "job_id": job_id, "state": state, "exit_code": status.and_then(platform::exit_code), "reason": reason,
            "finished_at_utc": now(), "duration_ms": t0.elapsed().as_millis() as u64, "finished_by": "supervisor",
            "kill_verified": kill.as_ref().map(|k| k.kill_verified),
            "killed_pids": kill.as_ref().map(|k| k.killed_pids.clone()).unwrap_or_default(),
            "descendants_killed": kill.as_ref().map_or(0, |k| k.descendants),
        });
        write_json_if_absent(&dir.join("result.json"), &result);
        Ok(())
    };
    if let Some(m) = read_json(&dir.join("meta.json")) {
        meta_job_id = m["job_id"].clone();
    }
    match run() {
        Ok(()) => 0,
        Err(message) => {
            let result = json!({
                "job_id": meta_job_id, "state": "unknown", "exit_code": null, "reason": "supervisor_error",
                "message": message, "finished_at_utc": now(), "finished_by": "supervisor", "kill_verified": null,
            });
            write_json_if_absent(&dir.join("result.json"), &result);
            1
        }
    }
}

// ------------------------------------------------------------ tools

pub fn execute(name: &str, a: &Value) -> Envelope {
    let t0 = Instant::now();
    let mut r = envelope::base(name);
    let mut validated = false;
    let out: Result<Value, TextError> = (|| match name {
        "windows_job_start" => {
            let args = prepare_start(a).map_err(|e| TextError::msg(&e.kind, e.message))?;
            validated = true;
            start(&args, "powershell", &[], None)
        }
        "windows_job_status" => {
            validated = true;
            status(
                &opt_str(a, "job_id").unwrap_or_default(),
                opt_i64(a, "tail_bytes").unwrap_or(4096) as usize,
            )
        }
        "windows_job_cancel" => {
            validated = true;
            cancel(
                &opt_str(a, "job_id").unwrap_or_default(),
                opt_i64(a, "wait_seconds").unwrap_or(10) as u64,
            )
        }
        "windows_job_list" => {
            validated = true;
            list(
                opt_i64(a, "limit").unwrap_or(20) as usize,
                &opt_str(a, "state").unwrap_or_else(|| "all".into()),
                opt_i64(a, "prune_finished_older_than_hours"),
            )
        }
        _ => Err(err("unknown_tool")),
    })();
    match out {
        Ok(data) => {
            r.insert("exit_code".into(), json!(0));
            r.insert(
                "job_id".into(),
                data.get("job_id").cloned().unwrap_or(Value::Null),
            );
            envelope::succeed(&mut r, data);
        }
        Err(e) => {
            if e.kind == "timeout" {
                r.insert("timed_out".into(), json!(true));
                r.insert("windows_termination_confirmed".into(), json!(false));
            }
            envelope::fail(&mut r, &e.kind, &e.message);
            if e.kind == "timeout" {
                r.insert("status".into(), json!("timed_out"));
            }
            let mutating = validated && matches!(name, "windows_job_start" | "windows_job_cancel");
            r.insert(
                "may_have_changed".into(),
                json!(e.may_have_changed || mutating),
            );
        }
    }
    if !r.contains_key("job_id")
        && let Some(id) = opt_str(a, "job_id")
    {
        r.insert("job_id".into(), json!(id));
    }
    envelope::stamp(&mut r, t0);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_id_rule() {
        assert!(valid_job_id("0f8fad5b-d9cb-469f-a165-70867728950e"));
        assert!(!valid_job_id("0F8FAD5B-d9cb-469f-a165-70867728950e"));
        assert!(!valid_job_id("../x"));
    }

    #[test]
    fn json_if_absent_first_wins() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("result.json");
        assert!(write_json_if_absent(&p, &json!({"a": 1})));
        assert!(!write_json_if_absent(&p, &json!({"a": 2})));
        assert_eq!(read_json(&p).unwrap()["a"], 1);
        write_json(&p, &json!({"a": 3})).unwrap();
        assert_eq!(read_json(&p).unwrap()["a"], 3);
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn tail_and_state() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("stdout.log"), "hello world").unwrap();
        let t = job_tail(&d.path().join("stdout.log"), 5);
        assert_eq!((t.bytes, t.tail.as_str(), t.truncated), (11, "world", true));
        let t = job_tail(&d.path().join("missing.log"), 5);
        assert_eq!(t.bytes, 0);
        std::fs::write(d.path().join("x.log"), b"\xe2\x82\xac").unwrap();
        assert!(job_tail(&d.path().join("x.log"), 2).replaced);
        // no launch, dead launcher -> unknown; result wins
        write_json(
            &d.path().join("meta.json"),
            &json!({"launcher_supervisor_pid": 0}),
        )
        .unwrap();
        assert_eq!(job_state(&job_read(d.path())).state, "unknown");
        let me = std::process::id();
        write_json(&d.path().join("launch.json"), &json!({"supervisor_pid": me, "supervisor_start_utc": platform::proc_start(me), "worker_pid": 0})).unwrap();
        let st = job_state(&job_read(d.path()));
        assert_eq!((st.state.as_str(), st.supervisor_alive), ("running", true));
        write_json(&d.path().join("launch.json"), &json!({"supervisor_pid": me, "supervisor_start_utc": "1999-01-01T00:00:00.0000000Z", "worker_pid": 0})).unwrap();
        assert_eq!(job_state(&job_read(d.path())).state, "unknown");
        write_json(
            &d.path().join("result.json"),
            &json!({"state": "succeeded"}),
        )
        .unwrap();
        assert_eq!(job_state(&job_read(d.path())).state, "succeeded");
    }

    #[test]
    fn bootstrap_is_utf16_base64() {
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD
            .decode(bootstrap("wrapper.ps1", "C:\\j"))
            .unwrap();
        let s = String::from_utf16(
            &b.chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(s.contains("-JobDir $p.job_dir"));
    }
}

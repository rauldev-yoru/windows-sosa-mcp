//! Win32 process control: job objects for tree kills, process identity
//! (PID + creation time), and detached launch that escapes enclosing jobs.

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, GetLastError, HANDLE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectBasicProcessIdList, QueryInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
};

use super::{KillReport, LaunchMethod};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const STILL_ACTIVE: u32 = 259;
const ERROR_ACCESS_DENIED: u32 = 5;

/// Owned kernel handle.
pub struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: handle owned by this wrapper, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

// SAFETY: kernel handles are process-wide and usable from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

pub fn no_window(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// No-op on Windows (tree membership comes from the job object).
pub fn prepare_tree(_cmd: &mut Command) {}

fn create_job() -> Option<Handle> {
    // SAFETY: anonymous job object with default security.
    let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    (!h.is_null()).then_some(Handle(h))
}

fn job_pids(job: &Handle) -> Vec<u32> {
    let header = 8 / std::mem::size_of::<usize>();
    let mut buf = vec![0usize; header + 1024];
    let size = (buf.len() * std::mem::size_of::<usize>()) as u32;
    // SAFETY: buffer is large enough for the header plus 1024 ids.
    let ok = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr().cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Vec::new();
    }
    // SAFETY: first u32 pair is the header written by the kernel.
    let n = unsafe { *(buf.as_ptr().cast::<u32>().add(1)) } as usize;
    buf[header..header + n.min(1024)]
        .iter()
        .map(|&p| p as u32)
        .collect()
}

fn job_active(job: &Handle) -> u32 {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: struct sized for this information class.
    let ok = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        u32::MAX
    } else {
        info.ActiveProcesses
    }
}

/// A process tree contained in a job object (best effort: children spawned
/// before assignment, within the first milliseconds, are not contained).
pub struct KillTree {
    job: Option<Handle>,
    root: u32,
}

impl KillTree {
    pub fn attach(child: &Child) -> Self {
        let job = create_job().filter(|j| {
            // SAFETY: valid job and process handles.
            unsafe { AssignProcessToJobObject(j.0, child.as_raw_handle() as HANDLE) != 0 }
        });
        Self {
            job,
            root: child.id(),
        }
    }

    /// Terminate the whole tree; true when no process of it remains.
    pub fn kill(&self, child: &mut Child) -> bool {
        self.kill_report(child).kill_verified
    }

    pub fn kill_report(&self, child: &mut Child) -> KillReport {
        match &self.job {
            Some(job) => {
                let pids = job_pids(job);
                // SAFETY: valid job handle.
                unsafe { TerminateJobObject(job.0, 1) };
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut verified = false;
                while Instant::now() < deadline {
                    if job_active(job) == 0 {
                        verified = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = child.try_wait();
                let descendants = pids.iter().filter(|p| **p != self.root).count();
                KillReport {
                    kill_verified: verified,
                    killed_pids: pids,
                    descendants,
                }
            }
            None => {
                let start = proc_start(self.root);
                let r = kill_tree(self.root, start.as_deref());
                let _ = child.try_wait();
                r
            }
        }
    }
}

fn filetime_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

fn open(pid: u32, access: u32) -> Result<Handle, u32> {
    // SAFETY: plain OpenProcess; null on failure.
    let h = unsafe { OpenProcess(access, 0, pid) };
    if h.is_null() {
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(h))
    }
}

fn creation_100ns(h: &Handle) -> Option<u64> {
    let z = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    // SAFETY: valid process handle and out pointers.
    (unsafe { GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u) } != 0).then(|| filetime_u64(c))
}

fn alive(h: &Handle) -> bool {
    let mut code = 0u32;
    // SAFETY: valid process handle.
    unsafe { GetExitCodeProcess(h.0, &mut code) != 0 && code == STILL_ACTIVE }
}

fn fmt_filetime(ft: u64) -> String {
    let unix = ft.saturating_sub(116_444_736_000_000_000);
    crate::paths::fmt_o(SystemTime::UNIX_EPOCH + Duration::from_nanos(unix * 100))
}

/// `Job-ProcStart`: creation time ('o' format, UTC) of a live process,
/// `"unknown"` when it exists but cannot be queried, `None` when gone.
pub fn proc_start(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    match open(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
        Ok(h) if alive(&h) => {
            Some(creation_100ns(&h).map_or_else(|| "unknown".into(), fmt_filetime))
        }
        Ok(_) => None,
        Err(ERROR_ACCESS_DENIED) => Some("unknown".into()),
        Err(_) => None,
    }
}

fn snapshot() -> Vec<(u32, u32)> {
    // SAFETY: toolhelp snapshot iteration with a correctly sized entry.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap as isize == -1 {
            return Vec::new();
        }
        let snap = Handle(snap);
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut out = Vec::new();
        let mut ok = Process32FirstW(snap.0, &mut e);
        while ok != 0 {
            out.push((e.th32ProcessID, e.th32ParentProcessID));
            ok = Process32NextW(snap.0, &mut e);
        }
        out
    }
}

/// PID -> `Get-Process` `ProcessName` (image name without `.exe`) of every
/// running process, from one toolhelp snapshot. PID 0 is `Idle`, as there.
pub fn process_names() -> std::collections::HashMap<u32, String> {
    let mut out = std::collections::HashMap::new();
    // SAFETY: toolhelp snapshot iteration with a correctly sized entry.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap as isize == -1 {
            return out;
        }
        let snap = Handle(snap);
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap.0, &mut e);
        while ok != 0 {
            let len = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            let mut name = String::from_utf16_lossy(&e.szExeFile[..len]);
            if name.len() > 4 && name.to_ascii_lowercase().ends_with(".exe") {
                name.truncate(name.len() - 4);
            }
            out.insert(e.th32ProcessID, name);
            ok = Process32NextW(snap.0, &mut e);
        }
    }
    out.insert(0, "Idle".into());
    out
}

/// `Job-KillTree`: descendants (created no earlier than root start - 2 s,
/// to avoid PID-reuse victims) deepest-first, then the root; verified 5 s.
pub fn kill_tree(root: u32, root_start: Option<&str>) -> KillReport {
    let not_before = root_start
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| {
            (d.timestamp_nanos_opt().unwrap_or(0) / 100) as u64 + 116_444_736_000_000_000
                - 20_000_000
        })
        .unwrap_or(0);
    let all = snapshot();
    let mut kids = Vec::new();
    let mut queue = vec![root];
    while let Some(cur) = queue.pop() {
        for &(pid, parent) in &all {
            if parent == cur && pid != root && !kids.contains(&pid) {
                let created = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)
                    .ok()
                    .and_then(|h| creation_100ns(&h));
                if created.is_some_and(|c| c < not_before) {
                    continue;
                }
                kids.push(pid);
                queue.push(pid);
            }
        }
    }
    let mut targets: Vec<u32> = kids.clone();
    targets.sort_unstable_by(|a, b| b.cmp(a));
    targets.push(root);
    targets.dedup();
    for &pid in &targets {
        if let Ok(h) = open(pid, PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION) {
            // SAFETY: valid handle with terminate access.
            unsafe { TerminateProcess(h.0, 1) };
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut verified = false;
    while Instant::now() < deadline {
        if targets.iter().all(|p| proc_start(*p).is_none()) {
            verified = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    KillReport {
        kill_verified: verified,
        killed_pids: targets,
        descendants: kids.len(),
    }
}

pub fn in_job() -> bool {
    let mut r = 0;
    // SAFETY: pseudo handle for the current process; null job = any job.
    unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut r) != 0 && r != 0 }
}

fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    format!("\"{}\"", arg.replace('"', "\\\""))
}

/// Start `exe args` detached from this process tree so it survives the MCP
/// server and any job object that encloses it (enishi / Task Scheduler).
/// Order: not in a job -> plain; else CREATE_BREAKAWAY_FROM_JOB; if denied
/// -> WMI `Win32_Process.Create` (created by WmiPrvSE, outside our jobs).
pub fn spawn_detached(
    exe: &str,
    args: &[String],
    cwd: &str,
) -> std::io::Result<(u32, LaunchMethod)> {
    let base = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    let run = |flags: u32| {
        Command::new(exe)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
            .map(|c| c.id())
    };
    if std::env::var_os("WINDOWS_SOSA_MCP_FORCE_WMI").is_none() {
        if !in_job() {
            return run(base).map(|p| (p, LaunchMethod::Direct));
        }
        if let Ok(pid) = run(base | CREATE_BREAKAWAY_FROM_JOB) {
            return Ok((pid, LaunchMethod::Breakaway));
        }
    }
    let cmdline = std::iter::once(exe.to_string())
        .chain(args.iter().cloned())
        .map(|a| quote(&a))
        .collect::<Vec<_>>()
        .join(" ");
    let payload = serde_json::json!({"cmd": cmdline, "cwd": cwd}).to_string();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
    let script = format!(
        "$ErrorActionPreference='Stop'; $p=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{b64}'))|ConvertFrom-Json; \
         $si=New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{{ShowWindow=[uint16]0}}; \
         $r=Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine=$p.cmd; CurrentDirectory=$p.cwd; ProcessStartupInformation=$si}}; \
         [Console]::Out.Write(('{{0}} {{1}}' -f $r.ReturnValue,$r.ProcessId))"
    );
    let enc: String = {
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        base64::engine::general_purpose::STANDARD.encode(utf16)
    };
    let out = Command::new(crate::tools::shell::powershell_exe())
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &enc,
        ])
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    match (
        it.next().and_then(|s| s.parse::<u32>().ok()),
        it.next().and_then(|s| s.parse::<u32>().ok()),
    ) {
        (Some(0), Some(pid)) => Ok((pid, LaunchMethod::Wmi)),
        _ => Err(std::io::Error::other(format!(
            "Win32_Process.Create failed: {} {}",
            text.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

/// A worker launched by the job supervisor, contained in a job object.
pub struct Contained {
    pub child: Child,
    pub tree: KillTree,
}

pub fn spawn_contained(mut cmd: Command) -> std::io::Result<Contained> {
    no_window(&mut cmd);
    let child = cmd.spawn()?;
    let tree = KillTree::attach(&child);
    Ok(Contained { child, tree })
}

/// 32-bit exit code (`GetExitCodeProcess` reinterpreted as signed, like
/// .NET `Process.ExitCode`).
pub fn exit_code(status: std::process::ExitStatus) -> Option<i64> {
    status.code().map(i64::from)
}

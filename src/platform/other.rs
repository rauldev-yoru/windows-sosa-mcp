use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{HostInfo, KillReport, LaunchMethod, path_string};

/// Portable subset; Windows-only facts stay `None`.
pub fn host_info() -> HostInfo {
    HostInfo {
        computer: std::env::var("HOSTNAME").ok().or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        }),
        user: std::env::var("USER").ok(),
        user_profile: std::env::var("HOME").ok(),
        temp_directory: Some(path_string(std::env::temp_dir())),
        cwd: std::env::current_dir().ok().map(path_string),
        ..HostInfo::default()
    }
}

/// No Windows registry: user/machine PATH scopes are empty.
pub fn registry_paths() -> (Option<String>, Option<String>) {
    (None, None)
}

pub fn no_window(_cmd: &mut Command) {}

/// Put the child in its own process group so the tree can be signalled.
pub fn prepare_tree(cmd: &mut Command) {
    cmd.process_group(0);
}

fn kill_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status();
}

pub struct KillTree {
    root: u32,
}

impl KillTree {
    pub fn attach(child: &Child) -> Self {
        Self { root: child.id() }
    }

    pub fn kill(&self, child: &mut Child) -> bool {
        self.kill_report(child).kill_verified
    }

    pub fn kill_report(&self, child: &mut Child) -> KillReport {
        kill_group(self.root);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut verified = false;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                verified = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        KillReport {
            kill_verified: verified,
            killed_pids: vec![self.root],
            descendants: 0,
        }
    }
}

/// Process identity token: start time in clock ticks since boot
/// (`/proc/<pid>/stat` field 22). `None` when gone or a zombie.
pub fn proc_start(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.first() == Some(&"Z") || fields.first() == Some(&"X") {
        return None;
    }
    Some(format!("ticks:{}", fields.get(19)?))
}

pub fn kill_tree(root: u32, _root_start: Option<&str>) -> KillReport {
    kill_group(root);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut verified = false;
    while Instant::now() < deadline {
        if proc_start(root).is_none() {
            verified = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    KillReport {
        kill_verified: verified,
        killed_pids: vec![root],
        descendants: 0,
    }
}

pub fn in_job() -> bool {
    false
}

/// Detached (own session-less process group) child, reaped by a thread.
pub fn spawn_detached(
    exe: &str,
    args: &[String],
    cwd: &str,
) -> std::io::Result<(u32, LaunchMethod)> {
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok((pid, LaunchMethod::Direct))
}

pub struct Contained {
    pub child: Child,
    pub tree: KillTree,
}

pub fn spawn_contained(mut cmd: Command) -> std::io::Result<Contained> {
    cmd.process_group(0);
    let child = cmd.spawn()?;
    let tree = KillTree::attach(&child);
    Ok(Contained { child, tree })
}

pub fn exit_code(status: std::process::ExitStatus) -> Option<i64> {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .map(i64::from)
        .or_else(|| status.signal().map(|s| 128 + i64::from(s)))
}

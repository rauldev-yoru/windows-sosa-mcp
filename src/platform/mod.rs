//! Host introspection. Windows uses Win32 APIs; other platforms return the
//! portable subset so the crate builds and tests on Linux CI.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod winproc;
#[cfg(windows)]
pub use windows::{host_info, registry_paths};
#[cfg(windows)]
pub use winproc::*;

#[cfg(not(windows))]
mod other;
#[cfg(not(windows))]
pub use other::*;

/// Outcome of a process-tree kill (`Job-KillTree` equivalent).
#[derive(Debug, Clone, Default)]
pub struct KillReport {
    pub kill_verified: bool,
    pub killed_pids: Vec<u32>,
    pub descendants: usize,
}

/// How a detached job supervisor was launched (see design D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMethod {
    /// Parent is not in a job object: plain detached child.
    Direct,
    /// `CREATE_BREAKAWAY_FROM_JOB` accepted by every enclosing job.
    Breakaway,
    /// Breakaway denied: created by WMI `Win32_Process.Create` (parent WmiPrvSE).
    Wmi,
}

impl LaunchMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Breakaway => "breakaway",
            Self::Wmi => "wmi",
        }
    }
}

/// Facts reported by `windows_health`. `None` = unknown / not applicable.
#[derive(Debug, Default, Clone)]
pub struct HostInfo {
    pub computer: Option<String>,
    pub user: Option<String>,
    pub user_profile: Option<String>,
    pub temp_directory: Option<String>,
    pub cwd: Option<String>,
    pub administrator_in_current_token: Option<bool>,
    pub elevated: Option<bool>,
    pub process_session_id: Option<u32>,
    pub os_version: Option<String>,
    pub os_product_name: Option<String>,
    pub os_display_version: Option<String>,
    pub powershell_version: Option<String>,
    pub windows_powershell: Option<String>,
    pub pwsh: Option<String>,
}

pub(crate) fn path_string(p: std::path::PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

//! Win32 implementation of [`HostInfo`].

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, GetTokenInformation, SECURITY_MAX_SID_SIZE,
    TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation, WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::SystemInformation::{ComputerNameNetBIOS, GetComputerNameExW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{HostInfo, path_string};

const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
const PS_ENGINE: &str = r"SOFTWARE\Microsoft\PowerShell\3\PowerShellEngine";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn computer_name() -> Option<String> {
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: buffer and length describe valid writable memory.
    let ok = unsafe { GetComputerNameExW(ComputerNameNetBIOS, buf.as_mut_ptr(), &mut len) };
    (ok != 0).then(|| from_wide(&buf[..len as usize]))
}

fn user_name() -> Option<String> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: buffer and length describe valid writable memory.
    let ok = unsafe { GetUserNameExW(NameSamCompatible, buf.as_mut_ptr(), &mut len) };
    ok.then(|| from_wide(&buf[..len as usize]))
}

fn reg_string(subkey: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(subkey), wide(value));
    let mut buf = [0u16; 512];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: pointers are valid; RegGetValueW null-terminates REG_SZ data.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast::<c_void>(),
            &mut bytes,
        )
    };
    (rc == 0).then(|| from_wide(&buf))
}

fn reg_dword(subkey: &str, value: &str) -> Option<u32> {
    let (k, v) = (wide(subkey), wide(value));
    let mut data = 0u32;
    let mut bytes = 4u32;
    // SAFETY: pointers are valid and sized for a DWORD.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast::<c_void>(),
            &mut bytes,
        )
    };
    (rc == 0).then_some(data)
}

/// Closes a token handle on drop.
struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: handle came from OpenProcessToken and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn elevated() -> Option<bool> {
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess is a pseudo-handle; out pointer is valid.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } == 0 {
        return None;
    }
    let token = Token(handle);
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret = 0u32;
    // SAFETY: buffer is a TOKEN_ELEVATION of the size we pass.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
    };
    (ok != 0).then_some(elevation.TokenIsElevated != 0)
}

/// Same semantics as PowerShell `WindowsPrincipal.IsInRole(Administrator)`:
/// membership is evaluated on the *current* (possibly filtered UAC) token.
fn administrator_in_current_token() -> Option<bool> {
    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = SECURITY_MAX_SID_SIZE;
    // SAFETY: sid buffer has SECURITY_MAX_SID_SIZE bytes.
    let ok = unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            sid.as_mut_ptr().cast::<c_void>(),
            &mut size,
        )
    };
    if ok == 0 {
        return None;
    }
    let mut member = 0;
    // SAFETY: null token = impersonation/primary token of the caller.
    let ok = unsafe {
        CheckTokenMembership(
            std::ptr::null_mut(),
            sid.as_mut_ptr().cast::<c_void>(),
            &mut member,
        )
    };
    (ok != 0).then_some(member != 0)
}

fn session_id() -> Option<u32> {
    let mut id = 0u32;
    // SAFETY: out pointer is valid.
    let ok = unsafe { ProcessIdToSessionId(std::process::id(), &mut id) };
    (ok != 0).then_some(id)
}

fn os_version() -> Option<String> {
    let major = reg_dword(CURRENT_VERSION, "CurrentMajorVersionNumber")?;
    let minor = reg_dword(CURRENT_VERSION, "CurrentMinorVersionNumber").unwrap_or(0);
    let build = reg_string(CURRENT_VERSION, "CurrentBuildNumber")?;
    Some(match reg_dword(CURRENT_VERSION, "UBR") {
        Some(ubr) => format!("{major}.{minor}.{build}.{ubr}"),
        None => format!("{major}.{minor}.{build}"),
    })
}

/// `ProductName` still says "Windows 10" on Windows 11; builds >= 22000 are 11.
fn product_name() -> Option<String> {
    let name = reg_string(CURRENT_VERSION, "ProductName")?;
    let build: u32 = reg_string(CURRENT_VERSION, "CurrentBuildNumber")?
        .parse()
        .ok()?;
    Some(if build >= 22000 && name.starts_with("Windows 10") {
        name.replacen("Windows 10", "Windows 11", 1)
    } else {
        name
    })
}

fn existing(p: PathBuf) -> Option<String> {
    p.is_file().then(|| path_string(p))
}

fn find_on_path(exe: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| existing(dir.join(exe)))
}

fn windows_powershell() -> Option<String> {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    existing(Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"))
}

fn pwsh() -> Option<String> {
    find_on_path("pwsh.exe").or_else(|| {
        let pf = std::env::var_os("ProgramFiles")?;
        existing(Path::new(&pf).join(r"PowerShell\7\pwsh.exe"))
    })
}

/// Collect every health fact (each one fails soft to `None`).
pub fn host_info() -> HostInfo {
    HostInfo {
        computer: computer_name(),
        user: user_name(),
        user_profile: std::env::var("USERPROFILE").ok(),
        temp_directory: Some(path_string(std::env::temp_dir())),
        cwd: std::env::current_dir().ok().map(path_string),
        administrator_in_current_token: administrator_in_current_token(),
        elevated: elevated(),
        process_session_id: session_id(),
        os_version: os_version(),
        os_product_name: product_name(),
        os_display_version: reg_string(CURRENT_VERSION, "DisplayVersion"),
        powershell_version: reg_string(PS_ENGINE, "PowerShellVersion"),
        windows_powershell: windows_powershell(),
        pwsh: pwsh(),
    }
}

/// REG_SZ / REG_EXPAND_SZ (expanded) of any size.
fn reg_string_any(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(subkey), wide(value));
    let mut bytes = 0u32;
    // SAFETY: size query with a null data pointer.
    let rc = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if rc != 0 {
        return None;
    }
    let mut buf = vec![0u16; bytes as usize / 2 + 2];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buffer is `size` bytes long; RegGetValueW null-terminates.
    let rc = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast::<c_void>(),
            &mut size,
        )
    };
    (rc == 0).then(|| from_wide(&buf))
}

/// User and machine `Path` values from the registry (expanded).
pub fn registry_paths() -> (Option<String>, Option<String>) {
    (
        reg_string_any(HKEY_CURRENT_USER, "Environment", "Path"),
        reg_string_any(
            HKEY_LOCAL_MACHINE,
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
            "Path",
        ),
    )
}

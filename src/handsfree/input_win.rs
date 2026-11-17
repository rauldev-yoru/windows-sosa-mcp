//! `SendInput` implementation of the [`Injector`](super::input::Injector)
//! trait.
//!
//! The server process is Per-Monitor-V2 DPI aware (D20), so cursor
//! positions, `GetSystemMetrics(SM_*VIRTUALSCREEN)` and every coordinate
//! the tools accept are physical virtual-desktop pixels.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use serde_json::Value;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HGLOBAL, HWND, POINT};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenIntegrityLevel,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS,
    KEYBDINPUT, MAP_VIRTUAL_KEY_TYPE, MOUSE_EVENT_FLAGS, MOUSEINPUT, MapVirtualKeyExW, SendInput,
    VIRTUAL_KEY, VkKeyScanExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowTextW,
    GetWindowThreadProcessId, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};
use windows::core::PWSTR;

use super::input::{Ev, Fg, INPUT_MAGIC, Injector, LayoutKey};

const CF_UNICODETEXT: u32 = 13;

/// The real injector. Create one per batch.
pub struct WinInjector {
    sw: Instant,
    /// The active session is a dry run (D29/D33): events are counted, never
    /// sent, and the clipboard is simulated in memory — the desktop is
    /// never touched.
    dry: bool,
    dry_clip: Option<String>,
    /// Dry run: where the counted absolute moves would have left the
    /// cursor, so placement checks pass without moving it (D36).
    dry_cursor: Option<(i32, i32)>,
}

impl WinInjector {
    pub fn new() -> Self {
        let dry = super::state::read_state().is_some_and(|s| s["dry_run"] == true)
            || super::session::dry_run_env();
        Self {
            sw: Instant::now(),
            dry,
            dry_clip: None,
            dry_cursor: None,
        }
    }
}

impl Default for WinInjector {
    fn default() -> Self {
        Self::new()
    }
}

fn hval(h: HWND) -> i64 {
    h.0 as i64
}

fn to_input(e: &Ev) -> INPUT {
    match *e {
        Ev::Mouse {
            dx,
            dy,
            flags,
            data,
        } => INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: MOUSE_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: INPUT_MAGIC,
                },
            },
        },
        Ev::Key { vk, scan, flags } => INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: KEYBD_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: INPUT_MAGIC,
                },
            },
        },
    }
}

/// Process name without `.exe` (like `Get-Process`.ProcessName).
pub fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: handle closed below; buffer length passed in/out.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n);
        let _ = CloseHandle(h);
        r.ok()?;
        let path = String::from_utf16_lossy(&buf[..n as usize]);
        let file = path.rsplit('\\').next().unwrap_or(&path);
        Some(
            file.strip_suffix(".exe")
                .or_else(|| file.strip_suffix(".EXE"))
                .unwrap_or(file)
                .to_string(),
        )
    }
}

/// `IntegrityOf`: integrity RID of the process owning `hwnd`.
pub fn integrity_of(hwnd: i64) -> i64 {
    // SAFETY: handles closed below; token buffer sized by the first call.
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(HWND(hwnd as *mut c_void), Some(&mut pid));
        if pid == 0 {
            return -1;
        }
        let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return -1;
        };
        let mut tok = HANDLE::default();
        let mut out = -1;
        if OpenProcessToken(p, TOKEN_QUERY, &mut tok).is_ok() {
            let mut len = 0u32;
            let _ = GetTokenInformation(tok, TokenIntegrityLevel, None, 0, &mut len);
            if len > 0 {
                let mut buf = vec![0u8; len as usize];
                if GetTokenInformation(
                    tok,
                    TokenIntegrityLevel,
                    Some(buf.as_mut_ptr() as *mut c_void),
                    len,
                    &mut len,
                )
                .is_ok()
                {
                    let tml = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
                    let sid = tml.Label.Sid;
                    let count = *GetSidSubAuthorityCount(sid);
                    if count > 0 {
                        out = i64::from(*GetSidSubAuthority(sid, u32::from(count) - 1));
                    }
                }
            }
            let _ = CloseHandle(tok);
        }
        let _ = CloseHandle(p);
        out
    }
}

/// `FgInfo`.
pub fn fg_info() -> Fg {
    // SAFETY: plain Win32 queries with owned buffers.
    unsafe {
        let h = GetForegroundWindow();
        let mut r = GetAncestor(h, GA_ROOT);
        if r.0.is_null() {
            r = h;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(r, Some(&mut pid));
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(r, &mut buf).max(0) as usize;
        Fg {
            hwnd: hval(h),
            root_hwnd: hval(r),
            pid,
            process: process_name(pid),
            title: String::from_utf16_lossy(&buf[..n])
                .chars()
                .take(60)
                .collect(),
        }
    }
}

/// Virtual desktop `(x, y, w, h)`.
pub fn virtual_screen() -> (i32, i32, i32, i32) {
    // SAFETY: plain Win32 calls.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn open_clipboard() -> bool {
    for _ in 0..20 {
        // SAFETY: plain Win32 call.
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

impl Injector for WinInjector {
    fn send(&mut self, evs: &[Ev]) -> usize {
        if self.dry {
            let vs = virtual_screen();
            for e in evs {
                if let Some(p) = super::input::simulated_cursor(e, vs) {
                    self.dry_cursor = Some(p);
                }
            }
            return evs.len();
        }
        let inputs: Vec<INPUT> = evs.iter().map(to_input).collect();
        // SAFETY: well-formed INPUT array.
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) as usize }
    }

    fn cursor(&self) -> (i32, i32) {
        if let Some(p) = self.dry_cursor {
            return p;
        }
        let mut p = POINT::default();
        // SAFETY: out struct.
        let _ = unsafe { GetCursorPos(&mut p) };
        (p.x, p.y)
    }

    fn foreground(&self) -> Fg {
        fg_info()
    }

    fn integrity(&self, root_hwnd: i64) -> i64 {
        integrity_of(root_hwnd)
    }

    fn virtual_screen(&self) -> (i32, i32, i32, i32) {
        virtual_screen()
    }

    fn now_ms(&self) -> f64 {
        self.sw.elapsed().as_secs_f64() * 1000.0
    }

    fn sleep_ms(&mut self, ms: u64) {
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }

    fn wait_until_ms(&mut self, target: f64) {
        let left = target - self.now_ms();
        if left > 16.0 {
            std::thread::sleep(Duration::from_millis((left - 14.0) as u64));
        }
        while self.now_ms() < target {
            std::thread::yield_now();
        }
    }

    fn session(&self) -> Option<Value> {
        super::state::read_state()
    }

    fn clipboard_get(&mut self) -> Option<String> {
        if self.dry {
            return self.dry_clip.clone();
        }
        if !open_clipboard() {
            return None;
        }
        // SAFETY: clipboard opened above; the global is locked only while
        // copying the NUL-terminated UTF-16 text.
        let out = unsafe {
            match GetClipboardData(CF_UNICODETEXT) {
                Ok(h) if !h.0.is_null() => {
                    let g = HGLOBAL(h.0);
                    let p = GlobalLock(g) as *const u16;
                    if p.is_null() {
                        None
                    } else {
                        let mut n = 0usize;
                        while *p.add(n) != 0 {
                            n += 1;
                        }
                        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                        let _ = GlobalUnlock(g);
                        Some(s)
                    }
                }
                _ => None,
            }
        };
        // SAFETY: opened above.
        let _ = unsafe { CloseClipboard() };
        out
    }

    fn clipboard_set(&mut self, text: &str) -> bool {
        let mut w: Vec<u16> = text.encode_utf16().collect();
        w.push(0);
        if self.dry {
            self.dry_clip = Some(text.to_string());
            return true;
        }
        if !open_clipboard() {
            return false;
        }
        // SAFETY: clipboard opened; ownership of the global passes to the
        // system when SetClipboardData succeeds.
        let ok = unsafe {
            let _ = EmptyClipboard();
            match GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) {
                Ok(g) => {
                    let p = GlobalLock(g) as *mut u16;
                    if p.is_null() {
                        false
                    } else {
                        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                        let _ = GlobalUnlock(g);
                        SetClipboardData(CF_UNICODETEXT, Some(HANDLE(g.0))).is_ok()
                    }
                }
                Err(_) => false,
            }
        };
        // SAFETY: opened above.
        let _ = unsafe { CloseClipboard() };
        ok
    }

    fn caps_lock(&self) -> bool {
        // SAFETY: plain Win32 call.
        unsafe { GetKeyState(0x14) & 1 == 1 }
    }

    fn layout_key(&self, unit: u16) -> Option<LayoutKey> {
        // SAFETY: plain Win32 queries.
        unsafe {
            let fg = GetForegroundWindow();
            let tid = GetWindowThreadProcessId(fg, None);
            let hkl = GetKeyboardLayout(tid);
            let r = VkKeyScanExW(unit, hkl);
            if r == -1 {
                return None;
            }
            let vk = (r as u16) & 0xFF;
            let mods = ((r as u16) >> 8) as u8;
            let scan = MapVirtualKeyExW(u32::from(vk), MAP_VIRTUAL_KEY_TYPE(0), Some(hkl)) as u16;
            Some(LayoutKey { vk, mods, scan })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only queries only: nothing is injected.
    #[test]
    fn read_only_queries_work() {
        let (_, _, w, h) = virtual_screen();
        assert!(w > 0 && h > 0);
        let me = std::process::id();
        assert!(process_name(me).is_some());
        let _ = fg_info();
        assert_eq!(std::mem::size_of::<INPUT>(), 40);
    }
}

//! Windows platform of the hands-free session: low-level hooks on a
//! dedicated thread and the glowing click-through overlay.
//!
//! Hooks (D27): `WH_MOUSE_LL` + `WH_KEYBOARD_LL` live on their own thread
//! with a plain `GetMessage` loop, so the callbacks never wait behind the
//! overlay/state work (Windows silently unhooks callbacks that take more
//! than ~300 ms). Callbacks only touch atomics. Rules:
//! * injected events (`LLMHF_INJECTED` / `LLKHF_INJECTED`) always pass —
//!   this server's own `SendInput`, UIA's activation key and accessibility
//!   tools — except the self-probe (`PROBE_MAGIC` in `dwExtraInfo`), which
//!   is counted and swallowed so no app ever sees it;
//! * real events are recorded; once armed (guard=block) mouse events and,
//!   with `block_keyboard`, key events are swallowed, except the UP of a
//!   key/button that was already down when arming (nothing stays stuck);
//! * the release hotkey (main key down while its modifiers are physically
//!   held) is swallowed and ends the session; it works before arming too;
//! * extra fail-open guard: past `expires + 5 s` nothing is blocked even
//!   if the main loop is wedged (the watchdog also exits the process).
//!
//! Ctrl+Alt+Del (the secure attention sequence) never reaches LL hooks:
//! the user can always reach the secure desktop, and locking ends the
//! session (`session_locked`).

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CALCRECT, DT_NOPREFIX, DT_SINGLELINE, DeleteDC,
    DeleteObject, DrawTextW, EnumDisplayMonitors, FW_NORMAL, GetDC, GetMonitorInfoW, HBITMAP, HDC,
    HFONT, HGDIOBJ, HMONITOR, MONITORINFO, OUT_DEFAULT_PRECIS, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    HHOOK, HWND_TOPMOST, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, MsgWaitForMultipleObjects,
    PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW, QS_ALLINPUT, RegisterClassExW,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowDisplayAffinity,
    SetWindowPos, SetWindowsHookExW, ShowWindow, TranslateMessage, ULW_ALPHA, UnhookWindowsHookEx,
    UpdateLayeredWindow, WDA_EXCLUDEFROMCAPTURE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_QUIT,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BOOL, w};

use super::paint::{self, Edge};
use super::session::{Config, Counters, Guard, Platform, RealInput};

/// `dwExtraInfo` of the hook self-probe.
pub const PROBE_MAGIC: usize = 0x5742_0001;
pub use super::input::INPUT_MAGIC;

// ------------------------------------------------------------- hook state

static BLOCK_MODE: AtomicBool = AtomicBool::new(true);
static BLOCK_KB: AtomicBool = AtomicBool::new(true);
static HOT_MAIN: AtomicU32 = AtomicU32::new(0x48);
static HOT_MODS: AtomicU32 = AtomicU32::new(7);
static ARMED: AtomicBool = AtomicBool::new(false);
/// Unix ms after which nothing is blocked any more (expires + 5 s).
static FAILOPEN_AT_MS: AtomicI64 = AtomicI64::new(i64::MAX);
static LAST_REAL_MS: AtomicI64 = AtomicI64::new(0);
static LAST_KIND: AtomicU32 = AtomicU32::new(0);
static LAST_CODE: AtomicU32 = AtomicU32::new(0);
static REAL: AtomicU64 = AtomicU64::new(0);
static INJECTED: AtomicU64 = AtomicU64::new(0);
static OWN_INJECTED: AtomicU64 = AtomicU64::new(0);
static BLOCKED: AtomicU64 = AtomicU64::new(0);
static HOTKEY_HITS: AtomicU64 = AtomicU64::new(0);
static PROBE_SEEN: AtomicU64 = AtomicU64::new(0);
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);
static KBD_HOOK: AtomicIsize = AtomicIsize::new(0);

#[allow(clippy::declare_interior_mutable_const)]
const FALSE: AtomicBool = AtomicBool::new(false);
/// Keys physically held right now (swallowed keys never reach GetKeyState).
static REAL_DOWN: [AtomicBool; 256] = [FALSE; 256];
/// Keys/buttons the OS believes are down at arm time: their UP passes.
static PASS_DOWN: [AtomicBool; 256] = [FALSE; 256];

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn blocking() -> bool {
    BLOCK_MODE.load(Ordering::Relaxed)
        && ARMED.load(Ordering::Relaxed)
        && now_ms() < FAILOPEN_AT_MS.load(Ordering::Relaxed)
}

fn real(kind: u32, code: u32) {
    LAST_REAL_MS.store(now_ms(), Ordering::Relaxed);
    REAL.fetch_add(1, Ordering::Relaxed);
    LAST_KIND.store(kind, Ordering::Relaxed);
    LAST_CODE.store(code, Ordering::Relaxed);
}

fn held(a: usize, b: usize, c: usize) -> bool {
    REAL_DOWN[a].load(Ordering::Relaxed)
        || REAL_DOWN[b].load(Ordering::Relaxed)
        || REAL_DOWN[c].load(Ordering::Relaxed)
}

fn hot_mods_held() -> bool {
    let m = HOT_MODS.load(Ordering::Relaxed);
    !((m & 1 != 0 && !held(0x11, 0xA2, 0xA3))
        || (m & 2 != 0 && !held(0x12, 0xA4, 0xA5))
        || (m & 4 != 0 && !held(0x10, 0xA0, 0xA1))
        || (m & 8 != 0 && !held(0x5B, 0x5C, 0x5C)))
}

fn hhook(v: isize) -> Option<HHOOK> {
    (v != 0).then_some(HHOOK(v as *mut _))
}

/// Mouse button that a `WM_*BUTTONUP` releases (VK_LBUTTON..VK_XBUTTON2).
fn mouse_up_vk(msg: u32, mouse_data: u32) -> Option<usize> {
    match msg {
        0x202 => Some(1),
        0x205 => Some(2),
        0x208 => Some(4),
        0x20C => Some(if (mouse_data >> 16) == 1 { 5 } else { 6 }),
        _ => None,
    }
}

unsafe extern "system" fn on_mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_MOUSE_LL, lParam points to an MSLLHOOKSTRUCT.
        let s = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) };
        if s.flags & 0x1 != 0 {
            INJECTED.fetch_add(1, Ordering::Relaxed);
            if s.dwExtraInfo == PROBE_MAGIC {
                PROBE_SEEN.fetch_add(1, Ordering::Relaxed);
                return LRESULT(1);
            }
            if s.dwExtraInfo == INPUT_MAGIC {
                OWN_INJECTED.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            let m = wp.0 as u32;
            real(1, m);
            if blocking() {
                match mouse_up_vk(m, s.mouseData) {
                    Some(vk) if PASS_DOWN[vk].swap(false, Ordering::Relaxed) => {}
                    _ => {
                        BLOCKED.fetch_add(1, Ordering::Relaxed);
                        return LRESULT(1);
                    }
                }
            }
        }
    }
    // SAFETY: forwarding the unmodified hook arguments.
    unsafe { CallNextHookEx(hhook(MOUSE_HOOK.load(Ordering::Relaxed)), code, wp, lp) }
}

unsafe extern "system" fn on_kbd(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL, lParam points to a KBDLLHOOKSTRUCT.
        let s = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        if s.flags.0 & 0x10 != 0 {
            INJECTED.fetch_add(1, Ordering::Relaxed);
            if s.dwExtraInfo == PROBE_MAGIC {
                PROBE_SEEN.fetch_add(1, Ordering::Relaxed);
                return LRESULT(1);
            }
            if s.dwExtraInfo == INPUT_MAGIC {
                OWN_INJECTED.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            let m = wp.0 as u32;
            let vk = (s.vkCode & 0xFF) as usize;
            let down = m == 0x100 || m == 0x104;
            real(2, vk as u32);
            REAL_DOWN[vk].store(down, Ordering::Relaxed);
            if down && vk as u32 == HOT_MAIN.load(Ordering::Relaxed) && hot_mods_held() {
                HOTKEY_HITS.fetch_add(1, Ordering::Relaxed);
                return LRESULT(1);
            }
            if blocking() && BLOCK_KB.load(Ordering::Relaxed) {
                if !down && PASS_DOWN[vk].swap(false, Ordering::Relaxed) {
                    // was down before arming: let it come up
                } else {
                    BLOCKED.fetch_add(1, Ordering::Relaxed);
                    return LRESULT(1);
                }
            }
        }
    }
    // SAFETY: forwarding the unmodified hook arguments.
    unsafe { CallNextHookEx(hhook(KBD_HOOK.load(Ordering::Relaxed)), code, wp, lp) }
}

const MSG_INSTALL: u32 = WM_APP + 1;
const MSG_UNINSTALL: u32 = WM_APP + 2;

fn hinstance() -> HINSTANCE {
    // SAFETY: handle of our own module.
    unsafe {
        GetModuleHandleW(None)
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default()
    }
}

fn do_uninstall() {
    for h in [&MOUSE_HOOK, &KBD_HOOK] {
        if let Some(hk) = hhook(h.swap(0, Ordering::Relaxed)) {
            // SAFETY: a hook installed by this thread.
            let _ = unsafe { UnhookWindowsHookEx(hk) };
        }
    }
}

fn do_install() -> bool {
    do_uninstall();
    let hi = hinstance();
    // SAFETY: LL hooks with static callbacks; installed on this thread,
    // which pumps messages for as long as they exist.
    unsafe {
        let m = SetWindowsHookExW(WH_MOUSE_LL, Some(on_mouse), Some(hi), 0);
        let k = SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_kbd), Some(hi), 0);
        if let Ok(m) = m {
            MOUSE_HOOK.store(m.0 as isize, Ordering::Relaxed);
        }
        if let Ok(k) = k {
            KBD_HOOK.store(k.0 as isize, Ordering::Relaxed);
        }
    }
    let ok = MOUSE_HOOK.load(Ordering::Relaxed) != 0 && KBD_HOOK.load(Ordering::Relaxed) != 0;
    if !ok {
        do_uninstall();
    }
    ok
}

/// The hook thread: owns the hooks and pumps their messages.
struct HookThread {
    tid: u32,
    rx: mpsc::Receiver<bool>,
}

impl HookThread {
    fn spawn() -> Option<Self> {
        let (tx, rx) = mpsc::channel::<bool>();
        let (tid_tx, tid_rx) = mpsc::channel::<u32>();
        std::thread::Builder::new()
            .name("handsfree-hooks".into())
            .spawn(move || {
                let mut msg = MSG::default();
                // SAFETY: creates this thread's message queue before
                // anyone posts to it.
                unsafe {
                    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                    let _ = tid_tx.send(GetCurrentThreadId());
                    loop {
                        let r = GetMessageW(&mut msg, None, 0, 0);
                        if r.0 <= 0 {
                            break;
                        }
                        match msg.message {
                            MSG_INSTALL => {
                                let _ = tx.send(do_install());
                            }
                            MSG_UNINSTALL => {
                                do_uninstall();
                                let _ = tx.send(true);
                            }
                            _ => {
                                let _ = TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                        }
                    }
                }
                do_uninstall();
            })
            .ok()?;
        let tid = tid_rx.recv_timeout(Duration::from_secs(5)).ok()?;
        Some(Self { tid, rx })
    }

    fn request(&self, msg: u32) -> bool {
        while self.rx.try_recv().is_ok() {}
        // SAFETY: posting to our own thread's queue.
        if unsafe { PostThreadMessageW(self.tid, msg, WPARAM(0), LPARAM(0)) }.is_err() {
            return false;
        }
        self.rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap_or(false)
    }

    fn quit(&self) {
        // SAFETY: posting WM_QUIT to our own thread.
        let _ = unsafe { PostThreadMessageW(self.tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

// ---------------------------------------------------------------- overlay

struct Mon {
    rect: RECT,
    primary: bool,
    dpi: u32,
}

fn monitors() -> Vec<Mon> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        // SAFETY: lp is the &mut Vec passed below.
        let v = unsafe { &mut *(lp.0 as *mut Vec<Mon>) };
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let (mut dx, mut dy) = (96u32, 96u32);
        // SAFETY: plain queries with out parameters.
        unsafe {
            if GetMonitorInfoW(m, &mut mi).as_bool() {
                let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
                v.push(Mon {
                    rect: mi.rcMonitor,
                    primary: mi.dwFlags & 1 != 0,
                    dpi: dx,
                });
            }
        }
        BOOL(1)
    }
    let mut v: Vec<Mon> = Vec::new();
    // SAFETY: the callback only runs during this call.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: default handling only.
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn register_class() {
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance(),
        lpszClassName: w!("WindowsSosaMcpHandsFreeOverlay"),
        ..Default::default()
    };
    // SAFETY: static class name and window procedure.
    unsafe {
        RegisterClassExW(&wc);
    }
}

/// A 32-bit top-down DIB selected into a memory DC.
struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    w: i32,
    h: i32,
}

impl Surface {
    fn new(w: i32, h: i32) -> Option<Self> {
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: GDI objects owned by the Surface, released in Drop.
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            Some(Self {
                dc,
                bmp,
                old,
                bits: bits as *mut u32,
                w,
                h,
            })
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: the DIB has exactly w*h 32-bit pixels.
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: objects created in Surface::new.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn blend(alpha: u8) -> BLENDFUNCTION {
    BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: alpha,
        AlphaFormat: AC_SRC_ALPHA as u8,
    }
}

/// Create a passive layered window: never activates, never takes clicks,
/// always on top, hidden from screen captures (our own captures included).
fn layered_window(x: i32, y: i32, s: &Surface, alpha: u8) -> Option<HWND> {
    // SAFETY: plain window creation on this (UI) thread.
    unsafe {
        let h = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("WindowsSosaMcpHandsFreeOverlay"),
            w!("windows-sosa-mcp hands-free"),
            WS_POPUP,
            x,
            y,
            s.w,
            s.h,
            None,
            None,
            Some(hinstance()),
            None,
        )
        .ok()?;
        update_layered(h, x, y, s, alpha);
        let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        Some(h)
    }
}

fn update_layered(h: HWND, x: i32, y: i32, s: &Surface, alpha: u8) {
    let b = blend(alpha);
    let pt = POINT { x, y };
    let size = SIZE { cx: s.w, cy: s.h };
    let src = POINT { x: 0, y: 0 };
    // SAFETY: the surface DC holds a premultiplied 32-bit DIB.
    unsafe {
        let screen = GetDC(None);
        let _ = UpdateLayeredWindow(
            h,
            Some(screen),
            Some(&pt),
            Some(&size),
            Some(s.dc),
            Some(&src),
            COLORREF(0),
            Some(&b),
            ULW_ALPHA,
        );
        ReleaseDC(None, screen);
    }
}

fn set_alpha(h: HWND, alpha: u8) {
    let b = blend(alpha);
    // SAFETY: alpha-only update of an existing layered window.
    unsafe {
        let _ = UpdateLayeredWindow(
            h,
            None,
            None,
            None,
            None,
            None,
            COLORREF(0),
            Some(&b),
            ULW_ALPHA,
        );
    }
}

struct Label {
    hwnd: Option<HWND>,
    text: String,
    anchor: RECT,
    scale: f64,
}

impl Label {
    /// Render the pill for `text` and (re)position it top-centre.
    fn render(&mut self, text: &str) {
        if text == self.text && self.hwnd.is_some() {
            return;
        }
        self.text = text.to_string();
        let s = self.scale;
        let (padx, pady) = ((18.0 * s).round() as i32 + 1, (8.0 * s).round() as i32 + 1);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: GDI text measuring/drawing on a private memory DC.
        unsafe {
            let font: HFONT = CreateFontW(
                -((15.0 * s).round() as i32),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                0,
                w!("Segoe UI"),
            );
            let probe = CreateCompatibleDC(None);
            let oldf = SelectObject(probe, HGDIOBJ(font.0));
            let mut r = RECT::default();
            DrawTextW(
                probe,
                &mut wide,
                &mut r,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(probe, oldf);
            let _ = DeleteDC(probe);
            let (w, h) = (r.right - r.left + 2 * padx, r.bottom - r.top + 2 * pady);
            let Some(mut surf) = Surface::new(w.max(1), h.max(1)) else {
                let _ = DeleteObject(HGDIOBJ(font.0));
                return;
            };
            surf.pixels().fill(0);
            let oldf = SelectObject(surf.dc, HGDIOBJ(font.0));
            SetTextColor(surf.dc, COLORREF(0x00FF_FFFF));
            SetBkMode(surf.dc, TRANSPARENT);
            let mut tr = RECT {
                left: padx,
                top: pady,
                right: w - padx,
                bottom: h - pady,
            };
            DrawTextW(surf.dc, &mut wide, &mut tr, DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(surf.dc, oldf);
            let _ = DeleteObject(HGDIOBJ(font.0));
            let cov: Vec<u8> = surf
                .pixels()
                .iter()
                .map(|p| ((p >> 16) & 0xFF).max((p >> 8) & 0xFF).max(p & 0xFF) as u8)
                .collect();
            let px = paint::label_pixels(w as usize, h as usize, 16.0 * s, 1.0, &cov);
            surf.pixels().copy_from_slice(&px);
            let a = self.anchor;
            let x = a.left + ((a.right - a.left) - w) / 2;
            let y = a.top + (14.0 * s).round() as i32;
            match self.hwnd {
                Some(hw) => update_layered(hw, x, y, &surf, 255),
                None => self.hwnd = layered_window(x, y, &surf, 255),
            }
        }
    }
}

pub struct WinPlatform {
    hooks: Option<HookThread>,
    edges: Vec<HWND>,
    label: Label,
    t0: Instant,
    last_frame: Instant,
    monitors: usize,
}

impl WinPlatform {
    pub fn new(cfg: &Config) -> Self {
        crate::ui::win::init_process();
        BLOCK_MODE.store(cfg.guard == Guard::Block, Ordering::Relaxed);
        BLOCK_KB.store(cfg.block_keyboard, Ordering::Relaxed);
        HOT_MAIN.store(u32::from(cfg.hotkey.vk), Ordering::Relaxed);
        HOT_MODS.store(cfg.hotkey.mods, Ordering::Relaxed);
        FAILOPEN_AT_MS.store(
            now_ms() + (cfg.timeout_seconds as i64) * 1000 + 5000,
            Ordering::Relaxed,
        );
        register_class();
        let mons = monitors();
        let mut edges = Vec::new();
        let alpha = paint::pulse_alpha(0);
        for m in &mons {
            let r = m.rect;
            let glow = paint::scaled(cfg.glow_px, m.dpi);
            for (x, y, w, h, e) in
                paint::strips(r.left, r.top, r.right - r.left, r.bottom - r.top, glow)
            {
                if let Some(mut s) = Surface::new(w, h) {
                    let px = paint::edge_pixels(w as usize, h as usize, e);
                    s.pixels().copy_from_slice(&px);
                    if let Some(hw) = layered_window(x, y, &s, alpha) {
                        edges.push(hw);
                    }
                }
                let _: Edge = e;
            }
        }
        let primary = mons
            .iter()
            .find(|m| m.primary)
            .or(mons.first())
            .map(|m| (m.rect, m.dpi))
            .unwrap_or((RECT::default(), 96));
        Self {
            hooks: None,
            edges,
            label: Label {
                hwnd: None,
                text: String::new(),
                anchor: primary.0,
                scale: f64::from(primary.1.max(96)) / 96.0,
            },
            t0: Instant::now(),
            last_frame: Instant::now(),
            monitors: mons.len(),
        }
    }

    fn all_windows(&self) -> impl Iterator<Item = HWND> + '_ {
        self.edges.iter().copied().chain(self.label.hwnd)
    }
}

impl Platform for WinPlatform {
    fn install(&mut self) -> bool {
        if self.hooks.is_none() {
            self.hooks = HookThread::spawn();
        }
        self.hooks.as_ref().is_some_and(|h| h.request(MSG_INSTALL))
    }

    fn uninstall(&mut self) {
        ARMED.store(false, Ordering::Relaxed);
        if let Some(h) = &self.hooks {
            h.request(MSG_UNINSTALL);
        }
        // Safety net if the hook thread did not answer: unhooking from
        // another thread is allowed for LL hooks.
        do_uninstall();
    }

    fn arm(&mut self) {
        for (vk, slot) in PASS_DOWN.iter().enumerate().skip(1) {
            // SAFETY: plain key-state query.
            let down = unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
            slot.store(down, Ordering::Relaxed);
        }
        ARMED.store(true, Ordering::Relaxed);
    }

    fn hotkey_hits(&self) -> u64 {
        HOTKEY_HITS.load(Ordering::Relaxed)
    }

    fn last_real(&self) -> Option<RealInput> {
        let ms = LAST_REAL_MS.load(Ordering::Relaxed);
        if ms <= 0 {
            return None;
        }
        Some(RealInput {
            at: DateTime::<Utc>::from_timestamp_millis(ms)?,
            kind: LAST_KIND.load(Ordering::Relaxed) as u8,
            code: LAST_CODE.load(Ordering::Relaxed),
        })
    }

    fn reset_probe(&mut self) {
        PROBE_SEEN.store(0, Ordering::Relaxed);
    }

    fn send_probe(&mut self) -> bool {
        let i = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0xE8),
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: PROBE_MAGIC,
                },
            },
        };
        // SAFETY: one well-formed INPUT; the hook swallows it.
        unsafe { SendInput(&[i], std::mem::size_of::<INPUT>() as i32) == 1 }
    }

    fn probe_seen(&self) -> bool {
        PROBE_SEEN.load(Ordering::Relaxed) > 0
    }

    fn session_locked(&self) -> bool {
        crate::ui::win::session_locked().as_bool() == Some(true)
    }

    fn counters(&self) -> Counters {
        Counters {
            real: REAL.load(Ordering::Relaxed),
            blocked: BLOCKED.load(Ordering::Relaxed),
            injected: INJECTED.load(Ordering::Relaxed),
        }
    }

    fn monitors(&self) -> usize {
        self.monitors
    }

    fn set_label(&mut self, text: &str) {
        self.label.render(text);
    }

    fn retopmost(&mut self) {
        for h in self.all_windows().collect::<Vec<_>>() {
            // SAFETY: z-order only; never activates.
            unsafe {
                let _ = SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

    fn close_overlay(&mut self) {
        for h in self.all_windows().collect::<Vec<_>>() {
            // SAFETY: windows created by this thread.
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
        self.edges.clear();
        self.label.hwnd = None;
        if let Some(h) = self.hooks.take() {
            h.quit();
        }
    }

    fn pump(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let wait = left.min(Duration::from_millis(42)).as_millis() as u32;
            // SAFETY: standard message pump on the UI thread.
            unsafe {
                MsgWaitForMultipleObjects(None, false, wait, QS_ALLINPUT);
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if self.last_frame.elapsed() >= Duration::from_millis(42) {
                self.last_frame = Instant::now();
                let a = paint::pulse_alpha(self.t0.elapsed().as_millis() as u64);
                for h in &self.edges {
                    set_alpha(*h, a);
                }
            }
        }
    }
}

/// Own-injected events seen by the hooks (diagnostics, not in the contract).
pub fn own_injected() -> u64 {
    OWN_INJECTED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_up_maps_buttons() {
        assert_eq!(mouse_up_vk(0x202, 0), Some(1));
        assert_eq!(mouse_up_vk(0x205, 0), Some(2));
        assert_eq!(mouse_up_vk(0x208, 0), Some(4));
        assert_eq!(mouse_up_vk(0x20C, 1 << 16), Some(5));
        assert_eq!(mouse_up_vk(0x20C, 2 << 16), Some(6));
        assert_eq!(mouse_up_vk(0x200, 0), None);
        assert_ne!(PROBE_MAGIC, INPUT_MAGIC);
    }
}

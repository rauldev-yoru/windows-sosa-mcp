//! Windows backend of the UI tools: window enumeration, UI Automation
//! (native `IUIAutomation` through COM) and GDI capture.
//!
//! Top-level window enumeration, window resolution, snapshots, ref
//! resolution, actions and captures, with stable selection rules, error
//! kinds and messages.
//!
//! Threading (design D19): every UIA object lives on ONE resident COM MTA
//! thread created on first use; requests run there as closures sent over a
//! channel. A request that outlives its timeout abandons that thread (it
//! finishes or dies on its own) and the next request gets a fresh one, so a
//! hung provider cannot wedge the server. Window enumeration and captures
//! need no COM and run on short-lived threads with the same timeout rule.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use windows::Win32::Foundation::{
    HANDLE, HWND, LPARAM, RECT, VARIANT_BOOL, VARIANT_FALSE, VARIANT_TRUE,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetMonitorInfoW, HBITMAP,
    HDC, HGDIOBJ, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW,
    MonitorFromWindow, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayUnaccessData,
};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SESSION, WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VT_ARRAY, VT_BOOL, VT_BSTR, VT_I4, VT_R8, VariantClear,
};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GA_ROOT, GWL_EXSTYLE, GetAncestor, GetClassNameW,
    GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, SetForegroundWindow,
    WS_EX_TOOLWINDOW,
};
use windows::core::Interface;
use windows::core::{BOOL, BSTR, PWSTR};

use super::UiError;
use super::frame::{Frame, finish, intersects};
use super::policy::Policy;
use super::render::{Bounds, Classifier, Node, RawNode, ps_int, role_name, round_dp};
use super::store::Anchor;

// ---------------------------------------------------------------- process

/// Opt into Per-Monitor-V2 DPI awareness so window rects, UIA bounds and
/// captures are physical virtual-desktop pixels on every monitor (D20).
/// Called once at startup; failure (already set by a manifest) is fine.
pub fn init_process() {
    // SAFETY: plain Win32 call without pointers.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

// ---------------------------------------------------------------- threads

fn timeout_error() -> UiError {
    UiError::new(
        "timeout",
        "Windows termination unconfirmed; inspect effects before retrying",
    )
}

/// Run `f` on a short-lived thread, waiting at most `timeout`.
pub fn run_detached<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> Result<T, UiError> + Send + 'static,
) -> Result<T, UiError> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("ui-call".into())
        .spawn(move || {
            let r = catch_unwind(AssertUnwindSafe(f))
                .unwrap_or_else(|_| Err(UiError::new("io_error", "UI call panicked")));
            let _ = tx.send(r);
        })
        .map_err(|e| UiError::new("io_error", format!("cannot start UI thread: {e}")))?;
    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(timeout_error()),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(UiError::new("io_error", "UI thread ended without a result"))
        }
    }
}

type Job = Box<dyn FnOnce(Result<&Uia, &UiError>) + Send>;

static UIA_WORKER: Mutex<Option<mpsc::Sender<Job>>> = Mutex::new(None);

fn spawn_uia_worker() -> Result<mpsc::Sender<Job>, UiError> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("uia-mta".into())
        .spawn(move || {
            let ctx = Uia::new();
            for job in rx {
                job(ctx.as_ref());
            }
        })
        .map_err(|e| UiError::new("io_error", format!("cannot start UIA thread: {e}")))?;
    Ok(tx)
}

fn forget_uia_worker() {
    if let Ok(mut g) = UIA_WORKER.lock() {
        *g = None;
    }
}

/// Run `f` on the resident UIA thread, waiting at most `timeout`.
pub fn run_uia<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce(&Uia) -> Result<T, UiError> + Send + 'static,
) -> Result<T, UiError> {
    let tx = {
        let mut g = UIA_WORKER
            .lock()
            .map_err(|_| UiError::new("io_error", "UIA worker lock poisoned"))?;
        if g.is_none() {
            *g = Some(spawn_uia_worker()?);
        }
        g.as_ref().expect("worker present").clone()
    };
    let (rtx, rrx) = mpsc::sync_channel(1);
    let job: Job = Box::new(move |ctx| {
        let r = match ctx {
            Ok(u) => catch_unwind(AssertUnwindSafe(|| f(u)))
                .unwrap_or_else(|_| Err(UiError::new("io_error", "UIA call panicked"))),
            Err(e) => Err(e.clone()),
        };
        let _ = rtx.send(r);
    });
    if tx.send(job).is_err() {
        forget_uia_worker();
        return Err(UiError::new("io_error", "UIA worker is gone; retry"));
    }
    match rrx.recv_timeout(timeout) {
        Ok(Err(e)) if e.kind == "uia_unavailable" => {
            forget_uia_worker();
            Err(e)
        }
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            forget_uia_worker();
            Err(timeout_error())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            forget_uia_worker();
            Err(UiError::new(
                "io_error",
                "UIA worker ended without a result",
            ))
        }
    }
}

// ---------------------------------------------------------------- COM helpers

fn com_err(e: windows::core::Error) -> UiError {
    let code = e.code().0 as u32;
    let msg = e.message();
    let msg = if msg.trim().is_empty() {
        format!("HRESULT 0x{code:08X}")
    } else {
        format!("{} (HRESULT 0x{code:08X})", msg.trim())
    };
    match code {
        // UIA_E_ELEMENTNOTAVAILABLE (managed ElementNotAvailableException).
        0x8004_0201 => UiError::new(
            "ref_stale",
            "element vanished during the operation; take a new snapshot",
        ),
        0x8007_0005 => UiError::new("access_denied", msg),
        // UIA_E_INVALIDOPERATION / UIA_E_ELEMENTNOTENABLED: InvalidOperationException.
        0x8013_1509 | 0x8004_0200 => UiError::new("pattern_failed", msg),
        _ => UiError::new("io_error", msg),
    }
}

trait ComResult<T> {
    fn ui(self) -> Result<T, UiError>;
}

impl<T> ComResult<T> for windows::core::Result<T> {
    fn ui(self) -> Result<T, UiError> {
        self.map_err(com_err)
    }
}

/// An owned VARIANT, cleared on drop.
struct Var(VARIANT);

impl Drop for Var {
    fn drop(&mut self) {
        // SAFETY: the VARIANT is owned and initialised.
        let _ = unsafe { VariantClear(&mut self.0) };
    }
}

impl Var {
    fn vt(&self) -> VARENUM {
        // SAFETY: reading the discriminant of an initialised VARIANT.
        unsafe { self.0.Anonymous.Anonymous.vt }
    }

    fn boolean(b: bool) -> Self {
        let mut v = VARIANT::default();
        // SAFETY: writing a VT_BOOL VARIANT.
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = VT_BOOL;
            inner.Anonymous.boolVal = if b { VARIANT_TRUE } else { VARIANT_FALSE };
        }
        Self(v)
    }

    fn int(i: i32) -> Self {
        let mut v = VARIANT::default();
        // SAFETY: writing a VT_I4 VARIANT.
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = VT_I4;
            inner.Anonymous.lVal = i;
        }
        Self(v)
    }

    fn string(s: &str) -> Self {
        let mut v = VARIANT::default();
        // SAFETY: writing a VT_BSTR VARIANT (owned BSTR freed by VariantClear).
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = VT_BSTR;
            inner.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(s));
        }
        Self(v)
    }

    fn as_bool(&self) -> Option<bool> {
        // SAFETY: union read guarded by the discriminant.
        (self.vt() == VT_BOOL)
            .then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.boolVal } != VARIANT_BOOL(0))
    }

    fn as_i32(&self) -> Option<i32> {
        // SAFETY: union read guarded by the discriminant.
        (self.vt() == VT_I4).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.lVal })
    }

    fn as_f64(&self) -> Option<f64> {
        match self.vt() {
            // SAFETY: union reads guarded by the discriminant.
            VT_R8 => Some(unsafe { self.0.Anonymous.Anonymous.Anonymous.dblVal }),
            VT_I4 => Some(f64::from(unsafe {
                self.0.Anonymous.Anonymous.Anonymous.lVal
            })),
            _ => None,
        }
    }

    fn as_string(&self) -> Option<String> {
        // SAFETY: union read guarded by the discriminant.
        (self.vt() == VT_BSTR)
            .then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.bstrVal.to_string() })
    }

    fn array<T: Copy>(&self, elem: VARENUM) -> Option<Vec<T>> {
        if self.vt() != VARENUM(VT_ARRAY.0 | elem.0) {
            return None;
        }
        // SAFETY: VT_ARRAY variants carry a SAFEARRAY pointer.
        let sa = unsafe { self.0.Anonymous.Anonymous.Anonymous.parray };
        Some(unsafe { safearray_vec(sa) })
    }
}

/// Copy a one-dimensional SAFEARRAY of `T`.
unsafe fn safearray_vec<T: Copy>(sa: *const SAFEARRAY) -> Vec<T> {
    if sa.is_null() {
        return Vec::new();
    }
    // SAFETY: caller passes a valid one-dimensional SAFEARRAY of T.
    unsafe {
        let (Ok(lb), Ok(ub)) = (SafeArrayGetLBound(sa, 1), SafeArrayGetUBound(sa, 1)) else {
            return Vec::new();
        };
        let n = (ub - lb + 1).max(0) as usize;
        let mut p: *mut c_void = std::ptr::null_mut();
        if n == 0 || SafeArrayAccessData(sa, &mut p).is_err() || p.is_null() {
            return Vec::new();
        }
        let out = std::slice::from_raw_parts(p as *const T, n).to_vec();
        let _ = SafeArrayUnaccessData(sa);
        out
    }
}

// ---------------------------------------------------------------- UIA context

/// UIA objects owned by the worker thread.
pub struct Uia {
    uia: IUIAutomation,
    cache: IUIAutomationCacheRequest,
    cond_visible: IUIAutomationCondition,
    cond_all: IUIAutomationCondition,
}

const CACHED_PROPERTIES: [UIA_PROPERTY_ID; 33] = [
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_AutomationIdPropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_IsEnabledPropertyId,
    UIA_IsOffscreenPropertyId,
    UIA_ClassNamePropertyId,
    UIA_IsKeyboardFocusablePropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_RuntimeIdPropertyId,
    UIA_NativeWindowHandlePropertyId,
    UIA_IsPasswordPropertyId,
    UIA_ValueValuePropertyId,
    UIA_ValueIsReadOnlyPropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
    UIA_ScrollVerticalScrollPercentPropertyId,
    UIA_ScrollHorizontalScrollPercentPropertyId,
    UIA_ScrollVerticallyScrollablePropertyId,
    UIA_RangeValueValuePropertyId,
    UIA_RangeValueMinimumPropertyId,
    UIA_RangeValueMaximumPropertyId,
    UIA_WindowWindowVisualStatePropertyId,
    // Pattern availability instead of caching the pattern objects: same
    // answer as TryGetCachedPattern, nothing extra marshalled (D21).
    UIA_IsInvokePatternAvailablePropertyId,
    UIA_IsValuePatternAvailablePropertyId,
    UIA_IsTogglePatternAvailablePropertyId,
    UIA_IsSelectionItemPatternAvailablePropertyId,
    UIA_IsExpandCollapsePatternAvailablePropertyId,
    UIA_IsScrollPatternAvailablePropertyId,
    UIA_IsWindowPatternAvailablePropertyId,
    UIA_IsRangeValuePatternAvailablePropertyId,
    UIA_LocalizedControlTypePropertyId,
];

/// Pattern names in the worker's order with their availability property.
const PATTERNS: [(&str, UIA_PROPERTY_ID); 8] = [
    ("invoke", UIA_IsInvokePatternAvailablePropertyId),
    ("value", UIA_IsValuePatternAvailablePropertyId),
    ("toggle", UIA_IsTogglePatternAvailablePropertyId),
    ("select", UIA_IsSelectionItemPatternAvailablePropertyId),
    ("expand", UIA_IsExpandCollapsePatternAvailablePropertyId),
    ("scroll", UIA_IsScrollPatternAvailablePropertyId),
    ("window", UIA_IsWindowPatternAvailablePropertyId),
    ("range", UIA_IsRangeValuePatternAvailablePropertyId),
];

impl Uia {
    fn new() -> Result<Self, UiError> {
        let unavailable = |what: &str, e: windows::core::Error| {
            UiError::new("uia_unavailable", format!("{what}: {}", com_err(e).message))
        };
        // SAFETY: COM initialisation of this dedicated thread (MTA, D19).
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() {
                return Err(UiError::new(
                    "uia_unavailable",
                    format!("CoInitializeEx: 0x{:08X}", hr.0 as u32),
                ));
            }
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
                .or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER))
                .map_err(|e| unavailable("CoCreateInstance(CUIAutomation8)", e))?;
            // D22: the native client focuses the target before pattern calls
            // by default (AutoSetFocus=TRUE), which pulls the window to the
            // foreground on toggle/select. Background mode must not do that.
            if let Ok(u6) = uia.cast::<IUIAutomation6>() {
                let _ = u6.SetAutoSetFocus(false);
            }
            let cache = uia
                .CreateCacheRequest()
                .map_err(|e| unavailable("CreateCacheRequest", e))?;
            for p in CACHED_PROPERTIES {
                cache
                    .AddProperty(p)
                    .map_err(|e| unavailable("CacheRequest.AddProperty", e))?;
            }
            cache
                .SetTreeScope(TreeScope_Element)
                .map_err(|e| unavailable("SetTreeScope", e))?;
            let pc = |id: UIA_PROPERTY_ID, v: Var| uia.CreatePropertyCondition(id, &v.0);
            let mk =
                || -> windows::core::Result<(IUIAutomationCondition, IUIAutomationCondition)> {
                    // WinUI flags popup Windows (menus, flyouts) offscreen although
                    // visible: keep Window/Menu nodes regardless.
                    let onscreen = uia.CreateOrCondition(
                        &pc(UIA_IsOffscreenPropertyId, Var::boolean(false))?,
                        &uia.CreateOrCondition(
                            &pc(
                                UIA_ControlTypePropertyId,
                                Var::int(UIA_WindowControlTypeId.0),
                            )?,
                            &pc(UIA_ControlTypePropertyId, Var::int(UIA_MenuControlTypeId.0))?,
                        )?,
                    )?;
                    let control = pc(UIA_IsControlElementPropertyId, Var::boolean(true))?;
                    let visible = uia.CreateAndCondition(
                        &pc(UIA_IsEnabledPropertyId, Var::boolean(true))?,
                        &uia.CreateAndCondition(&onscreen, &control)?,
                    )?;
                    Ok((visible, control))
                };
            let (cond_visible, cond_all) = mk().map_err(|e| unavailable("conditions", e))?;
            Ok(Self {
                uia,
                cache,
                cond_visible,
                cond_all,
            })
        }
    }

    fn prop_cond(&self, id: UIA_PROPERTY_ID, v: Var) -> Result<IUIAutomationCondition, UiError> {
        // SAFETY: COM call on the owning thread.
        unsafe { self.uia.CreatePropertyCondition(id, &v.0) }.ui()
    }

    fn and(
        &self,
        a: &IUIAutomationCondition,
        b: &IUIAutomationCondition,
    ) -> Result<IUIAutomationCondition, UiError> {
        // SAFETY: COM call on the owning thread.
        unsafe { self.uia.CreateAndCondition(a, b) }.ui()
    }

    fn element_from_hwnd(&self, h: i64) -> Result<IUIAutomationElement, UiError> {
        // SAFETY: COM call on the owning thread; any HWND value is accepted.
        unsafe { self.uia.ElementFromHandle(hwnd(h)) }.ui()
    }

    fn updated(&self, e: &IUIAutomationElement) -> Result<IUIAutomationElement, UiError> {
        // SAFETY: COM call on the owning thread.
        unsafe { e.BuildUpdatedCache(&self.cache) }.ui()
    }

    fn find_all(
        &self,
        e: &IUIAutomationElement,
        scope: TreeScope,
        cond: &IUIAutomationCondition,
    ) -> Result<Vec<IUIAutomationElement>, UiError> {
        // SAFETY: COM calls on the owning thread.
        unsafe {
            let arr = e.FindAllBuildCache(scope, cond, &self.cache).ui()?;
            let n = arr.Length().ui()?;
            (0..n).map(|i| arr.GetElement(i).ui()).collect()
        }
    }

    fn find_first(
        &self,
        e: &IUIAutomationElement,
        cond: &IUIAutomationCondition,
    ) -> Option<IUIAutomationElement> {
        // SAFETY: COM call on the owning thread; no match is an Err/null.
        unsafe { e.FindFirst(TreeScope_Descendants, cond) }.ok()
    }
}

fn cached(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<Var> {
    // SAFETY: COM call on the owning thread.
    unsafe { e.GetCachedPropertyValue(id) }.ok().map(Var)
}

fn cached_bool(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> bool {
    cached(e, id).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn cached_str(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> String {
    cached(e, id)
        .and_then(|v| v.as_string())
        .unwrap_or_default()
}

/// Supported pattern names, in `PATTERNS` order.
fn patterns(e: &IUIAutomationElement) -> Vec<&'static str> {
    PATTERNS
        .iter()
        .filter(|(_, id)| cached_bool(e, *id))
        .map(|(n, _)| *n)
        .collect()
}

fn enum_name(names: &[&str], v: Option<i32>) -> Option<String> {
    let i = usize::try_from(v?).ok()?;
    names.get(i).map(|s| s.to_lowercase())
}

/// `StateOf`: pattern state read from the cache.
fn state_of(e: &IUIAutomationElement, pats: &[&str]) -> Map<String, Value> {
    let mut s = Map::new();
    let f = |id| cached(e, id).and_then(|v| v.as_f64());
    if pats.contains(&"value") {
        let value = cached(e, UIA_ValueValuePropertyId).and_then(|v| v.as_string());
        s.insert("value".into(), json!(value.unwrap_or_default()));
        s.insert(
            "read_only".into(),
            json!(cached_bool(e, UIA_ValueIsReadOnlyPropertyId)),
        );
    }
    if pats.contains(&"toggle")
        && let Some(t) = enum_name(
            &["Off", "On", "Indeterminate"],
            cached(e, UIA_ToggleToggleStatePropertyId).and_then(|v| v.as_i32()),
        )
    {
        s.insert("toggle".into(), json!(t));
    }
    if pats.contains(&"expand")
        && let Some(t) = enum_name(
            &["Collapsed", "Expanded", "PartiallyExpanded", "LeafNode"],
            cached(e, UIA_ExpandCollapseExpandCollapseStatePropertyId).and_then(|v| v.as_i32()),
        )
    {
        s.insert("expand".into(), json!(t));
    }
    if pats.contains(&"select") {
        s.insert(
            "selected".into(),
            json!(cached_bool(e, UIA_SelectionItemIsSelectedPropertyId)),
        );
    }
    if pats.contains(&"range")
        && let (Some(v), Some(lo), Some(hi)) = (
            f(UIA_RangeValueValuePropertyId),
            f(UIA_RangeValueMinimumPropertyId),
            f(UIA_RangeValueMaximumPropertyId),
        )
    {
        s.insert("range_value".into(), super::render::num(v));
        s.insert("range_min".into(), super::render::num(lo));
        s.insert("range_max".into(), super::render::num(hi));
    }
    if pats.contains(&"scroll")
        && let (Some(v), Some(h)) = (
            f(UIA_ScrollVerticalScrollPercentPropertyId),
            f(UIA_ScrollHorizontalScrollPercentPropertyId),
        )
    {
        let pct = |x: f64| {
            if x < 0.0 {
                Value::Null
            } else {
                super::render::num(round_dp(x, 1))
            }
        };
        s.insert("scroll_vertical_percent".into(), pct(v));
        s.insert("scroll_horizontal_percent".into(), pct(h));
    }
    if pats.contains(&"window")
        && let Some(t) = enum_name(
            &["Normal", "Maximized", "Minimized"],
            cached(e, UIA_WindowWindowVisualStatePropertyId).and_then(|v| v.as_i32()),
        )
    {
        s.insert("window_state".into(), json!(t));
    }
    s
}

fn runtime_id(e: &IUIAutomationElement) -> Vec<i32> {
    if let Some(v) = cached(e, UIA_RuntimeIdPropertyId).and_then(|v| v.array::<i32>(VT_I4)) {
        return v;
    }
    // SAFETY: COM call; the returned SAFEARRAY is ours to destroy.
    unsafe {
        match e.GetRuntimeId() {
            Ok(sa) => {
                let out = safearray_vec::<i32>(sa);
                let _ = SafeArrayDestroy(sa);
                out
            }
            Err(_) => Vec::new(),
        }
    }
}

fn bounds_of(e: &IUIAutomationElement) -> Option<Bounds> {
    let r = cached(e, UIA_BoundingRectanglePropertyId)?.array::<f64>(VT_R8)?;
    if r.len() != 4 || r.iter().any(|x| x.is_infinite()) {
        return None;
    }
    Some(Bounds {
        x: ps_int(r[0]),
        y: ps_int(r[1]),
        width: ps_int(r[2]),
        height: ps_int(r[3]),
    })
}

fn control_type(e: &IUIAutomationElement) -> i32 {
    cached(e, UIA_ControlTypePropertyId)
        .and_then(|v| v.as_i32())
        .unwrap_or(UIA_CustomControlTypeId.0)
}

fn raw_node(e: &IUIAutomationElement) -> RawNode {
    let pats = patterns(e);
    RawNode {
        role: role_name(control_type(e)),
        name: cached_str(e, UIA_NamePropertyId),
        automation_id: cached_str(e, UIA_AutomationIdPropertyId),
        class: cached_str(e, UIA_ClassNamePropertyId),
        enabled: cached_bool(e, UIA_IsEnabledPropertyId),
        offscreen: cached_bool(e, UIA_IsOffscreenPropertyId),
        focusable: cached_bool(e, UIA_IsKeyboardFocusablePropertyId),
        focused: cached_bool(e, UIA_HasKeyboardFocusPropertyId),
        password: cached_bool(e, UIA_IsPasswordPropertyId),
        state: state_of(e, &pats),
        patterns: pats,
        bounds: bounds_of(e),
        runtime_id: runtime_id(e),
    }
}

// ---------------------------------------------------------------- windows

fn hwnd(v: i64) -> HWND {
    HWND(v as isize as *mut c_void)
}

fn hval(h: HWND) -> i64 {
    h.0 as isize as i64
}

fn title(h: HWND) -> String {
    // SAFETY: plain Win32 calls with owned buffers.
    unsafe {
        let n = GetWindowTextLengthW(h);
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..got.min(buf.len())])
    }
}

fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: plain Win32 call with an owned buffer.
    let n = unsafe { GetClassNameW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

fn pid_of(h: HWND) -> u32 {
    let mut p = 0u32;
    // SAFETY: plain Win32 call with an out pointer.
    unsafe { GetWindowThreadProcessId(h, Some(&mut p)) };
    p
}

fn rect_of(h: HWND) -> Bounds {
    let mut r = RECT::default();
    // SAFETY: plain Win32 call with an out pointer.
    let _ = unsafe { GetWindowRect(h, &mut r) };
    Bounds {
        x: i64::from(r.left),
        y: i64::from(r.top),
        width: i64::from(r.right - r.left),
        height: i64::from(r.bottom - r.top),
    }
}

fn cloaked(h: HWND) -> bool {
    let mut v = 0u32;
    // SAFETY: DWMWA_CLOAKED writes a DWORD.
    unsafe { DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut v as *mut u32 as *mut c_void, 4) }.is_ok()
        && v != 0
}

fn foreground() -> HWND {
    // SAFETY: plain Win32 call.
    unsafe { GetForegroundWindow() }
}

fn root_of(h: HWND) -> HWND {
    // SAFETY: plain Win32 call.
    unsafe { GetAncestor(h, GA_ROOT) }
}

unsafe extern "system" fn collect_hwnd(h: HWND, l: LPARAM) -> BOOL {
    // SAFETY: l is the &mut Vec<i64> passed by the enumerating caller.
    let v = unsafe { &mut *(l.0 as *mut Vec<i64>) };
    v.push(hval(h));
    BOOL(1)
}

fn top_level_handles() -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    // SAFETY: the callback only pushes into `out`, alive for the call.
    let _ = unsafe { EnumWindows(Some(collect_hwnd), LPARAM(&mut out as *mut _ as isize)) };
    out
}

fn child_handles(h: HWND) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    // SAFETY: as above.
    let _ = unsafe {
        EnumChildWindows(
            Some(h),
            Some(collect_hwnd),
            LPARAM(&mut out as *mut _ as isize),
        )
    };
    out
}

/// A monitor as `Windows.Forms.Screen`.
#[derive(Debug, Clone)]
pub struct Screen {
    pub device: String,
    pub bounds: Bounds,
    pub primary: bool,
    handle: isize,
}

unsafe extern "system" fn collect_monitor(m: HMONITOR, _: HDC, _: *mut RECT, l: LPARAM) -> BOOL {
    // SAFETY: l is the &mut Vec<isize> passed by `screens`.
    let v = unsafe { &mut *(l.0 as *mut Vec<isize>) };
    v.push(m.0 as isize);
    BOOL(1)
}

fn monitor_info(m: isize) -> Option<Screen> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: MONITORINFOEXW starts with MONITORINFO and cbSize says EX.
    let ok = unsafe {
        GetMonitorInfoW(
            HMONITOR(m as *mut c_void),
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
    };
    if !ok.as_bool() {
        return None;
    }
    let r = info.monitorInfo.rcMonitor;
    let len = info
        .szDevice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szDevice.len());
    Some(Screen {
        device: String::from_utf16_lossy(&info.szDevice[..len]),
        bounds: Bounds {
            x: i64::from(r.left),
            y: i64::from(r.top),
            width: i64::from(r.right - r.left),
            height: i64::from(r.bottom - r.top),
        },
        primary: info.monitorInfo.dwFlags & 1 != 0,
        handle: m,
    })
}

/// `Screen.AllScreens` (EnumDisplayMonitors order).
pub fn screens() -> Vec<Screen> {
    let mut hs: Vec<isize> = Vec::new();
    // SAFETY: the callback only pushes into `hs`.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut hs as *mut _ as isize),
        )
    };
    hs.into_iter().filter_map(monitor_info).collect()
}

fn screen_contains(b: &Bounds, x: i64, y: i64) -> bool {
    x >= b.x && x < b.x + b.width && y >= b.y && y < b.y + b.height
}

/// Process names and start times for one request.
struct Procs {
    names: HashMap<u32, String>,
    starts: HashMap<u32, Option<String>>,
}

impl Procs {
    fn new() -> Self {
        Self {
            names: crate::platform::process_names(),
            starts: HashMap::new(),
        }
    }

    fn name(&self, pid: u32) -> Option<String> {
        self.names.get(&pid).cloned()
    }

    fn start(&mut self, pid: u32) -> Option<String> {
        self.starts
            .entry(pid)
            .or_insert_with(|| crate::platform::proc_start(pid))
            .clone()
    }
}

/// `WinInfo`.
#[derive(Debug, Clone)]
pub struct WinInfo {
    pub hwnd: i64,
    pub pid: u32,
    pub process: Option<String>,
    pub host_process: Option<String>,
    pub process_start_utc: Option<String>,
    pub title: String,
    pub class: String,
    pub bounds: Bounds,
    pub monitor: Option<usize>,
    pub state: &'static str,
    pub is_foreground: bool,
    pub excluded: Option<&'static str>,
}

impl WinInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "hwnd": self.hwnd,
            "pid": self.pid,
            "process": self.process,
            "host_process": self.host_process,
            "process_start_utc": self.process_start_utc,
            "title": if self.excluded.is_some() { Value::Null } else { json!(self.title) },
            "class": self.class,
            "bounds": self.bounds.to_json(),
            "monitor": self.monitor,
            "state": self.state,
            "is_foreground": self.is_foreground,
            "excluded": self.excluded,
        })
    }
}

/// `HostedApp`: the UWP app pid behind an ApplicationFrameHost window.
fn hosted_app(h: HWND) -> Option<u32> {
    child_handles(h)
        .into_iter()
        .map(hwnd)
        .find(|k| class_of(*k) == "Windows.UI.Core.CoreWindow")
        .map(pid_of)
}

fn win_info(h: HWND, policy: &Policy, procs: &mut Procs, scr: &[Screen]) -> WinInfo {
    let mut pid = pid_of(h);
    let mut pn = procs.name(pid);
    let mut host = None;
    if pn.as_deref() == Some("ApplicationFrameHost")
        && let Some(ap) = hosted_app(h).filter(|p| *p != 0)
    {
        host = pn.take();
        pid = ap;
        pn = procs.name(ap);
    }
    let t = title(h);
    let excluded = policy.excluded(pn.as_deref(), &t);
    // SAFETY: plain Win32 calls.
    let state = unsafe {
        if IsIconic(h).as_bool() {
            "minimized"
        } else if IsZoomed(h).as_bool() {
            "maximized"
        } else {
            "normal"
        }
    };
    let r = rect_of(h);
    let cx = r.x + ps_int(r.width as f64 / 2.0);
    let cy = r.y + ps_int(r.height as f64 / 2.0);
    let monitor = scr
        .iter()
        .enumerate()
        .filter(|(_, s)| screen_contains(&s.bounds, cx, cy))
        .map(|(i, _)| i)
        .next_back();
    let fg = foreground();
    WinInfo {
        hwnd: hval(h),
        pid,
        process: pn,
        host_process: host,
        process_start_utc: procs.start(pid),
        title: t,
        class: class_of(h),
        bounds: r,
        monitor,
        state,
        is_foreground: fg == h || root_of(fg) == h,
        excluded,
    }
}

/// `TopWindows`: visible, uncloaked, non-tool top-level windows.
fn top_windows(policy: &Policy, untitled: bool, procs: &mut Procs, scr: &[Screen]) -> Vec<WinInfo> {
    let mut out = Vec::new();
    for v in top_level_handles() {
        let h = hwnd(v);
        // SAFETY: plain Win32 calls on a possibly stale handle (they fail soft).
        let skip = unsafe {
            !IsWindowVisible(h).as_bool()
                || cloaked(h)
                || (GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0) != 0
        };
        if skip {
            continue;
        }
        if !untitled && title(h).is_empty() {
            continue;
        }
        out.push(win_info(h, policy, procs, scr));
    }
    out
}

fn ieq(a: Option<&str>, b: &str) -> bool {
    a.is_some_and(|a| a.to_lowercase() == b.to_lowercase())
}

fn title_has(w: &WinInfo, needle: &str) -> bool {
    w.excluded.is_none()
        && !w.title.is_empty()
        && super::render::contains_ignore_case(&w.title, needle)
}

/// `ForegroundInfo`.
fn foreground_info(procs: &Procs) -> Value {
    let h = foreground();
    let mut r = root_of(h);
    if r.0.is_null() {
        r = h;
    }
    json!({"hwnd": hval(h), "root_hwnd": hval(r), "process": procs.name(pid_of(h)), "title": title(r)})
}

/// `IdleMs`.
fn idle_ms() -> i64 {
    let mut li = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: plain Win32 calls with an out struct.
    unsafe {
        let _ = GetLastInputInfo(&mut li);
        i64::from(GetTickCount().wrapping_sub(li.dwTime) as i32)
    }
}

/// `SessionLocked`: WTSINFOEXW level 1 SessionFlags (offset 16) == LOCK.
pub fn session_locked() -> Value {
    let mut buf = PWSTR::null();
    let mut n = 0u32;
    // SAFETY: WTS allocates the buffer; freed below.
    unsafe {
        if WTSQuerySessionInformationW(
            Some(HANDLE::default()),
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buf,
            &mut n,
        )
        .is_err()
            || buf.is_null()
        {
            return Value::Null;
        }
        let flags = if n >= 20 {
            Some(std::ptr::read_unaligned(
                (buf.0 as *const u8).add(16) as *const i32
            ))
        } else {
            None
        };
        WTSFreeMemory(buf.0 as *mut c_void);
        flags.map_or(Value::Null, |f| json!(f == 0))
    }
}

fn monitors_json(scr: &[Screen]) -> Value {
    Value::Array(
        scr.iter()
            .enumerate()
            .map(|(i, s)| {
                json!({"index": i, "device": s.device, "x": s.bounds.x, "y": s.bounds.y,
                       "width": s.bounds.width, "height": s.bounds.height, "primary": s.primary})
            })
            .collect(),
    )
}

/// `list_windows` operation.
pub fn list_windows(
    policy: &Policy,
    process: Option<&str>,
    title_contains: Option<&str>,
    include_untitled: bool,
    limit: usize,
) -> Value {
    let mut procs = Procs::new();
    let scr = screens();
    let mut all = top_windows(policy, include_untitled, &mut procs, &scr);
    if let Some(p) = process.filter(|p| !p.is_empty()) {
        all.retain(|w| ieq(w.process.as_deref(), p));
    }
    if let Some(t) = title_contains.filter(|t| !t.is_empty()) {
        all.retain(|w| title_has(w, t));
    }
    let excluded = all.iter().filter(|w| w.excluded.is_some()).count();
    json!({
        "windows": all.iter().take(limit).map(WinInfo::to_json).collect::<Vec<_>>(),
        "count": all.len().min(limit),
        "total": all.len(),
        "excluded_count": excluded,
        "foreground": foreground_info(&procs),
        "user_idle_ms": idle_ms(),
        "session_locked": session_locked(),
        "monitors": monitors_json(&scr),
    })
}

/// Window selector: `{hwnd}` or `{process,title_contains}`, plus the
/// identity checks `{pid, process_start_utc}` of a snapshot record.
#[derive(Debug, Clone, Default)]
pub struct Selector {
    pub hwnd: Option<i64>,
    pub process: Option<String>,
    pub title_contains: Option<String>,
    pub pid: Option<i64>,
    pub process_start_utc: Option<String>,
}

impl Selector {
    pub fn from_json(v: &Value) -> Self {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        Self {
            hwnd: v.get("hwnd").and_then(Value::as_i64).filter(|h| *h != 0),
            process: s("process"),
            title_contains: s("title_contains"),
            pid: v.get("pid").and_then(Value::as_i64).filter(|p| *p != 0),
            process_start_utc: s("process_start_utc"),
        }
    }
}

/// `ResolveWindow` without the UIA element.
fn resolve_window(
    sel: &Selector,
    policy: &Policy,
    procs: &mut Procs,
    scr: &[Screen],
) -> Result<WinInfo, UiError> {
    let h = if let Some(hw) = sel.hwnd {
        let h = hwnd(hw);
        // SAFETY: plain Win32 call.
        if !unsafe { IsWindow(Some(h)) }.as_bool() {
            return Err(UiError::new("window_gone", "hwnd is not a window anymore"));
        }
        h
    } else {
        let tc = sel.title_contains.as_deref();
        let cands: Vec<WinInfo> = top_windows(policy, false, procs, scr)
            .into_iter()
            .filter(|w| {
                sel.process
                    .as_deref()
                    .is_none_or(|p| ieq(w.process.as_deref(), p))
            })
            .filter(|w| tc.is_none_or(|t| title_has(w, t)))
            .collect();
        if cands.is_empty() {
            return Err(UiError::new(
                "window_not_found",
                "no visible top-level window matches process/title",
            ));
        }
        if cands.len() > 1 {
            let shown: Vec<String> = cands
                .iter()
                .take(5)
                .map(|w| {
                    format!(
                        "{}={}:{}",
                        w.hwnd,
                        w.process.as_deref().unwrap_or(""),
                        if w.excluded.is_some() { "" } else { &w.title }
                    )
                })
                .collect();
            return Err(UiError::new(
                "window_ambiguous",
                format!(
                    "{} windows match; use hwnd: {}",
                    cands.len(),
                    shown.join("; ")
                ),
            ));
        }
        hwnd(cands[0].hwnd)
    };
    let info = win_info(h, policy, procs, scr);
    if let Some(ex) = info.excluded {
        return Err(UiError::new(
            "window_excluded",
            format!("policy: {ex} ({})", info.process.as_deref().unwrap_or("")),
        ));
    }
    if let Some(p) = sel.pid
        && p != i64::from(info.pid)
    {
        return Err(UiError::new(
            "window_identity_changed",
            "same hwnd now belongs to another process",
        ));
    }
    if let (Some(want), Some(have)) = (&sel.process_start_utc, &info.process_start_utc)
        && want != have
    {
        return Err(UiError::new(
            "window_identity_changed",
            "process restarted since snapshot",
        ));
    }
    Ok(info)
}

// ---------------------------------------------------------------- snapshot

pub struct SnapReq {
    pub window: Selector,
    pub scope: String,
    pub anchor: Option<Anchor>,
    pub max_nodes: usize,
    pub max_depth: usize,
    pub budget_ms: u64,
    pub include_offscreen: bool,
    pub name_contains: Option<String>,
}

pub struct SnapRaw {
    pub window: Value,
    pub scope_root: Value,
    pub nodes: Vec<Node>,
    pub stats: Value,
    pub foreground: Value,
    pub user_idle_ms: i64,
}

/// `ResolveRef`: the element whose RuntimeId equals the anchor's.
///
/// The window root is resolved first and without the role/name search
/// (D35): its Name is the window title, which apps change while they open
/// (restored tabs, "(loading)" suffixes), so a `ControlType+Name` search
/// would no longer match and `activate` on `e1` would fail with
/// `ref_stale`. The window identity (hwnd + pid + process
/// start) has already been verified by `resolve_window`.
fn resolve_ref(
    u: &Uia,
    win_el: &IUIAutomationElement,
    a: &Anchor,
) -> Result<(IUIAutomationElement, usize), UiError> {
    let root_rid = runtime_id(win_el);
    if a.window_root || (!a.runtime_id.is_empty() && root_rid == a.runtime_id) {
        return Ok((u.updated(win_el)?, 1));
    }
    let mut cond = u.prop_cond(UIA_ControlTypePropertyId, Var::int(a.control_type_id))?;
    if let Some(id) = &a.automation_id {
        cond = u.and(
            &cond,
            &u.prop_cond(UIA_AutomationIdPropertyId, Var::string(id))?,
        )?;
    } else if let Some(n) = &a.name {
        cond = u.and(&cond, &u.prop_cond(UIA_NamePropertyId, Var::string(n))?)?;
    }
    let cands = u.find_all(win_el, TreeScope_Subtree, &cond)?;
    for c in &cands {
        if runtime_id(c) == a.runtime_id {
            return Ok((c.clone(), cands.len()));
        }
    }
    if !cands.is_empty() {
        return Err(UiError::new(
            "ref_stale",
            format!(
                "element with same role/name exists ({}) but RuntimeId changed; take a new snapshot",
                cands.len()
            ),
        ));
    }
    Err(UiError::new(
        "ref_stale",
        "element no longer exists in this window; take a new snapshot",
    ))
}

/// `snapshot` operation (runs on the UIA thread).
pub fn snapshot(u: &Uia, policy: &Policy, req: &SnapReq) -> Result<SnapRaw, UiError> {
    let mut procs = Procs::new();
    let scr = screens();
    let info = resolve_window(&req.window, policy, &mut procs, &scr)?;
    let win_el = u.element_from_hwnd(info.hwnd)?;
    let sw = Instant::now();
    let mut root = win_el.clone();
    if req.scope == "web_document" {
        let doc = u.prop_cond(
            UIA_ControlTypePropertyId,
            Var::int(UIA_DocumentControlTypeId.0),
        )?;
        let rwa = u.and(
            &doc,
            &u.prop_cond(UIA_AutomationIdPropertyId, Var::string("RootWebArea"))?,
        )?;
        root = u
            .find_first(&win_el, &rwa)
            .or_else(|| u.find_first(&win_el, &doc))
            .ok_or_else(|| {
                UiError::new(
                    "web_document_not_found",
                    "no Document/RootWebArea under this window",
                )
            })?;
    } else if req.scope == "ref" {
        let a = req
            .anchor
            .as_ref()
            .ok_or_else(|| UiError::new("invalid_arguments", "scope=ref needs an anchor"))?;
        root = resolve_ref(u, &win_el, a)?.0;
    }
    let root = u.updated(&root)?;
    let cond = if req.include_offscreen {
        &u.cond_all
    } else {
        &u.cond_visible
    };
    let mut classifier = Classifier::new(req.name_contains.clone());
    let mut nodes: Vec<Node> = Vec::new();
    let (mut n, mut calls, mut depth_max) = (0usize, 0usize, 0usize);
    let mut reason = "exhausted";
    let mut q = std::collections::VecDeque::new();
    q.push_back((root, 0usize));
    while let Some((e, d)) = q.pop_front() {
        if n >= req.max_nodes {
            reason = "node_limit";
            break;
        }
        if sw.elapsed().as_millis() as u64 >= req.budget_ms {
            reason = "time_budget";
            break;
        }
        n += 1;
        nodes.push(classifier.node(raw_node(&e), d));
        if d >= req.max_depth {
            if reason == "exhausted" {
                reason = "depth_limit";
            }
            continue;
        }
        let Ok(kids) = u.find_all(&e, TreeScope_Children, cond) else {
            continue;
        };
        calls += 1;
        for k in kids {
            depth_max = depth_max.max(d + 1);
            q.push_back((k, d + 1));
        }
    }
    // The queue check precedes the limits in the worker: a limit only stops
    // the walk when something was left to visit.
    let scope_root = nodes
        .first()
        .map(|r| json!({"role": r.role, "name": r.name}))
        .unwrap_or(Value::Null);
    let truncated = reason != "exhausted" && reason != "depth_limit";
    Ok(SnapRaw {
        window: info.to_json(),
        scope_root,
        stats: json!({"nodes": n, "find_calls": calls, "depth": depth_max,
                      "ms": sw.elapsed().as_millis() as u64, "stop_reason": reason, "truncated": truncated}),
        nodes,
        foreground: foreground_info(&procs),
        user_idle_ms: idle_ms(),
    })
}

// ---------------------------------------------------------------- action

pub struct ActReq {
    pub window: Selector,
    pub anchor: Anchor,
    pub action: String,
    pub value: Option<Value>,
    pub scroll: Option<Value>,
    pub restore_foreground: bool,
    pub settle_ms: u64,
}

/// `[string]$request.value` of a JSON string or number.
fn value_text(v: &Option<Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// `action` operation (runs on the UIA thread). Every worker error of an
/// action reports `may_have_changed=true`.
pub fn action(u: &Uia, policy: &Policy, req: &ActReq) -> Result<Value, UiError> {
    action_inner(u, policy, req).map_err(|e| e.changed(true))
}

/// `activate`: bring the target to the foreground without foreground
/// hacks — restore if minimised, let the app activate itself by focusing
/// the window element and then a keyboard-focusable control (UIA's client
/// proxies unlock SetForegroundWindow with a dummy key), then a plain
/// `SetForegroundWindow`. Each step is verified; nothing is faked.
fn activate(
    u: &Uia,
    win_el: &IUIAutomationElement,
    e: &IUIAutomationElement,
    target: HWND,
) -> Result<Value, UiError> {
    let is_fg = || {
        let f = foreground();
        f == target || root_of(f) == target
    };
    let ms = |m: u64| std::thread::sleep(Duration::from_millis(m));
    // SAFETY: COM/Win32 calls on the owning UIA thread.
    unsafe {
        if let Ok(wp) =
            win_el.GetCurrentPatternAs::<IUIAutomationWindowPattern>(UIA_WindowPatternId)
            && wp.CurrentWindowVisualState().ok() == Some(WindowVisualState_Minimized)
        {
            let _ = wp.SetWindowVisualState(WindowVisualState_Normal);
            ms(400);
        }
        let mut steps: Vec<String> = Vec::new();
        if is_fg() {
            steps.push("already_foreground".into());
        }
        if !is_fg() {
            let _ = e.SetFocus();
            ms(150);
            steps.push("setfocus_window".into());
        }
        if !is_fg() {
            let cond = u.and(
                &u.prop_cond(UIA_IsKeyboardFocusablePropertyId, Var::boolean(true))?,
                &u.and(
                    &u.prop_cond(UIA_IsOffscreenPropertyId, Var::boolean(false))?,
                    &u.prop_cond(UIA_IsEnabledPropertyId, Var::boolean(true))?,
                )?,
            )?;
            if let Some(f) = u.find_first(win_el, &cond) {
                let _ = f.SetFocus();
                ms(200);
                let ct = f.CurrentControlType().map(|c| c.0).unwrap_or(0);
                steps.push(format!("setfocus_child:{}", role_name(ct)));
            }
        }
        if !is_fg() {
            let _ = SetForegroundWindow(target);
            ms(200);
            steps.push("setforegroundwindow".into());
        }
        if !is_fg() {
            return Err(UiError::new(
                "activation_denied",
                format!(
                    "Windows did not grant the foreground after: {}. The user input lock is active; ask the user to click the window or use invoke/set_value which do not need the foreground",
                    steps.join(",")
                ),
            ));
        }
        Ok(json!(steps))
    }
}

fn action_inner(u: &Uia, policy: &Policy, req: &ActReq) -> Result<Value, UiError> {
    let mut procs = Procs::new();
    let scr = screens();
    let info = resolve_window(&req.window, policy, &mut procs, &scr)?;
    let win_el = u.element_from_hwnd(info.hwnd)?;
    let fg_before = foreground_info(&procs);
    let prev_fg = foreground();
    let idle_before = idle_ms();
    let (e, candidates) = resolve_ref(u, &win_el, &req.anchor)?;
    let pats = patterns(&e);
    let before = state_of(&e, &pats);
    if !cached_bool(&e, UIA_IsEnabledPropertyId) {
        return Err(UiError::new("element_disabled", "target is disabled"));
    }
    let act = req.action.as_str();
    let mut activation_steps = Value::Null;
    // A minimized window reports IsOffscreen; `activate` restores it (D35).
    let root_activate = act == "activate" && req.anchor.window_root;
    if cached_bool(&e, UIA_IsOffscreenPropertyId)
        && !matches!(act, "set_focus" | "scroll")
        && !root_activate
    {
        return Err(UiError::new(
            "element_offscreen",
            "target is offscreen; scroll it into view or take a new snapshot",
        ));
    }
    let need = match act {
        "invoke" => Some("invoke"),
        "set_value" => Some("value"),
        "toggle" => Some("toggle"),
        "select" => Some("select"),
        "expand" | "collapse" => Some("expand"),
        "scroll" => Some("scroll"),
        "close_window" => Some("window"),
        "set_range" => Some("range"),
        _ => None,
    };
    if let Some(need) = need
        && !pats.contains(&need)
    {
        return Err(UiError::new(
            "pattern_unavailable",
            format!(
                "action '{act}' needs pattern '{need}'; element offers: {}",
                pats.join(",")
            ),
        ));
    }
    // SAFETY: COM calls on the owning thread.
    unsafe {
        match act {
            "invoke" => e
                .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                .ui()?
                .Invoke()
                .ui(),
            "set_value" => {
                let vp = e
                    .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                    .ui()?;
                if vp.CurrentIsReadOnly().ui()?.as_bool() {
                    return Err(UiError::new(
                        "element_read_only",
                        "ValuePattern is read-only",
                    ));
                }
                vp.SetValue(&BSTR::from(value_text(&req.value))).ui()
            }
            "toggle" => e
                .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                .ui()?
                .Toggle()
                .ui(),
            "select" => e
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
                .ui()?
                .Select()
                .ui(),
            "expand" | "collapse" => {
                let p = e
                    .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )
                    .ui()?;
                if act == "expand" {
                    p.Expand()
                } else {
                    p.Collapse()
                }
                .ui()
            }
            "scroll" => {
                let sp = e
                    .GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
                    .ui()?;
                let s = req.scroll.clone().unwrap_or(Value::Null);
                let page = s["amount"].as_str() == Some("page");
                let (inc, dec) = if page {
                    (ScrollAmount_LargeIncrement, ScrollAmount_LargeDecrement)
                } else {
                    (ScrollAmount_SmallIncrement, ScrollAmount_SmallDecrement)
                };
                let no = ScrollAmount_NoAmount;
                let times = s["times"].as_i64().unwrap_or(1).max(0);
                let mut r = Ok(());
                for _ in 0..times {
                    r = match s["direction"].as_str() {
                        Some("down") => sp.Scroll(no, inc),
                        Some("up") => sp.Scroll(no, dec),
                        Some("right") => sp.Scroll(inc, no),
                        Some("left") => sp.Scroll(dec, no),
                        _ => Ok(()),
                    };
                    if r.is_err() {
                        break;
                    }
                }
                r.ui()
            }
            "set_range" => {
                let text = value_text(&req.value);
                // `[double]$request.value`: null and "" convert to 0.
                let v = match &req.value {
                    Some(Value::Number(n)) => n.as_f64(),
                    _ if text.trim().is_empty() => Some(0.0),
                    _ => text.trim().parse::<f64>().ok(),
                }
                .ok_or_else(|| {
                    UiError::new(
                        "io_error",
                        format!(
                            "Cannot convert value \"{text}\" to type \"System.Double\". Error: \"Input string was not in a correct format.\""
                        ),
                    )
                })?;
                e.GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
                    .ui()?
                    .SetValue(v)
                    .ui()
            }
            "close_window" => e
                .GetCurrentPatternAs::<IUIAutomationWindowPattern>(UIA_WindowPatternId)
                .ui()?
                .Close()
                .ui(),
            "set_focus" => e.SetFocus().ui(),
            // The hands-free gate is checked by the caller (ui::call).
            "activate" => {
                activation_steps = activate(u, &win_el, &e, hwnd(info.hwnd))?;
                Ok(())
            }
            other => Err(UiError::new("invalid_action", other)),
        }
    }?;
    std::thread::sleep(Duration::from_millis(req.settle_ms));
    let after = match resolve_ref(u, &win_el, &req.anchor) {
        Ok((e2, _)) => {
            let mut s = state_of(&e2, &patterns(&e2));
            s.insert("exists".into(), json!(true));
            s.insert(
                "enabled".into(),
                json!(cached_bool(&e2, UIA_IsEnabledPropertyId)),
            );
            s.insert("name".into(), json!(cached_str(&e2, UIA_NamePropertyId)));
            Value::Object(s)
        }
        Err(_) => json!({"exists": false}),
    };
    let procs = Procs::new();
    let mut fg_after = foreground_info(&procs);
    let fg_after_h = hwnd(fg_after["hwnd"].as_i64().unwrap_or(0));
    let target = hwnd(info.hwnd);
    let stolen = fg_after["hwnd"] != fg_before["hwnd"]
        && (root_of(fg_after_h) == target || fg_after_h == target);
    let mut restored = Value::Null;
    if req.restore_foreground
        && stolen
        && fg_before["hwnd"].as_i64().unwrap_or(0) != 0
        && act != "activate"
    {
        // SAFETY: plain Win32 calls.
        let ok = unsafe { SetForegroundWindow(prev_fg) }.as_bool();
        std::thread::sleep(Duration::from_millis(150));
        restored = json!(ok && foreground() == prev_fg);
        fg_after = foreground_info(&procs);
    }
    Ok(json!({
        "applied": true,
        "executor": "uia_pattern",
        "action": act,
        "activation_steps": activation_steps,
        "before": before,
        "after": after,
        "foreground_changed": fg_after["hwnd"] != fg_before["hwnd"],
        "foreground_before": fg_before,
        "foreground_after": fg_after,
        "foreground_taken_by_target": stolen,
        "foreground_restored": restored,
        "user_idle_ms": idle_before,
        "candidates_with_same_identity": candidates,
    }))
}

// ---------------------------------------------------------------- wait

/// `windows_wait_until` conditions: server-side polling.
pub fn wait_until(u: &Uia, policy: &Policy, req: &Value) -> Result<Value, UiError> {
    let cond = req["condition"].as_str().unwrap_or("").to_string();
    let poll = req["poll_ms"].as_u64().unwrap_or(400).max(100);
    let timeout = req["timeout_ms"].as_u64().unwrap_or(15_000);
    let sw = Instant::now();
    let deadline = sw + Duration::from_millis(timeout);
    let want_hwnd = req["hwnd"].as_i64().filter(|h| *h != 0);
    let process = req["process"].as_str().filter(|s| !s.is_empty());
    let title_c = req["title_contains"].as_str().filter(|s| !s.is_empty());
    let exclude: Vec<i64> = req["exclude_hwnds"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let elem_cond = if cond.starts_with("element_") {
        Some(
            match req["automation_id"].as_str().filter(|s| !s.is_empty()) {
                Some(id) => u.prop_cond(UIA_AutomationIdPropertyId, Var::string(id))?,
                // SAFETY: COM call on the owning thread.
                None => unsafe { u.uia.CreateTrueCondition() }.ui()?,
            },
        )
    } else {
        None
    };
    let name_c = req["name_contains"].as_str().filter(|s| !s.is_empty());
    let mut polls = 0u64;
    let mut met;
    let mut obs;
    loop {
        polls += 1;
        let mut procs = Procs::new();
        let scr = screens();
        match cond.as_str() {
            "window_appears" | "window_disappears" => {
                let all: Vec<WinInfo> = top_windows(policy, false, &mut procs, &scr)
                    .into_iter()
                    .filter(|w| want_hwnd.is_none_or(|h| w.hwnd == h))
                    .filter(|w| process.is_none_or(|p| ieq(w.process.as_deref(), p)))
                    .filter(|w| !exclude.contains(&w.hwnd))
                    .filter(|w| title_c.is_none_or(|t| title_has(w, t)))
                    .collect();
                met = if cond == "window_appears" {
                    !all.is_empty()
                } else {
                    all.is_empty()
                };
                obs = json!({
                    "matches": all.len(),
                    "sample": all.iter().take(3).map(|w| json!({"hwnd": w.hwnd, "process": w.process, "title": w.title})).collect::<Vec<_>>(),
                    "foreground": foreground_info(&procs),
                    "user_idle_ms": idle_ms(),
                });
            }
            "foreground_is" => {
                let fg = foreground_info(&procs);
                met = want_hwnd.is_some_and(|h| {
                    fg["root_hwnd"].as_i64() == Some(h) || fg["hwnd"].as_i64() == Some(h)
                }) || process.is_some_and(|p| ieq(fg["process"].as_str(), p));
                obs = json!({"foreground": fg, "user_idle_ms": idle_ms()});
            }
            "element_appears" | "element_disappears" => {
                match resolve_window(
                    &Selector::from_json(&req["window"]),
                    policy,
                    &mut procs,
                    &scr,
                ) {
                    Err(e) if matches!(e.kind.as_str(), "window_not_found" | "window_gone") => {
                        obs = json!({"window": e.kind, "matches": 0});
                        met = cond == "element_disappears";
                    }
                    Err(e) => return Err(e),
                    Ok(info) => {
                        let el = u.element_from_hwnd(info.hwnd)?;
                        let els = u.find_all(
                            &el,
                            TreeScope_Descendants,
                            elem_cond.as_ref().expect("element condition"),
                        )?;
                        let mut hits = Vec::new();
                        for e in &els {
                            let nm = cached_str(e, UIA_NamePropertyId);
                            if name_c.is_some_and(|n| !super::render::contains_ignore_case(&nm, n))
                            {
                                continue;
                            }
                            hits.push(json!({
                                "role": role_name(control_type(e)),
                                "name": nm.chars().take(80).collect::<String>(),
                                "automation_id": cached_str(e, UIA_AutomationIdPropertyId),
                            }));
                            if hits.len() >= 50 {
                                break;
                            }
                        }
                        met = if cond == "element_appears" {
                            !hits.is_empty()
                        } else {
                            hits.is_empty()
                        };
                        obs = json!({
                            "window": {"hwnd": info.hwnd, "process": info.process, "title": info.title},
                            "matches": hits.len(),
                            "sample": hits.iter().take(5).cloned().collect::<Vec<_>>(),
                            "scanned": els.len(),
                        });
                    }
                }
            }
            "user_idle_above" => {
                let idle = idle_ms();
                met = idle >= req["idle_ms"].as_i64().unwrap_or(0);
                obs = json!({"user_idle_ms": idle});
            }
            "time" => {
                met = false;
                obs = json!({});
            }
            other => {
                return Err(UiError::new(
                    "invalid_arguments",
                    format!("unknown condition '{other}'"),
                ));
            }
        }
        if met {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let left = deadline - now;
        std::thread::sleep(left.min(Duration::from_millis(poll)));
    }
    if cond == "time" {
        met = true;
    }
    Ok(json!({
        "condition": cond,
        "met": met,
        "elapsed_ms": sw.elapsed().as_millis() as i64,
        "polls": polls,
        "observed": obs,
    }))
}

// ---------------------------------------------------------------- capture

/// A 32-bit top-down DIB selected into a memory DC.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: u32,
    h: u32,
}

impl Dib {
    fn new(w: u32, h: u32) -> Result<Self, UiError> {
        let fail = |what: &str| UiError::new("capture_backend_failed", format!("{what} failed"));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: GDI object creation; released in Drop.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err(fail("CreateCompatibleDC"));
            }
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bmp = match CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) if !bits.is_null() => b,
                _ => {
                    let _ = DeleteDC(dc);
                    return Err(fail("CreateDIBSection"));
                }
            };
            let old = SelectObject(dc, bmp.into());
            Ok(Self {
                dc,
                bmp,
                old,
                bits: bits as *mut u8,
                w,
                h,
            })
        }
    }

    fn frame(&self) -> Frame {
        let len = self.w as usize * self.h as usize * 4;
        // SAFETY: the DIB section holds exactly w*h*4 bytes.
        let mut bgra = unsafe { std::slice::from_raw_parts(self.bits, len) }.to_vec();
        // PrintWindow/BitBlt leave alpha undefined; the PNG is opaque RGB.
        for p in bgra.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
        Frame {
            width: self.w,
            height: self.h,
            bgra,
        }
    }

    /// `Graphics.CopyFromScreen(x, y, 0, 0, size)`.
    fn copy_from_screen(&self, x: i64, y: i64) -> bool {
        // SAFETY: screen DC acquired and released here.
        unsafe {
            let screen = GetDC(None);
            let ok = BitBlt(
                self.dc,
                0,
                0,
                self.w as i32,
                self.h as i32,
                Some(screen),
                x as i32,
                y as i32,
                SRCCOPY,
            )
            .is_ok();
            ReleaseDC(None, screen);
            ok
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: objects created in `new`.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// `capture_window` operation.
pub fn capture_window(
    policy: &Policy,
    sel: &Selector,
    max_width: u32,
) -> Result<(Value, String), UiError> {
    let mut procs = Procs::new();
    let scr = screens();
    let info = resolve_window(sel, policy, &mut procs, &scr)?;
    let h = hwnd(info.hwnd);
    let r = rect_of(h);
    if r.width <= 0 || r.height <= 0 {
        return Err(UiError::new("capture_backend_failed", "empty window rect"));
    }
    // SAFETY: plain Win32 call.
    if unsafe { IsIconic(h) }.as_bool() {
        return Err(UiError::new(
            "window_minimized",
            "PrintWindow cannot render a minimized window; restore it first (windows_ui_action close_window/set_focus do not restore)",
        ));
    }
    let dib = Dib::new(r.width as u32, r.height as u32)?;
    // SAFETY: PrintWindow into our memory DC (PW_RENDERFULLCONTENT).
    let ok = unsafe { PrintWindow(h, dib.dc, PRINT_WINDOW_FLAGS(2)) }.as_bool();
    let mut method = "printwindow";
    let mut frame = dib.frame();
    let mut nb = frame.non_black_ratio();
    if !ok || nb < 0.02 {
        dib.copy_from_screen(r.x, r.y);
        method = "bitblt";
        frame = dib.frame();
        nb = frame.non_black_ratio();
    }
    let (capture, png) = finish(&frame, max_width, method, r.x, r.y, nb)?;
    Ok((json!({"window": info.to_json(), "capture": capture}), png))
}

/// `capture_monitor` operation.
pub fn capture_monitor(
    policy: &Policy,
    monitor: &str,
    max_width: u32,
    redact: bool,
) -> Result<(Value, String), UiError> {
    let scr = screens();
    let s = if monitor == "primary" {
        scr.iter().find(|s| s.primary).cloned()
    } else if monitor == "foreground" {
        // SAFETY: plain Win32 call.
        let m = unsafe { MonitorFromWindow(foreground(), MONITOR_DEFAULTTONEAREST) };
        scr.iter()
            .find(|s| s.handle == m.0 as isize)
            .cloned()
            .or_else(|| scr.iter().find(|s| s.primary).cloned())
    } else {
        let i: usize = monitor
            .parse()
            .map_err(|_| UiError::new("invalid_arguments", "bad monitor"))?;
        if i >= scr.len() {
            return Err(UiError::new(
                "invalid_arguments",
                format!(
                    "monitor index {i} out of range (0..{})",
                    scr.len() as i64 - 1
                ),
            ));
        }
        Some(scr[i].clone())
    }
    .ok_or_else(|| UiError::new("capture_backend_failed", "no monitor found"))?;
    let b = s.bounds;
    let dib = Dib::new(b.width as u32, b.height as u32)?;
    if !dib.copy_from_screen(b.x, b.y) {
        return Err(UiError::new(
            "capture_backend_failed",
            "BitBlt from the screen failed",
        ));
    }
    let mut frame = dib.frame();
    let mut procs = Procs::new();
    let mut visible = Vec::new();
    let mut redacted = Vec::new();
    for w in top_windows(policy, false, &mut procs, &scr)
        .into_iter()
        .filter(|w| w.state != "minimized")
    {
        let wr = (
            w.bounds.x - b.x,
            w.bounds.y - b.y,
            w.bounds.width,
            w.bounds.height,
        );
        if !intersects(wr, (0, 0, b.width, b.height)) {
            continue;
        }
        let bounds = json!({"x": wr.0, "y": wr.1, "width": wr.2, "height": wr.3});
        if let (Some(reason), true) = (w.excluded, redact) {
            frame.fill_black(wr.0, wr.1, wr.2, wr.3);
            redacted.push(json!({"process": w.process, "reason": reason, "bounds": bounds}));
        } else {
            visible.push((w, wr));
        }
    }
    let nb = frame.non_black_ratio();
    let (capture, png) = finish(&frame, max_width, "bitblt", b.x, b.y, nb)?;
    let scale = capture["scale"].as_f64().unwrap_or(1.0);
    let sc = |v: i64| ps_int(v as f64 * scale);
    let windows: Vec<Value> = visible
        .into_iter()
        .map(|(w, wr)| {
            json!({"hwnd": w.hwnd, "process": w.process,
                   "title": if w.excluded.is_some() { Value::Null } else { json!(w.title) },
                   "bounds": {"x": sc(wr.0), "y": sc(wr.1), "width": sc(wr.2), "height": sc(wr.3)},
                   "is_foreground": w.is_foreground})
        })
        .collect();
    Ok((
        json!({
            "monitor": {"device": s.device, "x": b.x, "y": b.y, "width": b.width, "height": b.height, "primary": s.primary},
            "capture": capture,
            "windows": windows,
            "redacted": redacted,
            "foreground": foreground_info(&procs),
            "user_idle_ms": idle_ms(),
        }),
        png,
    ))
}

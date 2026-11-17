//! `windows_input` batch executor.
//!
//! The executor is platform independent: every OS effect goes through the
//! [`Injector`] trait, so the whole batch semantics (pre-flight refusals,
//! per-action checks, pacing, failure/skip bookkeeping, releasing held
//! keys/buttons) are unit tested on Linux with a recording fake that never
//! touches a desktop. `input_win` implements it with `SendInput`.
//!
//! Every injected event carries [`INPUT_MAGIC`] in `dwExtraInfo` so the
//! hands-free hooks can tell this server's input from foreign injection.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

pub use super::state::{STALE_AFTER_S, heartbeat_age, is_live};
use serde_json::{Map, Value, json};

use super::keys::{is_extended, parse_chord, vk_of};
use crate::ui::UiError;

/// `dwExtraInfo` stamped on every event this server injects.
pub const INPUT_MAGIC: usize = 0x5753_0002;
const F_MOVE: u32 = 0x0001;
const F_ABS: u32 = 0x8000;
const F_VDESK: u32 = 0x4000;
const F_NOCO: u32 = 0x2000;
const F_WHEEL: u32 = 0x0800;
const F_HWHEEL: u32 = 0x1000;
const K_EXT: u32 = 0x0001;
const K_UP: u32 = 0x0002;
const K_UNICODE: u32 = 0x0004;

/// One `INPUT` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ev {
    /// Mouse event: dx/dy (absolute 0..65535 when `F_ABS`), flags, data.
    Mouse {
        dx: i32,
        dy: i32,
        flags: u32,
        data: i32,
    },
    /// Keyboard event.
    Key { vk: u16, scan: u16, flags: u32 },
}

/// `FgInfo` of the worker.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fg {
    pub hwnd: i64,
    pub root_hwnd: i64,
    pub pid: u32,
    pub process: Option<String>,
    pub title: String,
}

impl Fg {
    pub fn to_json(&self) -> Value {
        json!({"hwnd": self.hwnd, "root_hwnd": self.root_hwnd, "pid": self.pid,
               "process": self.process, "title": self.title.chars().take(60).collect::<String>()})
    }
}

/// `VkKeyScanExW` result for one UTF-16 unit in the foreground layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutKey {
    pub vk: u16,
    /// High byte of VkKeyScanEx: 1 shift, 2 ctrl, 4 alt.
    pub mods: u8,
    pub scan: u16,
}

/// Everything the executor needs from the OS.
pub trait Injector {
    /// `SendInput`; returns how many events were accepted.
    fn send(&mut self, evs: &[Ev]) -> usize;
    fn cursor(&self) -> (i32, i32);
    fn foreground(&self) -> Fg;
    /// Integrity RID of the process owning `root_hwnd` (-1 unknown).
    fn integrity(&self, root_hwnd: i64) -> i64;
    /// Virtual desktop `(x, y, width, height)` in physical pixels.
    fn virtual_screen(&self) -> (i32, i32, i32, i32);
    /// Milliseconds on a monotonic stopwatch.
    fn now_ms(&self) -> f64;
    fn sleep_ms(&mut self, ms: u64);
    /// Sleep coarse then spin until the stopwatch reaches `target_ms`.
    fn wait_until_ms(&mut self, target_ms: f64);
    /// Hands-free `state.json` (shared read, retried).
    fn session(&self) -> Option<Value>;
    fn clipboard_get(&mut self) -> Option<String>;
    fn clipboard_set(&mut self, text: &str) -> bool;
    fn caps_lock(&self) -> bool;
    fn layout_key(&self, unit: u16) -> Option<LayoutKey>;
}

/// A batch request (already validated against the contract schema and,
/// for `capture_id`, remapped to virtual-desktop pixels).
pub struct Request<'a> {
    pub actions: &'a [Value],
    pub session_id: Option<&'a str>,
    pub expect_foreground_hwnd: Option<i64>,
    pub abort_on_foreground_change: bool,
    /// Hard budget for the whole batch.
    pub timeout_ms: f64,
}

struct Fail {
    kind: String,
    message: String,
}

fn fail<T>(kind: &str, message: impl Into<String>) -> Result<T, Fail> {
    Err(Fail {
        kind: kind.into(),
        message: message.into(),
    })
}

/// `RequireSession` of the worker.
pub fn require_session(
    state: Option<&Value>,
    expected: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Value, UiError> {
    let s = match state {
        Some(s) if s["active"].as_bool() == Some(true) => s,
        _ => {
            let by = state
                .and_then(|s| s["ended_by"].as_str())
                .filter(|b| !b.is_empty())
                .map(|b| format!(" (ended_by={b})"))
                .unwrap_or_default();
            return Err(UiError::new(
                "handsfree_required",
                format!("hands-free mode is not active{by}; call windows_handsfree start"),
            ));
        }
    };
    if heartbeat_age(Some(s), now).is_none_or(|a| a > STALE_AFTER_S) {
        return Err(UiError::new(
            "handsfree_stale",
            "session heartbeat older than 6 s; the session process may have died",
        ));
    }
    if let Some(e) = expected.filter(|e| !e.is_empty())
        && s["session_id"].as_str() != Some(e)
    {
        return Err(UiError::new(
            "handsfree_session_mismatch",
            format!(
                "active session is {}",
                s["session_id"].as_str().unwrap_or("")
            ),
        ));
    }
    Ok(s.clone())
}

/// `AbsX`/`AbsY`: pixel -> SendInput absolute coordinate over the virtual
/// desktop (works with negative origins: `v0` is the desktop's left/top).
pub fn abs_coord(p: i32, v0: i32, vsize: i32) -> i32 {
    let vsize = i64::from(vsize.max(1));
    let a = ((i64::from(p) - i64::from(v0)) * 65536) as f64 / vsize as f64;
    (a.floor() as i64 + 1).clamp(0, 65535) as i32
}

/// Inverse of [`abs_coord`]: the first pixel that maps to `a`.
pub fn abs_to_pixel(a: i32, v0: i32, vsize: i32) -> i32 {
    let vsize = i64::from(vsize.max(1));
    let n = (i64::from(a) - 1) * vsize;
    (i64::from(v0) + n.div_euclid(65536) + i64::from(n.rem_euclid(65536) != 0)) as i32
}

/// Where an absolute move would leave the cursor (dry-run cursor, D36).
/// `None` for button/wheel/relative events, which do not move it.
pub fn simulated_cursor(ev: &Ev, vs: (i32, i32, i32, i32)) -> Option<(i32, i32)> {
    match *ev {
        Ev::Mouse { dx, dy, flags, .. } if flags & F_MOVE != 0 && flags & F_ABS != 0 => {
            let (vl, vt, vw, vh) = vs;
            Some((abs_to_pixel(dx, vl, vw), abs_to_pixel(dy, vt, vh)))
        }
        _ => None,
    }
}

/// .NET `[Math]::Round` / `[int]` cast: banker's rounding.
pub fn round_even(v: f64) -> i64 {
    v.round_ties_even() as i64
}

/// Densify a drag polyline into segments of at most `step` px.
pub fn densify(path: &[(i32, i32)], step: i32) -> Vec<(i32, i32)> {
    let step = f64::from(step.max(1));
    let mut out = vec![path[0]];
    for w in path.windows(2) {
        let (x0, y0) = (f64::from(w[0].0), f64::from(w[0].1));
        let (x1, y1) = (f64::from(w[1].0), f64::from(w[1].1));
        let d = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
        let n = ((d / step).ceil() as i64).max(1);
        for j in 1..=n {
            let t = j as f64 / n as f64;
            out.push((
                round_even(x0 + (x1 - x0) * t) as i32,
                round_even(y0 + (y1 - y0) * t) as i32,
            ));
        }
    }
    out
}

/// `chars_per_second`, clamped to the contract range (5..120 in the
/// schema; the worker only enforced >= 1, this also caps at 120).
pub fn clamp_cps(v: Option<i64>) -> u32 {
    v.unwrap_or(50).clamp(1, 120) as u32
}

/// Capture pixel -> virtual desktop pixel (round half to even).
pub fn remap_point(x: f64, y: f64, ox: f64, oy: f64, scale: f64) -> (i64, i64) {
    let sc = if scale > 0.0 { scale } else { 1.0 };
    (round_even(ox + x / sc), round_even(oy + y / sc))
}

/// Remap every `x/y` and `path` of the actions through a capture record.
pub fn remap(actions: &[Value], cap: &Value) -> Vec<Value> {
    let ox = cap["origin_x"].as_f64().unwrap_or(0.0);
    let oy = cap["origin_y"].as_f64().unwrap_or(0.0);
    let sc = cap["scale"].as_f64().unwrap_or(1.0);
    actions
        .iter()
        .map(|a| {
            let mut a = a.clone();
            if let (Some(x), Some(y)) = (a["x"].as_f64(), a["y"].as_f64()) {
                let (nx, ny) = remap_point(x, y, ox, oy, sc);
                a["x"] = json!(nx);
                a["y"] = json!(ny);
            }
            if let Some(p) = a.get("path").and_then(Value::as_array) {
                let np: Vec<Value> = p
                    .iter()
                    .map(|q| {
                        let (nx, ny) = remap_point(
                            q["x"].as_f64().unwrap_or(0.0),
                            q["y"].as_f64().unwrap_or(0.0),
                            ox,
                            oy,
                            sc,
                        );
                        json!({"x": nx, "y": ny})
                    })
                    .collect();
                a["path"] = Value::Array(np);
            }
            a
        })
        .collect()
}

/// Time budget of a batch, in seconds.
pub fn batch_timeout_s(actions: &[Value]) -> u64 {
    let n = |a: &Value, k: &str| a[k].as_u64().unwrap_or(0);
    let points: u64 = actions
        .iter()
        .map(|a| {
            a["path"].as_array().map_or(0, |p| p.len() as u64)
                + a["text"]
                    .as_str()
                    .map_or(0, |t| t.encode_utf16().count() as u64)
        })
        .sum();
    let ms: u64 = actions.iter().map(|a| n(a, "ms")).sum();
    let dur: u64 = actions.iter().map(|a| n(a, "duration_ms")).sum();
    (30 + points / 20 + ms / 1000 + dur / 1000 + 3 * actions.len() as u64).min(280)
}

struct Exec<'a> {
    inj: &'a mut dyn Injector,
    vs: (i32, i32, i32, i32),
    held_buttons: BTreeSet<&'static str>,
    held_keys: BTreeSet<u16>,
    any_input: bool,
    deadline_ms: f64,
    stats: (u64, u64, bool),
}

fn btn_flags(button: &str, down: bool) -> Option<(&'static str, u32)> {
    Some(match (button, down) {
        ("left", true) => ("left", 0x0002),
        ("left", false) => ("left", 0x0004),
        ("right", true) => ("right", 0x0008),
        ("right", false) => ("right", 0x0010),
        ("middle", true) => ("middle", 0x0020),
        ("middle", false) => ("middle", 0x0040),
        _ => return None,
    })
}

impl Exec<'_> {
    fn send(&mut self, evs: &[Ev], what: &str) -> Result<(), Fail> {
        self.any_input = true;
        let n = self.inj.send(evs);
        if n != evs.len() {
            return fail("sendinput_failed", format!("{what} ({n}/{})", evs.len()));
        }
        Ok(())
    }

    fn check_deadline(&self) -> Result<(), Fail> {
        if self.inj.now_ms() > self.deadline_ms {
            return fail(
                "timeout",
                format!(
                    "input batch exceeded its {} s budget",
                    (self.deadline_ms / 1000.0) as u64
                ),
            );
        }
        Ok(())
    }

    fn move_to(&mut self, x: i32, y: i32, nocoalesce: bool) -> Result<(), Fail> {
        let (vl, vt, vw, vh) = self.vs;
        let mut f = F_MOVE | F_ABS | F_VDESK;
        if nocoalesce {
            f |= F_NOCO;
        }
        let ev = Ev::Mouse {
            dx: abs_coord(x, vl, vw),
            dy: abs_coord(y, vt, vh),
            flags: f,
            data: 0,
        };
        self.any_input = true;
        if self.inj.send(&[ev]) != 1 {
            return fail("sendinput_failed", "mouse move rejected");
        }
        Ok(())
    }

    fn btn(&mut self, button: &str, down: bool) -> Result<(), Fail> {
        let Some((name, f)) = btn_flags(button, down) else {
            return fail("invalid_arguments", format!("unknown button '{button}'"));
        };
        self.any_input = true;
        if self.inj.send(&[Ev::Mouse {
            dx: 0,
            dy: 0,
            flags: f,
            data: 0,
        }]) != 1
        {
            return fail("sendinput_failed", "mouse button rejected");
        }
        if down {
            self.held_buttons.insert(name);
        } else {
            self.held_buttons.remove(name);
        }
        Ok(())
    }

    fn key_event(&mut self, vk: u16, down: bool) -> Result<(), Fail> {
        let mut f = if down { 0 } else { K_UP };
        if is_extended(vk) {
            f |= K_EXT;
        }
        self.any_input = true;
        if self.inj.send(&[Ev::Key {
            vk,
            scan: 0,
            flags: f,
        }]) != 1
        {
            return fail("sendinput_failed", "key rejected");
        }
        Ok(())
    }

    fn chord(&mut self, spec: &str) -> Result<(), Fail> {
        let codes = parse_chord(spec).map_err(|e| Fail {
            kind: e.kind,
            message: e.message,
        })?;
        let (main, mods) = codes.split_last().expect("non-empty chord");
        for m in mods {
            self.key_event(*m, true)?;
            self.held_keys.insert(*m);
            self.inj.sleep_ms(15);
        }
        self.key_event(*main, true)?;
        self.held_keys.insert(*main);
        self.inj.sleep_ms(30);
        self.key_event(*main, false)?;
        self.held_keys.remove(main);
        self.inj.sleep_ms(15);
        let mut rel: Vec<u16> = mods.to_vec();
        rel.sort_unstable_by(|a, b| b.cmp(a));
        for m in rel {
            self.key_event(m, false)?;
            self.held_keys.remove(&m);
            self.inj.sleep_ms(15);
        }
        Ok(())
    }

    fn unicode_pair(unit: u16) -> [Ev; 2] {
        [
            Ev::Key {
                vk: 0,
                scan: unit,
                flags: K_UNICODE,
            },
            Ev::Key {
                vk: 0,
                scan: unit,
                flags: K_UNICODE | K_UP,
            },
        ]
    }

    /// `TypeText`: one SendInput per character, paced by the stopwatch.
    fn type_text(&mut self, text: &str, cps: u32, method: &str) -> Result<u64, Fail> {
        let units: Vec<u16> = text.encode_utf16().collect();
        let caps = self.inj.caps_lock();
        let use_layout = method == "layout";
        self.stats = (0, 0, caps);
        let start = self.inj.now_ms();
        let mut sent: u64 = 0;
        let mut controls: u64 = 0;
        let mut i = 0;
        while i < units.len() {
            self.check_deadline()?;
            let u = units[i];
            if u == 0x0D {
                i += 1;
                continue;
            }
            if u == 0x0A || u == 0x09 {
                let vk = if u == 0x0A { 0x0D } else { 0x09 };
                self.key_event(vk, true)?;
                self.key_event(vk, false)?;
                sent += 4;
                controls += 1;
            } else if (0xD800..0xDC00).contains(&u) && i + 1 < units.len() {
                let lo = units[i + 1];
                let mut evs = Self::unicode_pair(u).to_vec();
                evs.extend(Self::unicode_pair(lo));
                self.send(&evs, "key event rejected")?;
                i += 1;
                self.stats.1 += 1;
            } else {
                let is_letter = char::from_u32(u32::from(u)).is_some_and(char::is_alphabetic);
                let mut done = false;
                if use_layout
                    && !(caps && is_letter)
                    && let Some(k) = self.inj.layout_key(u)
                    && k.mods <= 1
                {
                    let evs: Vec<Ev> = if k.mods == 1 {
                        vec![
                            Ev::Key {
                                vk: 0x10,
                                scan: 0x2A,
                                flags: 0,
                            },
                            Ev::Key {
                                vk: k.vk,
                                scan: k.scan,
                                flags: 0,
                            },
                            Ev::Key {
                                vk: k.vk,
                                scan: k.scan,
                                flags: K_UP,
                            },
                            Ev::Key {
                                vk: 0x10,
                                scan: 0x2A,
                                flags: K_UP,
                            },
                        ]
                    } else {
                        vec![
                            Ev::Key {
                                vk: k.vk,
                                scan: k.scan,
                                flags: 0,
                            },
                            Ev::Key {
                                vk: k.vk,
                                scan: k.scan,
                                flags: K_UP,
                            },
                        ]
                    };
                    self.send(&evs, "key event rejected")?;
                    done = true;
                    self.stats.0 += 1;
                }
                if !done {
                    self.send(&Self::unicode_pair(u), "key event rejected")?;
                    self.stats.1 += 1;
                }
            }
            sent += 1;
            i += 1;
            self.inj
                .wait_until_ms(start + 1000.0 * sent as f64 / f64::from(cps.max(1)));
        }
        Ok(self.stats.0 + self.stats.1 + controls)
    }

    fn paste(&mut self, text: &str, restore: bool) -> Result<u64, Fail> {
        let prev = if restore {
            self.inj.clipboard_get()
        } else {
            None
        };
        if !self.inj.clipboard_set(text) {
            return fail("input_failed", "could not set the clipboard text");
        }
        self.inj.sleep_ms(60);
        self.chord("ctrl+v")?;
        self.inj.sleep_ms(150);
        if let Some(p) = prev {
            self.inj.sleep_ms(250);
            let _ = self.inj.clipboard_set(&p);
        }
        Ok(text.encode_utf16().count() as u64)
    }

    fn release_all(&mut self) {
        for b in std::mem::take(&mut self.held_buttons) {
            if let Some((_, f)) = btn_flags(b, false) {
                self.any_input = true;
                self.inj.send(&[Ev::Mouse {
                    dx: 0,
                    dy: 0,
                    flags: f,
                    data: 0,
                }]);
            }
        }
        for k in std::mem::take(&mut self.held_keys) {
            self.any_input = true;
            self.inj.send(&[Ev::Key {
                vk: k,
                scan: 0,
                flags: K_UP,
            }]);
        }
    }

    fn cursor_str(&self) -> String {
        let (x, y) = self.inj.cursor();
        format!("{x},{y}")
    }

    fn action(&mut self, a: &Value, detail: &mut Map<String, Value>) -> Result<(), Fail> {
        let ty = a["type"].as_str().unwrap_or("");
        let xy = a.get("x").map(|_| {
            (
                a["x"].as_i64().unwrap_or(0) as i32,
                a["y"].as_i64().unwrap_or(0) as i32,
            )
        });
        let button = a["button"].as_str().unwrap_or("left").to_string();
        match ty {
            "move" => {
                let (x, y) = xy.unwrap_or((0, 0));
                self.move_to(x, y, false)?;
                self.inj.sleep_ms(20);
                let (cx, cy) = self.inj.cursor();
                detail.insert("cursor".into(), json!(format!("{cx},{cy}")));
                if (cx - x).abs() > 2 || (cy - y).abs() > 2 {
                    return fail(
                        "cursor_placement_blocked",
                        format!("cursor landed at {cx},{cy} instead of {x},{y}"),
                    );
                }
            }
            "click" => {
                if let Some((x, y)) = xy {
                    self.move_to(x, y, false)?;
                    self.inj.sleep_ms(40);
                }
                let count = a["count"].as_i64().filter(|c| *c != 0).unwrap_or(1);
                for k in 0..count {
                    self.btn(&button, true)?;
                    self.inj.sleep_ms(30);
                    self.btn(&button, false)?;
                    if k < count - 1 {
                        self.inj.sleep_ms(80);
                    }
                }
                detail.insert("cursor".into(), json!(self.cursor_str()));
                detail.insert("button".into(), json!(button));
                detail.insert("count".into(), json!(count));
            }
            "mouse_down" => {
                if let Some((x, y)) = xy {
                    self.move_to(x, y, false)?;
                    self.inj.sleep_ms(30);
                }
                self.btn(&button, true)?;
                detail.insert("button".into(), json!(button));
            }
            "mouse_up" => {
                if let Some((x, y)) = xy {
                    self.move_to(x, y, true)?;
                    self.inj.sleep_ms(30);
                }
                self.btn(&button, false)?;
                detail.insert("button".into(), json!(button));
            }
            "drag" => {
                let pts: Vec<(i32, i32)> = a["path"]
                    .as_array()
                    .map(|p| {
                        p.iter()
                            .map(|q| {
                                (
                                    q["x"].as_i64().unwrap_or(0) as i32,
                                    q["y"].as_i64().unwrap_or(0) as i32,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if pts.len() < 2 {
                    return fail("invalid_arguments", "drag needs at least 2 points");
                }
                let step = a["step_px"].as_i64().filter(|s| *s != 0).unwrap_or(6) as i32;
                let dur = a["duration_ms"].as_i64().unwrap_or(0);
                let dense = densify(&pts, step);
                let total = dense.len();
                let interval = if dur > 0 {
                    (dur as f64 / (total.max(2) - 1) as f64).max(0.0)
                } else {
                    8.0
                };
                self.move_to(dense[0].0, dense[0].1, false)?;
                self.inj.sleep_ms(60);
                self.btn(&button, true)?;
                self.inj.sleep_ms(60);
                let t_start = self.inj.now_ms();
                for (i, p) in dense.iter().enumerate().skip(1) {
                    self.check_deadline()?;
                    self.move_to(p.0, p.1, true)?;
                    let wait = t_start + interval * i as f64 - self.inj.now_ms();
                    if wait > 0.0 {
                        self.inj.sleep_ms(round_even(wait) as u64);
                    }
                }
                self.inj.sleep_ms(40);
                self.btn(&button, false)?;
                detail.insert("points_sent".into(), json!(total));
                detail.insert("cursor".into(), json!(self.cursor_str()));
                detail.insert(
                    "duration_ms".into(),
                    json!((self.inj.now_ms() - t_start) as i64),
                );
            }
            "scroll" => {
                if let Some((x, y)) = xy {
                    self.move_to(x, y, false)?;
                    self.inj.sleep_ms(30);
                }
                let amount = a["amount"].as_i64().filter(|v| *v != 0).unwrap_or(3);
                let dir = a["direction"].as_str().unwrap_or("");
                for _ in 0..amount {
                    let flags = if matches!(dir, "up" | "down") {
                        F_WHEEL
                    } else {
                        F_HWHEEL
                    };
                    let data = if matches!(dir, "up" | "right") {
                        120
                    } else {
                        -120
                    };
                    self.any_input = true;
                    if self.inj.send(&[Ev::Mouse {
                        dx: 0,
                        dy: 0,
                        flags,
                        data,
                    }]) != 1
                    {
                        return fail("sendinput_failed", "wheel rejected");
                    }
                    self.inj.sleep_ms(25);
                }
                detail.insert("direction".into(), json!(dir));
                detail.insert("amount".into(), json!(amount));
            }
            "type" => {
                let cps = clamp_cps(a["chars_per_second"].as_i64().filter(|v| *v != 0));
                let method = a["method"].as_str().unwrap_or("unicode").to_string();
                let chars = self.type_text(a["text"].as_str().unwrap_or(""), cps, &method)?;
                detail.insert("chars".into(), json!(chars));
                detail.insert("method".into(), json!(method));
                detail.insert("keys_layout".into(), json!(self.stats.0));
                detail.insert("keys_unicode".into(), json!(self.stats.1));
                detail.insert("caps_lock".into(), json!(self.stats.2));
            }
            "paste" => {
                let chars = self.paste(
                    a["text"].as_str().unwrap_or(""),
                    a["restore_clipboard"].as_bool() == Some(true),
                )?;
                detail.insert("chars".into(), json!(chars));
                detail.insert("via".into(), json!("clipboard+ctrl+v"));
            }
            "key" => {
                let keys: Vec<String> = a["keys"]
                    .as_array()
                    .map(|k| {
                        k.iter()
                            .map(|v| v.as_str().unwrap_or("").to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                if keys.is_empty() {
                    return fail("invalid_arguments", "key needs keys[]");
                }
                for k in &keys {
                    self.check_deadline()?;
                    self.chord(k)?;
                    self.inj.sleep_ms(40);
                }
                detail.insert("chords".into(), json!(keys.len()));
            }
            "hold_key" => {
                let spec = a["key"].as_str().unwrap_or("").to_lowercase();
                let Some(vk) = vk_of(&spec) else {
                    return fail("invalid_arguments", format!("unknown key '{spec}'"));
                };
                self.key_event(vk, true)?;
                self.held_keys.insert(vk);
                self.inj.sleep_ms(a["duration_ms"].as_u64().unwrap_or(0));
                self.key_event(vk, false)?;
                self.held_keys.remove(&vk);
                detail.insert("key".into(), json!(spec));
            }
            "wait" => {
                let ms = a["ms"].as_u64().unwrap_or(0);
                self.inj.sleep_ms(ms);
                detail.insert("ms".into(), json!(ms));
            }
            other => return fail("invalid_action", other),
        }
        Ok(())
    }
}

/// Run one batch. `Err` = pre-flight refusal (nothing was injected);
/// `Ok` carries `failed`/`skipped` when an action failed mid-batch.
pub fn run_batch(inj: &mut dyn Injector, req: &Request) -> Result<Value, UiError> {
    let sess = require_session(inj.session().as_ref(), req.session_id, Utc::now())?;
    let fg0 = inj.foreground();
    if fg0.process.as_deref() == Some("LockApp") || fg0.hwnd == 0 {
        return Err(UiError::new(
            "session_locked",
            "lock screen is in the foreground",
        ));
    }
    if inj.integrity(fg0.root_hwnd) >= 0x3000 {
        return Err(UiError::new(
            "target_elevated",
            format!(
                "foreground process '{}' runs elevated; SendInput would be dropped by UIPI",
                fg0.process.as_deref().unwrap_or("")
            ),
        ));
    }
    if let Some(e) = req.expect_foreground_hwnd
        && e != fg0.root_hwnd
    {
        return Err(UiError::new(
            "foreground_mismatch",
            format!(
                "expected hwnd {e} in foreground, found {} ({})",
                fg0.root_hwnd,
                fg0.process.as_deref().unwrap_or("")
            ),
        ));
    }
    let vs = inj.virtual_screen();
    let t0 = inj.now_ms();
    let mut ex = Exec {
        deadline_ms: t0 + req.timeout_ms,
        inj,
        vs,
        held_buttons: BTreeSet::new(),
        held_keys: BTreeSet::new(),
        any_input: false,
        stats: (0, 0, false),
    };
    let mut executed = Vec::new();
    let mut failed = Value::Null;
    let mut failed_at = None;
    for (idx, a) in req.actions.iter().enumerate() {
        let ty = a["type"].as_str().unwrap_or("").to_string();
        let t_action = ex.inj.now_ms();
        let r = (|| -> Result<Map<String, Value>, Fail> {
            ex.check_deadline()?;
            if idx % 5 == 0 && idx > 0 {
                let s = ex.inj.session();
                let live = s
                    .as_ref()
                    .is_some_and(|s| s["active"].as_bool() == Some(true));
                if !live {
                    let by = s
                        .as_ref()
                        .and_then(|s| s["ended_by"].as_str())
                        .unwrap_or("")
                        .to_string();
                    return fail(
                        "handsfree_ended",
                        format!("session ended ({by}) during the batch"),
                    );
                }
                // A dead session process (stale heartbeat) stops the batch
                // too (D30).
                if heartbeat_age(s.as_ref(), Utc::now()).is_none_or(|age| age > STALE_AFTER_S) {
                    return fail(
                        "handsfree_ended",
                        "session heartbeat went stale during the batch",
                    );
                }
            }
            if req.abort_on_foreground_change && ty != "wait" {
                let now = ex.inj.foreground();
                if now.root_hwnd != fg0.root_hwnd {
                    return fail(
                        "foreground_changed_abort",
                        format!(
                            "foreground moved to {} '{}' before action {idx}",
                            now.process.as_deref().unwrap_or(""),
                            now.title.chars().take(60).collect::<String>()
                        ),
                    );
                }
            }
            let mut detail = Map::new();
            ex.action(a, &mut detail)?;
            Ok(detail)
        })();
        match r {
            Ok(detail) => executed.push(json!({
                "index": idx, "type": ty,
                "ms": (ex.inj.now_ms() - t_action) as i64, "detail": Value::Object(detail),
            })),
            Err(f) => {
                failed = json!({"index": idx, "type": ty, "kind": f.kind,
                    "message": f.message.chars().take(400).collect::<String>()});
                failed_at = Some(idx);
                break;
            }
        }
    }
    ex.release_all();
    let skipped: Vec<usize> = failed_at
        .map(|f| (f + 1..req.actions.len()).collect())
        .unwrap_or_default();
    let (vl, vt, vw, vh) = vs;
    Ok(json!({
        "session_id": sess["session_id"],
        "executed_count": executed.len(),
        "executed": executed,
        "failed": failed,
        "skipped": skipped,
        "total": req.actions.len(),
        "foreground_before": fg0.to_json(),
        "foreground_after": ex.inj.foreground().to_json(),
        "cursor": ex.cursor_str(),
        "virtual_screen": {"x": vl, "y": vt, "width": vw, "height": vh},
        "elapsed_ms": (ex.inj.now_ms() - t0) as i64,
        "any_input": ex.any_input,
    }))
}

#[cfg(test)]
pub mod fake {
    //! A recording injector: no OS calls, virtual clock.
    use super::*;

    pub struct Fake {
        pub sent: Vec<Ev>,
        pub clock: f64,
        pub cursor: (i32, i32),
        pub fg: Fg,
        /// Foreground switches to this after N `foreground()` calls.
        pub fg_switch: Option<(usize, Fg)>,
        pub fg_calls: std::cell::Cell<usize>,
        pub integrity: i64,
        pub vs: (i32, i32, i32, i32),
        pub state: Option<Value>,
        /// State becomes this after N `session()` calls.
        pub state_after: Option<(usize, Value)>,
        pub session_calls: std::cell::Cell<usize>,
        pub clipboard: Option<String>,
        pub caps: bool,
        pub reject_after: Option<usize>,
        /// Cursor follows absolute moves unless false (blocked placement).
        pub cursor_follows: bool,
    }

    pub fn live_state(id: &str) -> Value {
        json!({"active": true, "session_id": id,
               "heartbeat_utc": super::super::state::fmt_o(Utc::now())})
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                sent: vec![],
                clock: 0.0,
                cursor: (0, 0),
                fg: Fg {
                    hwnd: 100,
                    root_hwnd: 100,
                    pid: 7,
                    process: Some("notepad".into()),
                    title: "Untitled - Notepad".into(),
                },
                fg_switch: None,
                fg_calls: std::cell::Cell::new(0),
                integrity: 0x2000,
                vs: (-1920, 0, 3840, 1080),
                state: Some(live_state("abcdef0123456789")),
                state_after: None,
                session_calls: std::cell::Cell::new(0),
                clipboard: Some("prev".into()),
                caps: false,
                reject_after: None,
                cursor_follows: true,
            }
        }
    }

    impl Injector for Fake {
        fn send(&mut self, evs: &[Ev]) -> usize {
            if let Some(n) = self.reject_after
                && self.sent.len() + evs.len() > n
            {
                return 0;
            }
            for e in evs {
                if let Ev::Mouse { dx, dy, flags, .. } = e
                    && flags & F_ABS != 0
                    && self.cursor_follows
                {
                    let (vl, vt, vw, vh) = self.vs;
                    // inverse of abs_coord (approximate, within 1 px)
                    self.cursor = (
                        vl + ((f64::from(*dx - 1) * f64::from(vw)) / 65536.0).round() as i32,
                        vt + ((f64::from(*dy - 1) * f64::from(vh)) / 65536.0).round() as i32,
                    );
                }
            }
            self.sent.extend_from_slice(evs);
            evs.len()
        }
        fn cursor(&self) -> (i32, i32) {
            self.cursor
        }
        fn foreground(&self) -> Fg {
            let n = self.fg_calls.get() + 1;
            self.fg_calls.set(n);
            match &self.fg_switch {
                Some((k, f)) if n > *k => f.clone(),
                _ => self.fg.clone(),
            }
        }
        fn integrity(&self, _: i64) -> i64 {
            self.integrity
        }
        fn virtual_screen(&self) -> (i32, i32, i32, i32) {
            self.vs
        }
        fn now_ms(&self) -> f64 {
            self.clock
        }
        fn sleep_ms(&mut self, ms: u64) {
            self.clock += ms as f64;
        }
        fn wait_until_ms(&mut self, t: f64) {
            self.clock = self.clock.max(t);
        }
        fn session(&self) -> Option<Value> {
            let n = self.session_calls.get() + 1;
            self.session_calls.set(n);
            match &self.state_after {
                Some((k, v)) if n > *k => Some(v.clone()),
                _ => self.state.clone(),
            }
        }
        fn clipboard_get(&mut self) -> Option<String> {
            self.clipboard.clone()
        }
        fn clipboard_set(&mut self, t: &str) -> bool {
            self.clipboard = Some(t.into());
            true
        }
        fn caps_lock(&self) -> bool {
            self.caps
        }
        fn layout_key(&self, unit: u16) -> Option<LayoutKey> {
            match unit {
                0x61..=0x7A => Some(LayoutKey {
                    vk: unit - 0x20,
                    mods: 0,
                    scan: 0x1E,
                }),
                0x41..=0x5A => Some(LayoutKey {
                    vk: unit,
                    mods: 1,
                    scan: 0x1E,
                }),
                0x40 => Some(LayoutKey {
                    vk: 0x32,
                    mods: 6,
                    scan: 0x03,
                }), // AltGr
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Fake, live_state};
    use super::*;

    fn req(actions: &[Value]) -> Request<'_> {
        Request {
            actions,
            session_id: None,
            expect_foreground_hwnd: None,
            abort_on_foreground_change: true,
            timeout_ms: 60_000.0,
        }
    }

    #[test]
    fn abs_coords_cover_negative_origin_desktops() {
        // two monitors: left one at x=-1920
        assert_eq!(abs_coord(-1920, -1920, 3840), 1);
        assert_eq!(abs_coord(0, -1920, 3840), 32769);
        assert_eq!(abs_coord(1919, -1920, 3840), 65519);
        assert_eq!(abs_coord(5000, -1920, 3840), 65535);
        assert_eq!(abs_coord(-5000, -1920, 3840), 0);
    }

    #[test]
    fn simulated_cursor_inverts_abs_coord_and_clamps_off_desktop() {
        // Example multi-monitor layout: 7680x2880 from (-2560,-1440).
        let vs = (-2560, -1440, 7680, 2880);
        for x in [-2560, -2559, -1, 0, 1, 777, 5119] {
            for y in [-1440, -1, 0, 333, 1439] {
                let ev = Ev::Mouse {
                    dx: abs_coord(x, vs.0, vs.2),
                    dy: abs_coord(y, vs.1, vs.3),
                    flags: F_MOVE | F_ABS | F_VDESK,
                    data: 0,
                };
                let (cx, cy) = simulated_cursor(&ev, vs).unwrap();
                assert!(
                    (cx - x).abs() <= 1 && (cy - y).abs() <= 1,
                    "{x},{y} -> {cx},{cy}"
                );
            }
        }
        let off = Ev::Mouse {
            dx: abs_coord(-9000, vs.0, vs.2),
            dy: abs_coord(0, vs.1, vs.3),
            flags: F_MOVE | F_ABS | F_VDESK,
            data: 0,
        };
        assert_eq!(simulated_cursor(&off, vs).unwrap().0, -2560);
        let click = Ev::Mouse {
            dx: 0,
            dy: 0,
            flags: 0x0002,
            data: 0,
        };
        assert_eq!(simulated_cursor(&click, vs), None);
    }

    #[test]
    fn capture_remap_uses_origin_and_scale_with_bankers_rounding() {
        let cap = json!({"origin_x": -1920, "origin_y": 0, "scale": 0.5});
        let out = remap(
            &[
                json!({"type": "click", "x": 100, "y": 51}),
                json!({"type": "drag", "path": [{"x": 0, "y": 0}, {"x": 3, "y": 1}]}),
            ],
            &cap,
        );
        assert_eq!(out[0]["x"], -1720);
        assert_eq!(out[0]["y"], 102);
        assert_eq!(out[1]["path"][1], json!({"x": -1914, "y": 2}));
        // 0.5 ties to even
        assert_eq!(remap_point(1.0, 3.0, 0.5, 0.5, 2.0), (1, 2));
    }

    #[test]
    fn cps_clamped_and_timeout_formula() {
        assert_eq!(clamp_cps(None), 50);
        assert_eq!(clamp_cps(Some(500)), 120);
        assert_eq!(clamp_cps(Some(0)), 1);
        let acts = vec![
            json!({"type": "type", "text": "x".repeat(400)}),
            json!({"type": "wait", "ms": 5000}),
        ];
        assert_eq!(batch_timeout_s(&acts), 30 + 20 + 5 + 6);
        let huge: Vec<Value> = (0..50)
            .map(|_| json!({"type": "wait", "ms": 10000}))
            .collect();
        assert_eq!(batch_timeout_s(&huge), 280);
    }

    #[test]
    fn densify_steps() {
        let d = densify(&[(0, 0), (10, 0)], 6);
        assert_eq!(d, vec![(0, 0), (5, 0), (10, 0)]);
        assert_eq!(densify(&[(0, 0), (0, 0)], 6).len(), 2);
    }

    #[test]
    fn preflight_refusals_inject_nothing() {
        let a = [json!({"type": "click"})];
        let mut f = Fake {
            state: None,
            ..Default::default()
        };
        let e = run_batch(&mut f, &req(&a)).unwrap_err();
        assert_eq!(e.kind, "handsfree_required");
        assert_eq!(
            e.message,
            "hands-free mode is not active; call windows_handsfree start"
        );

        let mut f = Fake {
            state: Some(json!({"active": false, "ended_by": "hotkey"})),
            ..Default::default()
        };
        let e = run_batch(&mut f, &req(&a)).unwrap_err();
        assert!(e.message.contains("(ended_by=hotkey)"), "{}", e.message);

        let mut f = Fake {
            state: Some(json!({"active": true, "session_id": "x",
                "heartbeat_utc": "2020-01-01T00:00:00.0000000Z"})),
            ..Default::default()
        };
        assert_eq!(
            run_batch(&mut f, &req(&a)).unwrap_err().kind,
            "handsfree_stale"
        );

        let mut f = Fake::default();
        let mut r = req(&a);
        r.session_id = Some("other-session");
        assert_eq!(
            run_batch(&mut f, &r).unwrap_err().kind,
            "handsfree_session_mismatch"
        );

        let mut f = Fake::default();
        f.fg.process = Some("LockApp".into());
        assert_eq!(
            run_batch(&mut f, &req(&a)).unwrap_err().kind,
            "session_locked"
        );

        let mut f = Fake {
            integrity: 0x3000,
            ..Default::default()
        };
        assert_eq!(
            run_batch(&mut f, &req(&a)).unwrap_err().kind,
            "target_elevated"
        );

        let mut f = Fake::default();
        let mut r = req(&a);
        r.expect_foreground_hwnd = Some(999);
        let e = run_batch(&mut f, &r).unwrap_err();
        assert_eq!(e.kind, "foreground_mismatch");
        assert_eq!(
            e.message,
            "expected hwnd 999 in foreground, found 100 (notepad)"
        );
        assert!(f.sent.is_empty());
    }

    #[test]
    fn click_and_drag_events() {
        let a = [
            json!({"type": "click", "x": -1000, "y": 500, "count": 2}),
            json!({"type": "drag", "path": [{"x": 0, "y": 0}, {"x": 12, "y": 0}], "duration_ms": 100}),
        ];
        let mut f = Fake::default();
        let d = run_batch(&mut f, &req(&a)).unwrap();
        assert_eq!(d["failed"], Value::Null);
        assert_eq!(d["executed_count"], 2);
        assert_eq!(d["executed"][0]["detail"]["count"], 2);
        assert_eq!(d["executed"][1]["detail"]["points_sent"], 3);
        let downs = f
            .sent
            .iter()
            .filter(|e| matches!(e, Ev::Mouse { flags: 2, .. }))
            .count();
        let ups = f
            .sent
            .iter()
            .filter(|e| matches!(e, Ev::Mouse { flags: 4, .. }))
            .count();
        assert_eq!((downs, ups), (3, 3));
        // drag moves after the press are NOCOALESCE
        assert!(
            f.sent
                .iter()
                .any(|e| matches!(e, Ev::Mouse { flags, .. } if flags & F_NOCO != 0))
        );
        assert_eq!(
            d["virtual_screen"],
            json!({"x": -1920, "y": 0, "width": 3840, "height": 1080})
        );
        assert_eq!(d["any_input"], true);
    }

    #[test]
    fn type_paste_and_chords() {
        let a = [
            json!({"type": "type", "text": "a\r\nB😀", "chars_per_second": 10}),
            json!({"type": "type", "text": "aB@", "method": "layout"}),
            json!({"type": "paste", "text": "hola", "restore_clipboard": true}),
            json!({"type": "key", "keys": ["ctrl+shift+s"]}),
        ];
        let mut f = Fake::default();
        let d = run_batch(&mut f, &req(&a)).unwrap();
        let t = &d["executed"][0]["detail"];
        assert_eq!(t["chars"], 4); // a, \n, B, 😀 (pair)
        assert_eq!(t["keys_unicode"], 3);
        assert_eq!(t["method"], "unicode");
        let l = &d["executed"][1]["detail"];
        assert_eq!(
            (l["keys_layout"].as_u64(), l["keys_unicode"].as_u64()),
            (Some(2), Some(1))
        );
        assert_eq!(f.clipboard.as_deref(), Some("prev"), "clipboard restored");
        // ctrl down, shift down, s down, s up, shift up, ctrl up (desc VK order)
        let tail: Vec<(u16, u32)> = f
            .sent
            .iter()
            .rev()
            .take(6)
            .rev()
            .map(|e| match e {
                Ev::Key { vk, flags, .. } => (*vk, *flags),
                _ => (0, 0),
            })
            .collect();
        assert_eq!(
            tail,
            vec![
                (0x11, 0),
                (0x10, 0),
                (0x53, 0),
                (0x53, 2),
                (0x11, 2),
                (0x10, 2)
            ]
        );
    }

    #[test]
    fn pacing_follows_chars_per_second() {
        let a = [json!({"type": "type", "text": "abcdefghij", "chars_per_second": 20})];
        let mut f = Fake::default();
        let d = run_batch(&mut f, &req(&a)).unwrap();
        assert!(d["elapsed_ms"].as_i64().unwrap() >= 500, "{d}");
    }

    #[test]
    fn failure_mid_batch_releases_and_skips() {
        let a = [
            json!({"type": "mouse_down", "x": 10, "y": 10}),
            json!({"type": "key", "keys": ["ctrl+nope"]}),
            json!({"type": "click"}),
        ];
        let mut f = Fake::default();
        let d = run_batch(&mut f, &req(&a)).unwrap();
        assert_eq!(d["failed"]["kind"], "invalid_arguments");
        assert_eq!(d["failed"]["message"], "unknown key 'nope'");
        assert_eq!(d["skipped"], json!([2]));
        // left button released by ReleaseAll
        assert!(matches!(f.sent.last(), Some(Ev::Mouse { flags: 4, .. })));
    }

    #[test]
    fn foreground_change_and_session_end_abort() {
        let a = [json!({"type": "wait", "ms": 1}), json!({"type": "click"})];
        let mut f = Fake {
            fg_switch: Some((
                1,
                Fg {
                    hwnd: 5,
                    root_hwnd: 5,
                    pid: 1,
                    process: Some("explorer".into()),
                    title: "x".into(),
                },
            )),
            ..Default::default()
        };
        let d = run_batch(&mut f, &req(&a)).unwrap();
        assert_eq!(d["failed"]["kind"], "foreground_changed_abort");
        assert_eq!(d["failed"]["index"], 1);
        assert!(f.sent.is_empty());

        let a: Vec<Value> = (0..7).map(|_| json!({"type": "wait", "ms": 1})).collect();
        let mut f = Fake {
            state_after: Some((1, json!({"active": false, "ended_by": "hotkey"}))),
            ..Default::default()
        };
        let d = run_batch(&mut f, &req(&a)).unwrap();
        assert_eq!(d["failed"]["kind"], "handsfree_ended");
        assert_eq!(d["failed"]["index"], 5);
        assert_eq!(
            d["failed"]["message"],
            "session ended (hotkey) during the batch"
        );
        assert_eq!(d["executed_count"], 5);
    }

    #[test]
    fn cursor_placement_blocked_and_sendinput_rejected() {
        let a = [json!({"type": "move", "x": 300, "y": 300})];
        let mut f = Fake {
            cursor_follows: false,
            ..Default::default()
        };
        let d = run_batch(&mut f, &req(&a)).unwrap();
        assert_eq!(d["failed"]["kind"], "cursor_placement_blocked");

        let a = [json!({"type": "scroll", "direction": "down", "amount": 3})];
        let mut f = Fake {
            reject_after: Some(1),
            ..Default::default()
        };
        let d = run_batch(&mut f, &req(&a)).unwrap();
        assert_eq!(d["failed"]["kind"], "sendinput_failed");
        assert_eq!(d["failed"]["message"], "wheel rejected");
    }

    #[test]
    fn live_state_helper_is_live() {
        assert!(is_live(Some(&live_state("a")), Utc::now()));
        assert!(!is_live(None, Utc::now()));
    }
}

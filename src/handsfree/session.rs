//! The hands-free session process (`windows-sosa-mcp __handsfree-session
//! <job_dir>`).
//!
//! It runs as a detached job worker (D25), so it outlives MCP restarts and,
//! because the low-level hooks live in THIS process, Windows drops them the
//! moment it dies (fail-open). The portable [`Core`] owns the state machine
//! (arm delay, hotkey, timeout, stop marker, lock, hook self-probe,
//! heartbeat); a [`Platform`] supplies hooks and overlay. On Windows the
//! real platform is in `session_win`; [`DryPlatform`] (non-Windows, or
//! `WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN=1`) installs nothing and shows
//! nothing, which lets the full start/status/stop lifecycle be tested
//! without touching the user's desktop (D29).

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use super::keys::{Hotkey, parse_hotkey};
use super::state::{fmt_o, read_shared, stop_path, write_retrying};

pub const SESSION_ARG: &str = "__handsfree-session";
pub const DRYRUN_ENV: &str = "WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN";
/// Job `kind` of the session worker (`meta.json`).
pub const JOB_KIND: &str = "handsfree_session";

/// True when the dry-run switch is set in this process' environment.
pub fn dry_run_env() -> bool {
    std::env::var(DRYRUN_ENV).is_ok_and(|v| v == "1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    Block,
    EndOnInput,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub session_id: String,
    pub job_id: String,
    pub reason: String,
    pub timeout_seconds: u64,
    pub glow_px: u32,
    pub arm_delay_ms: u64,
    pub guard: Guard,
    pub block_keyboard: bool,
    pub hotkey: Hotkey,
    pub dry_run: bool,
}

impl Config {
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let hotkey = parse_hotkey(v["release_hotkey"].as_str().unwrap_or("ctrl+alt+shift+h"))
            .map_err(|e| e.message)?;
        let session_id = s("session_id");
        if session_id.is_empty() {
            return Err("handsfree.json has no session_id".into());
        }
        Ok(Self {
            session_id,
            job_id: s("job_id"),
            reason: s("reason"),
            timeout_seconds: v["timeout_seconds"].as_u64().unwrap_or(900).max(1),
            glow_px: v["glow_px"].as_u64().unwrap_or(44).clamp(4, 200) as u32,
            arm_delay_ms: v["arm_delay_ms"].as_u64().unwrap_or(2500),
            guard: if v["guard"].as_str() == Some("end_on_input") {
                Guard::EndOnInput
            } else {
                Guard::Block
            },
            block_keyboard: v["block_keyboard"].as_bool().unwrap_or(true),
            hotkey,
            dry_run: v["dry_run"].as_bool().unwrap_or(false) || dry_run_env() || !cfg!(windows),
        })
    }

    fn guard_str(&self) -> &'static str {
        match self.guard {
            Guard::Block => "block",
            Guard::EndOnInput => "end_on_input",
        }
    }
}

/// Counters published by the hooks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub real: u64,
    pub blocked: u64,
    pub injected: u64,
}

/// Last real (hardware) input seen by the hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealInput {
    pub at: DateTime<Utc>,
    /// 1 mouse, 2 keyboard.
    pub kind: u8,
    pub code: u32,
}

/// What the state machine needs from the OS.
pub trait Platform {
    /// Install the LL hooks; `false` when `SetWindowsHookEx` failed.
    fn install(&mut self) -> bool;
    fn uninstall(&mut self);
    /// Snapshot keys already held (their key-up passes) and start blocking.
    fn arm(&mut self);
    fn hotkey_hits(&self) -> u64;
    fn last_real(&self) -> Option<RealInput>;
    fn reset_probe(&mut self);
    /// Inject the tagged probe event; `false` if `SendInput` refused it.
    fn send_probe(&mut self) -> bool;
    fn probe_seen(&self) -> bool;
    fn session_locked(&self) -> bool;
    fn counters(&self) -> Counters;
    fn monitors(&self) -> usize;
    fn set_label(&mut self, text: &str);
    fn retopmost(&mut self);
    fn close_overlay(&mut self);
    /// Run the UI message loop for up to `ms` (animation lives here).
    fn pump(&mut self, ms: u64);
}

/// Overlay label text. Minutes round half to even.
pub fn label_text(cfg: &Config, remaining_s: i64, armed: bool) -> String {
    let mut r: String = cfg.reason.chars().take(60).collect();
    if cfg.reason.chars().count() > 60 {
        r.push('…');
    }
    let how = match cfg.guard {
        Guard::Block => format!(
            "{} · {} gives control back",
            if !armed {
                "arming…"
            } else if cfg.block_keyboard {
                "mouse and keyboard blocked"
            } else {
                "mouse blocked"
            },
            cfg.hotkey.label
        ),
        Guard::EndOnInput => "move the mouse or press a key to take control back".to_string(),
    };
    let mins = (remaining_s as f64 / 60.0).round_ties_even() as i64;
    format!("\u{25CF}  Agent in control · {r} · {mins} min · {how}")
}

fn secs_between(a: DateTime<Utc>, b: DateTime<Utc>) -> i64 {
    let ms = (b - a).num_milliseconds() as f64 / 1000.0;
    ms.round_ties_even() as i64
}

/// The session state machine (one tick per timer interval).
pub struct Core {
    pub cfg: Config,
    pub state: Map<String, Value>,
    started: DateTime<Utc>,
    deadline: DateTime<Utc>,
    grace: DateTime<Utc>,
    armed: bool,
    pub done: bool,
    probe_at: Option<DateTime<Utc>>,
    probe_retry: bool,
    next_probe: DateTime<Utc>,
    next_beat: DateTime<Utc>,
    write_failures: u64,
    writer: Box<dyn FnMut(&Value) -> bool>,
}

impl Core {
    /// Install hooks, write the first state; ends at once with
    /// `hooks_failed` when blocking was requested but hooks are missing.
    pub fn start(
        cfg: Config,
        p: &mut dyn Platform,
        now: DateTime<Utc>,
        writer: Box<dyn FnMut(&Value) -> bool>,
    ) -> Self {
        let hooks_ok = p.install();
        let deadline = now + chrono::Duration::seconds(cfg.timeout_seconds as i64);
        let grace = now + chrono::Duration::milliseconds(cfg.arm_delay_ms as i64);
        let st = json!({
            "active": true, "session_id": cfg.session_id, "job_id": cfg.job_id, "pid": std::process::id(),
            "reason": cfg.reason, "guard": cfg.guard_str(), "block_keyboard": cfg.block_keyboard,
            "release_hotkey": cfg.hotkey.label, "armed": false, "started_utc": fmt_o(now),
            "expires_utc": fmt_o(deadline), "armed_utc": fmt_o(grace), "hooks_installed": hooks_ok,
            "hook_reinstalls": 0, "monitors": p.monitors(), "heartbeat_utc": fmt_o(now),
            "ended_utc": null, "ended_by": null, "end_detail": null, "last_real_input_utc": null,
            "real_input_events": 0, "blocked_events": 0, "injected_events": 0, "state_write_failures": 0,
            "dry_run": cfg.dry_run,
        });
        let mut core = Self {
            state: st.as_object().cloned().unwrap_or_default(),
            started: now,
            deadline,
            grace,
            armed: false,
            done: false,
            probe_at: None,
            probe_retry: false,
            next_probe: now + chrono::Duration::seconds(10),
            next_beat: now + chrono::Duration::seconds(1),
            write_failures: 0,
            writer,
            cfg,
        };
        let remaining = secs_between(now, deadline);
        p.set_label(&label_text(&core.cfg, remaining, false));
        core.write();
        if !hooks_ok && core.cfg.guard == Guard::Block {
            core.end(
                p,
                now,
                "hooks_failed",
                "SetWindowsHookEx failed; refusing to run hands-free without a working release path",
            );
        }
        core
    }

    pub fn started(&self) -> DateTime<Utc> {
        self.started
    }

    pub fn deadline(&self) -> DateTime<Utc> {
        self.deadline
    }

    fn write(&mut self) {
        self.state
            .insert("state_write_failures".into(), json!(self.write_failures));
        let v = Value::Object(self.state.clone());
        if (self.writer)(&v) {
            self.write_failures = 0;
        } else {
            self.write_failures += 1;
        }
    }

    fn counters(&mut self, p: &dyn Platform) {
        let c = p.counters();
        self.state.insert("real_input_events".into(), json!(c.real));
        self.state.insert("blocked_events".into(), json!(c.blocked));
        self.state
            .insert("injected_events".into(), json!(c.injected));
    }

    /// `EndSession`: once; hooks off first (fail-open), then state, overlay.
    pub fn end(&mut self, p: &mut dyn Platform, now: DateTime<Utc>, why: &str, detail: &str) {
        if self.done {
            return;
        }
        self.done = true;
        p.uninstall();
        self.state.insert("active".into(), json!(false));
        self.state.insert("ended_utc".into(), json!(fmt_o(now)));
        self.state.insert("ended_by".into(), json!(why));
        self.state.insert("end_detail".into(), json!(detail));
        self.counters(p);
        self.write();
        p.close_overlay();
    }

    /// One 100 ms tick. `stop` reads (and consumes) the stop marker.
    pub fn tick(
        &mut self,
        p: &mut dyn Platform,
        now: DateTime<Utc>,
        stop: &mut dyn FnMut() -> Option<String>,
    ) {
        if self.done {
            return;
        }
        if !self.armed && now >= self.grace {
            p.arm();
            self.armed = true;
            self.state.insert("armed".into(), json!(true));
            self.write();
            p.set_label(&label_text(
                &self.cfg,
                secs_between(now, self.deadline),
                true,
            ));
        }
        if p.hotkey_hits() > 0 {
            let l = self.cfg.hotkey.label.clone();
            return self.end(p, now, "hotkey", &l);
        }
        if let Some(r) = p.last_real() {
            self.state
                .insert("last_real_input_utc".into(), json!(fmt_o(r.at)));
            if self.cfg.guard == Guard::EndOnInput && now > self.grace && r.at > self.grace {
                let kind = if r.kind == 2 { "keyboard" } else { "mouse" };
                return self.end(
                    p,
                    now,
                    "user_input",
                    &format!("kind={kind} code={}", r.code),
                );
            }
        }
        if now >= self.deadline {
            return self.end(p, now, "timeout", "");
        }
        if let Some(by) = stop() {
            return self.end(p, now, "stop", &by);
        }
        // Hook self-probe every 10 s (Windows silently removes LL hooks
        // whose callback stalls; a lost hook means nothing is blocked).
        if self.probe_at.is_none() && now >= self.next_probe {
            p.reset_probe();
            if p.send_probe() {
                self.probe_at = Some(now);
            }
            self.next_probe = now + chrono::Duration::seconds(10);
        }
        if let Some(at) = self.probe_at
            && (now - at).num_milliseconds() >= 400
        {
            if p.probe_seen() {
                self.probe_retry = false;
                self.probe_at = None;
            } else if self.probe_retry {
                return self.end(
                    p,
                    now,
                    "hook_lost",
                    "low-level hooks stopped receiving events and could not be reinstalled",
                );
            } else {
                p.uninstall();
                let ok = p.install();
                let n = self.state["hook_reinstalls"].as_u64().unwrap_or(0) + 1;
                self.state.insert("hook_reinstalls".into(), json!(n));
                self.probe_retry = true;
                if !ok {
                    return self.end(
                        p,
                        now,
                        "hook_lost",
                        "low-level hooks were removed by Windows and SetWindowsHookEx failed",
                    );
                }
                p.reset_probe();
                let _ = p.send_probe();
                self.probe_at = Some(now);
            }
        }
        if now >= self.next_beat {
            self.next_beat = now + chrono::Duration::seconds(1);
            if p.session_locked() {
                return self.end(p, now, "session_locked", "");
            }
            self.state.insert("heartbeat_utc".into(), json!(fmt_o(now)));
            self.counters(p);
            self.write();
            p.set_label(&label_text(
                &self.cfg,
                secs_between(now, self.deadline),
                self.armed,
            ));
            p.retopmost();
        }
    }
}

/// No hooks, no overlay: lifecycle tests and non-Windows hosts.
#[derive(Default)]
pub struct DryPlatform {
    pub label: String,
}

impl Platform for DryPlatform {
    fn install(&mut self) -> bool {
        true
    }
    fn uninstall(&mut self) {}
    fn arm(&mut self) {}
    fn hotkey_hits(&self) -> u64 {
        0
    }
    fn last_real(&self) -> Option<RealInput> {
        None
    }
    fn reset_probe(&mut self) {}
    fn send_probe(&mut self) -> bool {
        true
    }
    fn probe_seen(&self) -> bool {
        true
    }
    fn session_locked(&self) -> bool {
        false
    }
    fn counters(&self) -> Counters {
        Counters::default()
    }
    fn monitors(&self) -> usize {
        0
    }
    fn set_label(&mut self, text: &str) {
        self.label = text.to_string();
    }
    fn retopmost(&mut self) {}
    fn close_overlay(&mut self) {}
    fn pump(&mut self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Main-loop liveness for the watchdog (unix ms of the last tick).
static LAST_TICK_MS: AtomicI64 = AtomicI64::new(0);

/// If the main loop wedges for 5 s the hooks (on their own thread) would
/// keep blocking with nobody heart-beating: exit so Windows drops them.
fn spawn_watchdog() {
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let last = LAST_TICK_MS.load(Ordering::Relaxed);
            if last > 0 && Utc::now().timestamp_millis() - last > 5000 {
                eprintln!("handsfree: main loop stalled >5 s; exiting (fail-open)");
                std::process::exit(70);
            }
        }
    });
}

fn read_stop() -> Option<String> {
    let p = stop_path();
    if !p.exists() {
        return None;
    }
    let by = read_shared(&p)
        .and_then(|v| v["by"].as_str().map(String::from))
        .unwrap_or_else(|| "stop".to_string());
    let _ = std::fs::remove_file(&p);
    Some(by)
}

/// Entry point of `windows-sosa-mcp __handsfree-session <job_dir>`.
pub fn run(job_dir: &Path) -> i32 {
    // The job lives in `<root>/jobs/<id>`: share the server's state root
    // even when the launcher (WMI) did not inherit its environment.
    if let Some(root) = job_dir.parent().and_then(Path::parent)
        && job_dir.parent().and_then(Path::file_name) == Some("jobs".as_ref())
    {
        // SAFETY: called first thing in the session process, before any
        // other thread exists.
        unsafe { std::env::set_var(crate::state::HOME_ENV, root) };
    }
    let cfg = match crate::tools::jobs::read_json(&job_dir.join("handsfree.json"))
        .ok_or_else(|| "handsfree.json unreadable".to_string())
        .and_then(|v| Config::from_json(&v))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("handsfree: {e}");
            return 2;
        }
    };
    let _ = std::fs::create_dir_all(super::state::dir());
    let _ = std::fs::remove_file(stop_path());
    let state_path = super::state::state_path();
    let writer = Box::new(move |v: &Value| write_retrying(&state_path, v));
    let mut plat: Box<dyn Platform> = make_platform(&cfg);
    LAST_TICK_MS.store(Utc::now().timestamp_millis(), Ordering::Relaxed);
    spawn_watchdog();
    let mut core = Core::start(cfg, plat.as_mut(), Utc::now(), writer);
    let mut stop = read_stop;
    while !core.done {
        plat.pump(100);
        LAST_TICK_MS.store(Utc::now().timestamp_millis(), Ordering::Relaxed);
        let now = Utc::now();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            core.tick(plat.as_mut(), now, &mut stop);
        }));
        if let Err(e) = r {
            let m = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            core.end(
                plat.as_mut(),
                Utc::now(),
                "internal_error",
                &m.chars().take(300).collect::<String>(),
            );
        }
    }
    println!("WB_HANDSFREE_END:{}", Value::Object(core.state.clone()));
    0
}

#[cfg(windows)]
fn make_platform(cfg: &Config) -> Box<dyn Platform> {
    if cfg.dry_run {
        Box::new(DryPlatform::default())
    } else {
        Box::new(super::session_win::WinPlatform::new(cfg))
    }
}

#[cfg(not(windows))]
fn make_platform(_cfg: &Config) -> Box<dyn Platform> {
    Box::new(DryPlatform::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Fake {
        installed: bool,
        install_ok: Vec<bool>,
        installs: u32,
        armed: bool,
        hits: u64,
        real: Option<RealInput>,
        probe_seen: bool,
        probes: u32,
        locked: bool,
        closed: bool,
        label: String,
        retops: u32,
    }

    impl Platform for Fake {
        fn install(&mut self) -> bool {
            self.installs += 1;
            let ok = if self.install_ok.is_empty() {
                true
            } else {
                self.install_ok.remove(0)
            };
            self.installed = ok;
            ok
        }
        fn uninstall(&mut self) {
            self.installed = false;
        }
        fn arm(&mut self) {
            self.armed = true;
        }
        fn hotkey_hits(&self) -> u64 {
            self.hits
        }
        fn last_real(&self) -> Option<RealInput> {
            self.real
        }
        fn reset_probe(&mut self) {
            self.probe_seen = false;
        }
        fn send_probe(&mut self) -> bool {
            self.probes += 1;
            if self.installed {
                self.probe_seen = true;
            }
            true
        }
        fn probe_seen(&self) -> bool {
            self.probe_seen
        }
        fn session_locked(&self) -> bool {
            self.locked
        }
        fn counters(&self) -> Counters {
            Counters {
                real: 5,
                blocked: 3,
                injected: 1,
            }
        }
        fn monitors(&self) -> usize {
            2
        }
        fn set_label(&mut self, t: &str) {
            self.label = t.into();
        }
        fn retopmost(&mut self) {
            self.retops += 1;
        }
        fn close_overlay(&mut self) {
            self.closed = true;
        }
        fn pump(&mut self, _ms: u64) {}
    }

    fn cfg(guard: Guard) -> Config {
        Config::from_json(
            &json!({"session_id": "abc", "job_id": "j", "reason": "test", "timeout_seconds": 120,
            "arm_delay_ms": 2500, "guard": if guard == Guard::Block {"block"} else {"end_on_input"},
            "block_keyboard": true, "release_hotkey": "ctrl+alt+shift+h"}),
        )
        .unwrap()
    }

    type Writes = Rc<RefCell<Vec<Value>>>;

    fn core(g: Guard, f: &mut Fake, t0: DateTime<Utc>) -> (Core, Writes) {
        let w: Writes = Rc::default();
        let w2 = w.clone();
        let c = Core::start(
            cfg(g),
            f,
            t0,
            Box::new(move |v| {
                w2.borrow_mut().push(v.clone());
                true
            }),
        );
        (c, w)
    }

    fn t(ms: i64) -> DateTime<Utc> {
        super::super::state::parse_utc("2026-11-16T10:00:00Z").unwrap()
            + chrono::Duration::milliseconds(ms)
    }

    fn no_stop() -> Option<String> {
        None
    }

    #[test]
    fn arms_after_delay_then_heartbeats_and_times_out() {
        let mut f = Fake::default();
        let (mut c, w) = core(Guard::Block, &mut f, t(0));
        assert_eq!(w.borrow()[0]["active"], true);
        assert_eq!(w.borrow()[0]["armed"], false);
        assert_eq!(w.borrow()[0]["monitors"], 2);
        assert_eq!(w.borrow()[0]["expires_utc"], "2026-11-16T10:02:00.0000000Z");
        assert!(f.label.contains("arming…"), "{}", f.label);
        c.tick(&mut f, t(1000), &mut no_stop);
        assert!(!f.armed);
        c.tick(&mut f, t(2500), &mut no_stop);
        assert!(f.armed);
        assert!(
            f.label
                .contains("mouse and keyboard blocked · Ctrl+Alt+Shift+H gives control back")
        );
        assert!(
            f.label
                .starts_with("\u{25CF}  Agent in control · test · 2 min · ")
        );
        let last = w.borrow().last().cloned().unwrap();
        assert_eq!(last["armed"], true);
        assert_eq!(last["blocked_events"], 3);
        assert!(f.retops > 0);
        c.tick(&mut f, t(120_000), &mut no_stop);
        assert!(c.done && f.closed && !f.installed);
        let last = w.borrow().last().cloned().unwrap();
        assert_eq!(last["active"], false);
        assert_eq!(last["ended_by"], "timeout");
        assert_eq!(last["end_detail"], "");
    }

    #[test]
    fn hotkey_and_stop_end_the_session() {
        let mut f = Fake::default();
        let (mut c, w) = core(Guard::Block, &mut f, t(0));
        let mut stop = || Some("windows_handsfree".to_string());
        c.tick(&mut f, t(100), &mut stop);
        assert_eq!(w.borrow().last().unwrap()["ended_by"], "stop");
        assert_eq!(
            w.borrow().last().unwrap()["end_detail"],
            "windows_handsfree"
        );
        let mut f = Fake::default();
        let (mut c, w) = core(Guard::Block, &mut f, t(0));
        f.hits = 1;
        c.tick(&mut f, t(100), &mut no_stop);
        assert_eq!(w.borrow().last().unwrap()["ended_by"], "hotkey");
        assert_eq!(w.borrow().last().unwrap()["end_detail"], "Ctrl+Alt+Shift+H");
        // Ends once only.
        let n = w.borrow().len();
        c.tick(&mut f, t(200), &mut stop);
        c.end(&mut f, t(300), "x", "y");
        assert_eq!(w.borrow().len(), n);
    }

    #[test]
    fn real_input_ends_only_end_on_input_after_grace() {
        let mut f = Fake::default();
        let (mut c, w) = core(Guard::EndOnInput, &mut f, t(0));
        f.real = Some(RealInput {
            at: t(1000),
            kind: 2,
            code: 65,
        });
        c.tick(&mut f, t(3000), &mut no_stop);
        assert!(!c.done, "input before the grace period is ignored");
        f.real = Some(RealInput {
            at: t(2600),
            kind: 2,
            code: 65,
        });
        c.tick(&mut f, t(3100), &mut no_stop);
        assert!(c.done);
        assert_eq!(w.borrow().last().unwrap()["ended_by"], "user_input");
        assert_eq!(
            w.borrow().last().unwrap()["end_detail"],
            "kind=keyboard code=65"
        );
        assert_eq!(
            w.borrow().last().unwrap()["last_real_input_utc"],
            "2026-11-16T10:00:02.6000000Z"
        );
        // Block mode only records it.
        let mut f = Fake::default();
        let (mut c, _) = core(Guard::Block, &mut f, t(0));
        f.real = Some(RealInput {
            at: t(2600),
            kind: 1,
            code: 0x201,
        });
        c.tick(&mut f, t(3100), &mut no_stop);
        assert!(!c.done);
        assert_eq!(
            c.state["last_real_input_utc"],
            "2026-11-16T10:00:02.6000000Z"
        );
    }

    #[test]
    fn probe_reinstalls_once_then_gives_up() {
        let mut f = Fake::default();
        let (mut c, w) = core(Guard::Block, &mut f, t(0));
        c.tick(&mut f, t(10_000), &mut no_stop);
        assert_eq!(f.probes, 1);
        c.tick(&mut f, t(10_400), &mut no_stop);
        assert!(!c.done);
        // Windows silently drops the hook: the next probe is not seen.
        f.installed = false;
        c.tick(&mut f, t(20_400), &mut no_stop);
        c.tick(&mut f, t(20_800), &mut no_stop); // reinstall + reprobe (seen)
        assert_eq!(c.state["hook_reinstalls"], 1);
        assert!(f.installed && !c.done);
        c.tick(&mut f, t(21_200), &mut no_stop);
        assert!(!c.done);
        // Lost again and the reinstall "works" but events never arrive.
        f.installed = false;
        c.tick(&mut f, t(31_300), &mut no_stop);
        f.install_ok = vec![true];
        c.tick(&mut f, t(31_700), &mut no_stop);
        f.installed = false;
        f.probe_seen = false;
        c.tick(&mut f, t(32_100), &mut no_stop);
        assert!(c.done);
        assert_eq!(w.borrow().last().unwrap()["ended_by"], "hook_lost");
        // A failed reinstall ends at once.
        let mut f = Fake::default();
        let (mut c, w) = core(Guard::Block, &mut f, t(0));
        c.tick(&mut f, t(10_000), &mut no_stop);
        f.installed = false;
        f.probe_seen = false;
        f.install_ok = vec![false];
        c.tick(&mut f, t(10_400), &mut no_stop);
        assert!(c.done);
        assert!(
            w.borrow().last().unwrap()["end_detail"]
                .as_str()
                .unwrap()
                .contains("SetWindowsHookEx failed")
        );
    }

    #[test]
    fn lock_and_hook_failure() {
        let mut f = Fake::default();
        let (mut c, w) = core(Guard::Block, &mut f, t(0));
        f.locked = true;
        c.tick(&mut f, t(1000), &mut no_stop);
        assert_eq!(w.borrow().last().unwrap()["ended_by"], "session_locked");
        let mut f = Fake {
            install_ok: vec![false],
            ..Default::default()
        };
        let (c, w) = core(Guard::Block, &mut f, t(0));
        assert!(c.done && f.closed);
        assert_eq!(w.borrow().len(), 2);
        assert_eq!(w.borrow()[1]["ended_by"], "hooks_failed");
        assert_eq!(w.borrow()[0]["hooks_installed"], false);
        // end_on_input keeps running without hooks.
        let mut f = Fake {
            install_ok: vec![false],
            ..Default::default()
        };
        let (c, _) = core(Guard::EndOnInput, &mut f, t(0));
        assert!(!c.done);
    }

    #[test]
    fn write_failures_are_counted_not_fatal() {
        let mut f = Fake::default();
        let n = Rc::new(RefCell::new(0));
        let n2 = n.clone();
        let seen: Writes = Rc::default();
        let s2 = seen.clone();
        let mut c = Core::start(
            cfg(Guard::Block),
            &mut f,
            t(0),
            Box::new(move |v| {
                *n2.borrow_mut() += 1;
                s2.borrow_mut().push(v.clone());
                *n2.borrow() > 2
            }),
        );
        c.tick(&mut f, t(1000), &mut no_stop);
        c.tick(&mut f, t(2000), &mut no_stop);
        assert!(!c.done);
        assert_eq!(seen.borrow()[1]["state_write_failures"], 1);
        assert_eq!(seen.borrow()[2]["state_write_failures"], 2);
        c.tick(&mut f, t(3000), &mut no_stop);
        assert_eq!(seen.borrow().last().unwrap()["state_write_failures"], 0);
    }

    #[test]
    fn labels() {
        let mut c = cfg(Guard::Block);
        c.block_keyboard = false;
        c.reason = "x".repeat(61);
        let l = label_text(&c, 899, true);
        assert!(
            l.contains(&format!("{}… · 15 min · mouse blocked", "x".repeat(60))),
            "{l}"
        );
        assert!(label_text(&c, 90, true).contains(" · 2 min · "));
        assert!(label_text(&c, 30, true).contains(" · 0 min · "));
        let c = cfg(Guard::EndOnInput);
        assert!(
            label_text(&c, 60, true)
                .ends_with("move the mouse or press a key to take control back")
        );
    }

    #[test]
    fn config_parses_and_clamps() {
        let c = Config::from_json(
            &json!({"session_id": "s", "release_hotkey": "ctrl+alt+f1", "glow_px": 1000}),
        )
        .unwrap();
        assert_eq!(c.timeout_seconds, 900);
        assert_eq!(c.glow_px, 200);
        assert_eq!(c.hotkey.label, "Ctrl+Alt+F1");
        assert_eq!(c.guard, Guard::Block);
        assert!(Config::from_json(&json!({"session_id": "s", "release_hotkey": "h"})).is_err());
        assert!(Config::from_json(&json!({})).is_err());
    }
}

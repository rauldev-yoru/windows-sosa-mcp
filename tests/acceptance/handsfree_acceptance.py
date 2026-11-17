#!/usr/bin/env python3
"""Hands-free acceptance run for windows-sosa-mcp — ATTENDED ONLY.

This script takes over the Windows desktop: it starts a real hands-free
session (guard=block, short TTL), injects keyboard/mouse input into its OWN
test windows and asks the operator to perform manual steps (look at the
overlay, touch the mouse, press the release hotkey). Never run it
unattended.

Test windows: S2-S4 never open or read other applications. They launch
`hf_test_app.ps1` (WinForms, title `SosaAcceptance-<hex>`, own PID) via
`windows_job_start`, find its hwnd with `wait_until window_appears
{title_contains, exclude_hwnds}`, snapshot/act ONLY on that hwnd (and its
own "Save changes?" dialog), verify effects through UIA and the app's JSON
state file, and close it by UIA / job_cancel / its PID at the end. Unless
`--app` points to an existing copy on the Windows host, the script uploads
its sibling `hf_test_app.ps1` into a fresh directory under the Windows
user's temp folder.

`--server-cmd` is any command that speaks MCP over stdio (parsed with
shlex). Locally on the Windows host:

    python tests/acceptance/handsfree_acceptance.py \\
        --server-cmd "C:/tools/windows-sosa-mcp/windows-sosa-mcp.exe" \\
        [--timeout 120] [--only S1,S2] [--yes] [--json out.json]

From another machine through enishi (the server published as service
`sosa` on device `<device>`):

    python3 tests/acceptance/handsfree_acceptance.py \\
        --server-cmd "enishi mcp <device>/sosa"

Dry run (no takeover, safe while someone uses the PC): start the server
with WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN=1 and pass `--dry-run`. The session
has no hooks/overlay and `windows_input` counts events without sending
them; the script checks the flow and skips the effects that need real
input (reported as SKIP). `ui_action activate` is still REAL: it brings the
test window to the foreground. The script refuses to run when `--dry-run`
and the server's environment disagree.

Manual prompts are answered in THIS terminal; while the session is armed
the Windows keyboard is blocked, so those steps are timed countdowns and
the questions are asked after the session ends. Run the script from
another device or answer once input is released.

Abort at any moment: Ctrl+Alt+Shift+H on the Windows host (release
hotkey), or Ctrl+Alt+Del (Windows never lets hooks block it), or Ctrl+C
here (the session then expires by its TTL).
"""
from __future__ import annotations

import argparse
import base64
import json
import math
import re
import struct
import sys
import time
import uuid
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mcp_client  # noqa: E402

APP_LOCAL = Path(__file__).resolve().parent / "hf_test_app.ps1"
APP_PREFIX = "SosaAcceptance-"
HOTKEY = "Ctrl+Alt+Shift+H"


# ---------------------------------------------------------------- PNG

def png_pixels(data: bytes):
    pos, chunks, w, h, ct = 8, [], 0, 0, 0
    while pos < len(data):
        ln, typ = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + ln]
        pos += 12 + ln
        if typ == b"IHDR":
            w, h, bd, ct = struct.unpack(">IIBB", body[:10])
            assert bd == 8 and ct in (2, 6), (bd, ct)
        elif typ == b"IDAT":
            chunks.append(body)
    raw = zlib.decompress(b"".join(chunks))
    bpp = 4 if ct == 6 else 3
    stride = w * bpp
    rows, prev, i = [], bytearray(stride), 0
    for _ in range(h):
        f = raw[i]
        i += 1
        line = bytearray(raw[i:i + stride])
        i += stride
        for x in range(stride):
            a = line[x - bpp] if x >= bpp else 0
            b = prev[x]
            c = prev[x - bpp] if x >= bpp else 0
            if f == 1:
                line[x] = (line[x] + a) & 255
            elif f == 2:
                line[x] = (line[x] + b) & 255
            elif f == 3:
                line[x] = (line[x] + ((a + b) >> 1)) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append(bytes(line))
        prev = line
    return w, h, bpp, rows


def dark_count(png: bytes, region, threshold=200) -> int:
    w, h, bpp, rows = png_pixels(png)
    x0, y0, rw, rh = region
    n = 0
    for y in range(max(0, y0), min(h, y0 + rh)):
        row = rows[y]
        for x in range(max(0, x0), min(w, x0 + rw)):
            o = x * bpp
            if row[o] + row[o + 1] + row[o + 2] < threshold:
                n += 1
    return n


def center(b):
    return b["x"] + b["width"] // 2, b["y"] + b["height"] // 2


def region(cap, b):
    sc = cap["scale"]
    return (int((b["x"] - cap["origin_x"]) * sc), int((b["y"] - cap["origin_y"]) * sc),
            int(b["width"] * sc), int(b["height"] * sc))


# ---------------------------------------------------------------- harness

class H:
    def __init__(self, cmd: str, ttl: int, yes: bool, dry: bool, app: str | None) -> None:
        self.p = mcp_client.McpProcess("server", cmd, 180)
        self.p.request("initialize", {"protocolVersion": mcp_client.PROTOCOL, "capabilities": {},
                                      "clientInfo": {"name": "handsfree-acceptance", "version": "0.2"}})
        self.p.notify("notifications/initialized")
        self.ttl, self.yes, self.dry, self.app = ttl, yes, dry, app
        self.tmp = ""
        self.checks: list[dict] = []
        self.scn = "-"
        self.sid = None
        self.apps: list[dict] = []
        self.monitors: list[dict] = []
        self.t0 = time.time()

    def call(self, name, args, quiet=False):
        msg, ms = self.p.request("tools/call", {"name": name, "arguments": args})
        res = msg.get("result") or {}
        sc = res.get("structuredContent") or {}
        img = next((c for c in res.get("content", []) if c.get("type") == "image"), None)
        err = res.get("isError", False)
        if not quiet:
            extra = ""
            if name == "windows_input" and sc.get("data"):
                extra = "executed=%s/%s" % (sc["data"].get("executed_count"), sc["data"].get("total"))
            print("    %-24s %6d ms  %-9s %s %s" % (name, ms, sc.get("status", "?"), sc.get("error_kind") or "", extra), flush=True)
        return sc, (base64.b64decode(img["data"]) if img else None), err

    def check(self, label: str, cond, detail: object = "") -> bool:
        self.checks.append({"scenario": self.scn, "label": label, "ok": bool(cond), "detail": str(detail)[:300]})
        print("  %s %s%s" % ("PASS" if cond else "FAIL", label,
                             (" — " + str(detail)[:200]) if detail and not cond else ""), flush=True)
        return bool(cond)

    def effect(self, label: str, cond_fn, detail_fn=lambda: "") -> bool:
        """A check that needs REAL input (text typed, pixels drawn, a real
        click): skipped in dry-run, where windows_input sends nothing."""
        if self.dry:
            self.checks.append({"scenario": self.scn, "label": label, "ok": True, "skipped": True, "detail": "dry-run"})
            print("  SKIP %s (dry-run)" % label, flush=True)
            return True
        return self.check(label, cond_fn(), detail_fn())

    def scenario(self, name):
        self.scn = name
        print("\n=== %s ===" % name, flush=True)

    def ask(self, prompt: str) -> bool:
        """Manual step: the operator answers y/n (or Enter when only acknowledging)."""
        print("\n>>> OPERATOR: " + prompt, flush=True)
        if self.yes or self.dry:
            print("    (%s: assumed yes)" % ("dry-run" if self.dry else "--yes"))
            return True
        ans = input("    [s/n] > ").strip().lower()
        return ans in ("", "y", "yes")

    def manual(self, label: str, prompt: str) -> bool:
        """A check only the operator can answer; skipped in dry-run."""
        if self.dry:
            self.checks.append({"scenario": self.scn, "label": label, "ok": True, "skipped": True, "detail": "dry-run"})
            print("  SKIP %s (dry-run, manual)" % label, flush=True)
            return True
        return self.check(label, self.ask(prompt))

    def countdown(self, prompt: str, secs: int) -> None:
        """Manual step while input is BLOCKED: the operator cannot press Enter on
        the PC, so the step is timed instead of acknowledged."""
        if self.dry:
            print("\n>>> (dry-run: countdown skipped) %s" % prompt, flush=True)
            return
        print("\n>>> OPERATOR (no keyboard, %d s): %s" % (secs, prompt), flush=True)
        for left in range(secs, 0, -1):
            print("    %2d s..." % left, end="\r", flush=True)
            time.sleep(1)
        print("    listo.        ", flush=True)

    def tell(self, prompt: str) -> None:
        print("\n>>> OPERATOR: " + prompt, flush=True)
        if not (self.yes or self.dry):
            input("    (Enter to continue) ")

    # --- scratch dir on the Windows host and the test app
    def prepare(self) -> None:
        self.tmp = self.ps("$d = Join-Path ([IO.Path]::GetTempPath()) 'sosa-hf-acceptance-%s'; "
                           "New-Item -ItemType Directory -Force -Path $d | Out-Null; $d"
                           % uuid.uuid4().hex[:8])
        if not self.app:
            self.app = self.tmp + "\\hf_test_app.ps1"
            sc, _, err = self.call("windows_write_text_file", {
                "path": self.app, "content": APP_LOCAL.read_text(encoding="utf-8"), "mode": "create",
                "encoding": "utf-8", "bom": True, "newline": "crlf"}, quiet=True)
            if err:
                raise SystemExit("ABORT: could not upload hf_test_app.ps1: %s" % sc.get("error"))
        print("    scratch: %s, test app: %s" % (self.tmp, self.app))

    # --- safety: dry-run flag must match the server's environment
    def verify_mode(self) -> None:
        env = self.ps("[string]$env:WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN")
        server_dry = env.strip() == "1"
        if server_dry != self.dry:
            raise SystemExit("ABORT: --dry-run=%s but the server has WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN=%r; "
                             "refusing to start a session in the wrong mode" % (self.dry, env))
        print("    mode: %s" % ("DRY-RUN (no hooks, no SendInput)" if self.dry else "REAL (takes over the desktop)"))

    # --- session
    def status(self):
        return self.call("windows_handsfree", {"action": "status"}, quiet=True)[0].get("data") or {}

    def start(self, reason, timeout=None, **kw):
        args = {"action": "start", "reason": reason, "timeout_seconds": timeout or self.ttl}
        args.update(kw)
        sc, _, err = self.call("windows_handsfree", args)
        if not err:
            self.sid = sc["data"]["session_id"]
            # The status view does not expose dry_run; the session's own
            # state.json carries it.
            got = self.ps("$r = if ($env:WINDOWS_SOSA_MCP_HOME) { $env:WINDOWS_SOSA_MCP_HOME } "
                          "else { Join-Path $env:LOCALAPPDATA 'windows-sosa-mcp' }; "
                          "[string](Get-Content (Join-Path $r 'handsfree\\state.json') -Raw | ConvertFrom-Json).dry_run")
            if (got.strip().lower() == "true") != self.dry:
                self.stop()
                raise SystemExit("ABORT: session dry_run=%r but --dry-run=%s" % (got, self.dry))
        return sc, err

    def ensure_session(self, reason):
        st = self.status()
        if st.get("active"):
            self.sid = st["session_id"]
            return st
        sc, err = self.start(reason)
        if err:
            raise RuntimeError("handsfree start failed: %s" % sc.get("error"))
        time.sleep(3.0)  # arm_delay
        return sc["data"]

    def stop(self):
        return self.call("windows_handsfree", {"action": "stop"}, quiet=True)[0].get("data") or {}

    # --- windows
    def ps(self, script, timeout=30):
        sc, _, err = self.call("windows_run_powershell", {"script": script, "timeout_seconds": timeout}, quiet=True)
        if err:
            raise RuntimeError("powershell failed: %s %s" % (sc.get("error"), sc.get("stderr", "")[:300]))
        return sc.get("stdout", "").strip()

    def hwnds(self, **filters):
        args = {"limit": 200, "include_untitled": False}
        args.update(filters)
        return self.call("windows_list_windows", args, quiet=True)[0]["data"]["windows"]

    def launch_app(self, retitle_ms=0, minimized=False, seconds=600):
        """Start a self-made test window and return its identity. Only this
        hwnd (and its own dialogs) is ever snapshotted or acted on."""
        title = APP_PREFIX + uuid.uuid4().hex[:8]
        state = "%s\\%s.json" % (self.tmp, title)
        before = [w["hwnd"] for w in self.hwnds()]
        script = ("powershell.exe -NoProfile -ExecutionPolicy Bypass -File '%s' -Title '%s' -StateFile '%s' "
                  "-Seconds %d -RetitleAfterMs %d%s" % (self.app, title, state, seconds, retitle_ms,
                                                        " -Minimized" if minimized else ""))
        sc, _, err = self.call("windows_job_start", {"script": script, "timeout_seconds": seconds + 60,
                                                     "label": "hands-free acceptance " + title}, quiet=True)
        if err:
            raise RuntimeError("job_start failed: %s" % sc.get("error"))
        app = {"title": title, "state_file": state, "job_id": sc["data"]["job_id"], "hwnd": None, "pid": None}
        self.apps.append(app)
        sc, _, err = self.wait(condition="window_appears", title_contains=title, exclude_hwnds=before,
                               timeout_seconds=30, poll_ms=250)
        if err or not sc["data"]["met"]:
            raise RuntimeError("test window %s did not appear: %s" % (title, sc.get("error") or sc.get("data")))
        app["hwnd"] = sc["data"]["observed"]["sample"][0]["hwnd"]
        info = next(w for w in self.hwnds(title_contains=title) if w["hwnd"] == app["hwnd"])
        app.update(pid=info["pid"], bounds=info["bounds"], state=info.get("state"), window_title=info["title"])
        print("    -> %s hwnd=%s pid=%s title=%r state=%s bounds=%s" % (title, app["hwnd"], app["pid"], info["title"],
                                                                      info.get("state"), info["bounds"]))
        return app

    def app_state(self, app, wait_for=None, timeout=4.0):
        """The app's JSON state file (optionally waiting for a predicate)."""
        deadline = time.time() + timeout
        st = {}
        while True:
            sc, _, err = self.call("windows_read_text_file", {"path": app["state_file"], "max_bytes": 65536,
                                                              "max_lines": 5}, quiet=True)
            if not err:
                try:
                    st = json.loads("".join(ln["text"] for ln in sc["data"]["lines"]))
                except ValueError:
                    st = {}
            if wait_for is None or (st and wait_for(st)) or time.time() > deadline:
                return st
            time.sleep(0.25)

    def close_app(self, app):
        """Cleanup: cancel its job and kill its PID — only if that PID is
        still a powershell process whose window carries our unique title."""
        if app.get("closed"):
            return
        app["closed"] = True
        try:
            self.call("windows_job_cancel", {"job_id": app["job_id"], "wait_seconds": 5}, quiet=True)
        except Exception:  # noqa: BLE001
            pass
        if app.get("pid"):
            self.ps("$p=Get-Process -Id %d -ErrorAction SilentlyContinue; "
                    "if ($p -and $p.ProcessName -eq 'powershell' -and $p.MainWindowTitle -like '*%s*') "
                    "{ Stop-Process -Id %d -Force }; 'ok'" % (app["pid"], app["title"], app["pid"]))

    def snap(self, hwnd, **kw):
        args = {"window": {"hwnd": hwnd}, "max_nodes": kw.pop("max_nodes", 400)}
        args.update(kw)
        sc, _, err = self.call("windows_ui_snapshot", args)
        if err:
            raise RuntimeError("snapshot failed: %s" % sc.get("error"))
        return sc["data"]

    @staticmethod
    def find(snapd, name=None, aid=None, role=None, contains=None, regex=None):
        for e in snapd["elements"]:
            if aid and e.get("automation_id") != aid:
                continue
            if name and (e.get("name") or "") != name:
                continue
            if contains and contains.lower() not in (e.get("name") or "").lower():
                continue
            if regex and not re.search(regex, e.get("name") or "", re.I):
                continue
            if role and e.get("role") != role:
                continue
            return e
        return None

    def act(self, snapd, el, action, **kw):
        args = {"snapshot_id": snapd["snapshot_id"], "ref": el["ref"],
                "element": (el.get("name") or el.get("automation_id") or el["role"])[:200], "action": action}
        args.update(kw)
        return self.call("windows_ui_action", args)

    def act_aid(self, hwnd, aid, action, **kw):
        d = self.snap(hwnd, max_nodes=80)
        el = self.find(d, aid=aid)
        if not el:
            raise RuntimeError("%s not found in own window %s" % (aid, hwnd))
        return self.act(d, el, action, **kw)

    def activate(self, hwnd, snapd=None):
        d = snapd or self.snap(hwnd, max_nodes=40)
        root = next(e for e in d["elements"] if e["ref"] == "e1")
        sc, _, err = self.act(d, root, "activate")
        return sc, err

    def input(self, actions, hwnd=None, abort=True, capture_after="none", capture_id=None, session=True,
              refocus=True):
        if refocus and self.dry and hwnd and any(a["hwnd"] == hwnd for a in self.apps):
            # Dry run: someone may keep using the PC, so the test window may have
            # lost the foreground; take it back (own window only).
            sc, _, err = self.call("windows_wait_until", {"condition": "foreground_is", "hwnd": hwnd,
                                                          "timeout_seconds": 1}, quiet=True)
            if err or not sc["data"]["met"]:
                print("    (dry-run: the test window lost the foreground; re-activating)")
                self.activate(hwnd)
        args = {"actions": actions, "abort_on_foreground_change": abort, "capture_after": capture_after}
        if session and self.sid:
            args["session_id"] = self.sid
        if hwnd:
            args["expect_foreground_hwnd"] = hwnd
        if capture_id:
            args["capture_id"] = capture_id
        return self.call("windows_input", args)

    def wait(self, **cond):
        return self.call("windows_wait_until", cond)

    def capture(self, hwnd, max_width=2560):
        sc, img, err = self.call("windows_capture_window", {"window": {"hwnd": hwnd}, "max_width": max_width})
        if err:
            raise RuntimeError("capture failed: %s" % sc.get("error"))
        return sc["data"]["capture"], img

    def stuck_keys(self):
        out = self.ps(r"""Add-Type -Namespace SosaHF -Name SK -MemberDefinition '[DllImport("user32.dll")] public static extern short GetAsyncKeyState(int vk);'
$names=@{1='LBUTTON';2='RBUTTON';4='MBUTTON';16='SHIFT';17='CONTROL';18='ALT';91='LWIN';92='RWIN'}
@($names.Keys | Where-Object { ([SosaHF.SK]::GetAsyncKeyState($_) -band 0x8000) -ne 0 } | ForEach-Object { $names[$_] }) -join ','""")
        return [x for x in out.split(",") if x]

    def activate_own(self, app, snapd=None, label="activate"):
        sc, err = self.activate(app["hwnd"], snapd)
        self.check("%s test window (applied)" % label, not err and sc["data"]["applied"],
                   sc.get("error") or sc["data"].get("activation_steps"))
        if not err:
            print("    activation_steps:", sc["data"].get("activation_steps"))
        sc, _, err = self.wait(condition="foreground_is", hwnd=app["hwnd"], timeout_seconds=3)
        self.check("wait_until foreground_is test window", not err and sc["data"]["met"],
                   (sc.get("data") or {}).get("observed"))
        return not err


def norm(text: str) -> str:
    return (text or "").replace("\r\n", "\n").replace("\r", "\n")


def dist_to_segment(p, a, b):
    (px, py), (ax, ay), (bx, by) = p, a, b
    dx, dy = bx - ax, by - ay
    if dx == 0 and dy == 0:
        return math.hypot(px - ax, py - ay)
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


def dist_to_path(p, path):
    pts = [(q["x"], q["y"]) for q in path]
    return min(dist_to_segment(p, pts[i], pts[i + 1]) for i in range(len(pts) - 1))


# ---------------------------------------------------------------- scenarios

def s0_preflight(h: H):
    h.scenario("S0 preflight (no takeover)")
    sc, _, _ = h.call("windows_health", {})
    h.check("health ok", sc.get("status") == "succeeded", sc.get("error"))
    msg, _ = h.p.request("tools/list", {})
    h.check("24 tools", len(msg["result"]["tools"]) == 24, len(msg["result"]["tools"]))
    sc, _, _ = h.call("windows_list_windows", {"limit": 5})
    h.monitors = sc["data"]["monitors"]
    print("    monitors:", [(m["index"], m["x"], m["y"], m["width"], m["height"], m["primary"]) for m in h.monitors])
    h.check("session_locked=false", sc["data"]["session_locked"] is False)
    h.stop()
    st = h.status()
    h.check("status inactive at start", st.get("active") is False, st)
    sc, _, err = h.input([{"type": "wait", "ms": 10}], session=False)
    h.check("input without session -> handsfree_required, may_have_changed=false",
            err and sc.get("error_kind") == "handsfree_required" and sc.get("may_have_changed") is False, sc.get("error"))
    sc, _, err = h.call("windows_ui_action", {"snapshot_id": "0" * 32, "ref": "e1", "element": "x", "action": "activate"})
    h.check("activate without session -> handsfree_required", err and sc.get("error_kind") == "handsfree_required", sc.get("error"))
    sc, _, err = h.wait(condition="time", timeout_seconds=1)
    h.check("wait_until time 1 s -> met", not err and sc["data"]["met"] and sc["data"]["elapsed_ms"] >= 900, sc.get("data"))


def s1_lifecycle(h: H):
    h.scenario("S1 lifecycle + overlay + real input blocking")
    if not h.ask("About to START hands-free mode (guard=block, TTL %d s). Your mouse and keyboard will be "
                 "BLOCKED. To take them back at any time: %s (or Ctrl+Alt+Del). Start?" % (h.ttl, HOTKEY)):
        raise SystemExit("cancelled by the operator")
    if not h.monitors:  # S1 run on its own (--only S1): S0 did not fill them
        h.monitors = h.call("windows_list_windows", {})[0]["data"]["monitors"]
    sc, err = h.start("acceptance S1", timeout=h.ttl)
    d = sc.get("data") or {}
    h.check("start ok", not err, sc.get("error"))
    h.effect("hooks installed, overlay on every monitor",
             lambda: d.get("hooks_installed") is True and d.get("monitors") == len(h.monitors),
             lambda: {k: d.get(k) for k in ("hooks_installed", "monitors")})
    h.check("guard=block, hotkey %s, keyboard blocked, not armed yet" % HOTKEY,
            d.get("guard") == "block" and d.get("release_hotkey") == HOTKEY and d.get("block_keyboard") is True and d.get("armed") is False,
            {k: d.get(k) for k in ("guard", "release_hotkey", "block_keyboard", "armed")})
    sc, err = h.start("acceptance bad hotkey", release_hotkey="ctrl+h")
    h.check("hotkey with a single modifier -> invalid_arguments",
            err and sc.get("error_kind") in ("invalid_arguments", "handsfree_already_active"), sc.get("error"))
    time.sleep(3.2)
    st = h.status()
    h.check("armed after arm_delay, blocked_events is an integer", st.get("armed") is True and isinstance(st.get("blocked_events"), int),
            {k: st.get(k) for k in ("armed", "blocked_events")})
    sc, err = h.start("acceptance dup")
    h.check("segundo start -> handsfree_already_active", err and sc.get("error_kind") == "handsfree_already_active", sc.get("error"))
    st = h.status()
    h.check("heartbeat fresco (< 4 s)", st.get("active") and (st.get("heartbeat_age_s") or 99) < 4, st.get("heartbeat_age_s"))
    before = h.status().get("blocked_events") or 0
    h.countdown("Look at the glowing border and the label with %s on EVERY monitor. Then MOVE THE MOUSE, "
                "click and press a few letters (NOT the hotkey): nothing should happen on screen." % HOTKEY, 15)
    st = h.status()
    h.check("session still active after the countdown", st.get("active") is True and st.get("ended_by") is None,
            {k: st.get(k) for k in ("active", "ended_by")})
    h.effect("input real bloqueado: blocked_events %s -> %s" % (before, st.get("blocked_events")),
             lambda: (st.get("blocked_events") or 0) > before and bool(st.get("last_real_input_utc")),
             lambda: {k: st.get(k) for k in ("blocked_events", "last_real_input_utc")})
    d = h.stop()
    h.check("stop -> ended_by=stop", d.get("active") is False and d.get("ended_by") == "stop", d.get("ended_by"))
    h.manual("operator saw the border + label '%s' on every monitor" % HOTKEY,
             "Did you see the glowing border and the label with %s on all your monitors?" % HOTKEY)
    h.manual("operator confirms mouse/keyboard had no effect while armed",
             "Confirm that the mouse and keyboard did nothing during the countdown?")
    h.manual("operator gets mouse/keyboard back and the border is gone after stop",
             "Is the border gone and do your mouse/keyboard work again?")
    d = h.stop()
    h.check("stop without session -> nothing to stop", "nothing" in (d.get("message") or ""))


def s2_text(h: H):
    h.scenario("S2 text: activate while loading, Unicode type + paste + chords, UIA value, 'Save changes?'")
    h.tell("S2-S4 type/draw ONLY into the script's own test windows ('%s…', powershell) with your input "
           "blocked; they never open or read your apps. No need to touch anything (to stop: %s)." % (APP_PREFIX, HOTKEY))
    h.ensure_session("acceptance S2 text")
    app = h.launch_app(retitle_ms=1500)
    # Snapshot while the app is still "initializing" (title gets a suffix
    # and a control is added 1.5 s later): activate must still resolve the
    # window root by hwnd instead of failing with ref_stale (D35).
    d0 = h.snap(app["hwnd"], max_nodes=40)
    loading = "(loading)" in (d0["elements"][0].get("name") or "")
    print("    snapshot while loading: root=%r" % d0["elements"][0].get("name"))
    sc, _, err = h.wait(condition="element_appears", window={"hwnd": app["hwnd"]}, automation_id="lblLoaded",
                        timeout_seconds=10, poll_ms=200)
    h.check("wait_until element_appears lblLoaded (the window finished loading)", not err and sc["data"]["met"], sc.get("error"))
    h.check("the earlier snapshot was taken with the loading title", loading, d0["elements"][0].get("name"))
    h.activate_own(app, snapd=d0, label="activate with the loading snapshot (title changed)")
    text = "Hello áñ — hands-free ✓ 😀\nLine 2 with tab\tend\nrow three"
    first, rest = text.split("\n", 1)
    sc, _, err = h.input([{"type": "type", "text": first + "\n", "chars_per_second": 50},
                          {"type": "paste", "text": rest, "restore_clipboard": True}], hwnd=app["hwnd"])
    h.check("type (Unicode+emoji) + paste ejecutados", not err and sc["data"]["executed_count"] == 2, sc.get("error"))
    d = h.snap(app["hwnd"], max_nodes=80)
    edit = h.find(d, aid="txtInput")
    h.check("snapshot: txtInput with ValuePattern", edit is not None and "value" in (edit.get("state") or {}), edit)
    val = norm((edit or {}).get("state", {}).get("value", ""))
    h.effect("UIA value of txtInput == text (emoji and tab included)", lambda: val == text, lambda: repr(val)[:200])
    st = h.app_state(app, wait_for=lambda s: norm(s.get("text")) == text, timeout=3)
    h.effect("app state: exact text", lambda: norm(st.get("text")) == text, lambda: repr(st.get("text"))[:200])
    sc, _, err = h.input([{"type": "key", "keys": ["ctrl+a"]}, {"type": "key", "keys": ["ctrl+c"]},
                          {"type": "wait", "ms": 200}, {"type": "key", "keys": ["ctrl+end"]},
                          {"type": "type", "text": "!"}], hwnd=app["hwnd"])
    h.check("chords ctrl+a/ctrl+c/ctrl+end + type ejecutados", not err and sc["data"]["executed_count"] == 5, sc.get("error"))
    st = h.app_state(app, wait_for=lambda s: norm(s.get("text")).endswith("!"), timeout=3)
    h.effect("the app saw ctrl+a and ctrl+c", lambda: {"ctrl+a", "ctrl+c"} <= set(st.get("keys") or []), lambda: st.get("keys"))
    h.effect("ctrl+c copied exactly the selection (verified inside the app)",
             lambda: st.get("copied_matches_selection") is True and st.get("copied_length") == len(text.replace("\n", "\r\n")),
             lambda: (st.get("copied_matches_selection"), st.get("copied_length")))
    h.effect("ctrl+end + type '!' at the end", lambda: norm(st.get("text")) == text + "!", lambda: repr(st.get("text"))[-30:])
    # wait_until transitions driven by the app (UIA invoke: works in both modes).
    h.act_aid(app["hwnd"], "btnShowLater", "invoke")
    sc, _, err = h.wait(condition="element_appears", window={"hwnd": app["hwnd"]}, automation_id="lblReady",
                        timeout_seconds=5, poll_ms=150)
    h.check("wait_until element_appears lblReady (~800 ms)", not err and sc["data"]["met"] and sc["data"]["elapsed_ms"] >= 400,
            sc.get("data"))
    h.act_aid(app["hwnd"], "btnHideLater", "invoke")
    sc, _, err = h.wait(condition="element_disappears", window={"hwnd": app["hwnd"]}, automation_id="lblReady",
                        timeout_seconds=5, poll_ms=150)
    h.check("wait_until element_disappears lblReady", not err and sc["data"]["met"], sc.get("data"))
    # Close with unsaved text -> own "Save changes?" dialog -> Don't save.
    before = [w["hwnd"] for w in h.hwnds()]
    if h.dry:  # alt+f4 would not be sent: make it dirty and close by UIA
        h.act_aid(app["hwnd"], "txtInput", "set_value", value="dry-run dirty text")
        dd = h.snap(app["hwnd"], max_nodes=40)
        h.act(dd, dd["elements"][0], "close_window")
    else:
        sc, _, err = h.input([{"type": "key", "keys": ["alt+f4"]}], hwnd=app["hwnd"])
        h.check("alt+f4 enviado", not err, sc.get("error"))
    dlg_title = app["title"] + " - Save changes?"
    sc, _, err = h.wait(condition="window_appears", title_contains=dlg_title, exclude_hwnds=before,
                        timeout_seconds=8, poll_ms=200)
    dlg = sc["data"]["observed"]["sample"][0] if not err and sc["data"]["met"] else None
    h.check("the app's own 'Save changes?' dialog appears", dlg is not None, sc.get("error"))
    if dlg:
        dd = h.snap(dlg["hwnd"], max_nodes=40)
        btn = h.find(dd, aid="btnDontSave")
        sc, _, err = h.act(dd, btn, "invoke") if btn else ({}, None, True)
        sc2, _, err2 = h.wait(condition="window_disappears", hwnd=app["hwnd"], timeout_seconds=8, poll_ms=200)
        h.check("'Don't save' via UIA -> wait_until window_disappears", btn is not None and not err and not err2
                and sc2["data"]["met"], (sc.get("error"), sc2.get("error")))
        st = h.app_state(app, wait_for=lambda s: s.get("closed_by"), timeout=3)
        h.check("the app recorded closed_by=dont_save", st.get("closed_by") == "dont_save", st.get("closed_by"))
    h.check("no stuck keys", not h.stuck_keys())
    h.close_app(app)


def s3_canvas(h: H):
    h.scenario("S3 canvas: activate from minimized, pixel drags, capture_after, click by capture_id")
    h.ensure_session("acceptance S3 canvas")
    app = h.launch_app(minimized=True)
    h.check("the test window started minimized", app.get("state") == "minimized", app.get("state"))
    h.activate_own(app, label="activate (restores the minimized window)")
    info = next((w for w in h.hwnds(title_contains=app["title"]) if w["hwnd"] == app["hwnd"]), {})
    h.check("restored after activate", info.get("state") not in (None, "minimized"), info.get("state"))
    d = h.snap(app["hwnd"], max_nodes=80)
    canvas, target = h.find(d, aid="pnlCanvas"), h.find(d, aid="btnTarget")
    h.check("snapshot: canvas + target button", canvas and target, (bool(canvas), bool(target)))
    if not (canvas and target):
        h.close_app(app)
        return
    cb = canvas["bounds"]
    cap0, png0 = h.capture(app["hwnd"])
    reg = region(cap0, cb)
    base = dark_count(png0, reg)
    x0, y0, side = cb["x"] + 30, cb["y"] + 30, 180
    square = [{"x": x0, "y": y0}, {"x": x0 + side, "y": y0}, {"x": x0 + side, "y": y0 + side},
              {"x": x0, "y": y0 + side}, {"x": x0, "y": y0}]
    cx, cy, r = cb["x"] + 340, cb["y"] + 120, 80
    circle = [{"x": int(round(cx + r * math.cos(t * 2 * math.pi / 72))), "y": int(round(cy + r * math.sin(t * 2 * math.pi / 72)))}
              for t in range(73)]
    lx0, ly, lx1, ly1 = cb["x"] + 40, cb["y"] + 240, cb["x"] + 300, cb["y"] + 280
    sc, _, err = h.input([{"type": "drag", "path": square, "step_px": 6, "duration_ms": 1200}, {"type": "wait", "ms": 150},
                          {"type": "drag", "path": circle, "step_px": 5, "duration_ms": 1000}, {"type": "wait", "ms": 150},
                          {"type": "mouse_down", "x": lx0, "y": ly}, {"type": "move", "x": lx1, "y": ly},
                          {"type": "move", "x": lx1, "y": ly1}, {"type": "mouse_up"}],
                         hwnd=app["hwnd"], capture_after="foreground_window")
    h.check("square + circle drags + mouse_down/move/up executed", not err and sc["data"]["executed_count"] == 8, sc.get("error"))
    h.check("capture_after devuelve capture_id",
            bool(((sc.get("data") or {}).get("capture_after") or {}).get("capture", {}).get("capture_id")),
            (sc.get("data") or {}).get("capture_after"))
    cap1, png1 = h.capture(app["hwnd"])
    after = dark_count(png1, region(cap1, cb))
    h.effect("dark pixels on the canvas %d -> %d (> +1500)" % (base, after), lambda: after - base > 1500)
    st = h.app_state(app, wait_for=lambda s: len(s.get("strokes") or []) >= 3, timeout=3)
    strokes = st.get("strokes") or []
    h.effect("the app recorded 3 strokes", lambda: len(strokes) == 3, lambda: [len(s) for s in strokes])
    if len(strokes) == 3:
        sq, ci, el = strokes
        h.effect("square: starts and ends at (%d,%d) ±2 px, every point ≤3 px from the outline" % (x0, y0),
                 lambda: max(abs(sq[0][0] - x0), abs(sq[0][1] - y0), abs(sq[-1][0] - x0), abs(sq[-1][1] - y0)) <= 2
                 and max(dist_to_path(p, square) for p in sq) <= 3,
                 lambda: (sq[0], sq[-1], round(max(dist_to_path(p, square) for p in sq), 1)))
        h.effect("circle: every point at r=%d ±3 px from the centre" % r,
                 lambda: max(abs(math.hypot(p[0] - cx, p[1] - cy) - r) for p in ci) <= 3,
                 lambda: round(max(abs(math.hypot(p[0] - cx, p[1] - cy) - r) for p in ci), 1))
        h.effect("L (mouse_down/move/up): from (%d,%d) to (%d,%d) ±2 px" % (lx0, ly, lx1, ly1),
                 lambda: max(abs(el[0][0] - lx0), abs(el[0][1] - ly), abs(el[-1][0] - lx1), abs(el[-1][1] - ly1)) <= 2,
                 lambda: (el[0], el[-1]))
    d = h.snap(app["hwnd"], max_nodes=80)
    cname = (h.find(d, aid="pnlCanvas") or {}).get("name") or ""
    h.effect("UIA: canvas name 'Canvas strokes=3'", lambda: cname.startswith("Canvas strokes=3 "), lambda: cname)
    capw, _ = h.capture(app["hwnd"], max_width=400)
    tx, ty = center(target["bounds"])
    sx, sy = (tx - capw["origin_x"]) * capw["scale"], (ty - capw["origin_y"]) * capw["scale"]
    sc, _, err = h.input([{"type": "click", "x": int(round(sx)), "y": int(round(sy))}], hwnd=app["hwnd"],
                         capture_id=capw["capture_id"])
    h.check("click by capture_id (scale %.3f) executed in capture space:" % capw["scale"],
            not err and "capture:" in (sc["data"].get("coordinate_space") or ""), sc.get("error") or sc["data"].get("coordinate_space"))
    st = h.app_state(app, wait_for=lambda s: s.get("target_clicks"), timeout=3)
    tol = 2 + 1 / capw["scale"]
    pos = st.get("target_click_screen") or [None, None]
    h.effect("target button clicked once within ±%.1f px of (%d,%d)" % (tol, tx, ty),
             lambda: st.get("target_clicks") == 1 and abs(pos[0] - tx) <= tol and abs(pos[1] - ty) <= tol,
             lambda: (st.get("target_clicks"), pos))
    h.check("no stuck keys/buttons", not h.stuck_keys())
    d = h.snap(app["hwnd"], max_nodes=40)
    h.act(d, d["elements"][0], "close_window")
    sc, _, err = h.wait(condition="window_disappears", hwnd=app["hwnd"], timeout_seconds=8, poll_ms=200)
    h.check("test window closed (close_window, no text -> no dialog)", not err and sc["data"]["met"])
    h.close_app(app)


def s4_safety(h: H):
    h.scenario("S4 safety: foreground_mismatch, invalid chord, blocked cursor, foreground_changed_abort")
    h.ensure_session("acceptance S4 safety")
    app = h.launch_app()
    h.activate_own(app)
    sc, _, err = h.input([{"type": "click", "x": 5, "y": 5}], hwnd=app["hwnd"] + 2)
    h.check("expect_foreground_hwnd equivocado -> foreground_mismatch, may_have_changed=false",
            err and sc.get("error_kind") == "foreground_mismatch" and sc.get("may_have_changed") is False, sc.get("error"))
    sc, _, err = h.input([{"type": "key", "keys": ["ctrl+;"]}])
    h.check("invalid chord -> invalid_arguments without data", err and sc.get("error_kind") == "invalid_arguments" and not sc.get("data"), sc.get("error"))
    sc, _, err = h.input([{"type": "move", "x": -9000, "y": 0}, {"type": "wait", "ms": 10}], hwnd=app["hwnd"])
    d = sc.get("data") or {}
    h.check("move outside the desktop -> cursor_placement_blocked, executed 0, skipped [1], may_have_changed",
            err and sc.get("error_kind") == "cursor_placement_blocked" and d.get("executed_count") == 0 and d.get("skipped") == [1]
            and sc.get("may_have_changed") is True, (sc.get("error"), d.get("skipped"), sc.get("may_have_changed")))
    # The foreground is taken by the app's OWN popup (never by another of
    # other windows, unlike alt+tab).
    dsnap = h.snap(app["hwnd"], max_nodes=80)
    popup, canvas = h.find(dsnap, aid="btnPopup"), h.find(dsnap, aid="pnlCanvas")
    px, py = center(popup["bounds"])
    if h.dry:  # the click below is only counted: open the popup by UIA
        h.input([{"type": "wait", "ms": 10}], hwnd=app["hwnd"])  # dry-run refocus, before the timer starts
        h.act(dsnap, popup, "invoke")  # popup shows 1.5 s later (-PopupDelayMs)
    sc, _, err = h.input([{"type": "click", "x": px, "y": py}, {"type": "wait", "ms": 2500},
                          {"type": "click", "x": center(canvas["bounds"])[0], "y": center(canvas["bounds"])[1]}],
                         hwnd=app["hwnd"], refocus=False)
    d = sc.get("data") or {}
    h.check("the app's popup takes the foreground -> foreground_changed_abort before action 2",
            err and sc.get("error_kind") == "foreground_changed_abort" and d.get("executed_count") == 2
            and (d.get("failed") or {}).get("index") == 2, (sc.get("error"), d.get("executed_count"), d.get("failed")))
    st = h.app_state(app, wait_for=lambda s: s.get("popup_shown"), timeout=3)
    h.check("the app opened its popup", st.get("popup_shown") is True)
    h.effect("the aborted click never reached the canvas", lambda: not st.get("strokes"), lambda: st.get("strokes"))
    h.check("no stuck keys after the abort", not h.stuck_keys())
    d = h.snap(app["hwnd"], max_nodes=40)
    h.act(d, d["elements"][0], "close_window")
    sc, _, err = h.wait(condition="window_disappears", hwnd=app["hwnd"], timeout_seconds=8, poll_ms=200)
    h.check("test window (and its popup) closed", not err and sc["data"]["met"])
    h.close_app(app)


def s5_hotkey(h: H):
    h.scenario("S5 release hotkey (manual)")
    if h.dry:
        h.checks.append({"scenario": h.scn, "label": "hotkey (manual)", "ok": True, "skipped": True, "detail": "dry-run"})
        print("  SKIP S5 (dry-run: no hooks, no hotkey)")
        return
    h.ensure_session("acceptance S5 hotkey")
    print("\n>>> OPERATOR: press %s now on the Windows keyboard (all four keys together); waiting up to 60 s." % HOTKEY, flush=True)
    deadline = time.time() + 60
    st = h.status()
    while time.time() < deadline and st.get("active"):
        time.sleep(1)
        st = h.status()
    h.check("hotkey -> ended_by=hotkey", st.get("active") is False and st.get("ended_by") == "hotkey",
            {k: st.get(k) for k in ("active", "ended_by", "end_detail")})
    h.check("no stuck keys after the hotkey", not h.stuck_keys())
    h.check("operator has control back after the hotkey", h.ask("Is the border gone and do your mouse/keyboard work?"))


def s6_fail_open(h: H):
    h.scenario("S6 fail-open: killing the session process releases input")
    sc, err = h.start("acceptance S6 fail-open")
    time.sleep(3.0)
    st = h.status()
    sc, _, err = h.call("windows_job_cancel", {"job_id": st.get("job_id")})
    h.check("job_cancel of the session (kill_verified)", not err and sc["data"].get("kill_verified"), sc.get("error"))
    h.manual("operator gets mouse/keyboard back at once (hooks gone with the process)",
             "Do your mouse/keyboard work now and is the border gone?")
    time.sleep(7.5)
    st = h.status()
    h.check("status after the kill: inactive + stale notice", st.get("active") is False and "stale" in (st.get("message") or ""), st.get("message"))
    sc, _, err = h.input([{"type": "wait", "ms": 10}])
    h.check("input with a dead session -> handsfree_stale/required, may_have_changed=false",
            err and sc.get("error_kind") in ("handsfree_stale", "handsfree_required") and sc.get("may_have_changed") is False, sc.get("error"))


def s7_ttl(h: H):
    h.scenario("S7 TTL 30 s")
    sc, err = h.start("acceptance S7 TTL", timeout=30)
    h.check("start with TTL 30 s", not err, sc.get("error"))
    time.sleep(33)
    st = h.status()
    h.check("TTL vencido -> ended_by=timeout", st.get("active") is False and st.get("ended_by") == "timeout", st.get("ended_by"))
    h.manual("operator has control back after the TTL", "Do your mouse/keyboard work and is the border gone?")


PLAN = {"S0": s0_preflight, "S1": s1_lifecycle, "S2": s2_text, "S3": s3_canvas,
        "S4": s4_safety, "S5": s5_hotkey, "S6": s6_fail_open, "S7": s7_ttl}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--server-cmd", required=True,
                    help="command that runs windows-sosa-mcp over stdio (shlex-parsed), e.g. "
                         "'windows-sosa-mcp.exe' or 'enishi mcp <device>/sosa'")
    ap.add_argument("--timeout", type=int, default=120, help="hands-free TTL per session (s)")
    ap.add_argument("--only", default=",".join(PLAN), help="comma list of scenarios")
    ap.add_argument("--yes", action="store_true", help="assume yes for manual prompts (NOT for an attended real run)")
    ap.add_argument("--dry-run", action="store_true",
                    help="server runs with WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN=1: no takeover, effects skipped")
    ap.add_argument("--app", help="Windows path of an existing hf_test_app.ps1 (default: upload the bundled copy)")
    ap.add_argument("--json")
    a = ap.parse_args()
    h = H(a.server_cmd, a.timeout, a.yes, a.dry_run, a.app)
    try:
        h.verify_mode()
        h.prepare()
        for key in [k.strip() for k in a.only.split(",") if k.strip()]:
            try:
                PLAN[key](h)
            except SystemExit:
                raise
            except Exception as e:  # noqa: BLE001 - report and continue
                h.check("scenario without exception", False, "%s: %s" % (type(e).__name__, e))
    finally:
        try:
            h.stop()
        except Exception:  # noqa: BLE001
            pass
        for app in h.apps:
            try:
                h.close_app(app)
            except Exception:  # noqa: BLE001
                pass
    passed = sum(1 for c in h.checks if c["ok"] and not c.get("skipped"))
    skipped = sum(1 for c in h.checks if c.get("skipped"))
    failed = [c for c in h.checks if not c["ok"]]
    print("\n==== RESULT%s: %d PASS, %d FAIL, %d SKIP in %.0f s ====" % (
        " (DRY-RUN)" if h.dry else "", passed, len(failed), skipped, time.time() - h.t0))
    for c in failed:
        print("  FAIL [%s] %s — %s" % (c["scenario"], c["label"], c["detail"]))
    if a.json:
        Path(a.json).write_text(json.dumps({"dry_run": h.dry, "checks": h.checks}, ensure_ascii=False, indent=1))
    h.p.close()
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())

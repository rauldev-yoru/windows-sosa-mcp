//! End-to-end test: spawn the real binary and speak newline-delimited
//! JSON-RPC over stdio, like an MCP client does.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn spawn() -> Self {
        Self::spawn_env(&[])
    }

    fn spawn_env(env: &[(&str, &Path)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_windows-sosa-mcp"));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn server");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let init = mcp.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "stdio-test", "version": "0"}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "windows-sosa-mcp");
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        mcp
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "EOF");
            let msg: Value = serde_json::from_str(&line).expect("stdout must be JSON-RPC only");
            if msg["id"] == id {
                return msg;
            }
        }
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let r = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let result = r["result"].clone();
        let sc = result["structuredContent"].clone();
        let text = result["content"][0]["text"].as_str().expect("text content");
        assert_eq!(serde_json::from_str::<Value>(text).unwrap(), sc);
        assert_eq!(result["isError"], json!(sc["status"] != "succeeded"));
        for key in ["status", "tool_name", "server_version", "error_kind"] {
            assert!(sc.get(key).is_some(), "{name}: missing {key}");
        }
        sc
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn tools_list_matches_contract_exactly() {
    let contract: Value = serde_json::from_str(include_str!("../contracts/tools.json")).unwrap();
    let mut mcp = Mcp::spawn();
    let listed = mcp.request("tools/list", json!({}))["result"]["tools"].clone();
    let listed = listed.as_array().unwrap();
    let expected = contract["tools"].as_array().unwrap();
    assert_eq!(listed.len(), 24);
    assert_eq!(listed.len(), expected.len());
    for (got, want) in listed.iter().zip(expected) {
        assert_eq!(got["name"], want["name"]);
        for key in ["description", "inputSchema", "annotations"] {
            assert_eq!(got[key], want[key], "{} {key}", want["name"]);
        }
        // outputSchema: null in the contract is serialized as absent.
        assert_eq!(
            got.get("outputSchema").unwrap_or(&Value::Null),
            &want["outputSchema"],
            "{} outputSchema",
            want["name"]
        );
    }
}

#[test]
fn health_and_typed_failures() {
    let mut mcp = Mcp::spawn();
    let h = mcp.call("windows_health", json!({}));
    assert_eq!(h["tool_name"], "windows_health");
    assert_eq!(h["verification"]["unicode"], "UTF-8 check: áéíóú ñ 日本 ✓");
    if cfg!(windows) {
        assert_eq!(h["status"], "succeeded");
        assert_eq!(h["error_kind"], Value::Null);
    } else {
        assert_eq!(h["error_kind"], "unsupported_platform");
    }

    let n = mcp.call("windows_handsfree", json!({"action": "status"}));
    assert_eq!(n["status"], "succeeded");
    assert_eq!(n["data"]["active"], false);

    let u = mcp.call("windows_does_not_exist", json!({}));
    assert_eq!(u["error_kind"], "unknown_tool");

    let i = mcp.call("windows_run_powershell", json!({}));
    assert_eq!(i["error_kind"], "invalid_arguments");

    // Server is still alive after every failure kind.
    let h2 = mcp.call("windows_health", json!({}));
    assert_eq!(
        h2["verification"]["audit_session_id"],
        h["verification"]["audit_session_id"]
    );
}

fn p(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

/// Vendor dir with the host's ripgrep and its receipt (Linux: `rg` on PATH;
/// Windows: the pinned `vendor\rg.exe` next to the test binary if present).
fn vendor(dir: &Path) -> Option<std::path::PathBuf> {
    let exe = if cfg!(windows) { "rg.exe" } else { "rg" };
    let src = std::env::var_os("SOSA_TEST_RG")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|d| d.join(exe))
                .find(|p| p.is_file())
        })?;
    let v = dir.join("vendor");
    std::fs::create_dir_all(&v).ok()?;
    std::fs::copy(&src, v.join(exe)).ok()?;
    let bytes = std::fs::read(v.join(exe)).ok()?;
    use sha2::Digest;
    let hex: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(v.join("rg.sha256"), hex).ok()?;
    Some(v)
}

#[test]
fn files_write_read_edit_stat_list_find_search() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let w = work.path();
    let v = vendor(home.path());
    let mut env: Vec<(&str, &Path)> = vec![("WINDOWS_SOSA_MCP_HOME", home.path())];
    if let Some(v) = v.as_deref() {
        env.push(("WINDOWS_SOSA_MCP_VENDOR", v));
    }
    let mut mcp = Mcp::spawn_env(&env);
    let f = p(w, "nota.txt");
    let r = mcp.call(
        "windows_write_text_file",
        json!({"path": f, "content": "hola\r\nmundo ñ\r\n", "mode": "create"}),
    );
    assert_eq!(r["status"], "succeeded", "{r}");
    let sha = r["data"]["sha256_after"].as_str().unwrap().to_string();
    let again = mcp.call(
        "windows_write_text_file",
        json!({"path": f, "content": "x", "mode": "create"}),
    );
    assert_eq!(again["error_kind"], "already_exists", "{again}");
    let rd = mcp.call("windows_read_text_file", json!({"path": f}));
    assert_eq!(rd["status"], "succeeded", "{rd}");
    assert_eq!(rd["data"]["newline"], "crlf", "{rd}");
    assert!(rd["data"]["lines"].to_string().contains("mundo ñ"), "{rd}");
    let e = mcp.call(
        "windows_edit_text_file",
        json!({"path": f, "expected_sha256": sha, "edits": [{"old_text": "mundo", "new_text": "world"}]}),
    );
    assert_eq!(e["status"], "succeeded", "{e}");
    let rd = mcp.call("windows_read_text_file", json!({"path": f}));
    assert!(rd["data"]["lines"].to_string().contains("world ñ"), "{rd}");
    assert_eq!(rd["data"]["newline"], "crlf", "{rd}");
    let st = mcp.call("windows_stat_path", json!({"path": f}));
    assert_eq!(st["status"], "succeeded", "{st}");
    let ls = mcp.call(
        "windows_list_directory",
        json!({"path": w.to_string_lossy()}),
    );
    assert_eq!(ls["status"], "succeeded", "{ls}");
    let ff = mcp.call(
        "windows_find_files",
        json!({"path": w.to_string_lossy(), "name_pattern": "*.txt"}),
    );
    assert_eq!(ff["status"], "succeeded", "{ff}");
    assert!(ff["data"].to_string().contains("nota.txt"), "{ff}");
    let sr = mcp.call(
        "windows_search_text",
        json!({"path": w.to_string_lossy(), "pattern": "world"}),
    );
    if v.is_some() {
        assert_eq!(sr["status"], "succeeded", "{sr}");
        assert_eq!(
            sr["data"]["matches"].as_array().map(Vec::len),
            Some(1),
            "{sr}"
        );
    } else {
        assert_eq!(sr["error_kind"], "dependency_missing");
    }
    let missing = mcp.call("windows_stat_path", json!({"path": p(w, "nope")}));
    assert_eq!(missing["status"], "succeeded", "{missing}");
    if cfg!(windows) {
        let unc = mcp.call(
            "windows_write_text_file",
            json!({"path": "\\\\srv\\share\\x.txt", "content": "x", "mode": "create"}),
        );
        assert_eq!(unc["error_kind"], "unsupported_path", "{unc}");
    }
}

fn script(win: &str, sh: &str) -> String {
    if cfg!(windows) { win.into() } else { sh.into() }
}

fn wait_finished(mcp: &mut Mcp, id: &str, secs: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let s = mcp.call("windows_job_status", json!({"job_id": id}));
        assert_eq!(s["status"], "succeeded", "{s}");
        if !matches!(s["data"]["state"].as_str(), Some("starting" | "running"))
            || Instant::now() > deadline
        {
            return s;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn jobs_survive_server_exit_and_cancel_kills_tree() {
    let home = tempfile::tempdir().unwrap();
    let env = [("WINDOWS_SOSA_MCP_HOME", home.path())];
    let id = {
        let mut mcp = Mcp::spawn_env(&env);
        let s = mcp.call(
            "windows_job_start",
            json!({"script": script("Start-Sleep -Seconds 3; Write-Output 'done ñ'; exit 3", "sleep 3; echo 'done ñ'; exit 3"),
                   "label": "survive"}),
        );
        assert_eq!(s["status"], "succeeded", "{s}");
        assert_eq!(s["data"]["launch_confirmed"], true, "{s}");
        s["data"]["job_id"].as_str().unwrap().to_string()
    }; // server killed here; the job must keep running
    let mut mcp = Mcp::spawn_env(&env);
    let st = wait_finished(&mut mcp, &id, 60);
    assert_eq!(st["data"]["state"], "failed", "{st}");
    assert_eq!(st["data"]["exit_code"], 3);
    assert_eq!(st["data"]["finished_by"], "supervisor");
    assert!(
        st["data"]["stdout_tail"]
            .as_str()
            .unwrap()
            .contains("done ñ"),
        "{st}"
    );

    let long = mcp.call(
        "windows_job_start",
        json!({"script": script("Start-Process -NoNewWindow powershell -ArgumentList '-c','Start-Sleep 120'; Start-Sleep -Seconds 120", "sleep 120 & sleep 120")}),
    );
    let lid = long["data"]["job_id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_secs(2));
    let c = mcp.call("windows_job_cancel", json!({"job_id": lid}));
    assert_eq!(c["status"], "succeeded", "{c}");
    assert_eq!(c["data"]["state"], "cancelled", "{c}");
    assert_eq!(c["data"]["kill_verified"], true, "{c}");
    assert_eq!(c["data"]["action"], "supervisor", "{c}");
    let c2 = mcp.call("windows_job_cancel", json!({"job_id": lid}));
    assert_eq!(c2["data"]["already_finished"], true);
    let l = mcp.call("windows_job_list", json!({"state": "finished"}));
    assert_eq!(l["data"]["count"], 2, "{l}");
    let nf = mcp.call(
        "windows_job_status",
        json!({"job_id": "00000000-0000-4000-8000-000000000000"}),
    );
    assert_eq!(nf["error_kind"], "not_found");
}

#[test]
fn busy_lock_across_processes_and_audit() {
    let home = tempfile::tempdir().unwrap();
    let fake = home.path().join(if cfg!(windows) {
        "winget.cmd"
    } else {
        "winget.sh"
    });
    #[cfg(windows)]
    {
        std::fs::write(&fake, "@ping -n 4 127.0.0.1 >nul\r\n@echo No installed package found matching input criteria.\r\n@exit /b -1978335212\r\n").unwrap();
    }
    #[cfg(unix)]
    {
        std::fs::write(&fake, "#!/bin/sh\nsleep 3\necho 'No installed package found matching input criteria.'\nexit 20\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let env = [
        ("WINDOWS_SOSA_MCP_HOME", home.path()),
        ("WINDOWS_SOSA_MCP_WINGET", fake.as_path()),
    ];
    let mut b = Mcp::spawn_env(&env);
    let (hp, fp) = (home.path().to_path_buf(), fake.clone());
    let h = std::thread::spawn(move || {
        let mut a = Mcp::spawn_env(&[
            ("WINDOWS_SOSA_MCP_HOME", hp.as_path()),
            ("WINDOWS_SOSA_MCP_WINGET", fp.as_path()),
        ]);
        a.call("windows_package_query", json!({"id": "Example.Fake"}))
    });
    std::thread::sleep(Duration::from_millis(1200));
    let busy = b.call("windows_job_list", json!({}));
    assert_eq!(busy["error_kind"], "busy", "{busy}");
    let q = h.join().unwrap();
    assert_eq!(q["status"], "succeeded", "{q}");
    assert_eq!(q["data"]["installed"], false);
    let ok = b.call("windows_job_list", json!({}));
    assert_eq!(ok["status"], "succeeded");
    let bad = b.call(
        "windows_write_text_file",
        json!({"path": "relative.txt", "content": "x", "mode": "create"}),
    );
    assert_eq!(bad["status"], "failed");
    let log = std::fs::read_to_string(home.path().join("audit").join("events.jsonl")).unwrap();
    let phases: Vec<String> = log
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .map(|v| {
            format!(
                "{}:{}",
                v["phase"].as_str().unwrap_or(""),
                v["error_kind"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert!(phases.contains(&"rejected:busy".to_string()), "{phases:?}");
    assert!(
        phases.iter().any(|p| p.starts_with("started")),
        "{phases:?}"
    );
    assert!(
        phases.iter().any(|p| p.starts_with("finished")),
        "{phases:?}"
    );
    assert!(
        phases.iter().any(|p| p.starts_with("session")),
        "{phases:?}"
    );
    assert!(
        !log.contains("\"content\":\"x\""),
        "audit must not store content"
    );
}

#[test]
fn ui_tools_answer_natively() {
    let home = tempfile::tempdir().unwrap();
    let policy = home.path().join("missing_policy.json");
    let mut mcp = Mcp::spawn_env(&[
        ("WINDOWS_SOSA_MCP_HOME", home.path()),
        ("WINDOWS_SOSA_MCP_UI_POLICY", &policy),
    ]);
    let lw = mcp.call("windows_list_windows", json!({}));
    let gone = mcp.call("windows_ui_snapshot", json!({"window": {"hwnd": 1}}));
    let stale = mcp.call(
        "windows_ui_action",
        json!({"snapshot_id": "0123456789abcdef0123456789abcdef", "ref": "e1",
               "action": "invoke", "element": "test"}),
    );
    if cfg!(windows) {
        assert_eq!(lw["status"], "succeeded", "{lw}");
        assert!(
            lw["data"]["monitors"]
                .as_array()
                .is_some_and(|m| !m.is_empty())
        );
        assert_eq!(gone["error_kind"], "window_gone", "{gone}");
        assert_eq!(stale["error_kind"], "snapshot_not_found", "{stale}");
    } else {
        for r in [&lw, &gone] {
            assert_eq!(r["error_kind"], "unsupported_platform", "{r}");
        }
    }
}

/// Hands-free tools without touching the desktop: the hands-free session runs in
/// dry-run mode (no hooks, no overlay, input counted but never sent, D29/D33),
/// `windows_input` only uses `wait` actions and `windows_wait_until` only
/// read-only conditions.
#[test]
fn handsfree_dry_run_lifecycle_and_read_only_waits() {
    let home = tempfile::tempdir().unwrap();
    let policy = home.path().join("missing_policy.json");
    let mut mcp = Mcp::spawn_env(&[
        ("WINDOWS_SOSA_MCP_HOME", home.path()),
        ("WINDOWS_SOSA_MCP_UI_POLICY", &policy),
        ("WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN", Path::new("1")),
    ]);
    let wait_only = json!([{"type": "wait", "ms": 20}]);

    let st = mcp.call("windows_handsfree", json!({"action": "status"}));
    assert_eq!(st["status"], "succeeded", "{st}");
    assert_eq!(st["data"]["active"], false);
    assert_eq!(st["data"]["message"], "hands-free mode is not active");

    let no = mcp.call("windows_input", json!({"actions": wait_only}));
    assert_eq!(no["error_kind"], "handsfree_required", "{no}");
    // Gate before the snapshot lookup (D35): any snapshot id is refused.
    let act = mcp.call(
        "windows_ui_action",
        json!({"snapshot_id": "0123456789abcdef0123456789abcdef", "ref": "e1",
               "action": "activate", "element": "x"}),
    );
    let gate = if cfg!(windows) {
        "handsfree_required"
    } else {
        "unsupported_platform"
    };
    assert_eq!(act["error_kind"], gate, "{act}");

    let t = mcp.call(
        "windows_wait_until",
        json!({"condition": "time", "timeout_seconds": 1}),
    );
    let w = mcp.call(
        "windows_wait_until",
        json!({"condition": "window_appears", "process": "no-such-process-sosa",
               "timeout_seconds": 1, "poll_ms": 200}),
    );
    let bad = mcp.call("windows_wait_until", json!({"condition": "foreground_is"}));
    assert_eq!(bad["error_kind"], "invalid_arguments", "{bad}");
    if cfg!(windows) {
        assert_eq!(t["status"], "succeeded", "{t}");
        assert_eq!(t["data"]["met"], true);
        assert!(t["data"]["elapsed_ms"].as_i64().unwrap() >= 900);
        assert_eq!(w["error_kind"], "condition_not_met", "{w}");
        assert_eq!(w["data"]["met"], false);
        assert_eq!(w["data"]["observed"]["matches"], 0);
        assert!(w["data"]["polls"].as_u64().unwrap() >= 3);
    } else {
        assert_eq!(t["error_kind"], "unsupported_platform", "{t}");
        assert_eq!(w["error_kind"], "unsupported_platform", "{w}");
    }

    let start = mcp.call(
        "windows_handsfree",
        json!({"action": "start", "reason": "stdio dry-run test", "timeout_seconds": 60,
               "arm_delay_ms": 500}),
    );
    assert_eq!(start["status"], "succeeded", "{start}");
    assert_eq!(start["data"]["active"], true);
    assert_eq!(start["data"]["guard"], "block");
    assert_eq!(start["data"]["release_hotkey"], "Ctrl+Alt+Shift+H");
    let sid = start["data"]["session_id"].as_str().unwrap().to_string();
    assert_eq!(start["session_id"], sid.as_str());
    assert!(
        start["data"]["message"]
            .as_str()
            .unwrap()
            .contains("ends ONLY by Ctrl+Alt+Shift+H")
    );
    let again = mcp.call(
        "windows_handsfree",
        json!({"action": "start", "reason": "second"}),
    );
    assert_eq!(again["error_kind"], "handsfree_already_active", "{again}");

    std::thread::sleep(std::time::Duration::from_millis(1500));
    let st = mcp.call("windows_handsfree", json!({"action": "status"}));
    assert_eq!(st["data"]["active"], true, "{st}");
    assert_eq!(st["data"]["armed"], true, "{st}");
    assert!(st["data"]["heartbeat_age_s"].as_f64().unwrap() < 6.0);

    let mism = mcp.call(
        "windows_input",
        json!({"actions": wait_only, "session_id": "0000000000000000"}),
    );
    let ok = mcp.call(
        "windows_input",
        json!({"actions": wait_only, "session_id": sid}),
    );
    if cfg!(windows) {
        assert_eq!(mism["error_kind"], "handsfree_session_mismatch", "{mism}");
        // CI runners may have no interactive desktop (session_locked) or an
        // elevated terminal in the foreground (target_elevated): both are
        // environment-dependent guard results, not failures of the dry run.
        let env_guard = ["session_locked", "target_elevated"];
        if !env_guard.contains(&ok["error_kind"].as_str().unwrap_or("")) {
            assert_eq!(ok["status"], "succeeded", "{ok}");
            assert_eq!(ok["data"]["executed_count"], 1);
            assert_eq!(ok["data"]["any_input"], false);
            assert_eq!(ok["data"]["coordinate_space"], "virtual_desktop");
            // Dry-run cursor (D36): a counted move "lands", so the
            // placement check of the following click passes although the
            // real cursor never moved.
            let vs = &ok["data"]["virtual_screen"];
            let (x, y) = (
                vs["x"].as_i64().unwrap() + vs["width"].as_i64().unwrap() / 2 + 7,
                vs["y"].as_i64().unwrap() + vs["height"].as_i64().unwrap() / 2 + 3,
            );
            let mv = mcp.call(
                "windows_input",
                json!({"actions": [{"type": "move", "x": x, "y": y},
                                   {"type": "mouse_down"}, {"type": "mouse_up"}],
                       "session_id": sid, "abort_on_foreground_change": false}),
            );
            if !env_guard.contains(&mv["error_kind"].as_str().unwrap_or("")) {
                assert_eq!(mv["status"], "succeeded", "{mv}");
                assert_eq!(mv["data"]["executed_count"], 3, "{mv}");
            }
        }
    } else {
        assert_eq!(mism["error_kind"], "handsfree_session_mismatch", "{mism}");
        assert_eq!(ok["error_kind"], "unsupported_platform", "{ok}");
    }

    let stop = mcp.call("windows_handsfree", json!({"action": "stop"}));
    assert_eq!(stop["status"], "succeeded", "{stop}");
    assert_eq!(stop["data"]["active"], false);
    assert_eq!(stop["data"]["ended_by"], "stop");
    let after = mcp.call("windows_input", json!({"actions": wait_only}));
    assert_eq!(after["error_kind"], "handsfree_required", "{after}");
    assert!(
        after["error"]["message"]
            .as_str()
            .unwrap()
            .contains("(ended_by=stop)")
    );
    let nothing = mcp.call("windows_handsfree", json!({"action": "stop"}));
    assert_eq!(nothing["data"]["message"], "nothing to stop");
}

/// D35 on a real desktop: `activate` with the root ref of a snapshot taken
/// while the window was still initializing (title and subtree changed
/// since). Changes the foreground, so it is opt-in:
/// `cargo test --test stdio -- --ignored activate_root`.
#[cfg(windows)]
#[test]
#[ignore = "needs an interactive desktop; brings a self-made test window to the foreground"]
fn activate_root_after_retitle_e2e() {
    let home = tempfile::tempdir().unwrap();
    let policy = home.path().join("missing_policy.json");
    let mut mcp = Mcp::spawn_env(&[
        ("WINDOWS_SOSA_MCP_HOME", home.path()),
        ("WINDOWS_SOSA_MCP_UI_POLICY", &policy),
        ("WINDOWS_SOSA_MCP_HANDSFREE_DRYRUN", Path::new("1")),
    ]);
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/acceptance/hf_test_app.ps1");
    let title = format!("SosaE2E-{}", uuid::Uuid::new_v4().simple());
    let state = home.path().join("app.json");
    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _child = KillOnDrop(
        std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&app)
            .args([
                "-Title",
                &title,
                "-Seconds",
                "60",
                "-RetitleAfterMs",
                "1500",
                "-StateFile",
            ])
            .arg(&state)
            .spawn()
            .unwrap(),
    );
    let w = mcp.call(
        "windows_wait_until",
        json!({"condition": "window_appears", "title_contains": title, "timeout_seconds": 20}),
    );
    assert_eq!(w["data"]["met"], true, "{w}");
    let hwnd = w["data"]["observed"]["sample"][0]["hwnd"].as_i64().unwrap();
    let snap = mcp.call(
        "windows_ui_snapshot",
        json!({"window": {"hwnd": hwnd}, "max_nodes": 40}),
    );
    let root = &snap["data"]["elements"][0];
    assert!(
        root["name"].as_str().unwrap().ends_with("(loading)"),
        "{root}"
    );
    let loaded = mcp.call(
        "windows_wait_until",
        json!({"condition": "element_appears", "window": {"hwnd": hwnd},
               "automation_id": "lblLoaded", "timeout_seconds": 10}),
    );
    assert_eq!(loaded["data"]["met"], true, "{loaded}");
    let start = mcp.call(
        "windows_handsfree",
        json!({"action": "start", "reason": "stdio e2e activate", "timeout_seconds": 60,
               "arm_delay_ms": 500}),
    );
    assert_eq!(start["status"], "succeeded", "{start}");
    let act = mcp.call(
        "windows_ui_action",
        json!({"snapshot_id": snap["data"]["snapshot_id"], "ref": "e1",
               "action": "activate", "element": root["name"]}),
    );
    mcp.call("windows_handsfree", json!({"action": "stop"}));
    assert_eq!(act["status"], "succeeded", "{act}");
    assert_eq!(act["data"]["applied"], true, "{act}");
    assert_eq!(act["data"]["foreground_after"]["root_hwnd"], hwnd, "{act}");
}

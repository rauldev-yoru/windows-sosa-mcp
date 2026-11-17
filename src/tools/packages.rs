//! WinGet package tools: `windows_package_query` (exact id, synchronous) and
//! `windows_package_install` (decision from a fresh query, then a detached
//! job running the bundled `package_install.ps1`, bounded wait, verification
//! by re-listing). winget.exe is invoked directly (no PowerShell host) for queries.

use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::jobs;
use super::text::TextError;
use super::winget_codes::{self, NO_APPS, REBOOT_INITIATED, REBOOT_REQUIRED_TO_FINISH, describe};
use super::{opt_bool, opt_i64, opt_str};
use crate::envelope::{self, Envelope};

pub const INSTALL_SCRIPT: &str = include_str!("../../assets/package_install.ps1");
const MARKER: &str = "WINDOWS_SOSA_MCP_PACKAGE_RESULT:";

fn strip_ansi(s: &str) -> String {
    // \x1b\[[0-9;?]*[A-Za-z]
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            let mut look = it.clone();
            look.next();
            let mut consumed = 1;
            let mut ok = false;
            while let Some(&n) = look.peek() {
                if n.is_ascii_digit() || n == ';' || n == '?' {
                    look.next();
                    consumed += 1;
                } else {
                    ok = n.is_ascii_alphabetic();
                    break;
                }
            }
            if ok {
                for _ in 0..=consumed {
                    it.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

pub fn clean_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| strip_ansi(l).replace('\r', "").trim_end().to_string())
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !matches!(t, "-" | "\\" | "|" | "/")
        })
        .collect()
}

fn is_sep(l: &str) -> bool {
    let t = l.trim();
    t.len() >= 5 && t.bytes().all(|b| b == b'-')
}

fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Positional, locale-independent winget table parsing (docs/design.md, D16).
pub fn parse_table(
    lines: &[String],
    id: &str,
    source_filter: Option<&str>,
) -> (Vec<Value>, Vec<String>) {
    let (mut rows, mut warnings) = (Vec::new(), Vec::new());
    let Some(sep) = lines.iter().position(|l| is_sep(l)).filter(|s| *s > 0) else {
        return (rows, warnings);
    };
    let header_cols = lines[sep - 1].split_whitespace().count();
    let lid = id.to_lowercase();
    for line in &lines[sep + 1..] {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.is_empty() {
            break;
        }
        let Some(idx) = tokens.iter().position(|t| t.to_lowercase() == lid) else {
            if tokens.iter().any(|t| t.to_lowercase().starts_with(&lid)) {
                warnings.push(format!(
                    "Row with a truncated id was ignored: {}",
                    trunc(line, 200)
                ));
            }
            continue;
        };
        let rest = &tokens[idx + 1..];
        let version = rest.first().copied();
        let tail = rest.get(1..).unwrap_or(&[]);
        let (mut available, mut source) = (None, None);
        if header_cols >= 5 {
            if tail.len() >= 2 {
                (available, source) = (Some(tail[0]), Some(tail[1]));
            } else if let Some(t) = tail.first() {
                source = Some(*t);
            }
        } else if header_cols == 4
            && let Some(t) = tail.first()
        {
            if source_filter.is_some() {
                available = Some(*t)
            } else {
                source = Some(*t)
            }
        }
        if source.is_none() {
            source = source_filter;
        }
        let name = tokens[..idx].join(" ");
        rows.push(json!({
            "name": if name.is_empty() { None } else { Some(name) }, "id": tokens[idx], "version": version,
            "available": available, "source": source, "raw": trunc(line, 400),
        }));
    }
    (rows, warnings)
}

pub fn parse_versions(lines: &[String]) -> Vec<String> {
    let Some(sep) = lines.iter().position(|l| is_sep(l)) else {
        return Vec::new();
    };
    lines[sep + 1..]
        .iter()
        .map(|l| l.trim())
        .take_while(|t| !t.is_empty() && !t.contains(' '))
        .map(String::from)
        .collect()
}

pub fn winget_path() -> Option<String> {
    if let Ok(p) = std::env::var("WINDOWS_SOSA_MCP_WINGET") {
        return Some(p);
    }
    let local = std::env::var("LOCALAPPDATA")
        .ok()
        .map(|l| format!("{l}\\Microsoft\\WindowsApps\\winget.exe"));
    if let Some(p) = local.filter(|p| std::path::Path::new(p).exists()) {
        return Some(p);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| {
                d.join(if cfg!(windows) {
                    "winget.exe"
                } else {
                    "winget"
                })
            })
            .find(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
    })
}

/// Run winget with merged output lines (PowerShell `2>&1 | % {"$_"}` shape).
fn run_winget(
    exe: &str,
    args: &[String],
    timeout: Duration,
) -> Result<(i64, Vec<String>), TextError> {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    let c = super::shell::capture(cmd, timeout, 262_144)
        .map_err(|e| TextError::msg("launch_failed", e.to_string()))?;
    if c.timed_out {
        return Err(TextError::msg("timeout", "winget did not finish in time"));
    }
    let lines = c
        .stdout
        .split('\n')
        .chain(c.stderr.split('\n'))
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect::<Vec<_>>();
    // Drop the empty fragment after a final newline (PowerShell does not emit it).
    let lines = lines.into_iter().filter(|l| !l.is_empty()).collect();
    Ok((c.exit_code.map_or(-1, i64::from), lines))
}

fn hexs(code: i64) -> String {
    format!("0x{:x}", code as u32)
}

pub struct QueryArgs {
    pub id: String,
    pub source: Option<String>,
    pub include_available_versions: bool,
}

pub fn interpret_query(
    path: &str,
    list_code: i64,
    list: &[String],
    versions: Option<(i64, Vec<String>)>,
    a: &QueryArgs,
) -> Value {
    let raw: Vec<String> = list.iter().take(60).cloned().collect();
    let lines = clean_lines(&raw);
    let (rows, mut warnings) = parse_table(&lines, &a.id, a.source.as_deref());
    let installed = list_code == 0 && !rows.is_empty();
    if list_code == 0 && rows.is_empty() {
        warnings.push(
            "winget list returned 0 but no row matched the exact id; inspect raw_lines".into(),
        );
    } else if list_code != 0 && list_code != NO_APPS {
        let d = describe(list_code);
        warnings.push(format!(
            "winget list failed: {}",
            d["symbol"]
                .as_str()
                .map_or_else(|| hexs(list_code), String::from)
        ));
    }
    let first = rows.first().cloned().unwrap_or(json!({}));
    let pick = |k: &str| {
        if installed {
            first.get(k).cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    };
    let mut data = json!({
        "id": a.id, "source_filter": a.source, "winget_path": path, "installed": installed,
        "installed_version": pick("version"), "installed_source": pick("source"),
        "upgrade_available_version": pick("available"), "ambiguous": rows.len() > 1, "rows": rows,
        "list_exit": describe(list_code), "available_versions": null, "in_source": null, "versions_exit": null,
        "raw_lines": lines.iter().take(60).collect::<Vec<_>>(), "warnings": [],
    });
    if let Some((vcode, vlines)) = versions {
        let vraw: Vec<String> = vlines.into_iter().take(80).collect();
        let vl = clean_lines(&vraw);
        data["versions_exit"] = describe(vcode);
        data["available_versions"] = json!(if vcode == 0 {
            parse_versions(&vl)
        } else {
            vec![]
        });
        data["in_source"] = json!(vcode == 0);
        if vcode != 0 && vcode != NO_APPS {
            let d = describe(vcode);
            warnings.push(format!(
                "winget show failed: {}",
                d["symbol"]
                    .as_str()
                    .map_or_else(|| hexs(vcode), String::from)
            ));
        }
    }
    data["warnings"] = json!(warnings);
    data
}

pub fn query(a: &QueryArgs) -> Result<Value, TextError> {
    let exe = winget_path().ok_or_else(|| {
        TextError::msg(
            "dependency_missing",
            "winget.exe not found in WindowsApps or PATH",
        )
    })?;
    let mut list_args: Vec<String> = [
        "list",
        "--id",
        &a.id,
        "--exact",
        "--disable-interactivity",
        "--accept-source-agreements",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(s) = &a.source {
        list_args.extend(["--source".into(), s.clone()]);
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    let (list_code, list) = run_winget(&exe, &list_args, Duration::from_secs(120))?;
    let versions = if a.include_available_versions {
        let mut show: Vec<String> = [
            "show",
            "--id",
            &a.id,
            "--exact",
            "--versions",
            "--disable-interactivity",
            "--accept-source-agreements",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(s) = &a.source {
            show.extend(["--source".into(), s.clone()]);
        }
        Some(run_winget(
            &exe,
            &show,
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1)),
        )?)
    } else {
        None
    };
    Ok(interpret_query(&exe, list_code, &list, versions, a))
}

pub struct InstallArgs {
    pub id: String,
    pub version: Option<String>,
    pub source: Option<String>,
    pub scope: Option<String>,
    pub allow_upgrade: bool,
    pub wait_seconds: u64,
    pub job_timeout_seconds: i64,
    pub dry_run: bool,
}

pub fn install_args(a: &InstallArgs, action: &str) -> Vec<String> {
    let mut v: Vec<String> = [
        if action == "upgrade" {
            "upgrade"
        } else {
            "install"
        },
        "--id",
        &a.id,
        "--exact",
        "--silent",
        "--disable-interactivity",
        "--accept-package-agreements",
        "--accept-source-agreements",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for (flag, val) in [
        ("--version", &a.version),
        ("--scope", &a.scope),
        ("--source", &a.source),
    ] {
        if let Some(x) = val {
            v.extend([flag.to_string(), x.clone()]);
        }
    }
    v
}

pub fn plan(a: &InstallArgs, before: &Value) -> Result<(&'static str, String), TextError> {
    let iv = before["installed_version"]
        .as_str()
        .unwrap_or("None")
        .to_string();
    if before["installed"] != true {
        return Ok((
            "install",
            "Package not tracked by WinGet; installing".into(),
        ));
    }
    match &a.version {
        None if !a.allow_upgrade => Ok((
            "none",
            format!("Already installed ({iv}); allow_upgrade=false so nothing changes"),
        )),
        None => match before["upgrade_available_version"].as_str() {
            Some(up) => Ok(("upgrade", format!("Installed {iv}, upgrade {up} available"))),
            None => Ok((
                "none",
                format!("Already installed ({iv}) and WinGet reports no upgrade"),
            )),
        },
        Some(req) if *req == iv => Ok(("none", "Requested version already installed".into())),
        Some(req) if !a.allow_upgrade => Err(TextError::msg(
            "already_installed_different_version",
            format!(
                "Installed {iv}, requested {req}; pass allow_upgrade=true to change an installed package"
            ),
        )),
        Some(req) => Ok(("upgrade", format!("Installed {iv}, requested {req}"))),
    }
}

pub fn parse_marker(tail: &str) -> Option<Value> {
    tail.lines()
        .rev()
        .find_map(|l| l.strip_prefix(MARKER))
        .and_then(|j| serde_json::from_str(j).ok())
}

/// Returns (status, data, error).
pub fn execute_install(
    a: &InstallArgs,
) -> Result<(&'static str, Value, Option<TextError>), TextError> {
    let q = query(&QueryArgs {
        id: a.id.clone(),
        source: a.source.clone(),
        include_available_versions: false,
    })?;
    let before = json!({"installed": q["installed"], "version": q["installed_version"], "source": q["installed_source"]});
    let (action, message) = plan(a, &q)?;
    let mut data = json!({
        "id": a.id, "requested_version": a.version, "scope": a.scope, "source": a.source, "allow_upgrade": a.allow_upgrade,
        "dry_run": a.dry_run, "action": action, "before": before, "after": null, "job_id": null, "job_state": null,
        "winget": null, "winget_args": [], "verified": null, "reboot_required": false, "message": message,
    });
    if action == "none" {
        data["verified"] = json!(true);
        data["after"] = data["before"].clone();
        return Ok(("succeeded", data, None));
    }
    let argv = install_args(a, action);
    data["winget_args"] = json!(argv);
    if a.dry_run {
        data["message"] = json!(format!("dry_run: would run winget {}", argv.join(" ")));
        return Ok(("succeeded", data, None));
    }
    let files = vec![(
        "package.json".to_string(),
        json!({"winget_path": q["winget_path"], "id": a.id, "source": a.source, "args": argv})
            .to_string(),
    )];
    let start_args = jobs::StartArgs {
        script: INSTALL_SCRIPT.to_string(),
        cwd: None,
        timeout_seconds: a.job_timeout_seconds,
        max_output_bytes: 16_777_216,
        label: Some(format!("winget {action} {}", a.id)),
    };
    let started = jobs::start(&start_args, "package_install", &files, None)?;
    let job_id = started["job_id"].as_str().unwrap_or_default().to_string();
    data["job_id"] = json!(job_id);
    let deadline = Instant::now() + Duration::from_secs(a.wait_seconds);
    let status = loop {
        let s = jobs::status(&job_id, 65536)?;
        let running = matches!(s["state"].as_str(), Some("starting" | "running"));
        if !running || Instant::now() >= deadline {
            break s;
        }
        std::thread::sleep(
            Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    };
    data["job_state"] = status["state"].clone();
    if matches!(status["state"].as_str(), Some("starting" | "running")) {
        return Ok((
            "timed_out",
            data,
            Some(TextError::msg(
                "wait_expired",
                "Installer job still running; poll windows_job_status(job_id) then windows_package_query",
            )),
        ));
    }
    let Some(marker) = parse_marker(status["stdout_tail"].as_str().unwrap_or("")) else {
        let st = status["state"].as_str().unwrap_or("unknown").to_string();
        return Ok((
            "failed",
            data,
            Some(TextError::msg(
                "install_result_missing",
                format!("Job ended ({st}) without a package result; inspect job logs"),
            )),
        ));
    };
    let code = marker["install_exit_code"].as_i64().unwrap_or(-1);
    data["winget"] = describe(code);
    data["reboot_required"] = json!(code == REBOOT_REQUIRED_TO_FINISH || code == REBOOT_INITIATED);
    let list: Vec<String> = marker["list_lines"]
        .as_array()
        .map(|v| {
            v.iter()
                .map(|x| x.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    let (rows, _) = parse_table(&clean_lines(&list), &a.id, a.source.as_deref());
    let after_installed = marker["list_exit_code"].as_i64() == Some(0) && !rows.is_empty();
    let pick = |k: &str| {
        if after_installed {
            rows[0][k].clone()
        } else {
            Value::Null
        }
    };
    let after =
        json!({"installed": after_installed, "version": pick("version"), "source": pick("source")});
    data["after"] = after.clone();
    let verified = match &a.version {
        Some(req) => after_installed && after["version"].as_str() == Some(req.as_str()),
        None if action == "upgrade" => {
            after_installed && after["version"] != data["before"]["version"]
        }
        None => after_installed,
    };
    data["verified"] = json!(verified);
    let av = after["version"].as_str().unwrap_or("None").to_string();
    if verified {
        data["message"] = json!(format!("{action} verified: {} {av}", a.id));
        return Ok(("succeeded", data, None));
    }
    let w = &data["winget"];
    let symbol = w["symbol"]
        .as_str()
        .or_else(|| w["hex"].as_str())
        .unwrap_or("None")
        .to_string();
    let msg = format!(
        "winget exit {symbol}; post-install state: installed={} version={av}",
        if after_installed { "True" } else { "False" }
    );
    data["message"] = json!(msg);
    Ok((
        "failed",
        data,
        Some(TextError::msg("install_not_verified", msg)),
    ))
}

pub fn execute(name: &str, a: &Value) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base(name);
    let id = opt_str(a, "id").unwrap_or_default();
    let source = opt_str(a, "source");
    let out = if name == "windows_package_query" {
        query(&QueryArgs {
            id,
            source,
            include_available_versions: opt_bool(a, "include_available_versions").unwrap_or(false),
        })
        .map(|d| ("succeeded", d, None))
    } else {
        execute_install(&InstallArgs {
            id,
            version: opt_str(a, "version"),
            source,
            scope: opt_str(a, "scope"),
            allow_upgrade: opt_bool(a, "allow_upgrade").unwrap_or(false),
            wait_seconds: opt_i64(a, "wait_seconds").unwrap_or(240) as u64,
            job_timeout_seconds: opt_i64(a, "job_timeout_seconds").unwrap_or(1800),
            dry_run: opt_bool(a, "dry_run").unwrap_or(false),
        })
    };
    match out {
        Ok((status, data, err)) => {
            let job = data.get("job_id").cloned();
            r.insert("status".into(), json!(status));
            r.insert(
                "exit_code".into(),
                if status == "succeeded" {
                    json!(0)
                } else {
                    Value::Null
                },
            );
            if name == "windows_package_install" {
                r.insert("job_id".into(), job.clone().unwrap_or(Value::Null));
            }
            r.insert("data".into(), data);
            if let Some(e) = err {
                r.insert(
                    "error".into(),
                    json!({"kind": e.kind, "message": e.message}),
                );
                r.insert(
                    "may_have_changed".into(),
                    json!(job.is_some_and(|j| !j.is_null())),
                );
            }
        }
        Err(e) => {
            envelope::fail(&mut r, &e.kind, &e.message);
            if e.kind == "timeout" {
                r.insert("status".into(), json!("timed_out"));
                r.insert("timed_out".into(), json!(true));
                r.insert("windows_termination_confirmed".into(), json!(false));
            }
            r.insert("may_have_changed".into(), json!(e.may_have_changed));
        }
    }
    envelope::stamp(&mut r, start);
    r
}

#[allow(unused_imports)]
use winget_codes as _codes;

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn table_parsing_positional() {
        let lines = clean_lines(&s(&[
            "\x1b[2K   -\r",
            "Name           Id         Version Available Source",
            "---------------------------------------------------",
            "jq JSON tool   jqlang.jq  1.7.1   1.8.0     winget",
        ]));
        let (rows, w) = parse_table(&lines, "jqlang.jq", None);
        assert!(w.is_empty());
        assert_eq!(rows[0]["name"], "jq JSON tool");
        assert_eq!(rows[0]["available"], "1.8.0");
        assert_eq!(rows[0]["source"], "winget");
        let lines = s(&[
            "Nombre Id Versión Origen",
            "-----",
            "x jqlang.jq 1.7.1 winget",
            "y jqlang.jq.preview 1 w",
        ]);
        let (rows, w) = parse_table(&lines, "JQLANG.JQ", None);
        assert_eq!(
            (rows[0]["source"].as_str(), rows[0]["available"].is_null()),
            (Some("winget"), true)
        );
        assert_eq!(w.len(), 1);
        let (rows, _) = parse_table(
            &s(&["N I V A", "-----", "x jqlang.jq 1.7.1 1.8"]),
            "jqlang.jq",
            Some("winget"),
        );
        assert_eq!(
            (rows[0]["available"].as_str(), rows[0]["source"].as_str()),
            (Some("1.8"), Some("winget"))
        );
        assert_eq!(
            parse_versions(&s(&[
                "Found x", "Version", "-------", "1.8.0", "1.7.1", "", "x"
            ])),
            vec!["1.8.0", "1.7.1"]
        );
    }

    #[test]
    fn query_interpretation_and_plan() {
        let a = QueryArgs {
            id: "jqlang.jq".into(),
            source: None,
            include_available_versions: false,
        };
        let d = interpret_query(
            "w",
            NO_APPS,
            &s(&["No installed package found matching input criteria."]),
            None,
            &a,
        );
        assert_eq!(d["installed"], false);
        assert_eq!(d["list_exit"]["symbol"], "NO_APPLICATIONS_FOUND");
        assert!(d["warnings"].as_array().unwrap().is_empty());
        let ia = InstallArgs {
            id: "jqlang.jq".into(),
            version: None,
            source: None,
            scope: Some("user".into()),
            allow_upgrade: false,
            wait_seconds: 5,
            job_timeout_seconds: 60,
            dry_run: true,
        };
        assert_eq!(plan(&ia, &d).unwrap().0, "install");
        let inst = json!({"installed": true, "installed_version": "1.7.1", "upgrade_available_version": "1.8.0"});
        assert_eq!(plan(&ia, &inst).unwrap().0, "none");
        let ia2 = InstallArgs {
            version: Some("1.8.0".into()),
            ..ia
        };
        assert_eq!(
            plan(&ia2, &inst).unwrap_err().kind,
            "already_installed_different_version"
        );
        assert!(install_args(&ia2, "install").contains(&"--scope".to_string()));
        assert_eq!(
            parse_marker("a\nWINDOWS_SOSA_MCP_PACKAGE_RESULT:{\"install_exit_code\":0}\n").unwrap()
                ["install_exit_code"],
            0
        );
    }
}

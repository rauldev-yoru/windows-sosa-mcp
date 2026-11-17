//! Read-only filesystem tools, native (no PowerShell): `windows_list_directory`,
//! `windows_find_files`, `windows_stat_path`, `windows_read_text_file`
//! (prefix mode) and `windows_resolve_executable`.
//!
//! Every tool bounds its work (entry/depth/byte limits) and reports why it
//! stopped (`stop_reason`) with stable error kinds and data keys.

use std::path::Path;
use std::time::Instant;

use serde_json::{Map, Value, json};

use super::{ToolError, opt_bool, opt_i64, opt_str};
use crate::envelope::{self, Envelope};
use crate::paths::{self, Attrs, fmt_o, get_attributes};

pub fn io_kind(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "access_denied",
        std::io::ErrorKind::NotFound => "not_found",
        _ => "io_error",
    }
}

fn entry_json(name: &str, full: &str, meta: &std::fs::Metadata, a: Attrs) -> Value {
    json!({
        "name": name,
        "path": full,
        "kind": if a.directory { "directory" } else { "file" },
        "size_bytes": if a.directory { Value::Null } else { json!(meta.len()) },
        "modified_utc": meta.modified().map(fmt_o).unwrap_or_default(),
        "is_reparse_point": a.reparse,
    })
}

fn is_hidden(a: Attrs) -> bool {
    a.hidden || a.system
}

/// Root directory for list/find: the core's cwd rule plus "must exist".
fn root_dir(path: &str) -> Result<String, String> {
    if !paths::is_absolute_windows(path) {
        return Err("cwd must be an absolute Windows path".into());
    }
    let norm = if paths::unix_test_path(path) {
        path.to_string()
    } else {
        paths::normpath(path)
    };
    match std::fs::metadata(&norm) {
        Ok(m) if m.is_dir() => Ok(norm),
        Ok(_) => Err(format!(
            "Cannot find path '{path}' because it is not a directory."
        )),
        Err(e) => Err(format!("Cannot find path '{path}': {e}")),
    }
}

fn exec_failed(r: &mut Envelope, msg: String) {
    r.insert("exit_code".into(), json!(1));
    r.insert("stderr".into(), json!(msg.clone() + "\n"));
    r.insert("timed_out".into(), json!(false));
    r.insert("encoding_replaced".into(), json!(false));
    envelope::fail(r, "execution_failed", msg);
}

// ---------------------------------------------------------------- list

pub struct ListArgs {
    pub path: String,
    pub offset: usize,
    pub limit: usize,
    pub include_hidden: bool,
}

pub fn prepare_list(a: &Value) -> Result<ListArgs, ToolError> {
    Ok(ListArgs {
        path: opt_str(a, "path").unwrap_or_default(),
        offset: opt_i64(a, "offset").unwrap_or(0) as usize,
        limit: opt_i64(a, "limit").unwrap_or(50) as usize,
        include_hidden: opt_bool(a, "include_hidden").unwrap_or(false),
    })
}

fn sort_key(s: &str) -> String {
    s.to_lowercase()
}

pub fn list_directory(args: ListArgs) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base("windows_list_directory");
    let root = match root_dir(&args.path) {
        Ok(p) => p,
        Err(m) => {
            exec_failed(&mut r, m);
            envelope::stamp(&mut r, start);
            return r;
        }
    };
    let watch = Instant::now();
    let mut items: Vec<(String, String, Value)> = Vec::new();
    let (mut seen, mut skipped, mut reason) = (0usize, 0usize, "exhausted");
    match std::fs::read_dir(&root) {
        Err(e) => {
            exec_failed(&mut r, e.to_string());
            envelope::stamp(&mut r, start);
            return r;
        }
        Ok(it) => {
            for entry in it {
                if seen >= 5000 {
                    reason = "scan_limit";
                    break;
                }
                if watch.elapsed().as_millis() >= 5000 {
                    reason = "time_budget";
                    break;
                }
                let Ok(entry) = entry else { continue };
                seen += 1;
                let Ok(meta) = entry.metadata() else { continue };
                let a = paths::attrs(&meta, &entry.path());
                if !args.include_hidden && is_hidden(a) {
                    skipped += 1;
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let full = paths::join(&root, &name);
                items.push((
                    name.clone(),
                    full.clone(),
                    entry_json(&name, &full, &meta, a),
                ));
            }
        }
    }
    items.sort_by(|x, y| {
        sort_key(&x.0)
            .cmp(&sort_key(&y.0))
            .then_with(|| sort_key(&x.1).cmp(&sort_key(&y.1)))
            .then_with(|| x.0.cmp(&y.0))
    });
    let total = items.len();
    let page: Vec<Value> = items
        .into_iter()
        .skip(args.offset)
        .take(args.limit)
        .map(|i| i.2)
        .collect();
    let next = (args.offset + page.len() < total).then_some(args.offset + page.len());
    let data = json!({
        "path": root, "entries": page, "count": page.len(), "offset": args.offset,
        "next_offset": next, "available_in_scan": total, "scanned_entries": seen,
        "skipped_hidden": skipped, "scan_complete": reason == "exhausted", "stop_reason": reason,
        "operation_ms": watch.elapsed().as_millis() as u64,
    });
    envelope::succeed(&mut r, data);
    r.insert("timed_out".into(), json!(false));
    r.insert("encoding_replaced".into(), json!(false));
    envelope::stamp(&mut r, start);
    r
}

// ---------------------------------------------------------------- find

/// PowerShell `WildcardPattern` with `IgnoreCase`: `*`, `?`, `[set]`,
/// `[a-z]` ranges and the backtick escape.
#[derive(Debug, Clone)]
enum Tok {
    Lit(char),
    Any,
    Star,
    Set(Vec<(char, char)>),
}

pub struct Wildcard(Vec<Tok>);

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

impl Wildcard {
    pub fn new(p: &str) -> Self {
        let cs: Vec<char> = p.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < cs.len() {
            match cs[i] {
                '`' if i + 1 < cs.len() => {
                    out.push(Tok::Lit(fold(cs[i + 1])));
                    i += 2;
                    continue;
                }
                '*' => out.push(Tok::Star),
                '?' => out.push(Tok::Any),
                '[' => {
                    let mut j = i + 1;
                    let mut set = Vec::new();
                    while j < cs.len() && cs[j] != ']' {
                        let mut c = cs[j];
                        if c == '`' && j + 1 < cs.len() {
                            j += 1;
                            c = cs[j];
                        }
                        if j + 2 < cs.len() && cs[j + 1] == '-' && cs[j + 2] != ']' {
                            set.push((fold(c), fold(cs[j + 2])));
                            j += 3;
                        } else {
                            set.push((fold(c), fold(c)));
                            j += 1;
                        }
                    }
                    if j < cs.len() {
                        out.push(Tok::Set(set));
                        i = j + 1;
                        continue;
                    }
                    out.push(Tok::Lit('['));
                }
                c => out.push(Tok::Lit(fold(c))),
            }
            i += 1;
        }
        Wildcard(out)
    }

    pub fn is_match(&self, name: &str) -> bool {
        let s: Vec<char> = name.chars().map(fold).collect();
        fn m(p: &[Tok], s: &[char]) -> bool {
            match p.first() {
                None => s.is_empty(),
                Some(Tok::Star) => (0..=s.len()).any(|k| m(&p[1..], &s[k..])),
                Some(Tok::Any) => !s.is_empty() && m(&p[1..], &s[1..]),
                Some(Tok::Lit(c)) => s.first() == Some(c) && m(&p[1..], &s[1..]),
                Some(Tok::Set(set)) => {
                    s.first()
                        .is_some_and(|c| set.iter().any(|(a, b)| a <= c && c <= b))
                        && m(&p[1..], &s[1..])
                }
            }
        }
        m(&self.0, &s)
    }
}

pub struct FindArgs {
    pub path: String,
    pub name_pattern: String,
    pub kind: String,
    pub max_depth: usize,
    pub limit: usize,
    pub scan_limit: usize,
    pub budget_ms: u128,
    pub include_hidden: bool,
}

pub fn prepare_find(a: &Value) -> Result<FindArgs, ToolError> {
    Ok(FindArgs {
        path: opt_str(a, "path").unwrap_or_default(),
        name_pattern: opt_str(a, "name_pattern").unwrap_or_else(|| "*".into()),
        kind: opt_str(a, "kind").unwrap_or_else(|| "any".into()),
        max_depth: opt_i64(a, "max_depth").unwrap_or(3) as usize,
        limit: opt_i64(a, "limit").unwrap_or(50) as usize,
        scan_limit: opt_i64(a, "scan_limit").unwrap_or(10000) as usize,
        budget_ms: opt_i64(a, "budget_ms").unwrap_or(5000) as u128,
        include_hidden: opt_bool(a, "include_hidden").unwrap_or(false),
    })
}

pub fn find_files(args: FindArgs) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base("windows_find_files");
    let root = match root_dir(&args.path) {
        Ok(p) => p,
        Err(m) => {
            exec_failed(&mut r, m);
            envelope::stamp(&mut r, start);
            return r;
        }
    };
    let watch = Instant::now();
    let pattern = Wildcard::new(&args.name_pattern);
    let mut hits: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    let mut queue: std::collections::VecDeque<(String, usize)> = [(root.clone(), 0)].into();
    let (mut seen, mut dirs, mut links, mut hidden, mut depth_skips) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut reason, mut halt, mut error_count) = ("exhausted", false, 0usize);
    while let Some((dir, depth)) = (!halt).then(|| queue.pop_front()).flatten() {
        if watch.elapsed().as_millis() >= args.budget_ms {
            reason = "time_budget";
            break;
        }
        if dirs >= 1000 {
            reason = "directory_limit";
            break;
        }
        dirs += 1;
        let it = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) => {
                error_count += 1;
                if errors.len() < 20 {
                    errors.push(json!({"path": dir, "message": e.to_string()}));
                }
                continue;
            }
        };
        for entry in it {
            if watch.elapsed().as_millis() >= args.budget_ms {
                reason = "time_budget";
                halt = true;
                break;
            }
            if seen >= args.scan_limit {
                reason = "scan_limit";
                halt = true;
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    error_count += 1;
                    if errors.len() < 20 {
                        errors.push(json!({"path": dir, "message": e.to_string()}));
                    }
                    break;
                }
            };
            seen += 1;
            let Ok(meta) = entry.metadata() else { continue };
            let a = paths::attrs(&meta, &entry.path());
            if !args.include_hidden && is_hidden(a) {
                hidden += 1;
                continue;
            }
            let kind = if a.directory { "directory" } else { "file" };
            let name = entry.file_name().to_string_lossy().into_owned();
            let full = paths::join(&dir, &name);
            if (args.kind == "any" || args.kind == kind) && pattern.is_match(&name) {
                hits.push(entry_json(&name, &full, &meta, a));
                if hits.len() >= args.limit {
                    reason = "result_limit";
                    halt = true;
                    break;
                }
            }
            if a.directory {
                if a.reparse {
                    links += 1;
                } else if depth >= args.max_depth {
                    depth_skips += 1;
                } else if queue.len() + dirs >= 1000 {
                    reason = "directory_limit";
                    halt = true;
                    break;
                } else {
                    queue.push_back((full, depth + 1));
                }
            }
        }
    }
    let data = json!({
        "path": root, "name_pattern": args.name_pattern, "kind": args.kind, "max_depth": args.max_depth,
        "entries": hits, "count": hits.len(), "scanned_entries": seen, "scanned_directories": dirs,
        "skipped_reparse_directories": links, "skipped_hidden": hidden, "skipped_depth": depth_skips,
        "scan_complete": reason == "exhausted" && error_count == 0, "stop_reason": reason,
        "error_count": error_count, "errors": errors, "operation_ms": watch.elapsed().as_millis() as u64,
    });
    envelope::succeed(&mut r, data);
    r.insert("timed_out".into(), json!(false));
    r.insert("encoding_replaced".into(), json!(false));
    envelope::stamp(&mut r, start);
    r
}

// ---------------------------------------------------------------- stat

pub fn prepare_path(a: &Value) -> Result<String, ToolError> {
    paths::valid_path(&opt_str(a, "path").unwrap_or_default()).map_err(ToolError::invalid)
}

fn typed_done(r: &mut Envelope, start: Instant, out: Result<Value, (String, String)>) {
    match out {
        Ok(data) => envelope::succeed(r, data),
        Err((kind, msg)) => {
            r.insert("exit_code".into(), json!(0));
            envelope::fail(r, &kind, msg);
        }
    }
    r.insert("timed_out".into(), json!(false));
    r.insert("encoding_replaced".into(), json!(false));
    envelope::stamp(r, start);
}

pub fn stat_data(path: &str) -> Result<Value, (String, String)> {
    match get_attributes(Path::new(path)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({
            "path": path, "exists": false, "kind": null, "size_bytes": null,
            "modified_utc": null, "is_reparse_point": null,
        })),
        Err(e) => Err((io_kind(&e).into(), e.to_string())),
        Ok((meta, a)) => Ok(json!({
            "path": path, "exists": true,
            "kind": if a.directory { "directory" } else { "file" },
            "size_bytes": if a.directory { Value::Null } else { json!(meta.len()) },
            "modified_utc": meta.modified().map(fmt_o).ok(),
            "is_reparse_point": a.reparse,
        })),
    }
}

pub fn stat_path(path: String) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base("windows_stat_path");
    let out = stat_data(&path);
    typed_done(&mut r, start, out);
    r
}

// ---------------------------------------------------------------- read (prefix)

pub struct ReadArgs {
    pub path: String,
    pub include_sha256: bool,
    pub encoding: String,
    pub start_line: usize,
    pub max_lines: usize,
    pub max_bytes: usize,
}

pub fn prepare_read(a: &Value) -> Result<ReadArgs, ToolError> {
    Ok(ReadArgs {
        path: prepare_path(a)?,
        include_sha256: opt_bool(a, "include_sha256").unwrap_or(false),
        encoding: opt_str(a, "encoding").unwrap_or_else(|| "auto".into()),
        start_line: opt_i64(a, "start_line").unwrap_or(1) as usize,
        max_lines: opt_i64(a, "max_lines").unwrap_or(200) as usize,
        max_bytes: opt_i64(a, "max_bytes").unwrap_or(16384) as usize,
    })
}

/// Raw bytes of a bounded read, the `READ` template's `raw` object.
pub struct RawRead {
    pub path: String,
    pub size_bytes: u64,
    pub modified_utc: String,
    pub bytes: Vec<u8>,
    pub byte_limited: bool,
}

pub fn open_shared_read(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.share_mode(1); // FILE_SHARE_READ, like [IO.FileShare]::Read
    }
    o.open(path)
}

fn changed() -> (String, String) {
    ("io_error".into(), "File changed during read".into())
}

pub fn read_prefix(path: &str, max_bytes: usize) -> Result<RawRead, (String, String)> {
    use std::io::Read;
    let p = Path::new(path);
    let before = std::fs::metadata(p).map_err(|e| (io_kind(&e).to_string(), e.to_string()))?;
    if before.is_dir() {
        return Err(("not_a_file".into(), "not_a_file".into()));
    }
    let mut f = open_shared_read(p).map_err(|e| (io_kind(&e).to_string(), e.to_string()))?;
    let length = f
        .metadata()
        .map_err(|e| (io_kind(&e).to_string(), e.to_string()))?
        .len();
    let want = length.min(max_bytes as u64) as usize;
    let mut buf = vec![0u8; want];
    let mut n = 0;
    while n < want {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return Err((io_kind(&e).into(), e.to_string())),
        }
    }
    if n != want {
        return Err(changed());
    }
    let after = std::fs::metadata(p).map_err(|e| (io_kind(&e).to_string(), e.to_string()))?;
    if after.len() != before.len() || after.modified().ok() != before.modified().ok() {
        return Err(changed());
    }
    Ok(RawRead {
        path: path.to_string(),
        size_bytes: length,
        modified_utc: before.modified().map(fmt_o).unwrap_or_default(),
        bytes: buf,
        byte_limited: length > n as u64,
    })
}

pub fn read_text_file(args: ReadArgs) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base("windows_read_text_file");
    let out = read_prefix(&args.path, args.max_bytes).and_then(|raw| {
        crate::tools::text::text_data(&raw, &args.encoding, args.start_line, args.max_lines)
            .map_err(|k| {
                let msg = if k == "invalid_encoding" {
                    "Strict decoding failed; specify the known encoding, never guess.".to_string()
                } else {
                    k.clone()
                };
                (k, msg)
            })
    });
    typed_done(&mut r, start, out);
    r
}

// ---------------------------------------------------------------- resolve

pub struct ResolveArgs {
    pub name: String,
    pub additional: Vec<String>,
}

pub fn prepare_resolve(a: &Value) -> Result<ResolveArgs, ToolError> {
    let mut additional = Vec::new();
    if let Some(list) = a.get("additional_directories").and_then(Value::as_array) {
        for d in list {
            additional.push(
                paths::valid_path(d.as_str().unwrap_or_default()).map_err(ToolError::invalid)?,
            );
        }
    }
    Ok(ResolveArgs {
        name: opt_str(a, "name").unwrap_or_default(),
        additional,
    })
}

fn expand_env(s: &str) -> String {
    // [Environment]::ExpandEnvironmentVariables: %VAR% replaced when defined.
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let var = &after[..j];
                match (!var.is_empty()).then(|| std::env::var(var).ok()).flatten() {
                    Some(v) => {
                        out.push_str(&v);
                        rest = &after[j + 1..];
                    }
                    None => {
                        out.push('%');
                        out.push_str(var);
                        rest = &after[j..];
                    }
                }
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn path_scopes() -> Vec<(&'static str, String)> {
    let process = std::env::var("PATH").unwrap_or_default();
    let (user, machine) = crate::platform::registry_paths();
    vec![
        ("path_process", process),
        ("path_user", user.unwrap_or_default()),
        ("path_machine", machine.unwrap_or_default()),
    ]
}

pub fn resolve_data(args: &ResolveArgs) -> Value {
    let mut name = args.name.clone();
    if !name.to_lowercase().ends_with(".exe") {
        name.push_str(".exe");
    }
    let watch = Instant::now();
    let mut dirs: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |dirs: &mut Vec<(String, String)>, path: &str, source: String| {
        if path.trim().is_empty() {
            return;
        }
        let p = expand_env(path.trim().trim_matches('"'));
        if !paths::is_absolute_windows(&p) {
            return;
        }
        if seen.insert(p.to_lowercase()) {
            dirs.push((p, source));
        }
    };
    for d in &args.additional {
        add(&mut dirs, d, "explicit".into());
    }
    for (scope, value) in path_scopes() {
        for d in value.split(';') {
            add(&mut dirs, d, scope.into());
        }
    }
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    add(
        &mut dirs,
        &paths::join(&local, "Microsoft\\WindowsApps"),
        "windows_apps".into(),
    );
    add(
        &mut dirs,
        &paths::join(&local, "Microsoft\\WinGet\\Links"),
        "winget_links".into(),
    );
    add(
        &mut dirs,
        &paths::join(&local, "Microsoft\\WinGet\\Packages"),
        "winget_packages:0".into(),
    );
    let mut hits: Vec<Value> = Vec::new();
    let mut hit_seen = std::collections::HashSet::new();
    let mut errors: Vec<Value> = Vec::new();
    let (mut scanned, mut complete, mut reason) = (0usize, true, "exhausted");
    let mut i = 0;
    while i < dirs.len() {
        if watch.elapsed().as_millis() >= 5000 {
            complete = false;
            reason = "time_budget";
            break;
        }
        if scanned >= 1000 {
            complete = false;
            reason = "directory_limit";
            break;
        }
        let (dpath, source) = dirs[i].clone();
        i += 1;
        scanned += 1;
        let candidate = paths::join(&dpath, &name);
        match get_attributes(Path::new(&candidate)) {
            Ok((_, a)) => {
                if !a.directory && hit_seen.insert(candidate.to_lowercase()) {
                    hits.push(
                        json!({"path": candidate, "source": source, "is_reparse_point": a.reparse}),
                    );
                    if hits.len() >= 50 {
                        complete = false;
                        reason = "result_limit";
                        break;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                complete = false;
                if errors.len() < 20 {
                    errors.push(json!({"path": dpath, "message": e.to_string()}));
                }
                continue;
            }
        }
        if let Some(depth) = source
            .strip_prefix("winget_packages:")
            .and_then(|d| d.parse::<usize>().ok())
            && depth < 3
            && Path::new(&dpath).is_dir()
        {
            match std::fs::read_dir(&dpath) {
                Ok(it) => {
                    for child in it.flatten() {
                        if watch.elapsed().as_millis() >= 5000 || dirs.len() >= 1000 {
                            complete = false;
                            reason = "enumeration_limit";
                            break;
                        }
                        let Ok(meta) = child.metadata() else { continue };
                        let a = paths::attrs(&meta, &child.path());
                        if a.directory && !a.reparse {
                            let full = paths::join(&dpath, &child.file_name().to_string_lossy());
                            add(&mut dirs, &full, format!("winget_packages:{}", depth + 1));
                        }
                    }
                }
                Err(e) => {
                    complete = false;
                    if errors.len() < 20 {
                        errors.push(json!({"path": dpath, "message": e.to_string()}));
                    }
                }
            }
        }
    }
    let count = hits.len();
    let resolution = if count > 1 {
        "ambiguous"
    } else if !complete {
        "incomplete"
    } else if count == 1 {
        "found"
    } else {
        "not_found_in_scope"
    };
    json!({
        "name": name, "candidates": hits, "count": count, "ambiguous": count > 1,
        "scan_complete": complete, "stop_reason": reason, "errors": errors, "scanned_directories": scanned,
        "scope": "explicit, PATH, WindowsApps, WinGet Links, user WinGet Packages depth 3",
        "operation_ms": watch.elapsed().as_millis() as u64, "resolution": resolution,
    })
}

pub fn resolve_executable(args: ResolveArgs) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base("windows_resolve_executable");
    let data = resolve_data(&args);
    typed_done(&mut r, start, Ok(data));
    r
}

#[allow(dead_code)]
fn _unused(_: Map<String, Value>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_semantics() {
        let w = Wildcard::new("*.TXT");
        assert!(w.is_match("a.txt"));
        assert!(!w.is_match("a.txt.bak"));
        assert!(Wildcard::new("file?.[a-c]s").is_match("FILE1.bs"));
        assert!(!Wildcard::new("file?.[a-c]s").is_match("file1.ds"));
        assert!(Wildcard::new("a`*b").is_match("a*b"));
        assert!(!Wildcard::new("a`*b").is_match("axb"));
        assert!(Wildcard::new("*").is_match(""));
    }

    #[test]
    fn expand_env_keeps_unknown() {
        unsafe { std::env::set_var("SOSA_T_VAR", "X") };
        assert_eq!(
            expand_env("%SOSA_T_VAR%\\bin;%NOPE_SOSA%"),
            "X\\bin;%NOPE_SOSA%"
        );
    }

    #[test]
    fn stat_missing_and_existing() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        std::fs::write(&f, "hi").unwrap();
        let v = stat_data(f.to_str().unwrap()).unwrap();
        assert_eq!(v["exists"], true);
        assert_eq!(v["kind"], "file");
        assert_eq!(v["size_bytes"], 2);
        let v = stat_data(d.path().join("nope").to_str().unwrap()).unwrap();
        assert_eq!(v["exists"], false);
        assert!(v["kind"].is_null());
    }

    #[test]
    fn read_prefix_limits() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        std::fs::write(&f, "hello world").unwrap();
        let raw = read_prefix(f.to_str().unwrap(), 5).unwrap();
        assert_eq!(raw.bytes, b"hello");
        assert!(raw.byte_limited);
        let e = read_prefix(d.path().to_str().unwrap(), 5).err().unwrap();
        assert_eq!(e.0, "not_a_file");
    }
}

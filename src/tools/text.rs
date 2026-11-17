//! Text tools: strict decoding/encoding, `windows_read_text_file` paging,
//! `windows_write_text_file`, `windows_edit_text_file` and
//! `windows_search_text` (in-process ripgrep engine), with snapshot/compare-and-commit
//! writes.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::fs::{RawRead, io_kind, open_shared_read};
use super::{ToolError, opt_bool, opt_i64, opt_str};
use crate::envelope::{self, Envelope};
use crate::paths::{self, fmt_o};

pub const LIMIT: usize = 1_048_576;

pub fn digest(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Error raised by the text pipeline: `kind`, `message` (defaults to kind)
/// and whether the target may have changed (commit attempted).
#[derive(Debug, Clone)]
pub struct TextError {
    pub kind: String,
    pub message: String,
    pub may_have_changed: bool,
}

impl TextError {
    pub fn new(kind: &str) -> Self {
        Self {
            kind: kind.into(),
            message: kind.into(),
            may_have_changed: false,
        }
    }
    pub fn msg(kind: &str, message: impl Into<String>) -> Self {
        let m: String = message.into();
        Self {
            kind: kind.into(),
            message: if m.is_empty() { kind.into() } else { m },
            may_have_changed: false,
        }
    }
    fn io(e: &std::io::Error) -> Self {
        Self::msg(io_kind(e), e.to_string())
    }
    pub fn changed_flag(self, yes: bool) -> Self {
        self.changed(yes)
    }
    fn changed(mut self, yes: bool) -> Self {
        self.may_have_changed = self.may_have_changed || yes;
        self
    }
}

// ------------------------------------------------------------ codecs

const BOM_UTF8: &[u8] = b"\xef\xbb\xbf";
const BOM_UTF16_LE: &[u8] = b"\xff\xfe";
const BOM_UTF16_BE: &[u8] = b"\xfe\xff";
const BOM_UTF32_LE: &[u8] = b"\xff\xfe\x00\x00";
const BOM_UTF32_BE: &[u8] = b"\x00\x00\xfe\xff";

/// cp1252 0x80..=0x9F (None = undefined, a strict decode error).
const CP1252_HIGH: [Option<char>; 32] = [
    Some('\u{20AC}'),
    None,
    Some('\u{201A}'),
    Some('\u{0192}'),
    Some('\u{201E}'),
    Some('\u{2026}'),
    Some('\u{2020}'),
    Some('\u{2021}'),
    Some('\u{02C6}'),
    Some('\u{2030}'),
    Some('\u{0160}'),
    Some('\u{2039}'),
    Some('\u{0152}'),
    None,
    Some('\u{017D}'),
    None,
    None,
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{201C}'),
    Some('\u{201D}'),
    Some('\u{2022}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{02DC}'),
    Some('\u{2122}'),
    Some('\u{0161}'),
    Some('\u{203A}'),
    Some('\u{0153}'),
    None,
    Some('\u{017E}'),
    Some('\u{0178}'),
];

fn bad(enc: &str, why: &str) -> TextError {
    TextError::msg(
        "invalid_encoding",
        format!("'{enc}' codec can't decode: {why}"),
    )
}

/// Strict decode. With `final_=false` an incomplete trailing sequence is
/// withheld (incremental decoder semantics), not an error.
pub fn decode_bytes(b: &[u8], enc: &str, final_: bool) -> Result<String, TextError> {
    match enc {
        "utf-8" => match std::str::from_utf8(b) {
            Ok(s) => Ok(s.to_string()),
            Err(e) if !final_ && e.error_len().is_none() => {
                Ok(std::str::from_utf8(&b[..e.valid_up_to()])
                    .unwrap_or_default()
                    .to_string())
            }
            Err(e) => Err(bad(
                enc,
                &format!("invalid byte at position {}", e.valid_up_to()),
            )),
        },
        "utf-16-le" | "utf-16-be" | "utf-16le" | "utf-16be" => {
            let le = enc.contains("le");
            let mut n = b.len();
            if n % 2 == 1 {
                if final_ {
                    return Err(bad(enc, "truncated data"));
                }
                n -= 1;
            }
            let mut units: Vec<u16> = b[..n]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| {
                    if le {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                })
                .collect();
            if !final_ && units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
                units.pop();
            }
            String::from_utf16(&units).map_err(|_| bad(enc, "illegal UTF-16 surrogate"))
        }
        "windows-1252" => b
            .iter()
            .enumerate()
            .map(|(i, &x)| match x {
                0x80..=0x9F => CP1252_HIGH[(x - 0x80) as usize].ok_or_else(|| {
                    bad(
                        enc,
                        &format!("character maps to <undefined> at position {i}"),
                    )
                }),
                _ => Ok(x as char),
            })
            .collect(),
        other => Err(TextError::msg(
            "invalid_encoding",
            format!("unknown encoding: {other}"),
        )),
    }
}

pub fn encode_str(text: &str, enc: &str) -> Result<Vec<u8>, TextError> {
    match enc {
        "utf-8" => Ok(text.as_bytes().to_vec()),
        "utf-16-le" => Ok(text.encode_utf16().flat_map(u16::to_le_bytes).collect()),
        "utf-16-be" => Ok(text.encode_utf16().flat_map(u16::to_be_bytes).collect()),
        "windows-1252" => text
            .chars()
            .enumerate()
            .map(|(i, c)| {
                let u = c as u32;
                if u < 0x80 || (0xA0..=0xFF).contains(&u) {
                    Ok(u as u8)
                } else {
                    CP1252_HIGH
                        .iter()
                        .position(|m| *m == Some(c))
                        .map(|p| 0x80 + p as u8)
                        .ok_or_else(|| {
                            TextError::msg(
                                "invalid_encoding",
                                format!("'charmap' codec can't encode character at position {i}"),
                            )
                        })
                }
            })
            .collect(),
        other => Err(TextError::msg(
            "invalid_encoding",
            format!("unknown encoding: {other}"),
        )),
    }
}

fn is_binary_char(c: char) -> bool {
    let u = c as u32;
    (u < 32 && !matches!(c, '\t' | '\n' | '\r')) || (127..160).contains(&u)
}

/// Split after CRLF, LF or CR (the contract's `.*?(?:\r\n|\n|\r|$)` chunks).
pub fn chunks(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => {
                out.push(&text[start..i + 2]);
                i += 2;
                start = i;
            }
            b'\r' | b'\n' => {
                out.push(&text[start..i + 1]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < b.len() {
        out.push(&text[start..]);
    }
    out
}

pub fn newline_kind(text: &str) -> &'static str {
    let (mut crlf, mut lf, mut cr) = (false, false, false);
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => {
                crlf = true;
                i += 1;
            }
            b'\r' => cr = true,
            b'\n' => lf = true,
            _ => {}
        }
        i += 1;
    }
    match (crlf as u8 + lf as u8 + cr as u8, crlf, lf) {
        (0, ..) => "none",
        (1, true, _) => "crlf",
        (1, _, true) => "lf",
        (1, ..) => "cr",
        _ => "mixed",
    }
}

fn bom_of(b: &[u8]) -> Option<(&'static [u8], &'static str)> {
    [
        (BOM_UTF8, "utf-8"),
        (BOM_UTF16_LE, "utf-16-le"),
        (BOM_UTF16_BE, "utf-16-be"),
    ]
    .into_iter()
    .find(|(p, _)| b.starts_with(p))
}

/// `inspection_tools.text_data`: decode a (possibly byte-limited) prefix and
/// page it. Errors are bare kinds (`not_a_file`, `invalid_encoding`, ...).
pub fn text_data(
    raw: &RawRead,
    encoding: &str,
    start: usize,
    limit: usize,
) -> Result<Value, String> {
    let full_hash = (!raw.byte_limited).then(|| digest(&raw.bytes));
    let mut buf: &[u8] = &raw.bytes;
    if buf.starts_with(BOM_UTF32_LE) || buf.starts_with(BOM_UTF32_BE) {
        return Err("unsupported_utf32".into());
    }
    let mut bom = None;
    if let Some((p, enc)) = bom_of(buf) {
        bom = Some(enc);
        if encoding != "auto" && encoding != enc {
            return Err("encoding_bom_mismatch".into());
        }
        buf = &buf[p.len()..];
    }
    let enc = if encoding == "auto" {
        bom.unwrap_or("utf-8")
    } else {
        encoding
    };
    let text =
        decode_bytes(buf, enc, !raw.byte_limited).map_err(|_| "invalid_encoding".to_string())?;
    if text.chars().any(is_binary_char) {
        return Err("binary_content".into());
    }
    let all = chunks(&text);
    let selected: Vec<&str> = all.iter().skip(start - 1).take(limit).copied().collect();
    let mut lines = Vec::new();
    for (k, line) in selected.iter().enumerate() {
        let i = start + k;
        let terminated = line.ends_with('\r') || line.ends_with('\n');
        let mut complete = terminated || !raw.byte_limited;
        if raw.byte_limited && i == all.len() && line.ends_with('\r') {
            complete = false;
        }
        lines.push(
            json!({"number": i, "text": line.trim_end_matches(['\r', '\n']), "complete": complete}),
        );
    }
    let more = start - 1 + selected.len() < all.len();
    let reason = if more {
        "line_limit"
    } else if raw.byte_limited {
        "byte_limit"
    } else {
        "eof"
    };
    Ok(json!({
        "path": raw.path, "size_bytes": raw.size_bytes, "modified_utc": raw.modified_utc,
        "encoding": enc, "bom": bom.is_some(), "bytes_read": raw.bytes.len(),
        "start_line": start, "count": lines.len(), "lines": lines,
        "next_start_line": if more { json!(start + selected.len()) } else { Value::Null },
        "complete": reason == "eof", "stop_reason": reason, "byte_limited": raw.byte_limited,
        "hash_scope": if full_hash.is_some() { "full_file" } else { "none" }, "sha256": full_hash,
        "newline": newline_kind(&text), "newline_scope": if raw.byte_limited { "prefix" } else { "full_file" },
    }))
}

/// `text_tools.decode`: whole-file strict decode for write/edit.
pub fn decode(b: &[u8], encoding: &str) -> Result<(String, String, Vec<u8>), TextError> {
    if b.starts_with(BOM_UTF32_LE) || b.starts_with(BOM_UTF32_BE) {
        return Err(TextError::new("unsupported_utf32"));
    }
    let (prefix, detected) = match bom_of(b) {
        Some((p, e)) => (p.to_vec(), Some(e)),
        None => (Vec::new(), None),
    };
    if let Some(d) = detected
        && encoding != "auto"
        && encoding != d
    {
        return Err(TextError::new("encoding_bom_mismatch"));
    }
    let enc = if encoding == "auto" {
        detected.unwrap_or("utf-8").to_string()
    } else {
        encoding.to_string()
    };
    let text = decode_bytes(&b[prefix.len()..], &enc, true)?;
    if text.chars().any(is_binary_char) {
        return Err(TextError::new("binary_content"));
    }
    Ok((text, enc, prefix))
}

pub fn encode(text: &str, enc: &str, bom: bool) -> Result<Vec<u8>, TextError> {
    if text.chars().any(is_binary_char) {
        return Err(TextError::new("binary_content"));
    }
    let prefix: &[u8] = match enc {
        "utf-8" => BOM_UTF8,
        "utf-16-le" => BOM_UTF16_LE,
        "utf-16-be" => BOM_UTF16_BE,
        _ => b"",
    };
    if bom && prefix.is_empty() {
        return Err(TextError::new("unsupported_bom"));
    }
    let mut out = if bom { prefix.to_vec() } else { Vec::new() };
    out.extend(encode_str(text, enc)?);
    if out.len() > LIMIT {
        return Err(TextError::new("file_too_large"));
    }
    Ok(out)
}

/// Exact, unique, non-overlapping replacements against the original text.
pub fn exact_edit(text: &str, edits: &[(String, String)]) -> Result<String, TextError> {
    let mut spans: Vec<(usize, usize, &str)> = Vec::new();
    for (old, new) in edits {
        let Some(start) = text.find(old.as_str()) else {
            return Err(TextError::new("text_not_found"));
        };
        let next = start + text[start..].chars().next().map_or(1, char::len_utf8);
        if text[next..].contains(old.as_str()) {
            return Err(TextError::new("ambiguous_match"));
        }
        spans.push((start, start + old.len(), new.as_str()));
    }
    spans.sort();
    if spans.windows(2).any(|w| w[1].0 < w[0].1) {
        return Err(TextError::new("overlapping_edits"));
    }
    let mut out = text.to_string();
    for (s, e, new) in spans.into_iter().rev() {
        out.replace_range(s..e, new);
    }
    Ok(out)
}

/// `str.splitlines(keepends=True)` semantics for text that already passed the
/// binary check (only CR, LF, CRLF, U+2028 and U+2029 remain as breaks).
fn splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let end = match c {
            '\r' if it.peek().is_some_and(|(_, n)| *n == '\n') => {
                it.next();
                i + 2
            }
            '\r' | '\n' | '\u{2028}' | '\u{2029}' => i + c.len_utf8(),
            _ => continue,
        };
        out.push(&text[start..end]);
        start = end;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn range_unified(start: usize, stop: usize) -> String {
    let mut beginning = start + 1;
    let length = stop - start;
    if length == 1 {
        return format!("{beginning}");
    }
    if length == 0 {
        beginning -= 1;
    }
    format!("{beginning},{length}")
}

/// `difflib.unified_diff(..., fromfile='before', tofile='after', n=3)`
/// joined, using Myers opcodes from `similar` (hunks may differ from
/// difflib's Ratcliff/Obershelp matcher on ambiguous inputs).
pub fn unified_diff(old: &str, new: &str) -> Vec<String> {
    use similar::{DiffOp, capture_diff_slices, group_diff_ops};
    let a = splitlines(old);
    let b = splitlines(new);
    let ops = capture_diff_slices(similar::Algorithm::Myers, &a, &b);
    let groups = group_diff_ops(ops, 3);
    let mut out = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        if group.iter().all(|o| matches!(o, DiffOp::Equal { .. })) {
            continue;
        }
        if gi == 0 || out.is_empty() {
            out.push("--- before\n".to_string());
            out.push("+++ after\n".to_string());
        }
        let first = group.first().unwrap().as_tag_tuple();
        let last = group.last().unwrap().as_tag_tuple();
        out.push(format!(
            "@@ -{} +{} @@\n",
            range_unified(first.1.start, last.1.end),
            range_unified(first.2.start, last.2.end)
        ));
        for op in group {
            let (_, ra, rb) = op.as_tag_tuple();
            match op {
                DiffOp::Equal { .. } => out.extend(a[ra].iter().map(|l| format!(" {l}"))),
                _ => {
                    out.extend(a[ra].iter().map(|l| format!("-{l}")));
                    out.extend(b[rb].iter().map(|l| format!("+{l}")));
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------ file ops

/// Local drive path, no reparse point on the path or
/// any existing ancestor (covers OneDrive/cloud placeholders and junctions).
pub fn check_path(path: &str) -> Result<(), TextError> {
    if !paths::is_local_drive(path) || path.chars().skip(2).any(|c| c == ':') {
        return Err(TextError::new("unsupported_path"));
    }
    let mut cursor = Some(PathBuf::from(path));
    while let Some(c) = cursor {
        match paths::get_attributes(&c) {
            Ok((_, a)) if a.reparse => return Err(TextError::new("reparse_point_not_supported")),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.raw_os_error() == Some(3) => {} // ERROR_PATH_NOT_FOUND
            Err(e) => return Err(TextError::io(&e)),
        }
        cursor = c
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf);
    }
    Ok(())
}

/// Whole file up to `limit`, shared-read.
pub fn read_bytes(path: &str, limit: u64) -> Result<Vec<u8>, TextError> {
    let (_, a) = paths::get_attributes(Path::new(path)).map_err(|e| TextError::io(&e))?;
    if a.directory {
        return Err(TextError::new("not_a_file"));
    }
    let mut f = open_shared_read(Path::new(path)).map_err(|e| TextError::io(&e))?;
    let len = f.metadata().map_err(|e| TextError::io(&e))?.len();
    if len > limit {
        return Err(TextError::new("file_too_large"));
    }
    let mut buf = vec![0u8; len as usize];
    let mut i = 0;
    while i < buf.len() {
        match f.read(&mut buf[i..]) {
            Ok(0) => return Err(TextError::new("file_changed")),
            Ok(n) => i += n,
            Err(e) => return Err(TextError::io(&e)),
        }
    }
    Ok(buf)
}

pub struct Snapshot {
    pub bytes: Vec<u8>,
    pub modified_utc: String,
}

pub fn snapshot(path: &str, for_write: bool) -> Result<Snapshot, TextError> {
    if for_write {
        check_path(path)?;
    }
    let bytes = read_bytes(path, LIMIT as u64)?;
    let modified_utc = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(fmt_o)
        .map_err(|e| TextError::io(&e))?;
    Ok(Snapshot {
        bytes,
        modified_utc,
    })
}

/// The `exists` worker operation: Ok(true) if present, Ok(false) if missing.
pub fn exists(path: &str) -> Result<bool, TextError> {
    check_path(path)?;
    match paths::get_attributes(Path::new(path)) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(3) => {
            Ok(false)
        }
        Err(e) => Err(TextError::io(&e)),
    }
}

#[cfg(windows)]
fn replace_file(stage: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    let w = |p: &Path| {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let (t, s) = (w(target), w(stage));
    // SAFETY: both strings are NUL-terminated wide paths; no backup file.
    let ok = unsafe {
        ReplaceFileW(
            t.as_ptr(),
            s.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(stage: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(stage, target)
}

/// The `commit` worker operation: stage in the parent, re-check, then
/// create (no overwrite) or replace (optimistic hash check), read back.
pub fn commit(
    path: &str,
    mode: &str,
    bytes: &[u8],
    expected_sha256: Option<&str>,
) -> Result<(String, usize), TextError> {
    let mut attempted = false;
    let mut stage: Option<PathBuf> = None;
    let res = (|| {
        check_path(path)?;
        let target = Path::new(path);
        let parent = target
            .parent()
            .ok_or_else(|| TextError::new("parent_not_found"))?;
        if !parent.is_dir() {
            return Err(TextError::new("parent_not_found"));
        }
        if bytes.len() > LIMIT {
            return Err(TextError::new("file_too_large"));
        }
        let sp = parent.join(format!(
            ".windows-sosa-mcp-{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&sp)
            .map_err(|e| TextError::io(&e))?;
        stage = Some(sp.clone());
        f.write_all(bytes)
            .and_then(|_| f.sync_all())
            .map_err(|e| TextError::io(&e))?;
        drop(f);
        check_path(path)?;
        match mode {
            "create" => {
                if target.exists() {
                    return Err(TextError::new("already_exists"));
                }
                attempted = true;
                // Move without overwrite: hard_link+remove is not atomic; use
                // rename after the existence check, like [IO.File]::Move.
                std::fs::rename(&sp, target).map_err(|e| TextError::io(&e))?;
            }
            "replace" => {
                let current = read_bytes(path, LIMIT as u64)?;
                if Some(digest(&current).as_str()) != expected_sha256 {
                    return Err(TextError::new("version_conflict"));
                }
                attempted = true;
                replace_file(&sp, target).map_err(|e| TextError::io(&e))?;
            }
            _ => return Err(TextError::new("invalid_mode")),
        }
        stage = None;
        let after = read_bytes(path, LIMIT as u64)?;
        if digest(&after) != digest(bytes) {
            return Err(TextError::new("verification_failed"));
        }
        Ok((digest(&after), after.len()))
    })();
    if let Some(sp) = stage {
        let _ = std::fs::remove_file(sp);
    }
    res.map_err(|e| e.changed(attempted))
}

// ------------------------------------------------------------ tools

fn finish(r: &mut Envelope, start: Instant, out: Result<Value, TextError>) {
    match out {
        Ok(data) => envelope::succeed(r, data),
        Err(e) => {
            envelope::fail(r, &e.kind, &e.message);
            r.insert("may_have_changed".into(), json!(e.may_have_changed));
            if e.kind == "timeout" {
                r.insert("timed_out".into(), json!(true));
                r.insert("windows_termination_confirmed".into(), json!(false));
            }
        }
    }
    envelope::stamp(r, start);
}

/// `windows_read_text_file` with `include_sha256=true`: full snapshot (<= 1 MiB).
pub fn read_with_hash(args: super::fs::ReadArgs) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base("windows_read_text_file");
    let out = snapshot(&args.path, false).and_then(|s| {
        let prefix = s.bytes[..s.bytes.len().min(args.max_bytes)].to_vec();
        let raw = RawRead {
            path: args.path.clone(),
            size_bytes: s.bytes.len() as u64,
            modified_utc: s.modified_utc,
            byte_limited: s.bytes.len() > prefix.len(),
            bytes: prefix,
        };
        let mut data =
            text_data(&raw, &args.encoding, args.start_line, args.max_lines).map_err(|k| {
                if k == "invalid_encoding" {
                    TextError::msg(&k, "Strict decoding failed")
                } else {
                    TextError::new(&k)
                }
            })?;
        data["sha256"] = json!(digest(&s.bytes));
        data["hash_scope"] = json!("full_file");
        Ok(data)
    });
    finish(&mut r, start, out);
    r
}

pub struct WriteArgs {
    pub edit: bool,
    pub path: String,
    pub content: String,
    pub mode: String,
    pub expected_sha256: Option<String>,
    pub encoding: String,
    pub source_encoding: String,
    pub bom: Value,
    pub newline: String,
    pub dry_run: bool,
    pub edits: Vec<(String, String)>,
}

/// `text_tools.validate` for write/edit (after the schema check).
pub fn prepare_write(name: &str, a: &Value) -> Result<WriteArgs, ToolError> {
    let path =
        paths::valid_path(&opt_str(a, "path").unwrap_or_default()).map_err(ToolError::invalid)?;
    let edit = name == "windows_edit_text_file";
    let edits: Vec<(String, String)> = a
        .get("edits")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .map(|e| {
                    (
                        opt_str(e, "old_text").unwrap_or_default(),
                        opt_str(e, "new_text").unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if edit
        && edits
            .iter()
            .map(|(o, n)| o.chars().count() + n.chars().count())
            .sum::<usize>()
            > 2 * LIMIT
    {
        return Err(ToolError::kind("request_too_large", "request_too_large"));
    }
    if !paths::is_local_drive(&path) {
        return Err(ToolError::kind(
            "unsupported_path",
            "Writes require a local drive path",
        ));
    }
    Ok(WriteArgs {
        edit,
        path,
        content: opt_str(a, "content").unwrap_or_default(),
        mode: opt_str(a, "mode").unwrap_or_else(|| "create".into()),
        expected_sha256: opt_str(a, "expected_sha256"),
        encoding: opt_str(a, "encoding")
            .unwrap_or_else(|| if edit { "auto" } else { "preserve" }.into()),
        source_encoding: opt_str(a, "source_encoding").unwrap_or_else(|| "auto".into()),
        bom: a.get("bom").cloned().unwrap_or_else(|| json!("preserve")),
        newline: opt_str(a, "newline").unwrap_or_else(|| "preserve".into()),
        dry_run: opt_bool(a, "dry_run").unwrap_or(false),
        edits,
    })
}

fn ext_lower(path: &str) -> String {
    let base = path.rsplit(['\\', '/']).next().unwrap_or("");
    let trimmed = base.trim_start_matches('.');
    match trimmed.rfind('.') {
        Some(i) => trimmed[i..].to_lowercase(),
        None => String::new(),
    }
}

/// `text_tools.prepare`: new bytes + data (not yet applied).
pub fn prepare_change(
    args: &WriteArgs,
    before: Option<&[u8]>,
) -> Result<(Vec<u8>, Value), TextError> {
    if let Some(b) = before
        && Some(digest(b)) != args.expected_sha256
    {
        return Err(TextError::new("version_conflict"));
    }
    let (old, mut enc, mut prefix) = match before {
        Some(b) => decode(
            b,
            if args.edit {
                &args.encoding
            } else {
                &args.source_encoding
            },
        )?,
        None => (String::new(), "utf-8".to_string(), Vec::new()),
    };
    let (new, after) = if args.edit {
        let new = exact_edit(&old, &args.edits)?;
        let after = encode(&new, &enc, !prefix.is_empty())?;
        (new, after)
    } else {
        let mut new = args.content.clone();
        if args.encoding != "preserve" {
            enc = args.encoding.clone();
        }
        let mut bom = if before.is_some() {
            !prefix.is_empty()
        } else {
            enc.starts_with("utf-16")
                || matches!(ext_lower(&args.path).as_str(), ".ps1" | ".psm1" | ".psd1")
        };
        if let Some(b) = args.bom.as_bool() {
            bom = b;
        }
        let mut line = args.newline.clone();
        if line == "preserve" && before.is_some() {
            line = newline_kind(&old).to_string();
            if line == "mixed" {
                return Err(TextError::msg(
                    "mixed_newlines",
                    "Choose lf/crlf explicitly for a full replacement; edit preserves untouched terminators",
                ));
            }
        }
        if let Some(term) = match line.as_str() {
            "lf" => Some("\n"),
            "crlf" => Some("\r\n"),
            "cr" => Some("\r"),
            _ => None,
        } {
            new = chunks(&new)
                .into_iter()
                .map(|c| {
                    let body = c.trim_end_matches(['\r', '\n']);
                    if body.len() < c.len() {
                        format!("{body}{term}")
                    } else {
                        body.to_string()
                    }
                })
                .collect();
        }
        let after = encode(&new, &enc, bom)?;
        prefix = if bom {
            after[..after.len().min(3)].to_vec()
        } else {
            Vec::new()
        };
        (new, after)
    };
    let mut pieces = String::new();
    let (mut size, mut truncated) = (0usize, false);
    for chunk in unified_diff(&old, &new) {
        let n = chunk.chars().count();
        if size + n > 12000 {
            pieces.extend(chunk.chars().take(12000 - size));
            truncated = true;
            break;
        }
        pieces.push_str(&chunk);
        size += n;
    }
    let data = json!({
        "path": args.path, "changed": before != Some(after.as_slice()), "applied": false, "dry_run": args.dry_run,
        "sha256_before": before.map(digest), "sha256_after": digest(&after), "size_bytes": after.len(),
        "encoding": enc, "bom": !prefix.is_empty(), "newline": newline_kind(&new), "diff": pieces,
        "diff_truncated": truncated, "verified": false,
    });
    Ok((after, data))
}

pub fn write_or_edit(args: WriteArgs) -> Envelope {
    let start = Instant::now();
    let name = if args.edit {
        "windows_edit_text_file"
    } else {
        "windows_write_text_file"
    };
    let mut r = envelope::base(name);
    let out = (|| {
        let before = if args.edit || args.mode == "replace" {
            Some(snapshot(&args.path, true)?.bytes)
        } else {
            if exists(&args.path)? {
                return Err(TextError::new("already_exists"));
            }
            None
        };
        let (after, mut data) = prepare_change(&args, before.as_deref())?;
        if data["changed"] == true && !args.dry_run {
            let mode = if before.is_none() {
                "create"
            } else {
                "replace"
            };
            let (sha, _) = commit(&args.path, mode, &after, data["sha256_before"].as_str())?;
            if Some(sha.as_str()) != data["sha256_after"].as_str() {
                return Err(TextError {
                    may_have_changed: true,
                    ..TextError::new("verification_failed")
                });
            }
            data["applied"] = json!(true);
            data["verified"] = json!(true);
        }
        Ok(data)
    })();
    finish(&mut r, start, out);
    r
}

// ------------------------------------------------------------ search

pub struct SearchArgs {
    pub path: String,
    pub pattern: String,
    pub mode: String,
    pub case_sensitive: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_hidden: bool,
    pub respect_ignore: bool,
    pub max_depth: i64,
    pub limit: usize,
    pub context_lines: usize,
    pub encoding: String,
}

fn str_list(a: &Value, key: &str) -> Vec<String> {
    a.get(key)
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub fn prepare_search(a: &Value) -> Result<SearchArgs, ToolError> {
    Ok(SearchArgs {
        path: paths::valid_path(&opt_str(a, "path").unwrap_or_default())
            .map_err(ToolError::invalid)?,
        pattern: opt_str(a, "pattern").unwrap_or_default(),
        mode: opt_str(a, "mode").unwrap_or_else(|| "literal".into()),
        case_sensitive: opt_bool(a, "case_sensitive").unwrap_or(true),
        include: str_list(a, "include"),
        exclude: str_list(a, "exclude"),
        include_hidden: opt_bool(a, "include_hidden").unwrap_or(false),
        respect_ignore: opt_bool(a, "respect_ignore").unwrap_or(true),
        max_depth: opt_i64(a, "max_depth").unwrap_or(3),
        limit: opt_i64(a, "limit").unwrap_or(50) as usize,
        context_lines: opt_i64(a, "context_lines").unwrap_or(0) as usize,
        encoding: opt_str(a, "encoding").unwrap_or_else(|| "auto".into()),
    })
}

/// Search engine: ripgrep's library crates, linked into the server.
/// Kept in sync with Cargo.lock by the `engine_version_matches_lock` test.
pub const ENGINE_VERSION: &str = "grep-searcher 0.1.17";

const TIME_BUDGET: Duration = Duration::from_millis(10_000);
/// Budget for the raw records produced (characters), like the former
/// subprocess output cap.
const RAW_BUDGET: usize = 2_097_152;
const MAX_RETAINED: usize = 262_144;
const MAX_LINE_CHARS: usize = 4096;

/// Accumulates search records with the result, output and time budgets.
struct Collector<'a> {
    a: &'a SearchArgs,
    start: Instant,
    matches: Vec<Value>,
    count: usize,
    retained: usize,
    raw: usize,
    errors: Vec<String>,
    notes: Vec<String>,
    binary: bool,
    errored: bool,
    matched: bool,
    stop: Option<&'static str>,
}

impl<'a> Collector<'a> {
    fn new(a: &'a SearchArgs) -> Self {
        Self {
            a,
            start: Instant::now(),
            matches: Vec::new(),
            count: 0,
            retained: 0,
            raw: 0,
            errors: Vec::new(),
            notes: Vec::new(),
            binary: false,
            errored: false,
            matched: false,
            stop: None,
        }
    }

    /// False once any budget is exhausted (and records why).
    fn budget(&mut self) -> bool {
        if self.stop.is_some() {
            return false;
        }
        if self.start.elapsed() > TIME_BUDGET {
            self.stop = Some("time_budget");
        } else if self.raw > RAW_BUDGET {
            self.stop = Some("output_limit");
        }
        self.stop.is_none()
    }

    fn error(&mut self, msg: String) {
        self.errored = true;
        self.raw += msg.len();
        self.errors.push(msg);
    }

    /// One `match`/`context` record; false = stop searching.
    fn record(&mut self, kind: &'static str, path: &Path, line: Option<u64>, bytes: &[u8]) -> bool {
        if !self.budget() {
            return false;
        }
        self.raw += bytes.len() + path.as_os_str().len() + 96;
        if kind == "match" {
            self.matched = true;
        }
        let (Some(p), Ok(t)) = (path.to_str(), std::str::from_utf8(bytes)) else {
            self.notes.push("Non-UTF8 search record skipped".into());
            return true;
        };
        let limit = self.a.limit;
        if kind == "match" {
            if self.count >= limit {
                self.stop = Some("result_limit");
                return false;
            }
            self.count += 1;
        }
        if self.matches.len() >= limit * (1 + 2 * self.a.context_lines) {
            self.stop = Some("result_limit");
            return false;
        }
        let text = t.trim_end_matches(['\r', '\n']);
        let n = text.chars().count();
        self.retained += n.min(MAX_LINE_CHARS);
        if self.retained > MAX_RETAINED {
            self.stop = Some("output_limit");
            return false;
        }
        self.matches.push(json!({
            "path": p, "line_number": line.unwrap_or(1),
            "text": text.chars().take(MAX_LINE_CHARS).collect::<String>(), "kind": kind,
            "text_truncated": n > MAX_LINE_CHARS,
        }));
        true
    }
}

/// grep-searcher sink for one file.
struct FileSink<'c, 'a> {
    col: &'c mut Collector<'a>,
    path: &'c Path,
    output: bool,
    binary: bool,
}

impl grep_searcher::Sink for FileSink<'_, '_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _: &grep_searcher::Searcher,
        m: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        self.output = true;
        Ok(self
            .col
            .record("match", self.path, m.line_number(), m.bytes()))
    }

    fn context(
        &mut self,
        _: &grep_searcher::Searcher,
        c: &grep_searcher::SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        self.output = true;
        Ok(self
            .col
            .record("context", self.path, c.line_number(), c.bytes()))
    }

    fn binary_data(&mut self, _: &grep_searcher::Searcher, _: u64) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(true)
    }
}

fn walker(a: &SearchArgs) -> Result<ignore::Walk, TextError> {
    let mut over = ignore::overrides::OverrideBuilder::new(&a.path);
    let globs = a
        .include
        .iter()
        .cloned()
        .chain(a.exclude.iter().map(|g| format!("!{g}")));
    for g in globs {
        over.add(&g)
            .map_err(|e| TextError::msg("search_failed", e.to_string()))?;
    }
    let over = over
        .build()
        .map_err(|e| TextError::msg("search_failed", e.to_string()))?;
    let mut w = ignore::WalkBuilder::new(&a.path);
    w.hidden(!a.include_hidden)
        .parents(false)
        .git_global(false)
        .follow_links(false)
        .max_depth(Some(a.max_depth.max(0) as usize + 1))
        .max_filesize(Some(LIMIT as u64))
        .overrides(over);
    if a.respect_ignore {
        w.add_custom_ignore_filename(".rgignore");
    } else {
        w.ignore(false).git_ignore(false).git_exclude(false);
    }
    Ok(w.build())
}

fn searcher(a: &SearchArgs) -> Result<grep_searcher::Searcher, TextError> {
    let mut b = grep_searcher::SearcherBuilder::new();
    b.line_number(true)
        .before_context(a.context_lines)
        .after_context(a.context_lines)
        .memory_map(grep_searcher::MmapChoice::never())
        .bom_sniffing(true);
    if a.encoding != "auto" {
        let enc = grep_searcher::Encoding::new(&a.encoding)
            .map_err(|e| TextError::msg("search_failed", e.to_string()))?;
        b.encoding(Some(enc));
    }
    Ok(b.build())
}

fn matcher(a: &SearchArgs) -> Result<grep_regex::RegexMatcher, TextError> {
    grep_regex::RegexMatcherBuilder::new()
        .multi_line(true)
        .unicode(true)
        .octal(false)
        .case_insensitive(!a.case_sensitive)
        .fixed_strings(a.mode == "literal")
        .line_terminator(Some(b'\n'))
        .build(&a.pattern)
        .map_err(|e| TextError::msg("search_failed", e.to_string()))
}

/// Run the search in-process: single-threaded walk honouring ignore files,
/// globs, depth and the 1 MiB file cap; binary files are skipped (an
/// explicitly named file is searched with NULs converted, like ripgrep).
pub fn run_search(a: &SearchArgs) -> Result<Value, TextError> {
    if let Ok(m) = std::fs::metadata(&a.path)
        && m.is_file()
        && m.len() > LIMIT as u64
    {
        return Err(TextError::new("file_too_large"));
    }
    let matcher = matcher(a)?;
    let mut searcher = searcher(a)?;
    let mut col = Collector::new(a);
    for entry in walker(a)? {
        if !col.budget() {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                col.error(e.to_string());
                continue;
            }
        };
        if let Some(e) = entry.error() {
            col.notes.push(e.to_string());
        }
        let explicit = entry.depth() == 0;
        let is_file =
            entry.file_type().is_some_and(|t| t.is_file()) || (explicit && entry.path().is_file());
        if !is_file {
            continue;
        }
        searcher.set_binary_detection(if explicit {
            grep_searcher::BinaryDetection::convert(b'\x00')
        } else {
            grep_searcher::BinaryDetection::quit(b'\x00')
        });
        let path = entry.path();
        let mut sink = FileSink {
            col: &mut col,
            path,
            output: false,
            binary: false,
        };
        let res = searcher.search_path(&matcher, path, &mut sink);
        let (output, binary) = (sink.output, sink.binary);
        if output && binary {
            col.binary = true;
        }
        if let Err(e) = res {
            col.error(format!("{}: {e}", path.display()));
        }
    }
    finish_search(col)
}

fn finish_search(col: Collector<'_>) -> Result<Value, TextError> {
    let a = col.a;
    let mut warnings: Vec<String> = col.errors.iter().take(20).cloned().collect();
    warnings.extend(col.notes.iter().cloned());
    // ripgrep-compatible exit code: 0 = matched, 1 = no match, 2 = errors.
    let native_exit_code = if col.errored {
        2
    } else if col.matched {
        0
    } else {
        1
    };
    let mut reason = col.stop.unwrap_or("exhausted").to_string();
    if col.binary {
        warnings.push(
            "Binary detection stopped at least one file; matches may precede detection".into(),
        );
    }
    if native_exit_code == 2 && reason == "exhausted" {
        let joined: String = warnings.join("\n").chars().take(4000).collect();
        return Err(TextError::msg("search_failed", joined));
    }
    if !warnings.is_empty() && reason == "exhausted" {
        reason = "search_warnings".into();
    }
    warnings.truncate(20);
    Ok(json!({
        "path": a.path, "matches": col.matches, "count": col.count, "scan_complete": reason == "exhausted",
        "stop_reason": reason,
        "scope": {
            "mode": a.mode, "encoding": a.encoding, "max_depth": a.max_depth, "include_hidden": a.include_hidden,
            "respect_ignore": a.respect_ignore, "include": a.include, "exclude": a.exclude,
            "max_file_bytes": LIMIT, "follow_links": false, "ignore_parent": false, "ignore_global": false,
        },
        "warnings": warnings, "native_exit_code": native_exit_code, "engine_version": ENGINE_VERSION,
    }))
}

pub fn search_text(args: SearchArgs) -> Envelope {
    let start = Instant::now();
    let mut r = envelope::base("windows_search_text");
    let out = run_search(&args);
    finish(&mut r, start, out);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(bytes: &[u8], limited: bool) -> RawRead {
        RawRead {
            path: "C:\\x".into(),
            size_bytes: 99,
            modified_utc: "t".into(),
            bytes: bytes.to_vec(),
            byte_limited: limited,
        }
    }

    #[test]
    fn chunks_and_newlines() {
        assert_eq!(chunks("a\r\nb\nc\rd"), vec!["a\r\n", "b\n", "c\r", "d"]);
        assert_eq!(chunks(""), Vec::<&str>::new());
        assert_eq!(newline_kind("a\r\nb\r\n"), "crlf");
        assert_eq!(newline_kind("a\nb\r\n"), "mixed");
        assert_eq!(newline_kind("ab"), "none");
        assert_eq!(newline_kind("a\rb"), "cr");
    }

    #[test]
    fn text_data_pages_and_hashes() {
        let v = text_data(&raw(b"one\ntwo\nthree\n", false), "auto", 2, 1).unwrap();
        assert_eq!(v["lines"][0]["text"], "two");
        assert_eq!(v["next_start_line"], 3);
        assert_eq!(v["stop_reason"], "line_limit");
        assert_eq!(v["sha256"], digest(b"one\ntwo\nthree\n"));
        assert_eq!(v["newline"], "lf");
        let v = text_data(&raw(b"one\ntw", true), "auto", 1, 10).unwrap();
        assert_eq!(v["lines"][1]["complete"], false);
        assert_eq!(v["stop_reason"], "byte_limit");
        assert!(v["sha256"].is_null());
        let v = text_data(&raw(b"a\r", true), "auto", 1, 10).unwrap();
        assert_eq!(v["lines"][0]["complete"], false);
    }

    #[test]
    fn text_data_encodings() {
        let mut b = BOM_UTF16_LE.to_vec();
        b.extend("hé\n".encode_utf16().flat_map(u16::to_le_bytes));
        let v = text_data(&raw(&b, false), "auto", 1, 10).unwrap();
        assert_eq!(v["encoding"], "utf-16-le");
        assert_eq!(v["bom"], true);
        assert_eq!(v["lines"][0]["text"], "hé");
        assert_eq!(
            text_data(&raw(&b, false), "utf-8", 1, 10).unwrap_err(),
            "encoding_bom_mismatch"
        );
        assert_eq!(
            text_data(&raw(b"\xff\xfe\x00\x00", false), "auto", 1, 1).unwrap_err(),
            "unsupported_utf32"
        );
        assert_eq!(
            text_data(&raw(b"a\x00b", false), "auto", 1, 1).unwrap_err(),
            "binary_content"
        );
        assert_eq!(
            text_data(&raw(b"\xe9", false), "auto", 1, 1).unwrap_err(),
            "invalid_encoding"
        );
        // incomplete UTF-8 at a byte boundary is withheld, not an error
        let v = text_data(&raw(b"ab\xc3", true), "auto", 1, 1).unwrap();
        assert_eq!(v["lines"][0]["text"], "ab");
        assert_eq!(
            text_data(&raw(b"caf\xe9", false), "windows-1252", 1, 1).unwrap()["lines"][0]["text"],
            "café"
        );
        assert_eq!(
            text_data(&raw(b"\x81", false), "windows-1252", 1, 1).unwrap_err(),
            "invalid_encoding"
        );
    }

    #[test]
    fn encode_roundtrip_and_bom() {
        assert_eq!(encode("é", "windows-1252", false).unwrap(), b"\xe9");
        assert_eq!(encode("€", "windows-1252", false).unwrap(), b"\x80");
        assert_eq!(
            encode("x", "windows-1252", true).unwrap_err().kind,
            "unsupported_bom"
        );
        assert_eq!(
            encode("中", "windows-1252", false).unwrap_err().kind,
            "invalid_encoding"
        );
        assert_eq!(encode("a", "utf-16-le", true).unwrap(), b"\xff\xfea\x00");
        let (t, e, p) = decode(b"\xef\xbb\xbfhi", "auto").unwrap();
        assert_eq!((t.as_str(), e.as_str(), p.len()), ("hi", "utf-8", 3));
    }

    #[test]
    fn exact_edit_rules() {
        let e = |o: &str, n: &str| (o.to_string(), n.to_string());
        assert_eq!(
            exact_edit("a b c", &[e("b", "B"), e("c", "C")]).unwrap(),
            "a B C"
        );
        assert_eq!(
            exact_edit("abc", &[e("x", "y")]).unwrap_err().kind,
            "text_not_found"
        );
        assert_eq!(
            exact_edit("aa a", &[e("a", "b")]).unwrap_err().kind,
            "ambiguous_match"
        );
        assert_eq!(
            exact_edit("aaa", &[e("aa", "b")]).unwrap_err().kind,
            "ambiguous_match"
        );
        assert_eq!(
            exact_edit("abcd", &[e("abc", "x"), e("cd", "y")])
                .unwrap_err()
                .kind,
            "overlapping_edits"
        );
    }

    #[test]
    fn diff_matches_difflib_shape() {
        let d: String = unified_diff("a\nb\nc\n", "a\nB\nc\n").concat();
        assert_eq!(
            d,
            "--- before\n+++ after\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"
        );
        let d: String = unified_diff("", "x\n").concat();
        assert_eq!(d, "--- before\n+++ after\n@@ -0,0 +1 @@\n+x\n");
        assert!(unified_diff("same\n", "same\n").is_empty());
    }

    fn wargs(path: &str, mode: &str, sha: Option<String>, content: &str) -> WriteArgs {
        WriteArgs {
            edit: false,
            path: path.into(),
            content: content.into(),
            mode: mode.into(),
            expected_sha256: sha,
            encoding: "preserve".into(),
            source_encoding: "auto".into(),
            bom: json!("preserve"),
            newline: "preserve".into(),
            dry_run: false,
            edits: vec![],
        }
    }

    #[test]
    fn write_create_replace_edit_cycle() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f.txt");
        let ps = p.to_str().unwrap();
        let r = write_or_edit(wargs(ps, "create", None, "a\r\nb\r\n"));
        assert_eq!(r["status"], "succeeded", "{r:?}");
        assert_eq!(r["data"]["verified"], true);
        let r2 = write_or_edit(wargs(ps, "create", None, "x"));
        assert_eq!(r2["error"]["kind"], "already_exists");
        let sha = digest(&std::fs::read(&p).unwrap());
        let r3 = write_or_edit(wargs(ps, "replace", Some("0".repeat(64)), "z\n"));
        assert_eq!(r3["error"]["kind"], "version_conflict");
        let r4 = write_or_edit(wargs(ps, "replace", Some(sha.clone()), "z\nw\n"));
        assert_eq!(r4["status"], "succeeded", "{r4:?}");
        // newline preserved (crlf) on replace
        assert_eq!(std::fs::read(&p).unwrap(), b"z\r\nw\r\n");
        let sha = digest(&std::fs::read(&p).unwrap());
        let mut e = wargs(ps, "replace", Some(sha), "");
        e.edit = true;
        e.encoding = "auto".into();
        e.edits = vec![("w".into(), "W".into())];
        let r5 = write_or_edit(e);
        assert_eq!(r5["status"], "succeeded", "{r5:?}");
        assert_eq!(std::fs::read(&p).unwrap(), b"z\r\nW\r\n");
        assert!(
            r5["data"]["diff"]
                .as_str()
                .unwrap()
                .contains("-w\r\n+W\r\n")
        );
        // no stage files left behind
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn write_ps1_gets_bom_and_dry_run_does_not_write() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.ps1");
        let mut a = wargs(p.to_str().unwrap(), "create", None, "Write-Host hi\n");
        a.dry_run = true;
        let r = write_or_edit(a);
        assert_eq!(r["data"]["bom"], true);
        assert_eq!(r["data"]["applied"], false);
        assert!(!p.exists());
        let r = write_or_edit(wargs(
            d.path().join("missing").join("x.txt").to_str().unwrap(),
            "create",
            None,
            "x",
        ));
        assert_eq!(r["error"]["kind"], "parent_not_found");
        assert_eq!(r["may_have_changed"], false);
    }

    fn sargs(root: &Path, pattern: &str, extra: Value) -> SearchArgs {
        let mut v = json!({"path": root.to_str().unwrap(), "pattern": pattern});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        prepare_search(&v).unwrap()
    }

    fn search_tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::write(r.join("a.txt"), "one\nhit 1\r\nthree\nhit 2\n").unwrap();
        std::fs::write(r.join("b.rs"), "fn hit() {}\n").unwrap();
        std::fs::write(r.join(".hidden.txt"), "hit hidden\n").unwrap();
        std::fs::write(r.join("bin.dat"), b"hit\x00\x01\x02").unwrap();
        std::fs::write(r.join(".ignore"), "ignored.txt\n").unwrap();
        std::fs::write(r.join("ignored.txt"), "hit ignored\n").unwrap();
        std::fs::create_dir_all(r.join("d1").join("d2")).unwrap();
        std::fs::write(r.join("d1").join("d2").join("deep.txt"), "hit deep\n").unwrap();
        let mut u16 = vec![0xff, 0xfe];
        u16.extend("hit ñ utf16\n".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(r.join("w.txt"), u16).unwrap();
        d
    }

    fn files(v: &Value) -> Vec<String> {
        let mut f: Vec<String> = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["kind"] == "match")
            .map(|m| {
                Path::new(m["path"].as_str().unwrap())
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        f.sort();
        f.dedup();
        f
    }

    #[test]
    fn search_defaults_skip_hidden_ignored_and_binary() {
        let d = search_tree();
        let v = run_search(&sargs(d.path(), "hit", json!({}))).unwrap();
        assert_eq!(files(&v), ["a.txt", "b.rs", "deep.txt", "w.txt"], "{v}");
        assert_eq!(v["count"], 5);
        assert_eq!(v["stop_reason"], "exhausted");
        assert_eq!(v["scan_complete"], true);
        assert_eq!(v["native_exit_code"], 0);
        assert_eq!(v["engine_version"], ENGINE_VERSION);
        let m = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["text"] == "hit 1")
            .unwrap();
        assert_eq!(m["line_number"], 2);
        assert!(v.to_string().contains("hit ñ utf16"), "{v}");
    }

    #[test]
    fn search_options() {
        let d = search_tree();
        let r = d.path();
        let v = run_search(&sargs(
            r,
            "hit",
            json!({"include_hidden": true, "respect_ignore": false}),
        ))
        .unwrap();
        assert_eq!(
            files(&v),
            [
                ".hidden.txt",
                "a.txt",
                "b.rs",
                "deep.txt",
                "ignored.txt",
                "w.txt"
            ]
        );
        let v = run_search(&sargs(r, "hit", json!({"max_depth": 1}))).unwrap();
        assert!(!files(&v).contains(&"deep.txt".to_string()), "{v}");
        let v = run_search(&sargs(r, "hit", json!({"include": ["*.rs"]}))).unwrap();
        assert_eq!(files(&v), ["b.rs"]);
        let v = run_search(&sargs(r, "hit", json!({"exclude": ["*.txt"]}))).unwrap();
        assert_eq!(files(&v), ["b.rs"]);
        let v = run_search(&sargs(r, "HIT", json!({}))).unwrap();
        assert_eq!(v["count"], 0);
        assert_eq!(v["native_exit_code"], 1);
        let v = run_search(&sargs(r, "HIT", json!({"case_sensitive": false}))).unwrap();
        assert_eq!(v["count"], 5);
        let v = run_search(&sargs(r, "hit (1|2)", json!({}))).unwrap();
        assert_eq!(v["count"], 0, "literal mode treats the pattern verbatim");
        let v = run_search(&sargs(r, "^hit [12]$", json!({"mode": "regex"}))).unwrap();
        // like ripgrep without --crlf, `$` does not match before `\r`
        assert_eq!(v["count"], 1, "{v}");
        let v = run_search(&sargs(
            &r.join("a.txt"),
            "three",
            json!({"context_lines": 1}),
        ))
        .unwrap();
        let kinds: Vec<_> = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["kind"].as_str().unwrap(),
                    m["line_number"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(kinds, [("context", 2), ("match", 3), ("context", 4)]);
        let v = run_search(&sargs(
            &r.join("w.txt"),
            "ñ",
            json!({"encoding": "utf-16le"}),
        ))
        .unwrap();
        assert_eq!(v["count"], 1, "{v}");
    }

    #[test]
    fn search_limits_and_errors() {
        let d = search_tree();
        let r = d.path();
        let v = run_search(&sargs(r, "hit", json!({"limit": 1}))).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["stop_reason"], "result_limit");
        assert_eq!(v["scan_complete"], false);
        // an explicitly named binary file is searched (NULs converted)
        let v = run_search(&sargs(&r.join("bin.dat"), "hit", json!({}))).unwrap();
        assert_eq!(v["count"], 1, "{v}");
        assert!(
            v["warnings"][0].as_str().unwrap().starts_with("Binary"),
            "{v}"
        );
        let e = run_search(&sargs(r, "(", json!({"mode": "regex"}))).unwrap_err();
        assert_eq!(e.kind, "search_failed");
        let e = run_search(&sargs(&r.join("missing"), "x", json!({}))).unwrap_err();
        assert_eq!(e.kind, "search_failed");
        std::fs::write(r.join("big.txt"), vec![b'a'; LIMIT + 1]).unwrap();
        let e = run_search(&sargs(&r.join("big.txt"), "a", json!({}))).unwrap_err();
        assert_eq!(e.kind, "file_too_large");
        // files over 1 MiB are skipped during a walk
        let v = run_search(&sargs(r, "aaaa", json!({}))).unwrap();
        assert_eq!(v["count"], 0);
    }

    #[test]
    fn engine_version_matches_lock() {
        // Windows checkouts may convert the lockfile to CRLF.
        let lock = include_str!("../../Cargo.lock").replace("\r\n", "\n");
        let (name, version) = ENGINE_VERSION.split_once(' ').unwrap();
        assert!(
            lock.contains(&format!("name = \"{name}\"\nversion = \"{version}\"")),
            "update ENGINE_VERSION"
        );
    }
}

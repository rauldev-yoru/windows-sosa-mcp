//! Windows path rules shared by the typed tools: absolute-path validation,
//! lexical normalization (`ntpath.normpath` semantics) and the cwd check.

use std::path::Path;
use std::time::SystemTime;

/// `^(?:[A-Za-z]:[\\/]|\\\\[^\\/]+\\[^\\/]+(?:\\|$))` without a regex engine.
fn absolute_prefix(path: &str, forbid_slash_in_unc: bool) -> bool {
    let b = path.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        return true;
    }
    let Some(rest) = path.strip_prefix("\\\\") else {
        return false;
    };
    let bad = |c: char| c == '\\' || (forbid_slash_in_unc && c == '/');
    let server_len = rest
        .chars()
        .take_while(|c| !bad(*c))
        .map(char::len_utf8)
        .sum::<usize>();
    if server_len == 0 || !rest[server_len..].starts_with('\\') {
        return false;
    }
    let share = &rest[server_len + 1..];
    let share_len = share
        .chars()
        .take_while(|c| !bad(*c))
        .map(char::len_utf8)
        .sum::<usize>();
    if share_len == 0 {
        return false;
    }
    let tail = &share[share_len..];
    tail.is_empty() || tail.starts_with('\\')
}

/// The core runner's cwd rule (`cwd must be an absolute Windows path`).
pub fn is_absolute_windows(path: &str) -> bool {
    absolute_prefix(path, false) || unix_test_path(path)
}

/// `valid_path` of the typed inspection/text tools: ordinary absolute drive
/// or UNC path, no `\\?\`/`\\.\` devices, no `:` streams, no control chars.
/// Returns the `ntpath.normpath` form.
pub fn valid_path(path: &str) -> Result<String, String> {
    if unix_test_path(path) {
        return Ok(path.to_string());
    }
    let bad = !absolute_prefix(path, true)
        || path.starts_with("\\\\?\\")
        || path.starts_with("\\\\.\\")
        || path.chars().skip(2).any(|c| c == ':')
        || path.chars().any(|c| (c as u32) < 32);
    if bad {
        return Err(
            "Expected ordinary absolute Windows filesystem path (no devices or streams)".into(),
        );
    }
    Ok(normpath(path))
}

/// `ntpath.splitdrive` semantics for the shapes `valid_path` admits.
fn split_drive(p: &str) -> (String, String) {
    let b = p.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return (p[..2].to_string(), p[2..].to_string());
    }
    if p.starts_with("\\\\") && !p.starts_with("\\\\\\") {
        // \\server\share
        let rest = &p[2..];
        if let Some(i) = rest.find('\\') {
            let after = &rest[i + 1..];
            let j = after.find('\\').unwrap_or(after.len());
            if j == 0 {
                return (String::new(), p.to_string());
            }
            let end = 2 + i + 1 + j;
            return (p[..end].to_string(), p[end..].to_string());
        }
        return (p.to_string(), String::new());
    }
    (String::new(), p.to_string())
}

/// Lexical normalization with `ntpath.normpath` semantics.
pub fn normpath(path: &str) -> String {
    let p = path.replace('/', "\\");
    let (prefix, rest) = split_drive(&p);
    let mut prefix = prefix;
    let mut rest = rest.as_str();
    if rest.starts_with('\\') {
        prefix.push('\\');
        rest = rest.trim_start_matches('\\');
    }
    let mut comps: Vec<&str> = Vec::new();
    for c in rest.split('\\') {
        match c {
            "" | "." => {}
            ".." => {
                if comps.last().is_some_and(|l| *l != "..") {
                    comps.pop();
                } else if prefix.ends_with('\\') {
                    // ".." at the root is dropped
                } else {
                    comps.push("..");
                }
            }
            other => comps.push(other),
        }
    }
    if prefix.is_empty() && comps.is_empty() {
        return ".".into();
    }
    format!("{prefix}{}", comps.join("\\"))
}

/// .NET `DateTime.ToString("o")` of a UTC time: 7 fractional digits + `Z`.
pub fn fmt_o(t: SystemTime) -> String {
    let (secs, ticks) = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos() / 100),
        Err(e) => {
            let d = e.duration();
            let mut s = -(d.as_secs() as i64);
            let mut n = d.subsec_nanos();
            if n > 0 {
                s -= 1;
                n = 1_000_000_000 - n;
            }
            (s, n / 100)
        }
    };
    let dt = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default();
    format!("{}.{ticks:07}Z", dt.format("%Y-%m-%dT%H:%M:%S"))
}

/// Attribute bits of the entry itself (not following reparse points).
#[derive(Debug, Clone, Copy, Default)]
pub struct Attrs {
    pub directory: bool,
    pub hidden: bool,
    pub system: bool,
    pub reparse: bool,
}

pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[cfg(windows)]
pub fn attrs(meta: &std::fs::Metadata, _path: &Path) -> Attrs {
    use std::os::windows::fs::MetadataExt;
    let a = meta.file_attributes();
    Attrs {
        directory: a & FILE_ATTRIBUTE_DIRECTORY != 0,
        hidden: a & FILE_ATTRIBUTE_HIDDEN != 0,
        system: a & FILE_ATTRIBUTE_SYSTEM != 0,
        reparse: a & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    }
}

/// Portable approximation used for Linux tests: dotfiles are hidden,
/// symlinks are reparse points.
#[cfg(not(windows))]
pub fn attrs(meta: &std::fs::Metadata, path: &Path) -> Attrs {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Attrs {
        directory: meta.is_dir(),
        hidden: name.starts_with('.'),
        system: false,
        reparse: meta.file_type().is_symlink(),
    }
}

/// `FileAttributes` read without following the final reparse point
/// (`[IO.File]::GetAttributes`).
pub fn get_attributes(path: &Path) -> std::io::Result<(std::fs::Metadata, Attrs)> {
    let m = std::fs::symlink_metadata(path)?;
    let a = attrs(&m, path);
    Ok((m, a))
}

/// Join like PowerShell `Join-Path` / .NET: single separator between parts.
pub fn join(dir: &str, name: &str) -> String {
    if unix_test_path(dir) {
        return format!("{}/{name}", dir.trim_end_matches('/'));
    }
    if dir.ends_with('\\') || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// Non-Windows builds also accept absolute POSIX paths so the portable
/// tool logic can be exercised by the Linux test suite.
pub fn unix_test_path(path: &str) -> bool {
    cfg!(not(windows)) && path.starts_with('/')
}

/// `^[A-Za-z]:\\` — the "local drive path" rule for writes and jobs.
pub fn is_local_drive(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\')
        || unix_test_path(path)
}

/// Is the path on a local disk path (drive letter), not UNC?
pub fn is_unc(path: &str) -> bool {
    path.starts_with("\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normpath_matches_ntpath() {
        assert_eq!(
            normpath("C:/Users//alice/./x/../y\\"),
            "C:\\Users\\alice\\y"
        );
        assert_eq!(normpath("C:\\"), "C:\\");
        assert_eq!(normpath("C:\\.."), "C:\\");
        assert_eq!(normpath("\\\\srv\\share\\a\\..\\b"), "\\\\srv\\share\\b");
        assert_eq!(normpath("\\\\srv\\share"), "\\\\srv\\share");
    }

    #[test]
    fn valid_path_rules() {
        assert!(valid_path("C:\\Windows").is_ok());
        assert!(valid_path("c:/x").is_ok());
        assert!(valid_path("\\\\srv\\share\\x").is_ok());
        for bad in [
            "relative\\x",
            "\\\\?\\C:\\x",
            "\\\\.\\pipe\\x",
            "C:\\a:b",
            "C:\\a\u{1}",
            "C:",
            "\\\\srv",
        ] {
            assert!(valid_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cwd_rule() {
        assert!(is_absolute_windows("C:\\"));
        assert!(is_absolute_windows("D:/x"));
        assert!(is_absolute_windows("\\\\srv\\share"));
        assert!(!is_absolute_windows("C:x"));
        assert_eq!(is_absolute_windows("/tmp"), cfg!(not(windows)));
    }

    #[test]
    fn fmt_o_seven_digits() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_700);
        assert_eq!(fmt_o(t), "2023-11-14T22:13:20.1234567Z");
    }

    #[test]
    fn join_single_separator() {
        assert_eq!(join("C:\\x", "a.exe"), "C:\\x\\a.exe");
        assert_eq!(join("C:\\", "a.exe"), "C:\\a.exe");
    }
}

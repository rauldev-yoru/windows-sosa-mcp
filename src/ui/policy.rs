//! UI privacy policy (`ui_policy.json`).
//!
//! A deny-list of sensitive windows applied to snapshots, actions and
//! captures: excluded windows are listed without title, refused as
//! targets and blacked out in monitor captures. Built-in defaults cover
//! password managers and secret-looking titles; a user file overrides
//! individual keys.
//!
//! Location (D18): `%APPDATA%\windows-sosa-mcp\ui_policy.json`, override
//! with `WINDOWS_SOSA_MCP_UI_POLICY` (tests). Missing file = defaults.

use std::path::PathBuf;

use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value, json};

use super::UiError;

pub const DEFAULT_EXCLUDE_PROCESSES: [&str; 11] = [
    "KeePass",
    "KeePassXC",
    "1Password",
    "Bitwarden",
    "LastPass",
    "consent",
    "CredentialUIBroker",
    "LockApp",
    "SecHealthUI",
    "LogonUI",
    "PasswordOnWakeSettingFlyout",
];

pub const DEFAULT_EXCLUDE_TITLE_REGEX: &str = r"(?i)contrase[nñ]a|password|passcode|passphrase|\b2fa\b|c[oó]digo de verificaci[oó]n|verification code|private key|secret key|recovery code";

/// Effective policy.
#[derive(Debug, Clone)]
pub struct Policy {
    pub exclude_processes: Vec<String>,
    pub exclude_title_regex: Option<String>,
    title_re: Option<Regex>,
    pub allow_processes: Vec<String>,
    pub capture_monitor_enabled: bool,
    /// File path or `"defaults"`.
    pub source: String,
}

impl Default for Policy {
    fn default() -> Self {
        Self::build(
            DEFAULT_EXCLUDE_PROCESSES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            Some(DEFAULT_EXCLUDE_TITLE_REGEX.to_string()),
            Vec::new(),
            true,
            "defaults".into(),
        )
        .expect("default policy regex compiles")
    }
}

/// The policy file location (see module docs).
pub fn policy_path() -> PathBuf {
    if let Some(p) = std::env::var_os("WINDOWS_SOSA_MCP_UI_POLICY") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(crate::state::root);
    base.join("windows-sosa-mcp").join("ui_policy.json")
}

fn invalid(path: &str, msg: impl std::fmt::Display) -> UiError {
    UiError::new("policy_invalid", format!("{path}: {msg}"))
}

fn str_list(v: &Value, key: &str, path: &str) -> Result<Vec<String>, UiError> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(a) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .ok_or_else(|| invalid(path, format!("{key} must be a list of strings")))
            })
            .collect(),
        _ => Err(invalid(path, format!("{key} must be a list of strings"))),
    }
}

impl Policy {
    fn build(
        exclude_processes: Vec<String>,
        exclude_title_regex: Option<String>,
        allow_processes: Vec<String>,
        capture_monitor_enabled: bool,
        source: String,
    ) -> Result<Self, UiError> {
        // Title regexes are matched case-insensitively.
        let title_re = match exclude_title_regex.as_deref() {
            Some(r) if !r.is_empty() => Some(
                RegexBuilder::new(r)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| {
                        UiError::new("policy_invalid", format!("exclude_title_regex: {e}"))
                    })?,
            ),
            _ => None,
        };
        Ok(Self {
            exclude_processes,
            exclude_title_regex,
            title_re,
            allow_processes,
            capture_monitor_enabled,
            source,
        })
    }

    /// `load_policy`: defaults, overridden key by key by the user file.
    pub fn load() -> Result<Self, UiError> {
        let path = policy_path();
        if !path.is_file() {
            return Ok(Self::default());
        }
        let shown = path.display().to_string();
        let text = std::fs::read_to_string(&path).map_err(|e| invalid(&shown, e))?;
        Self::from_json(&text, &shown)
    }

    /// Parse a user policy document (keys not given keep the defaults).
    pub fn from_json(text: &str, source: &str) -> Result<Self, UiError> {
        let user: Value = serde_json::from_str(text).map_err(|e| invalid(source, e))?;
        let Value::Object(user) = user else {
            return Err(invalid(source, "policy must be a JSON object"));
        };
        let d = Self::default();
        let exclude_processes = match user.get("exclude_processes") {
            Some(v) => str_list(v, "exclude_processes", source)?,
            None => d.exclude_processes,
        };
        let allow_processes = match user.get("allow_processes") {
            Some(v) => str_list(v, "allow_processes", source)?,
            None => d.allow_processes,
        };
        let exclude_title_regex = match user.get("exclude_title_regex") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Null) => None,
            Some(_) => return Err(invalid(source, "exclude_title_regex must be a string")),
            None => d.exclude_title_regex,
        };
        let capture_monitor_enabled = match user.get("capture_monitor_enabled") {
            Some(Value::Bool(b)) => *b,
            // null disables, like false.
            Some(Value::Null) => false,
            Some(_) => return Err(invalid(source, "capture_monitor_enabled must be a boolean")),
            None => true,
        };
        Self::build(
            exclude_processes,
            exclude_title_regex,
            allow_processes,
            capture_monitor_enabled,
            source.to_string(),
        )
    }

    /// `Excluded` of the worker: reason or `None`. Process comparisons are
    /// case-insensitive (`-notin`/`-ieq`), the title regex too (`-match`).
    pub fn excluded(&self, process: Option<&str>, title: &str) -> Option<&'static str> {
        if !self.allow_processes.is_empty() {
            let allowed = process.is_some_and(|p| {
                self.allow_processes
                    .iter()
                    .any(|a| a.to_lowercase() == p.to_lowercase())
            });
            if !allowed {
                return Some("not_in_allow_list");
            }
        }
        if let Some(p) = process.filter(|p| !p.is_empty()) {
            let lp = p.to_lowercase();
            if self
                .exclude_processes
                .iter()
                .any(|x| !x.is_empty() && x.to_lowercase() == lp)
            {
                return Some("excluded_process");
            }
        }
        if !title.is_empty() && self.title_re.as_ref().is_some_and(|r| r.is_match(title)) {
            return Some("excluded_title");
        }
        None
    }

    /// Whether the policy excludes anything at all (`redact=false` gate).
    pub fn has_exclusions(&self) -> bool {
        !self.exclude_processes.is_empty()
            || self
                .exclude_title_regex
                .as_deref()
                .is_some_and(|s| !s.is_empty())
    }

    /// JSON view of the effective policy.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("exclude_processes".into(), json!(self.exclude_processes));
        m.insert(
            "exclude_title_regex".into(),
            json!(self.exclude_title_regex),
        );
        m.insert("allow_processes".into(), json!(self.allow_processes));
        m.insert(
            "capture_monitor_enabled".into(),
            json!(self.capture_monitor_enabled),
        );
        m.insert("source".into(), json!(self.source));
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy() {
        let p = Policy::default();
        assert_eq!(p.source, "defaults");
        assert_eq!(p.excluded(Some("KeePass"), "x"), Some("excluded_process"));
        assert_eq!(p.excluded(Some("keepassxc"), "x"), Some("excluded_process"));
        assert_eq!(p.excluded(Some("chrome"), "Gmail"), None);
        for t in [
            "Enter PASSWORD",
            "Contraseña de red",
            "Your 2FA code",
            "Código de verificación",
            "codigo de verificacion",
            "Recovery Code",
        ] {
            assert_eq!(p.excluded(Some("chrome"), t), Some("excluded_title"), "{t}");
        }
        // `\b2fa\b`: not inside a word.
        assert_eq!(p.excluded(Some("chrome"), "x2fax"), None);
        assert_eq!(p.excluded(None, ""), None);
        assert!(p.has_exclusions());
    }

    #[test]
    fn user_file_overrides_keys_and_allow_list() {
        let p = Policy::from_json(
            r#"{"exclude_processes": [], "exclude_title_regex": "secreto", "allow_processes": ["Notepad"], "capture_monitor_enabled": false}"#,
            "f.json",
        )
        .unwrap();
        assert!(!p.capture_monitor_enabled);
        assert_eq!(p.excluded(Some("chrome"), "x"), Some("not_in_allow_list"));
        assert_eq!(p.excluded(None, "x"), Some("not_in_allow_list"));
        assert_eq!(p.excluded(Some("notepad"), "x"), None);
        assert_eq!(
            p.excluded(Some("notepad"), "Mi SECRETO"),
            Some("excluded_title")
        );
        assert_eq!(p.source, "f.json");
        let p = Policy::from_json(r#"{"exclude_title_regex": ""}"#, "f").unwrap();
        assert_eq!(p.excluded(Some("x"), "password"), None);
        assert!(p.has_exclusions()); // default processes kept
    }

    #[test]
    fn invalid_policies_are_typed() {
        for bad in [
            "[]",
            "nope",
            r#"{"exclude_title_regex": "("}"#,
            r#"{"exclude_processes": "KeePass"}"#,
            r#"{"capture_monitor_enabled": "no"}"#,
        ] {
            let e = Policy::from_json(bad, "f").unwrap_err();
            assert_eq!(e.kind, "policy_invalid", "{bad}");
        }
    }
}

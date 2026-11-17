//! Native implementations of the non-UI tools.
//!
//! Each tool has a `prepare` step (argument shaping and path rules, run
//! before the busy lock, rejections are audited as `rejected`) and an
//! execution step that returns the result envelope.

pub mod fs;
pub mod jobs;
pub mod packages;
pub mod shell;
pub mod text;
pub mod winget_codes;

use serde_json::Value;

use crate::envelope::Envelope;

/// A rejection produced while preparing a request.
#[derive(Debug, Clone)]
pub struct ToolError {
    pub kind: String,
    pub message: String,
}

impl ToolError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::kind("invalid_arguments", message)
    }

    pub fn kind(kind: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

pub fn opt_str(a: &Value, key: &str) -> Option<String> {
    a.get(key).and_then(Value::as_str).map(String::from)
}

pub fn opt_i64(a: &Value, key: &str) -> Option<i64> {
    a.get(key).and_then(Value::as_i64)
}

pub fn opt_bool(a: &Value, key: &str) -> Option<bool> {
    a.get(key).and_then(Value::as_bool)
}

/// The 15 shell/file/job/package tools.
pub const NATIVE: [&str; 15] = [
    "windows_run_powershell",
    "windows_list_directory",
    "windows_find_files",
    "windows_stat_path",
    "windows_read_text_file",
    "windows_resolve_executable",
    "windows_search_text",
    "windows_write_text_file",
    "windows_edit_text_file",
    "windows_job_start",
    "windows_job_status",
    "windows_job_list",
    "windows_job_cancel",
    "windows_package_query",
    "windows_package_install",
];

/// A validated request ready to run under the busy lock.
pub enum Prepared {
    Run(shell::RunArgs),
    List(fs::ListArgs),
    Find(fs::FindArgs),
    Stat(String),
    Read(fs::ReadArgs),
    Resolve(fs::ResolveArgs),
    Search(text::SearchArgs),
    Write(text::WriteArgs),
    Job(String, Value),
    Package(String, Value),
    /// A UI/capture tool (name, arguments).
    Ui(String, Value),
    /// A hands-free/input/wait tool (name, arguments).
    Handsfree(String, Value),
}

pub fn is_native(name: &str) -> bool {
    if crate::ui::is_ui_tool(name) || crate::handsfree::tools::is_handsfree_tool(name) {
        return true;
    }
    NATIVE.contains(&name)
}

pub fn prepare(name: &str, a: &Value) -> Result<Prepared, ToolError> {
    Ok(match name {
        "windows_run_powershell" => Prepared::Run(shell::prepare(a)?),
        "windows_list_directory" => Prepared::List(fs::prepare_list(a)?),
        "windows_find_files" => Prepared::Find(fs::prepare_find(a)?),
        "windows_stat_path" => Prepared::Stat(fs::prepare_path(a)?),
        "windows_read_text_file" => Prepared::Read(fs::prepare_read(a)?),
        "windows_resolve_executable" => Prepared::Resolve(fs::prepare_resolve(a)?),
        "windows_search_text" => Prepared::Search(text::prepare_search(a)?),
        "windows_write_text_file" | "windows_edit_text_file" => {
            Prepared::Write(text::prepare_write(name, a)?)
        }
        "windows_job_start" => {
            jobs::prepare_start(a)?;
            Prepared::Job(name.into(), a.clone())
        }
        "windows_job_status" | "windows_job_list" | "windows_job_cancel" => {
            Prepared::Job(name.into(), a.clone())
        }
        "windows_package_query" | "windows_package_install" => {
            Prepared::Package(name.into(), a.clone())
        }
        n if crate::ui::is_ui_tool(n) => Prepared::Ui(n.to_string(), a.clone()),
        n if crate::handsfree::tools::is_handsfree_tool(n) => {
            Prepared::Handsfree(n.to_string(), a.clone())
        }
        _ => return Err(ToolError::kind("not_implemented", "not a native tool")),
    })
}

pub fn execute(p: Prepared) -> Envelope {
    match p {
        Prepared::Run(a) => shell::run(a),
        Prepared::List(a) => fs::list_directory(a),
        Prepared::Find(a) => fs::find_files(a),
        Prepared::Stat(p) => fs::stat_path(p),
        Prepared::Read(a) => fs::read_text_file(a),
        Prepared::Resolve(a) => fs::resolve_executable(a),
        Prepared::Search(a) => text::search_text(a),
        Prepared::Write(a) => text::write_or_edit(a),
        Prepared::Job(n, a) => jobs::execute(&n, &a),
        Prepared::Package(n, a) => packages::execute(&n, &a),
        Prepared::Ui(n, a) => crate::ui::execute(&n, &a),
        Prepared::Handsfree(n, a) => crate::handsfree::tools::execute(&n, &a),
    }
}

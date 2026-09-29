use anyhow::Result;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

use crate::history::{
    attach_history_metadata, directory_snapshot, missing_snapshot, no_history, record_change,
};
use crate::security::path_guard::GUARD;

fn error_reason(error_code: &str) -> &'static str {
    match error_code {
        "invalid_path" => "Path argument is missing or invalid.",
        "path_is_file" => "Target path points to a file, not a directory.",
        "already_exists" => "Target directory already exists.",
        "parent_missing" => "Parent directory does not exist.",
        "permission_denied" => "Operation was denied by filesystem permissions.",
        "invalid_input" => "Input is invalid for the requested operation.",
        "file_locked" => "Path is locked by another process.",
        "io_error" => "Filesystem I/O error occurred.",
        _ => "Unknown error.",
    }
}

pub fn schema() -> Value {
    json!({
        "name": "create_directory",
        "title": "Create directory",
        "description": "Create one directory with optional parent creation and structured success/error metadata. Use before creating files in a new path.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "create_parents": { "type": "boolean" },
                "allow_existing": { "type": "boolean", "default": true, "description": "Treat an existing directory as success. Defaults to true." }
            },
            "required": ["path"]
        }
    })
}

fn error_response(
    path: &Path,
    canonical: &Path,
    error_code: &str,
    message: impl Into<String>,
) -> Value {
    json!({
        "success": false,
        "path": crate::common::normalize_display_path(path),
        "canonical_path": crate::common::normalize_display_path(canonical),
        "error_code": error_code,
        "reason": error_reason(error_code),
        "message": message.into()
    })
}

fn io_error_response(
    path: &Path,
    canonical: &Path,
    operation: &str,
    err: &std::io::Error,
) -> Value {
    let error_code = super::file_io_error::classify(path, err, "not_found");

    json!({
        "success": false,
        "path": crate::common::normalize_display_path(path),
        "canonical_path": crate::common::normalize_display_path(canonical),
        "error_code": error_code,
        "reason": error_reason(error_code),
        "operation": operation,
        "message": format!("Failed to {}: {}", operation, err),
        "io_kind": format!("{:?}", err.kind()),
        "os_error": err.raw_os_error()
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");

    if path_str.is_empty() {
        let empty = PathBuf::from("");
        return Ok(error_response(
            Path::new(path_str),
            empty.as_path(),
            "invalid_path",
            "path is required",
        ));
    }

    let path = crate::common::resolve_write_tool_path(path_str);
    let canonical_from_guard = GUARD.check_path(&path);

    let create_parents = args
        .get("create_parents")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let allow_existing = args
        .get("allow_existing")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    if path.exists() {
        if path.is_file() {
            return Ok(error_response(
                &path,
                &canonical_from_guard,
                "path_is_file",
                "target path points to a file",
            ));
        }

        if !allow_existing {
            return Ok(error_response(
                &path,
                &canonical_from_guard,
                "already_exists",
                "directory already exists (set allow_existing=true to accept this state)",
            ));
        }

        let mut response = json!({
            "success": true,
            "path": crate::common::normalize_display_path(&path),
            "canonical_path": crate::common::normalize_display_path(&std::fs::canonicalize(&path).unwrap_or(canonical_from_guard)),
            "created": false,
            "existed_before": true,
            "message": "directory already exists"
        });
        attach_history_metadata(&mut response, &no_history("no filesystem change"));
        return Ok(response);
    }

    if !create_parents
        && let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return Ok(error_response(
            &path,
            &canonical_from_guard,
            "parent_missing",
            "parent directory does not exist (set create_parents=true)",
        ));
    }

    let create_result = if create_parents {
        fs::create_dir_all(&path)
    } else {
        fs::create_dir(&path)
    };

    if let Err(err) = create_result {
        return Ok(io_error_response(
            &path,
            &canonical_from_guard,
            "create directory",
            &err,
        ));
    }

    let canonical_path = std::fs::canonicalize(&path).unwrap_or(canonical_from_guard);
    let history_outcome = record_change(
        "create_directory",
        &path,
        missing_snapshot(),
        directory_snapshot(),
        "create directory",
    );
    let mut response = json!({
        "success": true,
        "path": crate::common::normalize_display_path(&path),
        "canonical_path": crate::common::normalize_display_path(&canonical_path),
        "created": true,
        "existed_before": false,
        "message": "directory created"
    });
    attach_history_metadata(&mut response, &history_outcome);
    Ok(response)
}

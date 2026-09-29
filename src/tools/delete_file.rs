use anyhow::Result;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

use crate::history::{
    attach_history_metadata, capture_snapshot, missing_snapshot, no_history, record_change,
};
use crate::security::path_guard::GUARD;

fn error_reason(error_code: &str) -> &'static str {
    match error_code {
        "invalid_path" => "Path argument is missing or invalid.",
        "file_not_found" => "Target file does not exist.",
        "path_is_directory" => "Target path points to a directory, not a file.",
        "permission_denied" => "Operation was denied by filesystem permissions.",
        "invalid_input" => "Input is invalid for the requested operation.",
        "file_locked" => "File is locked by another process.",
        "invalid_hash" => "Expected hash is not a valid SHA-256 digest.",
        "hash_mismatch" => "File contents do not match the expected SHA-256 digest.",
        "io_error" => "Filesystem I/O error occurred.",
        _ => "Unknown error.",
    }
}

pub fn schema() -> Value {
    json!({
        "name": "delete_file",
        "title": "Delete file",
        "description": "Delete one file and return structured success/error metadata. Use only for explicit user-requested cleanup; directories are rejected.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "missing_ok": { "type": "boolean" },
                "expected_hash": { "type": "string", "description": "Optional SHA-256 precondition. The delete is rejected if the current file does not match this 64-character hex digest." }
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
    let error_code = super::file_io_error::classify(path, err, "file_not_found");

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

fn hash_mismatch_response(
    path: &Path,
    canonical: &Path,
    expected_hash: &str,
    actual_hash: Option<&str>,
) -> Value {
    let mut response = error_response(
        path,
        canonical,
        "hash_mismatch",
        "file contents changed since the expected SHA-256 digest was captured",
    );
    response["expected_hash"] = json!(expected_hash);
    response["actual_hash"] = json!(actual_hash);
    response
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
    let expected_hash = match args.get("expected_hash").and_then(Value::as_str) {
        Some(raw) => match super::file_hash::normalize_expected_hash(raw) {
            Ok(hash) => Some(hash),
            Err(message) => {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "invalid_hash",
                    message,
                ));
            }
        },
        None => None,
    };

    let missing_ok = args
        .get("missing_ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !path.exists() {
        if let Some(expected_hash) = expected_hash.as_deref() {
            return Ok(hash_mismatch_response(
                &path,
                &canonical_from_guard,
                expected_hash,
                None,
            ));
        }
        if missing_ok {
            let mut response = json!({
                "success": true,
                "path": crate::common::normalize_display_path(&path),
                "canonical_path": crate::common::normalize_display_path(&canonical_from_guard),
                "deleted": false,
                "existed_before": false,
                "message": "path does not exist; nothing deleted"
            });
            attach_history_metadata(&mut response, &no_history("no filesystem change"));
            return Ok(response);
        }

        return Ok(error_response(
            &path,
            &canonical_from_guard,
            "file_not_found",
            "file does not exist",
        ));
    }

    if path.is_dir() {
        return Ok(error_response(
            &path,
            &canonical_from_guard,
            "path_is_directory",
            "target path is a directory; delete_file only removes files",
        ));
    }

    let size_before = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let before_snapshot = capture_snapshot(&path);
    let sha256_before = if let Some(expected_hash) = expected_hash.as_deref() {
        let current_hash = match super::file_hash::sha256_file(&path) {
            Ok(hash) => Some(hash),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Ok(io_error_response(
                    &path,
                    &canonical_from_guard,
                    "recheck expected hash",
                    &error,
                ));
            }
        };
        if current_hash.as_deref() != Some(expected_hash) {
            return Ok(hash_mismatch_response(
                &path,
                &canonical_from_guard,
                expected_hash,
                current_hash.as_deref(),
            ));
        }
        current_hash
    } else {
        None
    };

    if let Err(err) = fs::remove_file(&path) {
        return Ok(io_error_response(
            &path,
            &canonical_from_guard,
            "delete file",
            &err,
        ));
    }
    if let Some(parent) = path.parent() {
        crate::indexer::notify_content_directory_changed(parent);
    }

    let history_outcome = match before_snapshot {
        Ok(before) => record_change(
            "delete_file",
            &path,
            before,
            missing_snapshot(),
            "delete file",
        ),
        Err(reason) => no_history(reason),
    };

    let mut response = json!({
        "success": true,
        "path": crate::common::normalize_display_path(&path),
        "canonical_path": crate::common::normalize_display_path(&canonical_from_guard),
        "deleted": true,
        "existed_before": true,
        "bytes_removed": size_before,
        "message": "file deleted"
    });
    if let Some(sha256_before) = sha256_before {
        crate::common::insert_object_field(&mut response, "sha256_before", json!(sha256_before));
    }
    attach_history_metadata(&mut response, &history_outcome);
    Ok(response)
}

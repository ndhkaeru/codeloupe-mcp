use anyhow::Result;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

use crate::history::{
    attach_history_metadata, file_snapshot, missing_snapshot, no_history, record_change,
};
use crate::limits::MAX_IN_MEMORY_TEXT_FILE_BYTES;
use crate::security::path_guard::GUARD;
use crate::tools::read_file::{decode_fuzzy, is_probably_binary};
use crate::tools::text_encoding::TextEncoding;

const MAX_EDIT_FILE_BYTES: u64 = MAX_IN_MEMORY_TEXT_FILE_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExistingLineEndings {
    None,
    Lf,
    Crlf,
    Mixed,
}

fn error_reason(error_code: &str) -> &'static str {
    match error_code {
        "invalid_path" => "Path argument is missing or invalid.",
        "invalid_mode" => "Mode is not one of supported values.",
        "binary_file" => "Only text files can be edited.",
        "path_is_directory" => "Target path points to a directory, not a file.",
        "file_too_large" => "File exceeds allowed size for this operation.",
        "file_not_found" => "Target file does not exist.",
        "missing_content" => "Required content argument is missing for selected mode.",
        "missing_find" => "Required find argument is missing or empty.",
        "no_match" => "Find text was not found in current file content.",
        "replacement_mismatch" => "Actual replacements do not match expected count.",
        "invalid_line_ending" => "Requested line ending mode is invalid.",
        "invalid_encoding" => "Requested encoding is not supported.",
        "encoding_error" => "Content cannot be encoded with requested encoding.",
        "parent_missing" => "Parent directory does not exist.",
        "permission_denied" => "Operation was denied by filesystem permissions.",
        "not_found" => "Target path was not found.",
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
        "name": "edit_file",
        "title": "Edit file",
        "description": "Edit one text file using replace, append, prepend, or exact find-replace. Prefer find_replace with expected_replacements for surgical edits; use replace only when rewriting the whole file is intended.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to edit. Relative paths resolve against a configured workspace, then the active workspace." },
                "mode": {
                    "type": "string",
                    "enum": ["replace", "append", "prepend", "find_replace"],
                    "description": "Required: use find_replace for a surgical edit, or explicitly use replace to rewrite the whole file.",
                },
                "content": { "type": "string", "description": "New content for replace, append, or prepend modes. Not used by find_replace." },
                "find": { "type": "string", "description": "Exact text to find in find_replace mode. Required and must be non-empty for find_replace." },
                "replace": { "type": "string", "description": "Replacement text for find_replace mode. Required for find_replace; may be an empty string to delete matches." },
                "replace_all": { "type": "boolean", "description": "In find_replace mode, replace every match when true; replace only the first match when false or omitted." },
                "expected_replacements": { "type": "integer", "description": "Optional safety check for find_replace. The tool fails unless the actual replacement count equals this value. Useful with replace_all to avoid accidental broad edits." },
                "expected_hash": { "type": "string", "description": "Optional SHA-256 precondition. The edit is rejected if the current file does not match this 64-character hex digest." },
                "create_if_missing": { "type": "boolean", "description": "Allow creating the file when it does not exist. Defaults to false." },
                "create_parents": { "type": "boolean", "description": "Create missing parent directories when writing. Defaults to true." },
                "target_encoding": { "type": "string", "enum": ["UTF-8", "UTF-16LE", "UTF-16BE", "Windows-1252"], "description": "Output encoding. Defaults to the detected existing encoding, or UTF-8 for new files. UTF-16 output includes a BOM." },
                "target_line_ending": {
                    "type": "string",
                    "enum": ["preserve", "lf", "crlf"],
                }
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

fn normalize_line_endings(
    content: &str,
    target_line_ending: &str,
) -> std::result::Result<String, &'static str> {
    match target_line_ending {
        "preserve" => Ok(content.to_string()),
        "lf" => Ok(content.replace("\r\n", "\n")),
        "crlf" => {
            let normalized = content.replace("\r\n", "\n").replace('\n', "\r\n");
            Ok(normalized)
        }
        _ => Err("target_line_ending must be preserve, lf, or crlf"),
    }
}

fn detect_line_endings(content: &str) -> ExistingLineEndings {
    let bytes = content.as_bytes();
    let mut crlf = 0usize;
    let mut lf = 0usize;

    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        if index > 0 && bytes[index - 1] == b'\r' {
            crlf += 1;
        } else {
            lf += 1;
        }
    }

    match (crlf > 0, lf > 0) {
        (false, false) => ExistingLineEndings::None,
        (false, true) => ExistingLineEndings::Lf,
        (true, false) => ExistingLineEndings::Crlf,
        (true, true) => ExistingLineEndings::Mixed,
    }
}

fn normalize_fragment_to_existing(fragment: &str, existing: ExistingLineEndings) -> (String, bool) {
    let normalized = match existing {
        ExistingLineEndings::Lf => fragment.replace("\r\n", "\n"),
        ExistingLineEndings::Crlf => fragment.replace("\r\n", "\n").replace('\n', "\r\n"),
        ExistingLineEndings::None | ExistingLineEndings::Mixed => fragment.to_string(),
    };
    let changed = normalized != fragment;
    (normalized, changed)
}

fn line_ending_metadata(content: &str) -> Option<String> {
    match detect_line_endings(content) {
        ExistingLineEndings::None => None,
        ExistingLineEndings::Lf => Some("lf".to_string()),
        ExistingLineEndings::Crlf => Some("crlf".to_string()),
        ExistingLineEndings::Mixed => Some("mixed".to_string()),
    }
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

    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if !["replace", "append", "prepend", "find_replace"].contains(&mode.as_str()) {
        return Ok(error_response(
            &path,
            &canonical_from_guard,
            "invalid_mode",
            "mode is required: use mode=find_replace for find/replace, mode=append or mode=prepend for fragments, or explicitly set mode=replace to rewrite the whole file",
        ));
    }

    let create_if_missing = args
        .get("create_if_missing")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let create_parents = args
        .get("create_parents")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let existed_before = path.exists();

    let (old_content, old_bytes, previous_encoding): (String, Vec<u8>, Option<String>) =
        if existed_before {
            if path.is_dir() {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "path_is_directory",
                    "target path points to a directory",
                ));
            }

            let meta = match fs::metadata(&path) {
                Ok(m) => m,
                Err(err) => {
                    return Ok(io_error_response(
                        &path,
                        &canonical_from_guard,
                        "read metadata",
                        &err,
                    ));
                }
            };

            if meta.len() > MAX_EDIT_FILE_BYTES {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "file_too_large",
                    format!(
                        "file is too large for edit_file ({} bytes > {} bytes)",
                        meta.len(),
                        MAX_EDIT_FILE_BYTES
                    ),
                ));
            }

            let bytes = match fs::read(&path) {
                Ok(b) => b,
                Err(err) => {
                    return Ok(io_error_response(
                        &path,
                        &canonical_from_guard,
                        "read file",
                        &err,
                    ));
                }
            };

            if is_probably_binary(&bytes) {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "binary_file",
                    "edit_file only accepts text files",
                ));
            }

            let (decoded, detected_encoding) = decode_fuzzy(&bytes);
            let detected_encoding = TextEncoding::parse(detected_encoding)
                .map(TextEncoding::canonical_name)
                .unwrap_or(detected_encoding)
                .to_string();
            (decoded, bytes, Some(detected_encoding))
        } else {
            if !create_if_missing {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "file_not_found",
                    "file does not exist (set create_if_missing=true to create it)",
                ));
            }

            (String::new(), Vec::new(), None)
        };
    let sha256_before = existed_before.then(|| super::file_hash::sha256_bytes(&old_bytes));
    if let Some(expected_hash) = expected_hash.as_deref()
        && sha256_before.as_deref() != Some(expected_hash)
    {
        return Ok(hash_mismatch_response(
            &path,
            &canonical_from_guard,
            expected_hash,
            sha256_before.as_deref(),
        ));
    }

    let has_content = args.get("content").is_some();
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let target_line_ending = args
        .get("target_line_ending")
        .and_then(|v| v.as_str())
        .unwrap_or("preserve")
        .to_ascii_lowercase();
    let existing_line_endings = detect_line_endings(&old_content);
    let mixed_line_endings = existing_line_endings == ExistingLineEndings::Mixed;
    let mut line_endings_normalized = false;

    let mut replacements_applied: usize = 0;

    let mut new_content = match mode.as_str() {
        "replace" => {
            if !has_content {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "missing_content",
                    "content is required when mode=replace",
                ));
            }
            content.to_string()
        }
        "append" => {
            if !has_content {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "missing_content",
                    "content is required when mode=append",
                ));
            }
            let appended = if target_line_ending == "preserve" {
                let (normalized, changed) =
                    normalize_fragment_to_existing(content, existing_line_endings);
                line_endings_normalized |= changed;
                normalized
            } else {
                content.to_string()
            };
            format!("{}{}", old_content, appended)
        }
        "prepend" => {
            if !has_content {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "missing_content",
                    "content is required when mode=prepend",
                ));
            }
            let prepended = if target_line_ending == "preserve" {
                let (normalized, changed) =
                    normalize_fragment_to_existing(content, existing_line_endings);
                line_endings_normalized |= changed;
                normalized
            } else {
                content.to_string()
            };
            format!("{}{}", prepended, old_content)
        }
        "find_replace" => {
            let raw_find = args.get("find").and_then(|v| v.as_str()).unwrap_or("");
            if raw_find.is_empty() {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "missing_find",
                    "find is required and cannot be empty when mode=find_replace",
                ));
            }
            let raw_replace = args.get("replace").and_then(|v| v.as_str()).unwrap_or("");
            let (find, find_normalized) =
                normalize_fragment_to_existing(raw_find, existing_line_endings);
            let (replace, replace_normalized) =
                normalize_fragment_to_existing(raw_replace, existing_line_endings);
            line_endings_normalized |= find_normalized || replace_normalized;
            let replace_all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if replace_all {
                replacements_applied = old_content.matches(&find).count();
                if replacements_applied == 0 {
                    return Ok(error_response(
                        &path,
                        &canonical_from_guard,
                        "no_match",
                        "find text was not found in the file",
                    ));
                }
                old_content.replace(&find, &replace)
            } else {
                if !old_content.contains(&find) {
                    return Ok(error_response(
                        &path,
                        &canonical_from_guard,
                        "no_match",
                        "find text was not found in the file",
                    ));
                }
                replacements_applied = 1;
                old_content.replacen(&find, &replace, 1)
            }
        }
        _ => old_content.clone(),
    };

    if let Some(expected) = args
        .get("expected_replacements")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        && expected != replacements_applied
    {
        return Ok(json!({
            "success": false,
            "path": crate::common::normalize_display_path(&path),
            "canonical_path": crate::common::normalize_display_path(&canonical_from_guard),
            "error_code": "replacement_mismatch",
            "reason": error_reason("replacement_mismatch"),
            "message": format!(
                "expected {} replacements but got {}",
                expected,
                replacements_applied
            ),
            "expected_replacements": expected,
            "actual_replacements": replacements_applied
        }));
    }

    let normalized_content = match normalize_line_endings(&new_content, &target_line_ending) {
        Ok(v) => v,
        Err(msg) => {
            return Ok(error_response(
                &path,
                &canonical_from_guard,
                "invalid_line_ending",
                msg,
            ));
        }
    };
    line_endings_normalized |= normalized_content != new_content;
    new_content = normalized_content;

    let target_encoding = match args.get("target_encoding").and_then(Value::as_str) {
        Some(raw) => match TextEncoding::parse(raw) {
            Ok(encoding) => encoding,
            Err(message) => {
                return Ok(error_response(
                    &path,
                    &canonical_from_guard,
                    "invalid_encoding",
                    message,
                ));
            }
        },
        None => match previous_encoding.as_deref() {
            Some(previous) => TextEncoding::parse(previous).unwrap_or(TextEncoding::Utf8),
            None => TextEncoding::Utf8,
        },
    };
    let target_encoding_name = target_encoding.canonical_name().to_string();
    let encoding_changed = previous_encoding
        .as_deref()
        .is_some_and(|previous| previous != target_encoding_name);

    let final_bytes = match target_encoding.encode(&new_content) {
        Ok(bytes) => bytes,
        Err(msg) => {
            return Ok(error_response(
                &path,
                &canonical_from_guard,
                "encoding_error",
                msg,
            ));
        }
    };

    let changed = !existed_before || old_bytes != final_bytes;
    let sha256_after = super::file_hash::sha256_bytes(&final_bytes);
    if !changed {
        let canonical_after = std::fs::canonicalize(&path).unwrap_or(canonical_from_guard);
        let mut response = json!({
            "success": true,
            "path": crate::common::normalize_display_path(&path),
            "canonical_path": crate::common::normalize_display_path(&canonical_after),
            "mode": mode,
            "file_existed_before": true,
            "file_created": false,
            "changed": false,
            "replacements": replacements_applied,
            "bytes_before": old_bytes.len(),
            "bytes_written": old_bytes.len(),
            "previous_encoding": previous_encoding,
            "target_encoding": target_encoding_name,
            "encoding_changed": encoding_changed,
            "line_ending": target_line_ending,
            "line_endings_normalized": line_endings_normalized,
            "mixed_line_endings": mixed_line_endings,
            "sha256_before": sha256_before,
            "sha256_after": sha256_after,
            "message": "no content changes detected"
        });
        attach_history_metadata(&mut response, &no_history("no filesystem change"));
        return Ok(response);
    }

    if let Some(expected_hash) = expected_hash.as_deref() {
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
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        if !create_parents {
            return Ok(error_response(
                &path,
                &canonical_from_guard,
                "parent_missing",
                "parent directory does not exist (set create_parents=true)",
            ));
        }

        if let Err(err) = fs::create_dir_all(parent) {
            return Ok(io_error_response(
                &path,
                &canonical_from_guard,
                "create parent directories",
                &err,
            ));
        }
    }

    if let Err(err) = crate::tools::atomic_write::write_bytes(&path, &final_bytes, existed_before) {
        return Ok(io_error_response(
            &path,
            &canonical_from_guard,
            "write file atomically",
            &err,
        ));
    }

    let canonical_after = std::fs::canonicalize(&path).unwrap_or(canonical_from_guard);
    let before_snapshot = if existed_before {
        file_snapshot(
            old_bytes.clone(),
            previous_encoding.clone(),
            line_ending_metadata(&old_content),
        )
    } else {
        missing_snapshot()
    };
    let after_snapshot = file_snapshot(
        final_bytes.clone(),
        Some(target_encoding_name.clone()),
        line_ending_metadata(&new_content),
    );
    let history_outcome = record_change(
        "edit_file",
        &path,
        before_snapshot,
        after_snapshot,
        if existed_before {
            "edit file"
        } else {
            "create file via edit_file"
        },
    );

    let mut response = json!({
        "success": true,
        "path": crate::common::normalize_display_path(&path),
        "canonical_path": crate::common::normalize_display_path(&canonical_after),
        "mode": mode,
        "file_existed_before": existed_before,
        "file_created": !existed_before,
        "changed": true,
        "replacements": replacements_applied,
        "bytes_before": old_bytes.len(),
        "bytes_written": final_bytes.len(),
        "previous_encoding": previous_encoding,
        "target_encoding": target_encoding_name,
        "encoding_changed": encoding_changed,
        "line_ending": target_line_ending,
        "line_endings_normalized": line_endings_normalized,
        "mixed_line_endings": mixed_line_endings,
        "sha256_before": sha256_before,
        "sha256_after": sha256_after,
        "message": if existed_before { "file updated" } else { "file created and updated" }
    });
    attach_history_metadata(&mut response, &history_outcome);
    Ok(response)
}

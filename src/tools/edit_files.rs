use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::history::{
    PathSnapshot, SnapshotState, capture_snapshot, file_snapshot, no_history, record_change,
    snapshots_match_contents,
};
use crate::limits::MAX_IN_MEMORY_TEXT_FILE_BYTES;
use crate::security::path_guard::GUARD;
use crate::tools::ast_support::source_has_parse_error;
use crate::tools::read_file::{decode_fuzzy, is_probably_binary};
use crate::tools::text_encoding::TextEncoding;

const MAX_TRANSACTION_FILES: usize = 20;
const MAX_TRANSACTION_EDITS: usize = 100;
const MAX_TRANSACTION_FILE_BYTES: usize = MAX_IN_MEMORY_TEXT_FILE_BYTES as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineEndings {
    None,
    Lf,
    Crlf,
    Mixed,
}

struct PreparedFile {
    path: PathBuf,
    canonical_path: PathBuf,
    before: PathSnapshot,
    after: PathSnapshot,
    previous_encoding: String,
    target_encoding: String,
    encoding_changed: bool,
    replacements: usize,
    changed: bool,
    validation: &'static str,
    sha256_before: String,
    sha256_after: String,
}

pub fn schema() -> Value {
    json!({
        "name": "edit_files",
        "title": "Edit multiple files",
        "description": "Apply exact find-replace edits to up to 20 existing text files. Preflights every path, replacement count, and supported syntax before writing; writes each file atomically and rolls back earlier files if a later write fails. This is rollback-backed, not a filesystem-wide atomic transaction.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "description": "Existing text files and their ordered edits. Paths must be unique.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "expected_hash": { "type": "string", "description": "Optional SHA-256 precondition for this file. The transaction is rejected during preflight if the current file does not match." },
                            "edits": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "find": { "type": "string" },
                                        "replace": { "type": "string" },
                                        "replace_all": { "type": "boolean" },
                                        "expected_replacements": { "type": "integer", "minimum": 0 }
                                    },
                                    "required": ["find", "replace", "expected_replacements"]
                                }
                            },
                            "target_encoding": { "type": "string", "enum": ["UTF-8", "UTF-16LE", "UTF-16BE", "Windows-1252"] },
                            "target_line_ending": { "type": "string", "enum": ["preserve", "lf", "crlf"] }
                        },
                        "required": ["path", "edits"]
                    }
                },
                "validate_syntax": { "type": "boolean", "description": "Reject newly introduced JSON or Tree-sitter parse errors before writing. Defaults to true." }
            },
            "required": ["files"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let files = match args.get("files").and_then(Value::as_array) {
        Some(files) if !files.is_empty() => files,
        _ => {
            return Ok(error_response(
                "invalid_files",
                "files must be a non-empty array",
            ));
        }
    };
    if files.len() > MAX_TRANSACTION_FILES {
        return Ok(error_response(
            "too_many_files",
            format!("edit_files accepts at most {MAX_TRANSACTION_FILES} files"),
        ));
    }
    let validate_syntax = args
        .get("validate_syntax")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut prepared = Vec::with_capacity(files.len());
    let mut seen_paths = HashSet::new();
    let mut total_edits = 0usize;

    for (file_index, file) in files.iter().enumerate() {
        match prepare_file(
            file,
            file_index,
            validate_syntax,
            &mut total_edits,
            &mut seen_paths,
        ) {
            Ok(prepared_file) => prepared.push(prepared_file),
            Err(error) => return Ok(error),
        }
    }

    let mut writer = super::atomic_write::write_bytes;
    if let Err(error) = commit_prepared_with_writer(&prepared, &mut writer) {
        return Ok(error);
    }

    let mut file_results = Vec::with_capacity(prepared.len());
    let mut files_changed = 0usize;
    for file in prepared {
        let history = if file.changed {
            files_changed += 1;
            record_change(
                "edit_files",
                &file.path,
                file.before,
                file.after,
                format!("apply {} exact edits", file.replacements),
            )
        } else {
            no_history("no filesystem change")
        };
        file_results.push(json!({
            "path": crate::common::normalize_display_path(&file.path),
            "canonical_path": crate::common::normalize_display_path(&file.canonical_path),
            "changed": file.changed,
            "replacements": file.replacements,
            "previous_encoding": file.previous_encoding,
            "target_encoding": file.target_encoding,
            "encoding_changed": file.encoding_changed,
            "syntax_validation": file.validation,
            "sha256_before": file.sha256_before,
            "sha256_after": file.sha256_after,
            "history_recorded": history.recorded,
            "history_entry_id": history.entry_id,
            "history_reason": history.reason
        }));
    }

    Ok(json!({
        "success": true,
        "files": file_results,
        "files_total": file_results.len(),
        "files_changed": files_changed,
        "edits_total": total_edits,
        "rollback_performed": false,
        "atomicity": "preflight plus per-file atomic writes with rollback"
    }))
}

fn commit_prepared_with_writer<F>(
    prepared: &[PreparedFile],
    writer: &mut F,
) -> std::result::Result<Vec<usize>, Value>
where
    F: FnMut(&Path, &[u8], bool) -> std::io::Result<()>,
{
    let mut written = Vec::new();
    for (index, file) in prepared.iter().enumerate() {
        if !file.changed {
            continue;
        }
        let current_canonical = GUARD.check_path(&file.path);
        if current_canonical != file.canonical_path {
            let rollback_performed = !written.is_empty();
            let rollback_errors = rollback_files_with_writer(prepared, &written, writer);
            return Err(commit_error(
                "path_changed",
                "file now resolves to a different canonical path",
                index,
                rollback_performed,
                rollback_errors,
            ));
        }
        let current = match capture_snapshot(&file.path) {
            Ok(snapshot) => snapshot,
            Err(message) => {
                let rollback_performed = !written.is_empty();
                let rollback_errors = rollback_files_with_writer(prepared, &written, writer);
                return Err(commit_error(
                    "snapshot_failed",
                    message,
                    index,
                    rollback_performed,
                    rollback_errors,
                ));
            }
        };
        if !snapshots_match_contents(&current, &file.before) {
            let rollback_performed = !written.is_empty();
            let rollback_errors = rollback_files_with_writer(prepared, &written, writer);
            return Err(commit_error(
                "concurrent_change",
                "file changed after preflight; transaction was not applied",
                index,
                rollback_performed,
                rollback_errors,
            ));
        }
        let bytes = file.after.bytes.as_deref().unwrap_or_default();
        if let Err(error) = writer(&file.path, bytes, true) {
            let rollback_performed = !written.is_empty();
            let rollback_errors = rollback_files_with_writer(prepared, &written, writer);
            return Err(commit_error(
                "write_failed",
                format!("atomic write failed: {error}"),
                index,
                rollback_performed,
                rollback_errors,
            ));
        }
        written.push(index);
    }
    Ok(written)
}

fn prepare_file(
    file: &Value,
    file_index: usize,
    validate_syntax: bool,
    total_edits: &mut usize,
    seen_paths: &mut HashSet<PathBuf>,
) -> std::result::Result<PreparedFile, Value> {
    let path_str = file
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if path_str.is_empty() {
        return Err(file_error("invalid_path", "path is required", file_index));
    }
    let path = crate::common::resolve_write_tool_path(path_str);
    let canonical_path = GUARD.check_path(&path);
    if !path.is_file() {
        return Err(file_error(
            "file_not_found",
            "edit_files only accepts existing files",
            file_index,
        ));
    }
    let canonical_key = std::fs::canonicalize(&path).unwrap_or_else(|_| canonical_path.clone());
    if !seen_paths.insert(canonical_key) {
        return Err(file_error(
            "duplicate_path",
            "each transaction path must be unique",
            file_index,
        ));
    }
    let before = capture_snapshot(&path)
        .map_err(|message| file_error("snapshot_failed", message, file_index))?;
    let before_bytes = match (&before.state, before.bytes.as_deref()) {
        (SnapshotState::File, Some(bytes)) => bytes,
        _ => {
            return Err(file_error(
                "invalid_snapshot",
                "expected a file snapshot",
                file_index,
            ));
        }
    };
    let sha256_before = super::file_hash::sha256_bytes(before_bytes);
    if let Some(raw_expected_hash) = file.get("expected_hash").and_then(Value::as_str) {
        let expected_hash = super::file_hash::normalize_expected_hash(raw_expected_hash)
            .map_err(|message| file_error("invalid_hash", message, file_index))?;
        if sha256_before != expected_hash {
            let mut error = file_error(
                "hash_mismatch",
                "file contents changed since the expected SHA-256 digest was captured",
                file_index,
            );
            error["expected_hash"] = json!(expected_hash);
            error["actual_hash"] = json!(sha256_before);
            return Err(error);
        }
    }
    if is_probably_binary(before_bytes) {
        return Err(file_error(
            "binary_file",
            "edit_files only accepts text files",
            file_index,
        ));
    }
    let (mut content, detected_encoding) = decode_fuzzy(before_bytes);
    let previous_encoding = TextEncoding::parse(detected_encoding)
        .map(TextEncoding::canonical_name)
        .unwrap_or(detected_encoding)
        .to_string();
    let existing_line_endings = detect_line_endings(&content);
    let edits = file
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| file_error("invalid_edits", "edits must be an array", file_index))?;
    if edits.is_empty() {
        return Err(file_error(
            "invalid_edits",
            "each file must contain at least one edit",
            file_index,
        ));
    }
    *total_edits = total_edits.saturating_add(edits.len());
    if *total_edits > MAX_TRANSACTION_EDITS {
        return Err(error_response(
            "too_many_edits",
            format!("edit_files accepts at most {MAX_TRANSACTION_EDITS} edits"),
        ));
    }

    let mut replacements = 0usize;
    for (edit_index, edit) in edits.iter().enumerate() {
        let find = edit.get("find").and_then(Value::as_str).unwrap_or("");
        if find.is_empty() {
            return Err(edit_error(
                "invalid_find",
                "find must not be empty",
                file_index,
                edit_index,
            ));
        }
        let replace = edit.get("replace").and_then(Value::as_str).unwrap_or("");
        let replace_all = edit
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let expected = edit
            .get("expected_replacements")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                edit_error(
                    "missing_expected_replacements",
                    "expected_replacements is required",
                    file_index,
                    edit_index,
                )
            })?;
        let find = normalize_fragment(find, existing_line_endings);
        let replace = normalize_fragment(replace, existing_line_endings);
        let matches = content.match_indices(&find).count();
        let actual = if replace_all {
            matches
        } else {
            usize::from(matches > 0)
        };
        if actual != expected {
            return Err(json!({
                "success": false,
                "error_code": "replacement_mismatch",
                "message": format!("expected {expected} replacements but got {actual}"),
                "file_index": file_index,
                "edit_index": edit_index,
                "expected_replacements": expected,
                "actual_replacements": actual
            }));
        }
        content = if replace_all {
            content.replace(&find, &replace)
        } else {
            content.replacen(&find, &replace, 1)
        };
        replacements += actual;
    }

    let target_line_ending = file
        .get("target_line_ending")
        .and_then(Value::as_str)
        .unwrap_or("preserve")
        .to_ascii_lowercase();
    content = match target_line_ending.as_str() {
        "preserve" => content,
        "lf" => content.replace("\r\n", "\n"),
        "crlf" => content.replace("\r\n", "\n").replace('\n', "\r\n"),
        _ => {
            return Err(file_error(
                "invalid_line_ending",
                "target_line_ending must be preserve, lf, or crlf",
                file_index,
            ));
        }
    };
    let target_encoding = match file.get("target_encoding").and_then(Value::as_str) {
        Some(raw) => TextEncoding::parse(raw)
            .map_err(|message| file_error("invalid_encoding", message, file_index))?,
        None => TextEncoding::parse(&previous_encoding).unwrap_or(TextEncoding::Utf8),
    };
    let final_bytes = target_encoding
        .encode(&content)
        .map_err(|message| file_error("encoding_error", message, file_index))?;
    if final_bytes.len() > MAX_TRANSACTION_FILE_BYTES {
        return Err(file_error(
            "file_too_large",
            format!(
                "edited file exceeds {} byte transaction limit",
                MAX_TRANSACTION_FILE_BYTES
            ),
            file_index,
        ));
    }
    let validation = if validate_syntax {
        validate_new_syntax(&path, &decode_fuzzy(before_bytes).0, &content)
            .map_err(|message| file_error("syntax_validation_failed", message, file_index))?
    } else {
        "disabled"
    };
    let target_encoding_name = target_encoding.canonical_name().to_string();
    let sha256_after = super::file_hash::sha256_bytes(&final_bytes);
    let after = file_snapshot(
        final_bytes,
        Some(target_encoding_name.clone()),
        line_ending_metadata(&content),
    );
    let changed = !snapshots_match_contents(&before, &after);

    Ok(PreparedFile {
        path,
        canonical_path,
        before,
        after,
        previous_encoding: previous_encoding.clone(),
        target_encoding: target_encoding_name.clone(),
        encoding_changed: previous_encoding != target_encoding_name,
        replacements,
        changed,
        validation,
        sha256_before,
        sha256_after,
    })
}

fn validate_new_syntax(
    path: &Path,
    before: &str,
    after: &str,
) -> std::result::Result<&'static str, String> {
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        let before_error = serde_json::from_str::<Value>(before).is_err();
        let after_error = serde_json::from_str::<Value>(after).is_err();
        if !before_error && after_error {
            return Err("edit introduces invalid JSON".to_string());
        }
        return Ok(if after_error {
            "preexisting_errors"
        } else {
            "passed"
        });
    }
    let before_error =
        source_has_parse_error(path, before.as_bytes()).map_err(|error| error.to_string())?;
    let after_error =
        source_has_parse_error(path, after.as_bytes()).map_err(|error| error.to_string())?;
    match (before_error, after_error) {
        (Some(false), Some(true)) => Err("edit introduces Tree-sitter parse errors".to_string()),
        (_, Some(true)) => Ok("preexisting_errors"),
        (_, Some(false)) => Ok("passed"),
        _ => Ok("skipped"),
    }
}

fn rollback_files_with_writer<F>(
    prepared: &[PreparedFile],
    written: &[usize],
    writer: &mut F,
) -> Vec<String>
where
    F: FnMut(&Path, &[u8], bool) -> std::io::Result<()>,
{
    let mut errors = Vec::new();
    for index in written.iter().rev().copied() {
        let file = &prepared[index];
        let current_canonical = GUARD.check_path(&file.path);
        if current_canonical != file.canonical_path {
            errors.push(format!(
                "{}: path resolves to a different canonical location",
                crate::common::normalize_display_path(&file.path)
            ));
            continue;
        }
        let bytes = file.before.bytes.as_deref().unwrap_or_default();
        if let Err(error) = writer(&file.path, bytes, true) {
            errors.push(format!(
                "{}: {}",
                crate::common::normalize_display_path(&file.path),
                error
            ));
        }
    }
    errors
}

fn commit_error(
    error_code: &str,
    message: impl Into<String>,
    file_index: usize,
    rollback_performed: bool,
    rollback_errors: Vec<String>,
) -> Value {
    json!({
        "success": false,
        "error_code": error_code,
        "message": message.into(),
        "file_index": file_index,
        "rollback_performed": rollback_performed,
        "rollback_complete": rollback_errors.is_empty(),
        "rollback_errors": rollback_errors
    })
}

fn detect_line_endings(content: &str) -> LineEndings {
    let bytes = content.as_bytes();
    let mut crlf = false;
    let mut lf = false;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if index > 0 && bytes[index - 1] == b'\r' {
                crlf = true;
            } else {
                lf = true;
            }
        }
    }
    match (crlf, lf) {
        (false, false) => LineEndings::None,
        (false, true) => LineEndings::Lf,
        (true, false) => LineEndings::Crlf,
        (true, true) => LineEndings::Mixed,
    }
}

fn normalize_fragment(fragment: &str, existing: LineEndings) -> String {
    match existing {
        LineEndings::Lf => fragment.replace("\r\n", "\n"),
        LineEndings::Crlf => fragment.replace("\r\n", "\n").replace('\n', "\r\n"),
        LineEndings::None | LineEndings::Mixed => fragment.to_string(),
    }
}

fn line_ending_metadata(content: &str) -> Option<String> {
    match detect_line_endings(content) {
        LineEndings::None => None,
        LineEndings::Lf => Some("lf".to_string()),
        LineEndings::Crlf => Some("crlf".to_string()),
        LineEndings::Mixed => Some("mixed".to_string()),
    }
}

fn error_response(error_code: &str, message: impl Into<String>) -> Value {
    json!({
        "success": false,
        "error_code": error_code,
        "message": message.into(),
        "rollback_performed": false
    })
}

fn file_error(error_code: &str, message: impl Into<String>, file_index: usize) -> Value {
    let mut response = error_response(error_code, message);
    response["file_index"] = json!(file_index);
    response
}

fn edit_error(
    error_code: &str,
    message: impl Into<String>,
    file_index: usize,
    edit_index: usize,
) -> Value {
    let mut response = file_error(error_code, message, file_index);
    response["edit_index"] = json!(edit_index);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io;
    use tempfile::tempdir;

    #[test]
    fn commit_failure_rolls_back_already_written_files() {
        let dir = tempdir().unwrap();
        crate::indexer::ensure_workspace_index(dir.path().to_path_buf(), "edit_files_test".into());
        crate::workspace_control::register_configured_workspace(
            dir.path().to_path_buf(),
            "edit_files_test",
            true,
        );
        let first_path = dir.path().join("first.txt");
        let second_path = dir.path().join("second.txt");
        std::fs::write(&first_path, "alpha").unwrap();
        std::fs::write(&second_path, "beta").unwrap();

        let files = json!([
            {
                "path": first_path.to_str().unwrap(),
                "edits": [{"find": "alpha", "replace": "changed-alpha", "expected_replacements": 1}]
            },
            {
                "path": second_path.to_str().unwrap(),
                "edits": [{"find": "beta", "replace": "changed-beta", "expected_replacements": 1}]
            }
        ]);
        let mut prepared = Vec::new();
        let mut total_edits = 0;
        let mut seen_paths = HashSet::new();
        for (index, file) in files.as_array().unwrap().iter().enumerate() {
            prepared
                .push(prepare_file(file, index, true, &mut total_edits, &mut seen_paths).unwrap());
        }

        let mut write_count = 0;
        let mut writer = |path: &Path, bytes: &[u8], replace_existing: bool| {
            write_count += 1;
            if write_count == 2 {
                return Err(io::Error::other("injected write failure"));
            }
            super::super::atomic_write::write_bytes(path, bytes, replace_existing)
        };
        let error = commit_prepared_with_writer(&prepared, &mut writer).unwrap_err();

        assert_eq!(
            error.get("error_code").and_then(Value::as_str),
            Some("write_failed")
        );
        assert_eq!(
            error.get("rollback_performed").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            error.get("rollback_complete").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(std::fs::read_to_string(&first_path).unwrap(), "alpha");
        assert_eq!(std::fs::read_to_string(&second_path).unwrap(), "beta");
    }
}

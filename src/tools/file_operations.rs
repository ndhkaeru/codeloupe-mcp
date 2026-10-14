use super::file_transaction::{PathChange, commit};
use super::read_file::{decode_fuzzy, is_probably_binary};
use super::text_encoding::TextEncoding;
use super::tool_errors::error;
use crate::history::{
    MAX_TRACKED_SNAPSHOT_BYTES, PathSnapshot, SnapshotState, capture_snapshot, directory_snapshot,
    file_snapshot, missing_snapshot, record_change_set, snapshot_hash, snapshots_match_contents,
};
use crate::security::path_guard::GUARD;
use anyhow::Result;
use serde_json::{Value, json};

const MAX_PATHS: usize = 20;
const MAX_STEPS: usize = 100;

pub(crate) fn operations_schema() -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": 100,
        "items": {
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": [
                        "edit",
                        "create",
                        "delete",
                        "copy",
                        "move",
                        "transfer",
                        "mkdir"
                    ]
                },
                "path": {
                    "type": "string"
                },
                "source": {
                    "type": "string"
                },
                "destination": {
                    "type": "string"
                },
                "content": {
                    "type": "string"
                },
                "expected_hash": {
                    "type": "string"
                },
                "destination_expected_hash": {
                    "type": "string"
                },
                "overwrite": {
                    "type": "boolean"
                },
                "allow_existing": {
                    "type": "boolean"
                },
                "action": {
                    "type": "string",
                    "enum": [
                        "copy",
                        "move"
                    ]
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1
                },
                "end_line": {
                    "type": "integer",
                    "minimum": 1
                },
                "insert_before_line": {
                    "type": "integer",
                    "minimum": 1
                },
                "target_encoding": {
                    "type": "string",
                    "enum": [
                        "UTF-8",
                        "UTF-16LE",
                        "UTF-16BE",
                        "Windows-1252"
                    ]
                },
                "target_line_ending": {
                    "type": "string",
                    "enum": [
                        "preserve",
                        "lf",
                        "crlf"
                    ]
                },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "find": {
                                "type": "string"
                            },
                            "replace": {
                                "type": "string"
                            },
                            "replace_all": {
                                "type": "boolean"
                            },
                            "expected_replacements": {
                                "type": "integer",
                                "minimum": 0
                            }
                        },
                        "required": [
                            "find",
                            "replace",
                            "expected_replacements"
                        ]
                    }
                }
            },
            "required": [
                "op"
            ]
        }
    })
}

#[derive(Clone, Copy)]
enum SyntaxBaseline {
    /// Refers to an immutable on-disk before-snapshot, even after chained moves.
    Original(usize),
    Fresh,
}

struct VirtualState {
    changes: Vec<PathChange>,
    baselines: Vec<SyntaxBaseline>,
}

impl VirtualState {
    fn slot(&mut self, raw: &str) -> std::result::Result<usize, Value> {
        self.slot_kind(raw, false)
    }

    fn slot_kind(&mut self, raw: &str, allow_directory: bool) -> std::result::Result<usize, Value> {
        if raw.trim().is_empty() {
            return Err(error("invalid_path", "path must not be empty"));
        }
        let path = crate::common::resolve_write_tool_path(raw);
        let canonical = GUARD.check_path(&path);
        if let Some(index) = self.changes.iter().position(|c| c.canonical == canonical) {
            if self.changes[index].path != path {
                return Err(error(
                    "path_alias",
                    "operations must use one spelling for each canonical path",
                ));
            }
            if !allow_directory && self.changes[index].after.state == SnapshotState::Directory {
                return Err(error(
                    "not_a_regular_file",
                    "file operations cannot target a directory",
                ));
            }
            return Ok(index);
        }
        if self.changes.len() >= MAX_PATHS {
            return Err(error(
                "too_many_files",
                "operations support at most 20 unique paths",
            ));
        }
        match std::fs::symlink_metadata(&path) {
            Ok(m)
                if super::file_transaction::is_link(&m)
                    || (!m.is_file() && !(allow_directory && m.is_dir())) =>
            {
                return Err(error(
                    "not_a_regular_file",
                    "only regular files or mkdir directories are supported; symlinks and reparse points are excluded",
                ));
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(error("io_error", e.to_string()));
            }
            _ => {}
        }
        let parent_exists = path.parent().is_some_and(|parent| {
            let canonical = GUARD.check_path(parent);
            self.changes
                .iter()
                .find(|change| change.canonical == canonical)
                .map_or_else(
                    || parent.is_dir(),
                    |change| change.after.state == SnapshotState::Directory,
                )
        });
        if !parent_exists {
            return Err(error(
                "parent_missing",
                "parent directory must exist on disk or be declared by an earlier mkdir step",
            ));
        }
        let before = capture_snapshot(&path).map_err(|e| error("snapshot_failed", e))?;
        let index = self.changes.len();
        self.baselines
            .push(if before.state == SnapshotState::Missing {
                SyntaxBaseline::Fresh
            } else {
                SyntaxBaseline::Original(index)
            });
        self.changes.push(PathChange {
            path,
            canonical,
            after: before.clone(),
            before,
        });
        Ok(index)
    }

    fn require_file(&self, index: usize) -> std::result::Result<(), Value> {
        if self.changes[index].after.state == SnapshotState::File {
            Ok(())
        } else {
            Err(error(
                "file_not_found",
                "operation requires an existing virtual file",
            ))
        }
    }

    fn hash(
        &self,
        index: usize,
        raw: Option<&str>,
        required: bool,
    ) -> std::result::Result<(), Value> {
        let Some(raw) = raw else {
            return if required {
                Err(error(
                    "missing_expected_hash",
                    "range selection and overwrite require expected_hash",
                ))
            } else {
                Ok(())
            };
        };
        let expected =
            super::file_hash::normalize_expected_hash(raw).map_err(|e| error("invalid_hash", e))?;
        if snapshot_hash(&self.changes[index].after).as_deref() != Some(expected.as_str()) {
            return Err(error(
                "hash_mismatch",
                "expected_hash does not match the current virtual contents",
            ));
        }
        Ok(())
    }
}

fn arg<'a>(op: &'a Value, key: &str) -> std::result::Result<&'a str, Value> {
    op.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| error("missing_argument", format!("{key} is required")))
}

fn text(snapshot: &PathSnapshot) -> std::result::Result<(String, TextEncoding, bool), Value> {
    let bytes = snapshot
        .bytes
        .as_deref()
        .ok_or_else(|| error("file_not_found", "file has no text bytes"))?;
    if is_probably_binary(bytes) {
        return Err(error(
            "binary_file",
            "text operation rejects binary contents",
        ));
    }
    let (mut content, label) = decode_fuzzy(bytes);
    let encoding = TextEncoding::parse(label).map_err(|e| error("invalid_encoding", e))?;
    let bom = content.starts_with('\u{feff}');
    if bom {
        content.remove(0);
    }
    let encoded = encode(&content, encoding, bom)?;
    if encoded.bytes.as_deref() != Some(bytes) {
        return Err(error(
            "lossy_encoding",
            "contents cannot be decoded and re-encoded losslessly",
        ));
    }
    Ok((content, encoding, bom))
}

fn encode(
    content: &str,
    encoding: TextEncoding,
    bom: bool,
) -> std::result::Result<PathSnapshot, Value> {
    let mut bytes = encoding
        .encode(content)
        .map_err(|e| error("encoding_error", e))?;
    if bom && encoding == TextEncoding::Utf8 {
        bytes.splice(0..0, [0xef, 0xbb, 0xbf]);
    }
    if bytes.len() > MAX_TRACKED_SNAPSHOT_BYTES {
        return Err(error(
            "file_too_large",
            "virtual file exceeds 10 MiB snapshot limit",
        ));
    }
    Ok(file_snapshot(
        bytes,
        Some(encoding.canonical_name().into()),
        None,
    ))
}

fn line_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    if *offsets.last().unwrap() != content.len() {
        offsets.push(content.len());
    }
    offsets
}

fn number(op: &Value, field: &str) -> std::result::Result<usize, Value> {
    op.get(field)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            error(
                "invalid_range",
                format!("{field} must be a positive integer"),
            )
        })
}

fn transfer(
    state: &mut VirtualState,
    op: &Value,
    source: usize,
    destination: usize,
) -> std::result::Result<(), Value> {
    state.require_file(source)?;
    state.require_file(destination)?;
    state.hash(
        source,
        op.get("expected_hash").and_then(Value::as_str),
        true,
    )?;
    state.hash(
        destination,
        op.get("destination_expected_hash").and_then(Value::as_str),
        source != destination,
    )?;
    let action = arg(op, "action")?;
    if !matches!(action, "copy" | "move") {
        return Err(error("invalid_action", "action must be copy or move"));
    }
    let (src, src_encoding, src_bom) = text(&state.changes[source].after)?;
    let (mut dst, dst_encoding, dst_bom) = text(&state.changes[destination].after)?;
    let offsets = line_offsets(&src);
    let start = number(op, "start_line")?;
    let end = number(op, "end_line")?;
    if end < start || end >= offsets.len() {
        return Err(error("invalid_range", "source range is outside the file"));
    }
    let begin = offsets[start - 1];
    let finish = offsets[end];
    let target_offsets = line_offsets(&dst);
    let line = number(op, "insert_before_line")?;
    if line > target_offsets.len() {
        return Err(error(
            "invalid_range",
            "insertion is outside the destination; EOF is line_count+1",
        ));
    }
    let mut insert = target_offsets[line - 1];
    let mut fragment = src[begin..finish].to_string();
    let crlf = dst.contains("\r\n");
    let lf = dst
        .as_bytes()
        .iter()
        .enumerate()
        .any(|(i, b)| *b == b'\n' && (i == 0 || dst.as_bytes()[i - 1] != b'\r'));
    if source != destination {
        if crlf && !lf {
            fragment = fragment.replace("\r\n", "\n").replace('\n', "\r\n");
        } else if lf && !crlf {
            fragment = fragment.replace("\r\n", "\n");
        }
    }
    if insert > 0 && !dst[..insert].ends_with('\n') {
        return Err(error(
            "line_boundary",
            "cannot append lines after an unterminated destination line",
        ));
    }
    if insert < dst.len() && !fragment.ends_with('\n') {
        return Err(error(
            "line_boundary",
            "unterminated source line cannot be inserted before another line",
        ));
    }
    if source == destination && action == "move" {
        if insert > begin && insert < finish {
            return Err(error(
                "overlapping_range",
                "move insertion lies inside the selected source range",
            ));
        }
        dst.replace_range(begin..finish, "");
        if insert >= finish {
            insert -= finish - begin;
        }
        dst.insert_str(insert, &fragment);
        state.changes[source].after = encode(&dst, src_encoding, src_bom)?;
    } else {
        dst.insert_str(insert, &fragment);
        state.changes[destination].after = encode(&dst, dst_encoding, dst_bom)?;
        if action == "move" {
            let mut remaining = src;
            remaining.replace_range(begin..finish, "");
            state.changes[source].after = encode(&remaining, src_encoding, src_bom)?;
        }
    }
    Ok(())
}

fn simulate(
    state: &mut VirtualState,
    op: &Value,
    step: usize,
    total_edits: &mut usize,
) -> std::result::Result<(), Value> {
    let kind = arg(op, "op")?;
    let allowed: &[&str] = match kind {
        "edit" => &[
            "op",
            "path",
            "expected_hash",
            "edits",
            "target_encoding",
            "target_line_ending",
        ],
        "create" => &[
            "op",
            "path",
            "expected_hash",
            "content",
            "overwrite",
            "target_encoding",
            "target_line_ending",
        ],
        "delete" => &["op", "path", "expected_hash"],
        "mkdir" => &["op", "path", "allow_existing"],
        "copy" | "move" => &["op", "source", "destination", "expected_hash"],
        "transfer" => &[
            "op",
            "source",
            "destination",
            "expected_hash",
            "destination_expected_hash",
            "action",
            "start_line",
            "end_line",
            "insert_before_line",
        ],
        _ => return Err(error("invalid_operation", "unknown operation")),
    };
    if let Some(key) = op
        .as_object()
        .and_then(|fields| fields.keys().find(|key| !allowed.contains(&key.as_str())))
    {
        return Err(error(
            "invalid_operation_argument",
            format!("{key} is not supported for {kind}"),
        ));
    }
    match kind {
        "mkdir" => {
            let index = state.slot_kind(arg(op, "path")?, true)?;
            match state.changes[index].after.state {
                SnapshotState::Missing => {
                    state.changes[index].after = directory_snapshot();
                }
                SnapshotState::Directory => {
                    if op.get("allow_existing").and_then(Value::as_bool) == Some(false) {
                        return Err(error("already_exists", "mkdir directory already exists"));
                    }
                }
                _ => {
                    return Err(error(
                        "not_a_directory",
                        "mkdir path is an existing virtual file",
                    ));
                }
            }
        }
        "edit" | "create" | "delete" => {
            let index = state.slot(arg(op, "path")?)?;
            state.hash(
                index,
                op.get("expected_hash").and_then(Value::as_str),
                false,
            )?;
            match kind {
                "edit" => {
                    state.require_file(index)?;
                    let change = &state.changes[index];
                    let prepared = super::edit_files::prepare_snapshot(
                        op,
                        step,
                        false,
                        total_edits,
                        change.path.clone(),
                        change.canonical.clone(),
                        change.after.clone(),
                    )?;
                    state.changes[index].after = prepared.after;
                }
                "create" => {
                    if state.changes[index].after.state == SnapshotState::Missing {
                        state.baselines[index] = SyntaxBaseline::Fresh;
                    }
                    if state.changes[index].after.state != SnapshotState::Missing {
                        if op.get("overwrite").and_then(Value::as_bool) != Some(true) {
                            return Err(error(
                                "already_exists",
                                "create destination exists in virtual state",
                            ));
                        }
                        state.hash(index, op.get("expected_hash").and_then(Value::as_str), true)?;
                    }
                    let content = arg(op, "content")?;
                    let encoding = TextEncoding::parse(
                        op.get("target_encoding")
                            .and_then(Value::as_str)
                            .unwrap_or("UTF-8"),
                    )
                    .map_err(|e| error("invalid_encoding", e))?;
                    let content = match op
                        .get("target_line_ending")
                        .and_then(Value::as_str)
                        .unwrap_or("preserve")
                    {
                        "preserve" => content.to_string(),
                        "lf" => content.replace("\r\n", "\n"),
                        "crlf" => content.replace("\r\n", "\n").replace('\n', "\r\n"),
                        _ => return Err(error("invalid_line_ending", "unknown line ending")),
                    };
                    state.changes[index].after = encode(&content, encoding, false)?;
                }
                _ => {
                    state.require_file(index)?;
                    state.changes[index].after = missing_snapshot();
                    state.baselines[index] = SyntaxBaseline::Fresh;
                }
            }
        }
        "copy" | "move" | "transfer" => {
            let source = state.slot(arg(op, "source")?)?;
            let destination = state.slot(arg(op, "destination")?)?;
            state.require_file(source)?;
            if kind == "transfer" {
                return transfer(state, op, source, destination);
            }
            if source == destination {
                return Err(error(
                    "same_path",
                    "source and destination must differ; use move_file for case-only rename",
                ));
            }
            state.hash(
                source,
                op.get("expected_hash").and_then(Value::as_str),
                false,
            )?;
            if state.changes[destination].after.state != SnapshotState::Missing {
                return Err(error(
                    "already_exists",
                    "copy/move destination exists in virtual state",
                ));
            }
            state.changes[destination].after = state.changes[source].after.clone();
            state.baselines[destination] = state.baselines[source];
            if kind == "move" {
                state.changes[source].after = missing_snapshot();
            }
        }
        _ => return Err(error("invalid_operation", "unknown operation")),
    }
    Ok(())
}

pub(crate) fn execute(args: &Value) -> Result<Value> {
    let Some(operations) = args.get("operations").and_then(Value::as_array) else {
        return Ok(error("invalid_operations", "operations must be an array"));
    };
    if operations.is_empty() || operations.len() > MAX_STEPS {
        return Ok(error(
            "invalid_operations",
            "operations must contain 1..100 steps",
        ));
    }
    let validate = args
        .get("validate_syntax")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut state = VirtualState {
        changes: Vec::new(),
        baselines: Vec::new(),
    };
    let mut total_edits = 0;
    for (step, op) in operations.iter().enumerate() {
        if let Err(mut e) = simulate(&mut state, op, step, &mut total_edits) {
            e["operation_index"] = json!(step);
            return Ok(e);
        }
    }
    // Validate final contents at their final paths, including create and transfer.
    for (index, change) in state.changes.iter().enumerate() {
        if validate && change.after.state == SnapshotState::File {
            let after = change.after.bytes.as_deref().unwrap();
            if !is_probably_binary(after) {
                let before = match state.baselines[index] {
                    SyntaxBaseline::Original(origin) => {
                        decode_fuzzy(
                            state.changes[origin]
                                .before
                                .bytes
                                .as_deref()
                                .unwrap_or_default(),
                        )
                        .0
                    }
                    SyntaxBaseline::Fresh => {
                        if change
                            .path
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                        {
                            "null".to_string()
                        } else {
                            String::new()
                        }
                    }
                };
                if let Err(e) = super::edit_files::validate_new_syntax(
                    &change.path,
                    &before,
                    &decode_fuzzy(after).0,
                ) {
                    return Ok(error("syntax_validation_failed", e));
                }
            }
        }
    }
    let changes: Vec<_> = state
        .changes
        .iter()
        .filter(|c| !snapshots_match_contents(&c.before, &c.after))
        .cloned()
        .collect();
    let preview = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut files: Vec<_> = changes
        .iter()
        .map(|change| file_result(change, preview))
        .collect();
    if preview {
        return Ok(json!({
            "success": true,
            "dry_run": true,
            "files": files,
            "files_changed": changes.len(),
            "history_recorded": false
        }));
    }
    let publications = match commit(&state.changes) {
        Ok(statuses) => statuses,
        Err(error) => return Ok(error),
    };
    for (file, change) in files.iter_mut().zip(&changes) {
        if let Some(publication) = publications.get(&change.path) {
            publication.attach(file);
        }
    }
    let outcomes = record_change_set(
        "edit_files",
        changes
            .iter()
            .map(|c| {
                (
                    c.path.clone(),
                    c.before.clone(),
                    c.after.clone(),
                    "ordered file operations".into(),
                )
            })
            .collect(),
    );
    for (file, outcome) in files.iter_mut().zip(&outcomes) {
        crate::history::attach_history_metadata(file, outcome);
    }
    Ok(json!({
        "success": true,
        "files": files,
        "files_changed": changes.len(),
        "change_set_id": outcomes.first().and_then(|outcome| outcome.change_set_id.clone())
    }))
}

fn file_result(change: &PathChange, preview: bool) -> Value {
    let mut result = json!({
        "path": crate::common::normalize_display_path(&change.path),
        "sha256_before": snapshot_hash(&change.before),
        "sha256_after": snapshot_hash(&change.after),
        "before_state": format!("{:?}", change.before.state).to_lowercase(),
        "after_state": format!("{:?}", change.after.state).to_lowercase(),
        "changed": true
    });
    if preview
        && (change.before.state == SnapshotState::Directory
            || change.after.state == SnapshotState::Directory)
    {
        result["diff"] = json!({"diff_skipped": "directory state change"});
    } else if preview {
        let before = change.before.bytes.as_deref().unwrap_or_default();
        let after = change.after.bytes.as_deref().unwrap_or_default();
        result["diff"] = if is_probably_binary(before) || is_probably_binary(after) {
            json!({"diff_skipped": "binary contents"})
        } else {
            super::diff_files::bounded_diff(
                &decode_fuzzy(before).0,
                &decode_fuzzy(after).0,
                "before",
                "after",
                2048,
            )
        };
    }
    result
}

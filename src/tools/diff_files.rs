use super::read_file::{decode_fuzzy, is_probably_binary};
use super::tool_errors::error;
use crate::history::{SnapshotState, get_record};
use anyhow::Result;
use serde_json::{Value, json};

pub(crate) const MAX_DIFF_INPUT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_DIFF_OUTPUT_BYTES: usize = 64 * 1024;

pub fn schema() -> Value {
    let side = json!({"type":"object","properties":{
        "path":{"type":"string"}, "content":{"type":"string"}, "entry_id":{"type":"string"},
        "snapshot":{"type":"string","enum":["before","after"]}
    }});
    json!({
        "name":"diff_files", "title":"Diff files or snapshots",
        "description":"Diff two text sources: exactly one path, content, or history entry_id with snapshot before/after per side. Inputs are bounded to 1 MiB each; output is truncated explicitly. History is process-local; hash-only snapshots have no diff content.",
        "inputSchema":{"type":"object","properties":{
            "left":side, "right":side, "max_output_bytes":{"type":"integer","minimum":256,"maximum":65536}
        },"required":["left","right"]}
    })
}

pub(crate) fn bounded_diff(
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
    budget: usize,
) -> Value {
    if left.len() > MAX_DIFF_INPUT_BYTES || right.len() > MAX_DIFF_INPUT_BYTES {
        return json!({"diff_skipped":"input exceeds 1 MiB diff limit"});
    }
    let mut result = super::diff_support::build_diff_payload(left, right, left_label, right_label);
    let diff = result["unified_diff"].as_str().unwrap_or("");
    let total = diff.len();
    let mut cut = total.min(budget);
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    let output = diff[..cut].to_string();
    result["unified_diff"] = json!(output);
    result["output_truncated"] = json!(cut < total);
    result["omitted_bytes"] = json!(total - cut);
    result
}

fn source(args: &Value) -> std::result::Result<(String, String), Value> {
    let selectors = ["path", "content", "entry_id"]
        .iter()
        .filter(|key| args.get(**key).is_some())
        .count();
    if selectors != 1 {
        return Err(error(
            "invalid_source",
            "each side requires exactly one of path, content, entry_id",
        ));
    }
    if args.get("snapshot").is_some() && args.get("entry_id").is_none() {
        return Err(error(
            "invalid_source",
            "snapshot is only valid with entry_id",
        ));
    }
    let (bytes, label) = if let Some(path) = args.get("path").and_then(Value::as_str) {
        let path = crate::common::resolve_existing_tool_path(path)
            .map_err(|e| error("invalid_path", e.to_string()))?;
        let metadata = std::fs::metadata(&path).map_err(|e| error("read_failed", e.to_string()))?;
        if !metadata.is_file() {
            return Err(error("not_a_file", "diff source must be a file"));
        }
        if metadata.len() > MAX_DIFF_INPUT_BYTES as u64 {
            return Err(error("input_too_large", "diff input exceeds 1 MiB"));
        }
        // take() also bounds reads if the file grows after metadata().
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(&path)
            .and_then(|f| {
                f.take(MAX_DIFF_INPUT_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|e| error("read_failed", e.to_string()))?;
        (bytes, crate::common::normalize_display_path(&path))
    } else if let Some(content) = args.get("content").and_then(Value::as_str) {
        if content.len() > MAX_DIFF_INPUT_BYTES {
            return Err(error("input_too_large", "diff input exceeds 1 MiB"));
        }
        (content.as_bytes().to_vec(), "content".into())
    } else if let Some(id) = args.get("entry_id").and_then(Value::as_str) {
        let record = get_record(id).ok_or_else(|| {
            error(
                "history_not_found",
                "history entry is unavailable or evicted",
            )
        })?;
        let snapshot = match args.get("snapshot").and_then(Value::as_str) {
            Some("before") => record.before,
            Some("after") => record.after,
            _ => {
                return Err(error(
                    "invalid_snapshot",
                    "history source requires snapshot before or after",
                ));
            }
        };
        let bytes = match snapshot.state {
            SnapshotState::Missing => Vec::new(),
            SnapshotState::Directory => {
                return Err(error("not_a_file", "directory snapshots cannot be diffed"));
            }
            SnapshotState::File => snapshot.bytes.ok_or_else(|| {
                error(
                    "snapshot_content_unavailable",
                    "hash-only history has no retained content",
                )
            })?,
        };
        (
            bytes,
            format!("{id}:{}", args["snapshot"].as_str().unwrap()),
        )
    } else {
        return Err(error("invalid_source", "source selector must be a string"));
    };
    if bytes.len() > MAX_DIFF_INPUT_BYTES {
        return Err(error("input_too_large", "diff input exceeds 1 MiB"));
    }
    if is_probably_binary(&bytes) {
        return Err(error(
            "binary_file",
            "text diff does not accept binary input",
        ));
    }
    Ok((decode_fuzzy(&bytes).0, label))
}

pub async fn execute(args: &Value) -> Result<Value> {
    let (left, left_label) = match source(&args["left"]) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let (right, right_label) = match source(&args["right"]) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let budget = args
        .get("max_output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_DIFF_OUTPUT_BYTES as u64);
    if !(256..=MAX_DIFF_OUTPUT_BYTES as u64).contains(&budget) {
        return Ok(error(
            "invalid_budget",
            "max_output_bytes must be 256..65536",
        ));
    }
    let mut result = bounded_diff(&left, &right, &left_label, &right_label, budget as usize);
    result["success"] = json!(true);
    result["left_label"] = json!(left_label);
    result["right_label"] = json!(right_label);
    Ok(result)
}

use anyhow::Result;
use serde_json::{Value, json};

use crate::history::{
    MAX_HISTORY_BYTES, PathSnapshot, SnapshotState, list_records, retained_history_bytes,
};

pub fn schema() -> Value {
    json!({
        "name": "list_history",
        "title": "List write history",
        "description": "List recent in-memory write snapshots without returning file contents. History is process-local and bounded by entry count and retained bytes.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum newest entries to return. Defaults to 20." }
            }
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(20)
        .clamp(1, 100);
    let (records, total) = list_records(limit);
    let entries = records
        .into_iter()
        .map(|record| {
            json!({
                "entry_id": record.entry_id,
                "tool": record.tool_name,
                "path": crate::common::normalize_display_path(std::path::Path::new(&record.path)),
                "canonical_path": crate::common::normalize_display_path(std::path::Path::new(&record.canonical_path)),
                "summary": record.summary,
                "outside_declared": record.outside_declared,
                "before_state": state_name(&record.before.state),
                "after_state": state_name(&record.after.state),
                "before_bytes": snapshot_bytes(&record.before),
                "after_bytes": snapshot_bytes(&record.after),
                "before_encoding": record.before.encoding_label,
                "after_encoding": record.after.encoding_label,
                "before_line_ending": record.before.line_ending,
                "after_line_ending": record.after.line_ending
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "entries": entries,
        "returned": entries.len(),
        "total": total,
        "retained_bytes": retained_history_bytes(),
        "max_retained_bytes": MAX_HISTORY_BYTES,
        "process_local": true
    }))
}

fn state_name(state: &SnapshotState) -> &'static str {
    match state {
        SnapshotState::Missing => "missing",
        SnapshotState::File => "file",
        SnapshotState::Directory => "directory",
    }
}

fn snapshot_bytes(snapshot: &PathSnapshot) -> usize {
    snapshot.bytes.as_ref().map(Vec::len).unwrap_or(0)
}

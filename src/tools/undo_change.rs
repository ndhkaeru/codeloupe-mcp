use anyhow::Result;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

use crate::history::{
    PathSnapshot, SnapshotState, attach_history_metadata, capture_snapshot, get_record, no_history,
    record_change, snapshots_match_contents,
};
use crate::security::path_guard::GUARD;

pub fn schema() -> Value {
    json!({
        "name": "undo_change",
        "title": "Undo one recorded change",
        "description": "Restore the before-snapshot of one process-local history entry. Refuses to overwrite when the current path no longer matches the recorded after-snapshot.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "entry_id": { "type": "string", "description": "History entry ID returned by a write tool or list_history." }
            },
            "required": ["entry_id"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let entry_id = args
        .get("entry_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if entry_id.is_empty() {
        return Ok(error_response("invalid_entry_id", "entry_id is required"));
    }
    let Some(record) = get_record(entry_id) else {
        return Ok(error_response(
            "history_not_found",
            format!("history entry '{entry_id}' was not found or has been evicted"),
        ));
    };

    let path = crate::common::resolve_write_tool_path(&record.path);
    let canonical = GUARD.check_path(&path);
    if canonical != Path::new(&record.canonical_path) {
        return Ok(json!({
            "success": false,
            "error_code": "history_conflict",
            "entry_id": entry_id,
            "path": crate::common::normalize_display_path(&path),
            "canonical_path": crate::common::normalize_display_path(&canonical),
            "recorded_canonical_path": crate::common::normalize_display_path(Path::new(&record.canonical_path)),
            "message": "path now resolves to a different canonical location; refusing to restore the snapshot"
        }));
    }

    let current = match capture_snapshot(&path) {
        Ok(snapshot) => snapshot,
        Err(message) => return Ok(error_response("snapshot_failed", message)),
    };
    if !snapshots_match_contents(&current, &record.after) {
        return Ok(json!({
            "success": false,
            "error_code": "history_conflict",
            "entry_id": entry_id,
            "path": crate::common::normalize_display_path(&path),
            "message": "current path no longer matches the recorded after-snapshot; refusing to overwrite newer changes"
        }));
    }

    if let Err(message) = restore_snapshot(&path, &record.before) {
        return Ok(json!({
            "success": false,
            "error_code": "undo_failed",
            "entry_id": entry_id,
            "path": crate::common::normalize_display_path(&path),
            "message": message
        }));
    }

    let history_outcome = record_change(
        "undo_change",
        &path,
        current,
        record.before.clone(),
        format!("undo {} ({})", record.entry_id, record.summary),
    );
    let sha256_after = record
        .before
        .bytes
        .as_deref()
        .map(super::file_hash::sha256_bytes);
    let mut response = json!({
        "success": true,
        "undone_entry_id": record.entry_id,
        "path": crate::common::normalize_display_path(&path),
        "restored_state": state_name(&record.before.state),
        "sha256_after": sha256_after,
        "message": "history snapshot restored"
    });
    attach_history_metadata(&mut response, &history_outcome);
    Ok(response)
}

fn restore_snapshot(path: &Path, snapshot: &PathSnapshot) -> std::result::Result<(), String> {
    match snapshot.state {
        SnapshotState::Missing => {
            if path.is_file() {
                fs::remove_file(path).map_err(|error| format!("remove file failed: {error}"))?;
            } else if path.is_dir() {
                fs::remove_dir(path)
                    .map_err(|error| format!("remove directory failed: {error}"))?;
            }
        }
        SnapshotState::File => {
            let bytes = snapshot
                .bytes
                .as_deref()
                .ok_or_else(|| "file snapshot is missing bytes".to_string())?;
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("create parent directories failed: {error}"))?;
            }
            super::atomic_write::write_bytes(path, bytes, path.exists())
                .map_err(|error| format!("restore file failed: {error}"))?;
        }
        SnapshotState::Directory => {
            fs::create_dir_all(path)
                .map_err(|error| format!("restore directory failed: {error}"))?;
        }
    }
    if let Some(parent) = path.parent() {
        crate::indexer::notify_content_directory_changed(parent);
    }
    Ok(())
}

fn state_name(state: &SnapshotState) -> &'static str {
    match state {
        SnapshotState::Missing => "missing",
        SnapshotState::File => "file",
        SnapshotState::Directory => "directory",
    }
}

fn error_response(error_code: &str, message: impl Into<String>) -> Value {
    let mut response = json!({
        "success": false,
        "error_code": error_code,
        "message": message.into()
    });
    attach_history_metadata(&mut response, &no_history("undo was not applied"));
    response
}

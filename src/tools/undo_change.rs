use super::file_transaction::{PathChange, check_current, commit};
use crate::history::{
    SnapshotState, attach_history_metadata, get_change_set, get_record, no_history, record_change,
    record_change_set, snapshot_hash,
};
use crate::security::path_guard::GUARD;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub fn schema() -> Value {
    json!({
        "name": "undo_change",
        "title": "Undo recorded changes",
        "description": "Restore one entry_id or a complete change_set_id from bounded process-local history. Preflights all group paths and rolls back on failure. Multi-entry groups require change_set_id or allow_partial=true. Refuses newer contents or changed paths. Restart/upgrade loses history.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "entry_id": {"type": "string"},
                "change_set_id": {"type": "string"},
                "allow_partial": {"type": "boolean"}
            },
            "oneOf": [
                {"required": ["entry_id"]},
                {"required": ["change_set_id"]}
            ]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let entry = args.get("entry_id").and_then(Value::as_str).map(str::trim);
    let group = args
        .get("change_set_id")
        .and_then(Value::as_str)
        .map(str::trim);
    if entry == Some("") || group == Some("") {
        return Ok(super::tool_errors::error(
            "invalid_history_selector",
            "history ID must not be empty",
        ));
    }
    if entry.is_some() == group.is_some() {
        return Ok(super::tool_errors::error(
            "invalid_history_selector",
            "exactly one entry_id or change_set_id is required",
        ));
    }
    let records = if let Some(id) = group {
        get_change_set(id)
    } else {
        get_record(entry.unwrap()).into_iter().collect()
    };
    if records.is_empty() {
        return Ok(super::tool_errors::error(
            "history_not_found",
            "history entry or complete change set is unavailable or evicted",
        ));
    }
    let mut partial_group = None;
    if group.is_none()
        && let Some(group_id) = &records[0].change_set_id
    {
        let siblings = get_change_set(group_id);
        if siblings.is_empty() {
            return Ok(super::tool_errors::error(
                "history_not_found",
                "change set was evicted while resolving its entry",
            ));
        }
        if siblings.len() > 1 {
            if args.get("allow_partial").and_then(Value::as_bool) != Some(true) {
                let mut response = super::tool_errors::error(
                    "partial_undo_requires_opt_in",
                    "entry belongs to a multi-entry change set; undo change_set_id or set allow_partial=true",
                );
                response["change_set_id"] = json!(group_id);
                response["group_entries"] = json!(siblings.len());
                return Ok(response);
            }
            partial_group = Some(group_id.clone());
        }
    }
    if let Some(record) = records.iter().find(|r| r.move_link.is_some()) {
        if records.len() != 1 {
            return Ok(super::tool_errors::error(
                "unsupported_change_set",
                "mixed move history is not supported",
            ));
        }
        return Ok(match super::file_transfer::undo_move(record) {
            Ok(v) | Err(v) => v,
        });
    }
    let mut changes = Vec::new();
    for record in &records {
        let path = PathBuf::from(&record.path);
        if GUARD.check_path(&path) != Path::new(&record.canonical_path) {
            return Ok(super::tool_errors::error(
                "history_conflict",
                "path now resolves to a different canonical location",
            ));
        }
        if record.before.state == SnapshotState::File && record.before.bytes.is_none() {
            return Ok(super::tool_errors::error(
                "snapshot_content_unavailable",
                "hash-only snapshot has no bytes to restore",
            ));
        }
        let change = PathChange {
            canonical: GUARD.check_path(&path),
            path,
            before: record.after.clone(),
            after: record.before.clone(),
        };
        if let Err(e) = check_current(&change, &change.before) {
            return Ok(super::tool_errors::error("history_conflict", e));
        }
        changes.push(change);
    }
    let publications = match commit(&changes) {
        Ok(statuses) => statuses,
        Err(mut error) => {
            error["error_code"] = json!(if error["error_code"] == "write_failed" {
                "undo_failed"
            } else {
                "history_conflict"
            });
            return Ok(error);
        }
    };
    let restorable_inverse = changes
        .iter()
        .all(|c| c.before.state != SnapshotState::File || c.before.bytes.is_some());
    let snapshots = changes
        .iter()
        .map(|c| {
            (
                c.path.clone(),
                c.before.clone(),
                c.after.clone(),
                "undo change set".into(),
            )
        })
        .collect();
    let outcomes = if !restorable_inverse {
        changes
            .iter()
            .map(|_| no_history("undo completed; hash-only contents were not retained for redo"))
            .collect()
    } else if group.is_some() {
        record_change_set("undo_change", snapshots)
    } else {
        let c = &changes[0];
        vec![record_change(
            "undo_change",
            &c.path,
            c.before.clone(),
            c.after.clone(),
            format!("undo {}", entry.unwrap()),
        )]
    };
    if group.is_none() {
        let change = &changes[0];
        let mut response = json!({
            "success": true,
            "undone_entry_id": entry.unwrap(),
            "path": crate::common::normalize_display_path(&change.path),
            "restored_state": state_name(&change.after.state),
            "sha256_after": snapshot_hash(&change.after)
        });
        if let Some(publication) = publications.get(&change.path) {
            publication.attach(&mut response);
        }
        attach_history_metadata(&mut response, &outcomes[0]);
        if let Some(group_id) = partial_group {
            response["partial_undo"] = json!(true);
            response["partial_change_set_id"] = json!(group_id);
            response["warnings"] = json!([
                "Only this entry was undone; other paths in its change set remain changed."
            ]);
        }
        return Ok(response);
    }
    let files: Vec<_> = changes
        .iter()
        .zip(&outcomes)
        .map(|(change, outcome)| {
            let mut file = json!({
                "path": crate::common::normalize_display_path(&change.path),
                "sha256_after": snapshot_hash(&change.after),
                "restored_state": state_name(&change.after.state)
            });
            if let Some(publication) = publications.get(&change.path) {
                publication.attach(&mut file);
            }
            attach_history_metadata(&mut file, outcome);
            file
        })
        .collect();
    Ok(json!({
        "success": true,
        "undone_change_set_id": group.unwrap(),
        "change_set_id": outcomes.first().and_then(|outcome| outcome.change_set_id.clone()),
        "files": files,
        "files_changed": changes.len()
    }))
}

fn state_name(state: &SnapshotState) -> &'static str {
    match state {
        SnapshotState::Missing => "missing",
        SnapshotState::File => "file",
        SnapshotState::Directory => "directory",
    }
}

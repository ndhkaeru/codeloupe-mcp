use crate::history::{
    capture_hash_snapshot, hash_snapshot, missing_snapshot, record_change_set, record_move,
    snapshot_hash,
};
use crate::security::path_guard::GUARD;
use anyhow::Result;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn schema(name: &str, moving: bool) -> Value {
    json!({
        "name":name,"title":if moving { "Move file" } else { "Copy file" },
        "description":if moving {
            "Move one regular file without overwriting. Supports case-only rename on case-insensitive filesystems and verified streaming copy on cross-device moves. Parents must exist. History retains hashes and the inverse move, process-locally; imports are not rewritten."
        } else {
            "Copy one regular file byte-for-byte with verified streaming I/O. Never overwrites; parents must exist. Hash-only process-local history supports guarded removal on undo; it does not retain content for diff or redo."
        },
        "inputSchema":{"type":"object","properties":{
            "source":{"type":"string"},"destination":{"type":"string"},
            "expected_hash":{"type":"string"},"dry_run":{"type":"boolean"}
        },"required":["source","destination"]}
    })
}

fn path(args: &Value, key: &str) -> std::result::Result<PathBuf, Value> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(crate::common::resolve_write_tool_path)
        .ok_or_else(|| super::tool_errors::error("invalid_path", format!("{key} is required")))
}

pub(crate) fn case_rename(source: &Path, destination: &Path) -> bool {
    if source == destination || !fs::symlink_metadata(destination).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let parents_match = source
        .parent()
        .zip(destination.parent())
        .is_some_and(|(a, b)| {
            fs::canonicalize(a)
                .ok()
                .zip(fs::canonicalize(b).ok())
                .is_some_and(|(a, b)| a == b)
        });
    parents_match
        && source.file_name() != destination.file_name()
        && fs::canonicalize(source)
            .ok()
            .zip(fs::canonicalize(destination).ok())
            .is_some_and(|(a, b)| a == b)
}

fn regular(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "only regular files are supported; symlinks and directories are excluded",
        ))
    }
}

fn unchanged(source: &Path, expected: &str) -> std::io::Result<()> {
    regular(source)?;
    if super::file_hash::sha256_file(source)? == expected {
        Ok(())
    } else {
        Err(std::io::Error::other("file changed since preflight"))
    }
}

pub(crate) struct Transfer {
    method: &'static str,
    publication: super::atomic_write::Publication,
}

impl Transfer {
    fn atomic(method: &'static str) -> Self {
        Self {
            method,
            publication: super::atomic_write::Publication::ATOMIC,
        }
    }
}

pub(crate) fn move_no_replace(
    source: &Path,
    destination: &Path,
    hash: &str,
) -> std::io::Result<Transfer> {
    unchanged(source, hash)?;
    let source_canonical = GUARD.check_path(source);
    let target_canonical = GUARD.check_path(destination);
    if case_rename(source, destination) {
        #[cfg(windows)]
        super::atomic_write::rename_windows(source, destination, false)?;
        #[cfg(not(windows))]
        fs::rename(source, destination)?;
        super::atomic_write::notify(source);
        super::atomic_write::notify(destination);
        return Ok(Transfer::atomic("case_rename"));
    }
    #[cfg(windows)]
    {
        match super::atomic_write::rename_windows(source, destination, false) {
            Ok(()) => {
                super::atomic_write::notify(source);
                super::atomic_write::notify(destination);
                return Ok(Transfer::atomic("rename"));
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::CrossesDevices
                    || e.raw_os_error() == Some(17) => {}
            Err(e) => return Err(e),
        }
    }
    #[cfg(unix)]
    {
        match super::atomic_write::native_rename_no_replace(source, destination) {
            Ok(()) => {
                super::atomic_write::notify(source);
                super::atomic_write::notify(destination);
                return Ok(Transfer::atomic("rename"));
            }
            Err(e) if super::atomic_write::publication_fallback_allowed(&e) => {}
            Err(e) => return Err(e),
        }
        match fs::hard_link(source, destination) {
            Ok(()) => {
                return finish_move(
                    source,
                    destination,
                    hash,
                    &source_canonical,
                    &target_canonical,
                    Transfer::atomic("link_unlink"),
                );
            }
            Err(e) if super::atomic_write::publication_fallback_allowed(&e) => {}
            Err(e) => return Err(e),
        }
    }
    let publication = super::atomic_write::copy_verified(source, destination, hash)?;
    finish_move(
        source,
        destination,
        hash,
        &source_canonical,
        &target_canonical,
        Transfer {
            method: if publication.atomic {
                "copy_verify_delete"
            } else {
                "copy_exclusive_create_delete"
            },
            publication,
        },
    )
}

fn finish_move(
    source: &Path,
    destination: &Path,
    hash: &str,
    source_canonical: &Path,
    target_canonical: &Path,
    transfer: Transfer,
) -> std::io::Result<Transfer> {
    let attempt = (|| {
        if GUARD.check_path(source) != source_canonical
            || GUARD.check_path(destination) != target_canonical
        {
            return Err(std::io::Error::other("canonical path changed while moving"));
        }
        unchanged(source, hash)?;
        unchanged(destination, hash)?;
        #[cfg(unix)]
        std::fs::File::open(destination.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
        fs::remove_file(source)
    })();
    if let Err(error) = attempt {
        // Cleanup must not remove a target changed by an independent writer.
        if GUARD.check_path(destination) == target_canonical && unchanged(destination, hash).is_ok()
        {
            let _ = fs::remove_file(destination);
            super::atomic_write::notify(destination);
        }
        return Err(error);
    }
    super::atomic_write::notify(source);
    super::atomic_write::notify(destination);
    Ok(transfer)
}

pub(crate) async fn execute(args: &Value, moving: bool) -> Result<Value> {
    let source = match path(args, "source") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let destination = match path(args, "destination") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let reference_request = if moving {
        match super::move_reference_hints::parse(args) {
            Ok(request) => request,
            Err(error) => return Ok(error),
        }
    } else {
        None
    };
    let source_canonical = GUARD.check_path(&source);
    let target_canonical = GUARD.check_path(&destination);
    if let Err(e) = regular(&source) {
        return Ok(super::tool_errors::error("invalid_source", e.to_string()));
    }
    if source == destination {
        return Ok(super::tool_errors::error(
            "same_path",
            "source and destination must differ",
        ));
    }
    let case_only = moving && case_rename(&source, &destination);
    match fs::symlink_metadata(&destination) {
        Ok(_) if !case_only => {
            return Ok(super::tool_errors::error(
                "already_exists",
                "destination already exists",
            ));
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Ok(super::tool_errors::error("io_error", e.to_string()));
        }
        _ => {}
    }
    if !destination.parent().is_some_and(Path::is_dir) {
        return Ok(super::tool_errors::error(
            "parent_missing",
            "destination parent must exist",
        ));
    }
    let hash = match super::file_hash::sha256_file(&source) {
        Ok(v) => v,
        Err(e) => {
            return Ok(super::tool_errors::error(
                super::file_io_error::classify(&source, &e, "not_found"),
                e.to_string(),
            ));
        }
    };
    if let Some(raw) = args.get("expected_hash").and_then(Value::as_str) {
        let expected = match super::file_hash::normalize_expected_hash(raw) {
            Ok(v) => v,
            Err(e) => return Ok(super::tool_errors::error("invalid_hash", e)),
        };
        if expected != hash {
            return Ok(super::tool_errors::error(
                "hash_mismatch",
                "source does not match expected_hash",
            ));
        }
    }
    let size = match source_size(&source) {
        Ok(size) => size,
        Err(error) => return Ok(error),
    };
    let reference_hints = match reference_request {
        Some(request) => {
            Some(super::move_reference_hints::collect(request, &source, &destination, args).await)
        }
        None => None,
    };
    if crate::cancellation::token_from_args(args).is_some_and(|token| token.is_cancelled()) {
        return Ok(super::tool_errors::error(
            "cancelled",
            "move cancelled before filesystem mutation",
        ));
    }
    if args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let mut response = json!({
            "success": true,
            "dry_run": true,
            "source": crate::common::normalize_display_path(&source),
            "destination": crate::common::normalize_display_path(&destination),
            "sha256_before": hash,
            "size_bytes": size,
            "history_recorded": false
        });
        if let Some(hints) = reference_hints {
            response["reference_hints"] = hints;
        }
        return Ok(response);
    }
    if GUARD.check_path(&source) != source_canonical
        || GUARD.check_path(&destination) != target_canonical
    {
        return Ok(super::tool_errors::error(
            "path_changed",
            "path changed since preflight",
        ));
    }
    let result = if moving {
        move_no_replace(&source, &destination, &hash)
    } else {
        super::atomic_write::copy_verified(&source, &destination, &hash).map(|publication| {
            Transfer {
                method: if publication.atomic {
                    "copy_verify"
                } else {
                    "copy_exclusive_create"
                },
                publication,
            }
        })
    };
    let transfer = match result {
        Ok(transfer) => transfer,
        Err(e) => {
            let code = super::file_io_error::classify(&source, &e, "not_found");
            let mut result = super::tool_errors::error(code, e.to_string());
            result["source_exists"] = json!(source.exists());
            result["destination_exists"] = json!(destination.exists());
            result["history_recorded"] = json!(false);
            return Ok(result);
        }
    };
    let outcome = if moving {
        record_move("move_file", &source, &destination, hash.clone())
    } else {
        record_change_set(
            "copy_file",
            vec![(
                destination.clone(),
                missing_snapshot(),
                hash_snapshot(hash.clone()),
                "copy file".into(),
            )],
        )
        .remove(0)
    };
    let mut result = json!({
        "success": true,
        "source": crate::common::normalize_display_path(&source),
        "destination": crate::common::normalize_display_path(&destination),
        "sha256_after": hash,
        "size_bytes": size,
        "method": transfer.method
    });
    transfer.publication.attach(&mut result);
    if let Some(hints) = reference_hints {
        result["reference_hints"] = hints;
    }
    crate::history::attach_history_metadata(&mut result, &outcome);
    Ok(result)
}

pub(crate) fn undo_move(
    record: &crate::history::HistoryRecord,
) -> std::result::Result<Value, Value> {
    let link = record.move_link.as_ref().unwrap();
    let original = PathBuf::from(&record.path);
    let current = PathBuf::from(&link.destination);
    if GUARD.check_path(&original) != Path::new(&record.canonical_path)
        || GUARD.check_path(&current) != Path::new(&link.canonical_destination)
    {
        return Err(super::tool_errors::error(
            "history_conflict",
            "move paths now resolve differently",
        ));
    }
    let case_only = case_rename(&current, &original);
    let before = capture_hash_snapshot(&original)
        .map_err(|e| super::tool_errors::error("history_conflict", e))?;
    if !case_only && before.state != crate::history::SnapshotState::Missing {
        return Err(super::tool_errors::error(
            "history_conflict",
            "original source path is occupied",
        ));
    }
    let after = capture_hash_snapshot(&current)
        .map_err(|e| super::tool_errors::error("history_conflict", e))?;
    if snapshot_hash(&after).as_deref() != Some(&link.sha256) {
        return Err(super::tool_errors::error(
            "history_conflict",
            "moved file changed or is missing",
        ));
    }
    let transfer = move_no_replace(&current, &original, &link.sha256).map_err(|e| {
        let mut result = super::tool_errors::error("undo_failed", e.to_string());
        result["source_exists"] = json!(current.exists());
        result["destination_exists"] = json!(original.exists());
        result["history_recorded"] = json!(false);
        result
    })?;
    let outcome = record_move("undo_change", &current, &original, link.sha256.clone());
    let mut result = json!({
        "success": true,
        "undone_entry_id": record.entry_id,
        "undone_change_set_id": record.change_set_id,
        "path": crate::common::normalize_display_path(&original),
        "restored_state": "file",
        "sha256_after": link.sha256
    });
    transfer.publication.attach(&mut result);
    crate::history::attach_history_metadata(&mut result, &outcome);
    Ok(result)
}

fn source_size(path: &Path) -> std::result::Result<u64, Value> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| {
            super::tool_errors::error(
                super::file_io_error::classify(path, &error, "not_found"),
                error.to_string(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_source_after_hash_returns_a_structured_metadata_error() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::write(&source, b"data").unwrap();
        super::super::file_hash::sha256_file(&source).unwrap();
        fs::remove_file(&source).unwrap();
        let error = source_size(&source).unwrap_err();
        assert_eq!(error["success"], false);
        assert_eq!(error["error_code"], "not_found");
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_symlink_is_never_a_case_rename() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Source");
        let target = dir.path().join("source");
        fs::write(&source, b"data").unwrap();
        if target.exists() {
            return;
        }
        std::os::unix::fs::symlink(&source, &target).unwrap();
        assert!(!case_rename(&source, &target));
    }
}

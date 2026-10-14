use crate::history::{PathSnapshot, SnapshotState, capture_snapshot, snapshots_match_contents};
use crate::security::path_guard::GUARD;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) struct PathChange {
    pub path: PathBuf,
    pub canonical: PathBuf,
    pub before: PathSnapshot,
    pub after: PathSnapshot,
}

pub(crate) fn is_link(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0400 != 0 {
            return true;
        }
    }
    false
}

pub(crate) fn check_current(change: &PathChange, snapshot: &PathSnapshot) -> Result<(), String> {
    if GUARD.check_path(&change.path) != change.canonical {
        return Err("path resolves to a different canonical location".into());
    }
    if std::fs::symlink_metadata(&change.path).is_ok_and(|metadata| is_link(&metadata)) {
        return Err("path is now a symlink or reparse point".into());
    }
    let current = if snapshot.bytes.is_none() && snapshot.sha256.is_some() {
        crate::history::capture_hash_snapshot(&change.path)?
    } else {
        capture_snapshot(&change.path)?
    };
    if !snapshots_match_contents(&current, snapshot) {
        return Err("current state differs from the preflight snapshot".into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn restore(
    path: &Path,
    before: &PathSnapshot,
    after: &PathSnapshot,
) -> std::io::Result<()> {
    restore_with_status(path, before, after).map(|_| ())
}

fn restore_with_status(
    path: &Path,
    before: &PathSnapshot,
    after: &PathSnapshot,
) -> std::io::Result<super::atomic_write::Publication> {
    let mut publication = super::atomic_write::Publication::ATOMIC;
    match after.state {
        SnapshotState::File => {
            let bytes = after
                .bytes
                .as_deref()
                .ok_or_else(|| std::io::Error::other("snapshot has no restorable bytes"))?;
            publication = super::atomic_write::write_bytes_with_status(
                path,
                bytes,
                before.state != SnapshotState::Missing,
            )?;
        }
        SnapshotState::Missing => {
            if before.state == SnapshotState::Directory {
                std::fs::remove_dir(path)?;
            } else if before.state == SnapshotState::File {
                std::fs::remove_file(path)?;
            }
        }
        SnapshotState::Directory => {
            if before.state == SnapshotState::Missing {
                std::fs::create_dir(path)?;
            }
        }
    }
    super::atomic_write::notify(path);
    Ok(publication)
}

pub(crate) fn commit(
    changes: &[PathChange],
) -> Result<BTreeMap<PathBuf, super::atomic_write::Publication>, Value> {
    let mut publications = BTreeMap::new();
    let result = commit_with(changes, &mut |path, before, after| {
        let publication = restore_with_status(path, before, after)?;
        publications.insert(path.to_path_buf(), publication);
        Ok(())
    });
    match result {
        Ok(()) => Ok(publications),
        Err(mut error) => {
            for (field, atomic) in [
                ("non_atomic_created_paths", true),
                ("permissions_not_preserved_paths", false),
            ] {
                let paths: Vec<_> = publications
                    .iter()
                    .filter(|(_, status)| {
                        if atomic {
                            !status.atomic
                        } else {
                            !status.permissions_preserved
                        }
                    })
                    .map(|(path, _)| crate::common::normalize_display_path(path))
                    .collect();
                if !paths.is_empty() {
                    error[field] = json!(paths);
                }
            }
            Err(error)
        }
    }
}

fn preflight_directory_removals(changes: &[PathChange]) -> Result<(), Value> {
    for (index, change) in changes.iter().enumerate() {
        if change.before.state != SnapshotState::Directory
            || change.after.state != SnapshotState::Missing
        {
            continue;
        }
        let children = std::fs::read_dir(&change.path).map_err(|error| {
            failure(
                "concurrent_change",
                format!("cannot inspect directory before removal: {error}"),
                index,
                false,
                Vec::new(),
            )
        })?;
        for child in children {
            let child = child.map_err(|error| {
                failure(
                    "concurrent_change",
                    format!("cannot inspect directory child: {error}"),
                    index,
                    false,
                    Vec::new(),
                )
            })?;
            let path = child.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                failure(
                    "concurrent_change",
                    format!("cannot inspect directory child metadata: {error}"),
                    index,
                    false,
                    Vec::new(),
                )
            })?;
            let canonical = GUARD.check_path(&path);
            let tracked_removal = !is_link(&metadata)
                && changes.iter().any(|candidate| {
                    candidate.canonical == canonical
                        && candidate.before.state != SnapshotState::Missing
                        && candidate.after.state == SnapshotState::Missing
                });
            if !tracked_removal {
                return Err(failure(
                    "concurrent_change",
                    "directory contains an entry outside this change set; refusing removal".into(),
                    index,
                    false,
                    Vec::new(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn commit_with<F>(changes: &[PathChange], writer: &mut F) -> Result<(), Value>
where
    F: FnMut(&Path, &PathSnapshot, &PathSnapshot) -> std::io::Result<()>,
{
    // Every target is checked before any write. Each is checked again at commit.
    for (index, change) in changes.iter().enumerate() {
        if let Err(message) = check_current(change, &change.before) {
            return Err(failure(
                "concurrent_change",
                message,
                index,
                false,
                Vec::new(),
            ));
        }
    }
    preflight_directory_removals(changes)?;
    // Create parent directories first, then publish files, remove file sources,
    // and finally remove emptied directories from deepest to shallowest.
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by_key(|index| {
        let change = &changes[*index];
        let depth = change.path.components().count();
        match change.after.state {
            SnapshotState::Directory => (0, depth),
            SnapshotState::File => (1, 0),
            SnapshotState::Missing if change.before.state == SnapshotState::Directory => {
                (3, usize::MAX - depth)
            }
            SnapshotState::Missing => (2, 0),
        }
    });
    let mut written = Vec::new();
    for index in order {
        let change = &changes[index];
        if snapshots_match_contents(&change.before, &change.after) {
            continue;
        }
        let attempt = check_current(change, &change.before)
            .map_err(|e| ("concurrent_change", e))
            .and_then(|_| {
                writer(&change.path, &change.before, &change.after)
                    .map_err(|e| ("write_failed", e.to_string()))
            });
        if let Err((code, message)) = attempt {
            let mut errors = Vec::new();
            if check_current(change, &change.before).is_err() {
                // An exclusive-create write may have failed during cleanup, or
                // another writer may have claimed the destination. Preserve it.
                errors.push(format!(
                    "{}: failed path no longer matches its preflight state; refusing unsafe cleanup",
                    crate::common::normalize_display_path(&change.path)
                ));
            }
            for i in written.iter().rev().copied() {
                let previous: &PathChange = &changes[i];
                // Never roll back over an independent writer's newer contents.
                let result = check_current(previous, &previous.after).and_then(|_| {
                    writer(&previous.path, &previous.after, &previous.before)
                        .map_err(|e| e.to_string())
                });
                if let Err(e) = result {
                    errors.push(format!(
                        "{}: {e}",
                        crate::common::normalize_display_path(&previous.path)
                    ));
                }
            }
            return Err(failure(code, message, index, !written.is_empty(), errors));
        }
        written.push(index);
    }
    Ok(())
}

fn failure(code: &str, message: String, index: usize, rolled: bool, errors: Vec<String>) -> Value {
    json!({
        "success": false,
        "error_code": code,
        "message": message,
        "file_index": index,
        "rollback_performed": rolled,
        "rollback_complete": errors.is_empty(),
        "rollback_errors": errors
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{file_snapshot, missing_snapshot};

    fn change(path: PathBuf, before: &[u8], after: &[u8]) -> PathChange {
        std::fs::write(&path, before).unwrap();
        PathChange {
            canonical: GUARD.check_path(&path),
            path,
            before: file_snapshot(before.to_vec(), None, None),
            after: file_snapshot(after.to_vec(), None, None),
        }
    }

    #[test]
    fn failed_group_restore_rolls_back_earlier_restores() {
        let dir = tempfile::tempdir().unwrap();
        let changes = vec![
            change(dir.path().join("a"), b"edited-a", b"original-a"),
            change(dir.path().join("b"), b"edited-b", b"original-b"),
        ];
        let mut count = 0;
        let error = commit_with(&changes, &mut |path, before, after| {
            count += 1;
            if count == 2 {
                return Err(std::io::Error::other("injected restore failure"));
            }
            restore(path, before, after)
        })
        .unwrap_err();
        assert_eq!(error["rollback_complete"], true);
        assert_eq!(error["rollback_performed"], true);
        assert_eq!(std::fs::read(&changes[0].path).unwrap(), b"edited-a");
        assert_eq!(std::fs::read(&changes[1].path).unwrap(), b"edited-b");
    }

    #[test]
    fn rollback_refuses_independent_newer_contents() {
        let dir = tempfile::tempdir().unwrap();
        let changes = vec![
            change(dir.path().join("a"), b"before-a", b"after-a"),
            change(dir.path().join("b"), b"before-b", b"after-b"),
        ];
        let mut count = 0;
        let error = commit_with(&changes, &mut |path, before, after| {
            count += 1;
            if count == 2 {
                std::fs::write(&changes[0].path, b"independent writer")?;
                return Err(std::io::Error::other("injected failure"));
            }
            restore(path, before, after)
        })
        .unwrap_err();
        assert_eq!(error["rollback_complete"], false);
        assert_eq!(
            std::fs::read(&changes[0].path).unwrap(),
            b"independent writer"
        );
        assert_eq!(std::fs::read(&changes[1].path).unwrap(), b"before-b");
    }

    #[test]
    fn directory_creations_roll_back_in_reverse_depth_order_on_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let nested = parent.join("child");
        let target = nested.join("file.txt");
        let changes = vec![
            PathChange {
                canonical: GUARD.check_path(&parent),
                path: parent.clone(),
                before: missing_snapshot(),
                after: crate::history::directory_snapshot(),
            },
            PathChange {
                canonical: GUARD.check_path(&nested),
                path: nested.clone(),
                before: missing_snapshot(),
                after: crate::history::directory_snapshot(),
            },
            PathChange {
                canonical: GUARD.check_path(&target),
                path: target.clone(),
                before: missing_snapshot(),
                after: file_snapshot(b"bytes".to_vec(), None, None),
            },
        ];
        let mut attempts = 0;
        let error = commit_with(&changes, &mut |path, before, after| {
            attempts += 1;
            if attempts == 3 {
                return Err(std::io::Error::other("injected file publication failure"));
            }
            restore(path, before, after)
        })
        .unwrap_err();
        assert_eq!(error["rollback_performed"], true);
        assert_eq!(error["rollback_complete"], true);
        assert!(!parent.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn directory_removal_rejects_untracked_symlink_aliases_before_writes() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let file = parent.join("tracked");
        std::fs::write(&file, b"bytes").unwrap();
        std::os::unix::fs::symlink(&file, parent.join("alias")).unwrap();
        let changes = vec![
            PathChange {
                canonical: GUARD.check_path(&file),
                path: file.clone(),
                before: file_snapshot(b"bytes".to_vec(), None, None),
                after: missing_snapshot(),
            },
            PathChange {
                canonical: GUARD.check_path(&parent),
                path: parent.clone(),
                before: crate::history::directory_snapshot(),
                after: missing_snapshot(),
            },
        ];
        let mut attempts = 0;
        let error = commit_with(&changes, &mut |path, before, after| {
            attempts += 1;
            restore(path, before, after)
        })
        .unwrap_err();
        assert_eq!(attempts, 0);
        assert_eq!(error["error_code"], "concurrent_change");
        assert_eq!(std::fs::read(&file).unwrap(), b"bytes");
        assert!(parent.is_dir());
    }

    #[test]
    fn source_is_retained_when_destination_publication_fails() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"original").unwrap();
        let target = dir.path().join("target");
        let bytes = file_snapshot(b"original".to_vec(), None, None);
        let changes = vec![
            PathChange {
                path: source.clone(),
                canonical: GUARD.check_path(&source),
                before: bytes.clone(),
                after: missing_snapshot(),
            },
            PathChange {
                path: target.clone(),
                canonical: GUARD.check_path(&target),
                before: missing_snapshot(),
                after: bytes,
            },
        ];
        let error = commit_with(&changes, &mut |path, before, after| {
            if path == target {
                return Err(std::io::Error::other("destination failure"));
            }
            restore(path, before, after)
        })
        .unwrap_err();
        assert_eq!(error["rollback_performed"], false);
        assert_eq!(std::fs::read(&source).unwrap(), b"original");
        assert!(!target.exists());
    }
}

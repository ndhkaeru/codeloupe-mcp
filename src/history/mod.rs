use anyhow::Result;
use lazy_static::lazy_static;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use crate::common::ensure_object;
use crate::tools::read_file::decode_fuzzy;

const MAX_HISTORY_ENTRIES: usize = 200;
pub const MAX_HISTORY_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_TRACKED_SNAPSHOT_BYTES: usize = 10 * 1024 * 1024;

lazy_static! {
    static ref HISTORY_STATE: Mutex<HistoryState> = Mutex::new(HistoryState::default());
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotState {
    Missing,
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathSnapshot {
    pub state: SnapshotState,
    pub bytes: Option<Vec<u8>>,
    pub encoding_label: Option<String>,
    pub line_ending: Option<String>,
}

#[derive(Default)]
struct HistoryState {
    records: Vec<HistoryRecord>,
    next_entry_id: u64,
    retained_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct HistoryRecord {
    pub entry_id: String,
    pub tool_name: String,
    pub path: String,
    pub canonical_path: String,
    pub before: PathSnapshot,
    pub after: PathSnapshot,
    pub summary: String,
    pub outside_declared: bool,
}

#[derive(Clone, Debug)]
pub struct RecordOutcome {
    pub recorded: bool,
    pub entry_id: Option<String>,
    pub reason: Option<String>,
}

pub fn missing_snapshot() -> PathSnapshot {
    PathSnapshot {
        state: SnapshotState::Missing,
        bytes: None,
        encoding_label: None,
        line_ending: None,
    }
}

pub fn directory_snapshot() -> PathSnapshot {
    PathSnapshot {
        state: SnapshotState::Directory,
        bytes: None,
        encoding_label: None,
        line_ending: None,
    }
}

pub fn file_snapshot(
    bytes: Vec<u8>,
    encoding_label: Option<String>,
    line_ending: Option<String>,
) -> PathSnapshot {
    PathSnapshot {
        state: SnapshotState::File,
        bytes: Some(bytes),
        encoding_label,
        line_ending,
    }
}

pub fn capture_snapshot(path: &Path) -> Result<PathSnapshot, String> {
    if !path.exists() {
        return Ok(missing_snapshot());
    }

    if path.is_dir() {
        return Ok(directory_snapshot());
    }

    let metadata = fs::metadata(path).map_err(|err| format!("read metadata failed: {}", err))?;
    if metadata.len() as usize > MAX_TRACKED_SNAPSHOT_BYTES {
        return Err(format!(
            "file exceeds history snapshot limit ({} bytes > {} bytes)",
            metadata.len(),
            MAX_TRACKED_SNAPSHOT_BYTES
        ));
    }

    let bytes = fs::read(path).map_err(|err| format!("read file failed: {}", err))?;
    let (encoding_label, line_ending) = infer_text_metadata(&bytes);
    Ok(file_snapshot(bytes, encoding_label, line_ending))
}

fn infer_text_metadata(bytes: &[u8]) -> (Option<String>, Option<String>) {
    let (text, encoding) = decode_fuzzy(bytes);
    let has_crlf = text.contains("\r\n");
    let has_lf =
        text.as_bytes().iter().enumerate().any(|(index, byte)| {
            *byte == b'\n' && (index == 0 || text.as_bytes()[index - 1] != b'\r')
        });
    let line_ending = match (has_crlf, has_lf) {
        (false, false) => None,
        (false, true) => Some("lf".to_string()),
        (true, false) => Some("crlf".to_string()),
        (true, true) => Some("mixed".to_string()),
    };

    (
        Some(normalize_encoding_label(encoding).to_string()),
        line_ending,
    )
}

fn normalize_encoding_label(raw: &str) -> &'static str {
    if raw.eq_ignore_ascii_case("utf-16le") {
        "UTF-16LE"
    } else if raw.eq_ignore_ascii_case("utf-16be") {
        "UTF-16BE"
    } else if raw.eq_ignore_ascii_case("windows-1252") {
        "WINDOWS-1252"
    } else {
        "UTF-8"
    }
}

pub fn record_change(
    tool_name: &str,
    path: &Path,
    before: PathSnapshot,
    after: PathSnapshot,
    summary: impl Into<String>,
) -> RecordOutcome {
    if let Some(reason) = untrackable_reason(&before).or_else(|| untrackable_reason(&after)) {
        return RecordOutcome {
            recorded: false,
            entry_id: None,
            reason: Some(reason),
        };
    }

    let mut guard = match HISTORY_STATE.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return RecordOutcome {
                recorded: false,
                entry_id: None,
                reason: Some("history state is unavailable".to_string()),
            };
        }
    };

    let entry_id = format!("h{}", guard.next_entry_id);
    guard.next_entry_id += 1;

    let record = HistoryRecord {
        entry_id: entry_id.clone(),
        tool_name: tool_name.to_string(),
        path: path.to_string_lossy().to_string(),
        canonical_path: crate::common::canonicalize_with_existing_ancestor(path)
            .to_string_lossy()
            .to_string(),
        before,
        after,
        summary: summary.into(),
        outside_declared: crate::security::path_guard::GUARD
            .classify_path(path)
            .outside_declared,
    };
    guard.push_record(record, MAX_HISTORY_ENTRIES, MAX_HISTORY_BYTES);

    RecordOutcome {
        recorded: true,
        entry_id: Some(entry_id),
        reason: None,
    }
}

fn untrackable_reason(snapshot: &PathSnapshot) -> Option<String> {
    match (&snapshot.state, snapshot.bytes.as_ref()) {
        (SnapshotState::File, Some(bytes)) if bytes.len() > MAX_TRACKED_SNAPSHOT_BYTES => {
            Some(format!(
                "file exceeds history snapshot limit ({} bytes > {} bytes)",
                bytes.len(),
                MAX_TRACKED_SNAPSHOT_BYTES
            ))
        }
        (SnapshotState::File, None) => Some("file snapshot missing bytes".to_string()),
        _ => None,
    }
}

pub fn attach_history_metadata(response: &mut Value, outcome: &RecordOutcome) {
    let object = ensure_object(response);
    object.insert("history_recorded".to_string(), json!(outcome.recorded));
    object.insert("history_entry_id".to_string(), json!(outcome.entry_id));
    object.insert("history_reason".to_string(), json!(outcome.reason));
}

pub fn no_history(reason: impl Into<String>) -> RecordOutcome {
    RecordOutcome {
        recorded: false,
        entry_id: None,
        reason: Some(reason.into()),
    }
}

fn snapshot_bytes(snapshot: &PathSnapshot) -> usize {
    snapshot.bytes.as_ref().map(Vec::len).unwrap_or(0)
}

fn record_bytes(record: &HistoryRecord) -> usize {
    snapshot_bytes(&record.before) + snapshot_bytes(&record.after)
}

impl HistoryState {
    fn push_record(&mut self, record: HistoryRecord, max_entries: usize, max_bytes: usize) {
        self.retained_bytes = self.retained_bytes.saturating_add(record_bytes(&record));
        self.records.push(record);
        while self.records.len() > max_entries || self.retained_bytes > max_bytes {
            if self.records.is_empty() {
                self.retained_bytes = 0;
                break;
            }
            let removed = self.records.remove(0);
            self.retained_bytes = self.retained_bytes.saturating_sub(record_bytes(&removed));
        }
    }
}

pub fn retained_history_bytes() -> usize {
    HISTORY_STATE
        .lock()
        .map(|guard| guard.retained_bytes)
        .unwrap_or(0)
}

pub fn list_records(limit: usize) -> (Vec<HistoryRecord>, usize) {
    HISTORY_STATE
        .lock()
        .map(|guard| {
            (
                guard.records.iter().rev().take(limit).cloned().collect(),
                guard.records.len(),
            )
        })
        .unwrap_or_default()
}

pub fn get_record(entry_id: &str) -> Option<HistoryRecord> {
    HISTORY_STATE.lock().ok().and_then(|guard| {
        guard
            .records
            .iter()
            .find(|record| record.entry_id == entry_id)
            .cloned()
    })
}

pub fn snapshots_match_contents(left: &PathSnapshot, right: &PathSnapshot) -> bool {
    left.state == right.state && left.bytes == right.bytes
}

pub fn clear_history() {
    if let Ok(mut guard) = HISTORY_STATE.lock() {
        guard.records.clear();
        guard.next_entry_id = 0;
        guard.retained_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use tempfile::tempdir;

    static HISTORY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn history_test_guard() -> MutexGuard<'static, ()> {
        HISTORY_TEST_LOCK.lock().unwrap()
    }

    #[test]
    fn record_change_limits_stack() {
        let _guard = history_test_guard();
        clear_history();
        let dir = tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        let before = missing_snapshot();
        let after = file_snapshot(
            b"hello".to_vec(),
            Some("UTF-8".to_string()),
            Some("lf".to_string()),
        );

        let first = record_change("create_file", &path, before, after, "create");
        assert!(first.recorded);
        assert!(first.entry_id.is_some());
    }

    #[test]
    fn oversized_snapshot_is_skipped() {
        let _guard = history_test_guard();
        clear_history();
        let path = Path::new("large.txt");
        let oversized = vec![b'x'; MAX_TRACKED_SNAPSHOT_BYTES + 1];
        let outcome = record_change(
            "create_file",
            path,
            missing_snapshot(),
            file_snapshot(oversized, Some("UTF-8".to_string()), Some("lf".to_string())),
            "large write",
        );
        assert!(!outcome.recorded);
        assert!(outcome.reason.unwrap().contains("history snapshot limit"));
    }

    #[test]
    fn history_state_evicts_oldest_records_by_byte_budget() {
        let _guard = history_test_guard();
        let mut state = HistoryState::default();
        for index in 0..3 {
            state.push_record(
                HistoryRecord {
                    entry_id: format!("h{index}"),
                    tool_name: "test".to_string(),
                    path: format!("file-{index}.txt"),
                    canonical_path: format!("file-{index}.txt"),
                    before: file_snapshot(vec![b'a'; 4], None, None),
                    after: file_snapshot(vec![b'b'; 4], None, None),
                    summary: "test".to_string(),
                    outside_declared: false,
                },
                10,
                16,
            );
        }

        assert_eq!(state.retained_bytes, 16);
        assert_eq!(state.records.len(), 2);
        assert_eq!(state.records[0].entry_id, "h1");
    }
}

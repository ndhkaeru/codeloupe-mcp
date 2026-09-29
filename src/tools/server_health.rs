use crate::common::unix_timestamp_secs;
use crate::indexer::{
    IndexRuntimeSnapshot, default_index_max_age_secs, default_index_max_total_bytes,
    default_index_orphan_grace_secs, directory_size, get_active_runtime_snapshot,
    get_runtime_snapshots, index_storage_snapshot, stale_index_after_secs,
};
use crate::tools::text_search::search_telemetry;
use crate::version::SERVER_VERSION;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

lazy_static::lazy_static! {
    pub static ref START_TIME: u64 = unix_timestamp_secs();
}

pub fn schema() -> Value {
    json!({
        "name": "server_health",
        "title": "Check server health",
        "description": "Check server uptime, workspace roots, and indexing health. Use before broad searches in large repos: each index_workspaces entry has a path_index (powers fuzzy/path tools) and a content_index (powers text_search shortlisting and may only cover listed zones).",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    })
}

pub async fn execute(_args: &Value) -> Result<Value> {
    let now = unix_timestamp_secs();
    let uptime_secs = now - *START_TIME;

    let runtimes = get_runtime_snapshots();
    let active_runtime = get_active_runtime_snapshot();
    let index_storage = index_storage_snapshot();
    let total_indexed_entries: usize = runtimes
        .iter()
        .map(|runtime| runtime.indexed_entries_count)
        .sum();
    let index_status = if runtimes.is_empty() {
        "disabled"
    } else if total_indexed_entries > 0 {
        "active"
    } else {
        "idle"
    };
    let write_session = crate::workspace_control::write_session_snapshot();

    // Each workspace is described once, in `index_workspaces`. The previous
    // layout repeated the active workspace as ~40 flat fields plus two nested
    // copies, and reported a per-workspace index size that was always 0.
    Ok(json!({
        "status": "healthy",
        "uptime_seconds": uptime_secs,
        "version": SERVER_VERSION,
        "transport": "stdio (JSON-RPC)",
        "index_mode": crate::workspace_control::index_mode().as_str(),
        "write_scope": write_session.write_scope,
        "write_elicitation_supported": write_session.elicitation_supported,
        "write_approved_directories": write_session.approved_directories,
        "write_declined_directories": write_session.declined_directories,
        "configured_workspaces": crate::workspace_control::configured_workspace_snapshots(),
        "active_workspace_root": crate::workspace_control::active_workspace()
            .map(|(root, _)| crate::common::normalize_display_path(&root)),
        "active_workspace_source": crate::workspace_control::active_workspace()
            .map(|(_, source)| source),
        "index_candidates": crate::workspace_control::candidate_snapshots(),
        "index_status": index_status,
        "index_workspace_count": runtimes.len(),
        "indexed_entries_count": total_indexed_entries,
        "index_stale_after_seconds": stale_index_after_secs(),
        "index_storage_root": index_storage.root,
        "index_storage_size_bytes": index_storage.total_size_bytes,
        "index_storage_entry_count": index_storage.entry_count,
        "index_storage_loaded_count": index_storage.loaded_count,
        "index_storage_unloaded_count": index_storage.unloaded_count,
        "index_storage_orphaned_count": index_storage.orphaned_count,
        "index_gc_policy": {
            "max_age_seconds": default_index_max_age_secs(),
            "orphan_grace_seconds": default_index_orphan_grace_secs(),
            "max_total_bytes": default_index_max_total_bytes(),
            "min_free_bytes": crate::indexer::index_min_free_bytes(),
            "max_scan_entries": crate::indexer::index_max_entries(),
            "max_scan_seconds": crate::indexer::index_max_scan_seconds(),
            "max_loaded_workspaces": crate::indexer::index_max_loaded_workspaces()
        },
        "active_index_workspace_root": active_runtime.as_ref().map(|runtime| runtime.workspace_root.clone()),
        "active_index_workspace_source": active_runtime.as_ref().map(|runtime| runtime.workspace_source.clone()),
        "index_workspaces": runtimes.iter().map(workspace_summary).collect::<Vec<_>>(),
        "search_telemetry": search_telemetry()
    }))
}

fn workspace_summary(runtime: &IndexRuntimeSnapshot) -> Value {
    let now = unix_timestamp_secs();
    let content_index_age_secs = runtime
        .content_zone_indexed_at
        .values()
        .map(|indexed_at| now.saturating_sub(*indexed_at))
        .max();
    json!({
        "workspace_root": runtime.workspace_root,
        "workspace_source": runtime.workspace_source,
        "last_request_source": runtime.last_request_source,
        "refresh_running": runtime.refresh_running,
        "last_error": runtime.last_error,
        "path_index": {
            "status": runtime.metadata_index_status,
            "entries": runtime.indexed_entries_count,
            "files": runtime.indexed_files_count,
            "dirs": runtime.indexed_dirs_count,
            "scan_complete": runtime.scan_complete,
            "loaded_from_disk": runtime.loaded_from_disk,
            "last_scan_completed_at": runtime.last_scan_completed_at
        },
        "content_index": {
            "backend": runtime.content_index_backend,
            "status": runtime.content_index_status,
            "zones": runtime.content_index_zones,
            "zone_indexed_at": runtime.content_zone_indexed_at,
            "index_age_secs": content_index_age_secs,
            "partial": runtime.content_index_partial,
            "files": runtime.indexed_content_files,
            "bytes": runtime.indexed_content_bytes
        },
        "storage_dir": runtime.index_storage_dir,
        "storage_size_bytes": directory_size(Path::new(&runtime.index_storage_dir))
    })
}

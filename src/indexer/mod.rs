use dashmap::DashMap;
use fs2::{available_space, total_space};
use heed::types::{SerdeBincode, SerdeJson, Str};
use heed::{Database, Env, EnvOpenOptions};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tantivy::collector::TopDocs;
use tantivy::directory::error::{LockError, OpenWriteError};
use tantivy::query::QueryParser;
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TantivyDocument, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyError, Term, doc};
use tracing::{error, info, warn};

use crate::common::{
    INDEX_DIR_ENV_VARS, TANTIVY_ENABLED_ENV_VARS, bounded_walk_threads, env_var_os,
};

lazy_static::lazy_static! {
    static ref INDEX_RUNTIMES: RwLock<BTreeMap<String, IndexRuntime>> = RwLock::new(BTreeMap::new());
    static ref PATH_INDEXES: DashMap<String, Arc<RwLock<PathIndex>>> = DashMap::new();
    static ref INDEX_STORES: DashMap<String, Arc<IndexStore>> = DashMap::new();
    static ref INDEX_STORE_OPEN_LOCK: Mutex<()> = Mutex::new(());
    static ref RUNTIME_LOAD_LOCK: Mutex<()> = Mutex::new(());
    static ref ACTIVE_INDEX_LOADS: DashMap<String, ()> = DashMap::new();
    static ref DISABLED_WORKSPACES: DashMap<String, ()> = DashMap::new();
    static ref TANTIVY_SEARCHERS: DashMap<String, Arc<TantivySearchStore>> = DashMap::new();
    static ref TANTIVY_WRITE_LOCKS: DashMap<String, Arc<Mutex<()>>> = DashMap::new();
    static ref ACTIVE_REFRESHES: DashMap<String, ()> = DashMap::new();
    static ref ACTIVE_CONTENT_REFRESHES: DashMap<String, ()> = DashMap::new();
    static ref ACTIVE_WORKSPACE_KEY: RwLock<Option<String>> = RwLock::new(None);
}

const INDEX_SCHEMA_VERSION: u32 = 2;
const DEFAULT_STALE_INDEX_SECS: u64 = 60 * 60;
const MIN_REFRESH_INTERVAL_SECS: u64 = 30;
const MIN_INDEX_TERM_LEN: usize = 3;
const MAX_SHORTLIST_CANDIDATES: usize = 8_192;
pub const LARGE_WORKSPACE_FILE_THRESHOLD: usize = 50_000;
const DEFAULT_INDEX_MAP_SIZE_MB: u64 = 8_192;
const DEFAULT_TANTIVY_MAX_FILE_BYTES: u64 = 2_097_152;
const DEFAULT_TANTIVY_MAX_ZONE_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_TANTIVY_MAX_WORKSPACE_BYTES: u64 = 256 * 1024 * 1024;
const TANTIVY_WRITER_MEMORY_BYTES_PER_THREAD: usize = 16 * 1024 * 1024;
const TANTIVY_WRITER_RETRY_DELAY_MS: u64 = 100;
const TANTIVY_WRITER_RETRY_TIMEOUT_SECS: u64 = 30;
const CONTENT_WRITER_LOCK_FILE: &str = "content-writer.lock";
const INDEX_ACTIVE_LOCK_DIR: &str = ".locks";
const INDEX_LAST_USED_FILE: &str = "last-used";
const DEFAULT_INDEX_MAX_AGE_SECS: u64 = 90 * 24 * 60 * 60;
const DEFAULT_INDEX_ORPHAN_GRACE_SECS: u64 = 7 * 24 * 60 * 60;
const DEFAULT_INDEX_MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_INDEX_MAX_ENTRIES: usize = 300_000;
const DEFAULT_INDEX_MAX_SCAN_SECONDS: u64 = 60;
const DEFAULT_INDEX_MIN_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const DEFAULT_INDEX_MAX_LOADED_WORKSPACES: usize = 16;
const ROOT_CHILDREN_KEY: &str = ".";
static RUNTIME_ACCESS_SEQUENCE: AtomicU64 = AtomicU64::new(1);

type MetaDb = Database<Str, SerdeBincode<WorkspaceMeta>>;
type PathDb = Database<Str, SerdeBincode<IndexedPathEntry>>;
type ChildrenDb = Database<Str, SerdeBincode<Vec<String>>>;

#[derive(Debug, Clone)]
struct IndexRuntime {
    workspace_root: PathBuf,
    workspace_source: String,
    storage_dir: PathBuf,
    index_file: PathBuf,
    loaded_from_disk: bool,
    scan_complete: bool,
    refresh_running: bool,
    last_loaded_entries: usize,
    last_persisted_entries: usize,
    last_persisted_at: Option<u64>,
    last_scan_completed_at: Option<u64>,
    last_refresh_requested_at: Option<u64>,
    last_refresh_started_at: Option<u64>,
    last_refresh_completed_at: Option<u64>,
    last_access_sequence: u64,
    last_request_source: Option<String>,
    last_error: Option<String>,
    indexed_entries_count: usize,
    indexed_files_count: usize,
    indexed_dirs_count: usize,
    content_index_enabled: bool,
    content_index_status: String,
    content_index_zones: Vec<String>,
    content_zone_indexed_at: BTreeMap<String, u64>,
    content_index_partial: bool,
    indexed_content_files: usize,
    indexed_content_bytes: u64,
    index_map_size_bytes: u64,
    index_size_bytes: u64,
    _active_lock: Arc<File>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRuntimeSnapshot {
    pub workspace_root: String,
    pub workspace_source: String,
    pub index_file: String,
    pub loaded_from_disk: bool,
    pub scan_complete: bool,
    pub last_loaded_entries: usize,
    pub last_persisted_entries: usize,
    pub last_persisted_at: Option<u64>,
    pub last_scan_completed_at: Option<u64>,
    pub last_refresh_requested_at: Option<u64>,
    pub last_request_source: Option<String>,
    pub last_error: Option<String>,
    pub indexed_entries_count: usize,
    pub cached_files_count: usize,
    pub index_kind: &'static str,
    pub index_status: String,
    pub metadata_index_backend: &'static str,
    pub content_index_backend: &'static str,
    pub metadata_index_status: String,
    pub content_index_status: String,
    pub content_index_zones: Vec<String>,
    pub content_zone_indexed_at: BTreeMap<String, u64>,
    pub content_index_partial: bool,
    pub indexed_content_files: usize,
    pub indexed_content_bytes: u64,
    pub index_storage_dir: String,
    pub index_map_size_bytes: u64,
    pub index_size_bytes: u64,
    pub indexed_files_count: usize,
    pub indexed_dirs_count: usize,
    pub refresh_running: bool,
    pub last_refresh_started_at: Option<u64>,
    pub last_refresh_completed_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathIndexAge {
    pub root: String,
    pub indexed_at: Option<u64>,
    pub index_age_secs: Option<u64>,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct WorkspaceMeta {
    schema_version: u32,
    workspace_root: String,
    saved_at: u64,
    scan_complete: bool,
    indexed_entries_count: usize,
    indexed_files_count: usize,
    indexed_dirs_count: usize,
    last_full_scan_at: Option<u64>,
    content_index_enabled: bool,
    content_index_status: String,
    content_index_zones: Vec<String>,
    content_index_partial: bool,
    indexed_content_files: usize,
    indexed_content_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexedPathEntry {
    relative_path: String,
    is_dir: bool,
    size: u64,
    modified_at: u64,
    extension_lower: String,
    parent_relative_path: String,
    indexed_at: u64,
}

#[derive(Debug, Clone)]
struct PathIndexEntry {
    absolute_path: PathBuf,
    relative_path: String,
    extension_lower: String,
    is_dir: bool,
    size: u64,
    modified_at: u64,
}

#[derive(Debug, Default)]
struct PathIndex {
    entries: Vec<Option<PathIndexEntry>>,
    path_lookup: HashMap<String, usize>,
    term_postings: HashMap<String, Vec<usize>>,
    live_entries: usize,
}

#[derive(Debug, Clone)]
pub struct PathQueryCandidate {
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified_at: u64,
}

#[derive(Debug, Clone)]
pub struct IndexedPathRecord {
    pub path: PathBuf,
    pub relative_path: String,
    pub file_name: String,
    pub extension_lower: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_at: u64,
}

#[derive(Debug, Clone)]
pub struct ContentCandidateResult {
    pub paths: Vec<PathBuf>,
    pub content_index_used: bool,
    pub content_index_partial: bool,
    pub zones: Vec<String>,
    pub warming_zones: Vec<String>,
    pub fallback_reasons: Vec<String>,
    pub candidate_count: usize,
    pub candidate_limit: usize,
    pub candidates_truncated: bool,
    pub zone_indexed_at: Vec<ContentZoneAge>,
    pub index_age_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentZoneAge {
    pub workspace_root: String,
    pub zone: String,
    pub indexed_at: u64,
    pub index_age_secs: u64,
}

struct TantivyZoneSearchResult {
    paths: Vec<PathBuf>,
    truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentZoneStatus {
    pub input_path: String,
    pub workspace_root: Option<String>,
    pub zone: Option<String>,
    pub status: String,
    pub ready: bool,
    pub warming: bool,
    pub indexed: bool,
    pub partial: bool,
    pub indexed_at: Option<u64>,
    pub index_age_secs: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexStorageEntry {
    pub workspace_root: Option<String>,
    pub storage_dir: String,
    pub size_bytes: u64,
    pub last_used_at: u64,
    pub age_secs: u64,
    pub workspace_exists: bool,
    pub orphaned: bool,
    pub loaded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexStorageSnapshot {
    pub root: String,
    pub total_size_bytes: u64,
    pub entry_count: usize,
    pub loaded_count: usize,
    pub unloaded_count: usize,
    pub orphaned_count: usize,
    pub entries: Vec<IndexStorageEntry>,
}

#[derive(Debug, Clone)]
pub struct IndexGcOptions {
    pub apply: bool,
    pub remove_orphans: bool,
    pub max_age_secs: Option<u64>,
    pub max_total_bytes: Option<u64>,
    pub workspace_roots: Vec<PathBuf>,
    pub max_results: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexGcCandidate {
    pub workspace_root: Option<String>,
    pub storage_dir: String,
    pub size_bytes: u64,
    pub last_used_at: u64,
    pub reasons: Vec<String>,
    pub outcome: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexGcReport {
    pub apply: bool,
    pub storage_root: String,
    pub storage_size_before_bytes: u64,
    pub projected_size_after_bytes: u64,
    pub storage_size_after_bytes: u64,
    pub scanned_entries: usize,
    pub selected_entries: usize,
    pub deleted_entries: usize,
    pub freed_bytes: u64,
    pub skipped_loaded: usize,
    pub skipped_locked: usize,
    pub errors: Vec<String>,
    pub candidates_truncated: bool,
    pub candidates: Vec<IndexGcCandidate>,
}

struct IndexStore {
    env: Env,
    meta_db: MetaDb,
    path_db: PathDb,
    children_db: ChildrenDb,
    storage_dir: PathBuf,
    tantivy_dir: PathBuf,
}

struct TantivySearchStore {
    index: Index,
    reader: IndexReader,
    fields: TantivyFields,
}

struct ActiveIndexLoadGuard(String);

impl Drop for ActiveIndexLoadGuard {
    fn drop(&mut self) {
        ACTIVE_INDEX_LOADS.remove(&self.0);
    }
}

#[derive(Clone, Copy)]
struct TantivyFields {
    relative_path: Field,
    file_name: Field,
    path_tokens: Field,
    content: Field,
    extension: Field,
    size: Field,
    modified_at: Field,
}

pub fn get_runtime_snapshots() -> Vec<IndexRuntimeSnapshot> {
    match INDEX_RUNTIMES.read() {
        Ok(guard) => guard
            .values()
            .cloned()
            .map(runtime_snapshot_from_state)
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub fn get_active_runtime_snapshot() -> Option<IndexRuntimeSnapshot> {
    let active_key = match ACTIVE_WORKSPACE_KEY.read() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    }?;

    match INDEX_RUNTIMES.read() {
        Ok(guard) => guard
            .get(&active_key)
            .cloned()
            .map(runtime_snapshot_from_state),
        Err(_) => None,
    }
}

pub fn path_index_ages_for_paths(paths: &[PathBuf]) -> Vec<PathIndexAge> {
    let canonical_paths = paths
        .iter()
        .cloned()
        .map(canonicalize_or_original)
        .collect::<Vec<_>>();
    let now = current_unix_timestamp();
    let Ok(guard) = INDEX_RUNTIMES.read() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut ages = Vec::new();

    for path in canonical_paths {
        let Some(state) = guard
            .values()
            .filter(|state| {
                (state.scan_complete || state.indexed_entries_count > 0)
                    && path_belongs_to_workspace(&state.workspace_root, &path)
            })
            .min_by_key(|state| state.workspace_root.components().count())
        else {
            continue;
        };
        let root = crate::common::normalize_display_path(&state.workspace_root);
        if !seen.insert(root.clone()) {
            continue;
        }
        let indexed_at = state.last_scan_completed_at.or(state.last_persisted_at);
        ages.push(PathIndexAge {
            root,
            indexed_at,
            index_age_secs: indexed_at.map(|timestamp| now.saturating_sub(timestamp)),
            complete: state.scan_complete,
        });
    }

    ages.sort_by(|left, right| left.root.cmp(&right.root));
    ages
}

pub fn indexed_workspace_root_for_path(path: &Path) -> Option<PathBuf> {
    let canonical_path = canonicalize_or_original(path.to_path_buf());
    match INDEX_RUNTIMES.read() {
        Ok(guard) => guard
            .values()
            .filter(|state| path_belongs_to_workspace(&state.workspace_root, &canonical_path))
            .min_by_key(|state| state.workspace_root.components().count())
            .map(|state| state.workspace_root.clone()),
        Err(_) => None,
    }
}

pub fn is_path_index_ready(path: &Path) -> bool {
    let canonical_path = canonicalize_or_original(path.to_path_buf());
    match INDEX_RUNTIMES.read() {
        Ok(guard) => guard.values().any(|state| {
            state.scan_complete && path_belongs_to_workspace(&state.workspace_root, &canonical_path)
        }),
        Err(_) => false,
    }
}

pub fn is_path_index_available(path: &Path) -> bool {
    let canonical_path = canonicalize_or_original(path.to_path_buf());
    match INDEX_RUNTIMES.read() {
        Ok(guard) => guard.values().any(|state| {
            (state.scan_complete || state.indexed_entries_count > 0)
                && path_belongs_to_workspace(&state.workspace_root, &canonical_path)
        }),
        Err(_) => false,
    }
}

pub fn indexed_workspace_file_count(path: &Path) -> Option<usize> {
    let canonical_path = canonicalize_or_original(path.to_path_buf());
    match INDEX_RUNTIMES.read() {
        Ok(guard) => guard
            .values()
            .filter(|state| path_belongs_to_workspace(&state.workspace_root, &canonical_path))
            .max_by_key(|state| state.workspace_root.components().count())
            .map(|state| state.indexed_files_count),
        Err(_) => None,
    }
}

pub fn query_path_candidates(
    search_root: &Path,
    pattern: &str,
    shortlist_limit: usize,
) -> Option<Vec<PathQueryCandidate>> {
    let canonical_root = canonicalize_or_original(search_root.to_path_buf());
    let (workspace_key, workspace_root) = indexed_workspace_for_path(&canonical_root)?;
    let relative_root = relative_root_prefix(&workspace_root, &canonical_root)?;
    let anchor_terms = query_anchor_terms(pattern);
    if anchor_terms.is_empty() {
        return None;
    }

    let index = PATH_INDEXES.get(&workspace_key)?.value().clone();
    let guard = index.read().ok()?;
    guard.shortlist_candidates(&anchor_terms, relative_root.as_deref(), shortlist_limit)
}

pub fn visit_indexed_entries_under(
    search_root: &Path,
    visitor: impl FnMut(IndexedPathRecord) -> bool,
) -> Option<usize> {
    let canonical_root = canonicalize_or_original(search_root.to_path_buf());
    let (workspace_key, workspace_root) = indexed_workspace_for_path(&canonical_root)?;
    let relative_root = relative_root_prefix(&workspace_root, &canonical_root)?;
    let index = PATH_INDEXES.get(&workspace_key)?.value().clone();
    let guard = index.read().ok()?;
    Some(guard.visit_records_under(relative_root.as_deref(), visitor))
}

pub fn query_tantivy_content_candidates(
    search_paths: &[PathBuf],
    query: &str,
    limit: usize,
) -> ContentCandidateResult {
    let mut fallback_reasons = Vec::new();
    if !tantivy_enabled() {
        fallback_reasons.push("content_index_disabled".to_string());
    }
    if !is_tantivy_query_compatible(query) {
        fallback_reasons.push("query_not_tantivy_compatible".to_string());
    }
    if limit == 0 {
        fallback_reasons.push("candidate_limit_zero".to_string());
    }
    if !fallback_reasons.is_empty() {
        return ContentCandidateResult {
            paths: Vec::new(),
            content_index_used: false,
            content_index_partial: false,
            zones: Vec::new(),
            warming_zones: Vec::new(),
            fallback_reasons,
            candidate_count: 0,
            candidate_limit: limit,
            candidates_truncated: false,
            zone_indexed_at: Vec::new(),
            index_age_secs: None,
        };
    }

    let mut out = Vec::new();
    let mut zones = Vec::new();
    let mut warming_zones = Vec::new();
    let mut used = false;
    let mut partial = false;
    let mut candidates_truncated = false;
    let mut zone_indexed_at = Vec::new();

    for path in search_paths {
        let canonical_path = canonicalize_or_original(path.clone());
        let Some((workspace_key, workspace_root)) = registered_workspace_for_path(&canonical_path)
        else {
            fallback_reasons.push("path_not_in_indexed_workspace".to_string());
            continue;
        };
        let zone = content_zone_for_path(&workspace_root, &canonical_path);
        if zone.is_empty() {
            partial = true;
            fallback_reasons.push("root_scope_not_content_indexed".to_string());
            continue;
        }
        let covering_zone = covering_content_zone(&workspace_key, &zone);
        let ready_zone = ready_content_zone(&workspace_key, &zone);
        if ready_zone.is_none() {
            let refresh_zone = covering_zone.unwrap_or_else(|| zone.clone());
            schedule_content_zone_refresh(
                workspace_root,
                workspace_key,
                refresh_zone.clone(),
                false,
            );
            warming_zones.push(refresh_zone);
            partial = true;
            fallback_reasons.push("content_zone_not_ready".to_string());
            continue;
        }
        let ready_zone = ready_zone.unwrap_or(zone);
        if let Some(age) = content_zone_age(&workspace_key, &workspace_root, &ready_zone) {
            zone_indexed_at.push(age);
        }
        let explicit_ignored_scope =
            path_is_ignored_by_workspace_rules(&workspace_root, &canonical_path);

        let remaining = limit.saturating_sub(out.len());
        let Ok(search_result) = search_tantivy_zone(&workspace_root, query, remaining) else {
            partial = true;
            fallback_reasons.push("tantivy_search_error".to_string());
            continue;
        };
        used = true;
        if search_result.truncated {
            candidates_truncated = true;
        }
        let paths = search_result
            .paths
            .into_iter()
            .filter(|candidate| {
                candidate_belongs_to_search_path(candidate, &canonical_path)
                    && (explicit_ignored_scope
                        || path_index_contains(&workspace_key, &workspace_root, candidate))
            })
            .collect::<Vec<_>>();
        if !paths.is_empty() {
            out.extend(paths);
        }
        zones.push(ready_zone);
        if out.len() >= limit {
            candidates_truncated = true;
            break;
        }
    }

    out.sort();
    out.dedup();
    let candidate_count = out.len();
    if out.len() > limit {
        candidates_truncated = true;
    }
    out.truncate(limit);
    fallback_reasons.sort();
    fallback_reasons.dedup();
    warming_zones.sort();
    warming_zones.dedup();
    zones.sort();
    zones.dedup();
    zone_indexed_at.sort_by(|left, right| {
        left.workspace_root
            .cmp(&right.workspace_root)
            .then_with(|| left.zone.cmp(&right.zone))
    });
    zone_indexed_at.dedup_by(|left, right| {
        left.workspace_root == right.workspace_root && left.zone == right.zone
    });
    let index_age_secs = zone_indexed_at
        .iter()
        .map(|status| status.index_age_secs)
        .max();

    ContentCandidateResult {
        paths: out,
        content_index_used: used,
        content_index_partial: partial,
        zones,
        warming_zones,
        fallback_reasons,
        candidate_count,
        candidate_limit: limit,
        candidates_truncated,
        zone_indexed_at,
        index_age_secs,
    }
}

fn content_zone_age(
    workspace_key: &str,
    workspace_root: &Path,
    zone: &str,
) -> Option<ContentZoneAge> {
    let indexed_at = INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| guard.get(workspace_key).cloned())?
        .content_zone_indexed_at
        .get(zone)
        .copied()?;
    Some(ContentZoneAge {
        workspace_root: crate::common::normalize_display_path(workspace_root),
        zone: zone.to_string(),
        indexed_at,
        index_age_secs: current_unix_timestamp().saturating_sub(indexed_at),
    })
}

pub fn content_status_for_paths(paths: &[PathBuf]) -> Vec<ContentZoneStatus> {
    paths
        .iter()
        .map(|path| content_status_for_path(path.as_path()))
        .collect()
}

pub fn warm_content_index_paths(
    paths: &[PathBuf],
    force: bool,
    include_ignored: bool,
) -> Vec<ContentZoneStatus> {
    let mut out = Vec::new();
    for path in paths {
        let canonical_path = canonicalize_or_original(path.clone());
        let Some((workspace_key, workspace_root)) = registered_workspace_for_path(&canonical_path)
        else {
            out.push(content_status_for_path(path));
            continue;
        };
        let zone = content_zone_for_path(&workspace_root, &canonical_path);
        if zone.is_empty() {
            out.push(ContentZoneStatus {
                input_path: crate::common::normalize_display_path(path),
                workspace_root: Some(crate::common::normalize_display_path(&workspace_root)),
                zone: None,
                status: "root_scope_not_content_indexed".to_string(),
                ready: false,
                warming: false,
                indexed: false,
                partial: true,
                indexed_at: None,
                index_age_secs: None,
                last_error: None,
            });
            continue;
        }

        if !include_ignored && path_is_ignored_by_workspace_rules(&workspace_root, &canonical_path)
        {
            out.push(ContentZoneStatus {
                input_path: crate::common::normalize_display_path(path),
                workspace_root: Some(crate::common::normalize_display_path(&workspace_root)),
                zone: Some(zone),
                status: "ignored_by_ignore_rules".to_string(),
                ready: false,
                warming: false,
                indexed: false,
                partial: false,
                indexed_at: None,
                index_age_secs: None,
                last_error: None,
            });
            continue;
        }

        let refresh_zone =
            covering_content_zone(&workspace_key, &zone).unwrap_or_else(|| zone.clone());
        let current = content_status_for_path(path);
        if force || !current.ready {
            schedule_content_zone_refresh(
                workspace_root,
                workspace_key,
                refresh_zone,
                include_ignored,
            );
        }
        out.push(content_status_for_path(path));
    }
    out
}

pub fn notify_content_directory_changed(directory: &Path) {
    let canonical_path = canonicalize_or_original(directory.to_path_buf());
    let Some((workspace_key, workspace_root)) = registered_workspace_for_path(&canonical_path)
    else {
        return;
    };
    let zone = content_zone_for_path(&workspace_root, &canonical_path);
    if zone.is_empty() {
        return;
    }
    let Some(ready_zone) = ready_content_zone(&workspace_key, &zone) else {
        return;
    };
    schedule_content_zone_refresh(workspace_root, workspace_key, ready_zone, false);
}

fn content_status_for_path(path: &Path) -> ContentZoneStatus {
    let canonical_path = canonicalize_or_original(path.to_path_buf());
    let Some((workspace_key, workspace_root)) = registered_workspace_for_path(&canonical_path)
    else {
        return ContentZoneStatus {
            input_path: crate::common::normalize_display_path(path),
            workspace_root: None,
            zone: None,
            status: "path_not_in_indexed_workspace".to_string(),
            ready: false,
            warming: false,
            indexed: false,
            partial: false,
            indexed_at: None,
            index_age_secs: None,
            last_error: None,
        };
    };
    let zone = content_zone_for_path(&workspace_root, &canonical_path);
    if zone.is_empty() {
        return ContentZoneStatus {
            input_path: crate::common::normalize_display_path(path),
            workspace_root: Some(crate::common::normalize_display_path(&workspace_root)),
            zone: None,
            status: "root_scope_not_content_indexed".to_string(),
            ready: false,
            warming: false,
            indexed: false,
            partial: true,
            indexed_at: None,
            index_age_secs: None,
            last_error: None,
        };
    }

    match INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| guard.get(&workspace_key).cloned())
    {
        Some(state) => {
            let indexed_zone = covering_content_zone_in_state(&zone, &state);
            let active_zone = covering_active_content_zone(&workspace_key, &zone);
            let effective_zone = active_zone
                .as_deref()
                .or(indexed_zone.as_deref())
                .unwrap_or(&zone);
            let indexed = indexed_zone.is_some();
            let warming = active_zone.is_some();
            let ready =
                !warming && ready_content_zone_in_state(&workspace_key, &zone, &state).is_some();
            let status = if warming {
                "warming".to_string()
            } else if ready {
                "ready".to_string()
            } else if indexed {
                state.content_index_status.clone()
            } else if state.content_index_status == "writer_busy" {
                "writer_busy".to_string()
            } else if state.content_index_status == "error" {
                "error".to_string()
            } else if state.refresh_running
                || (!state.scan_complete && state.indexed_entries_count == 0)
            {
                "workspace_scanning".to_string()
            } else {
                "not_indexed".to_string()
            };
            let indexed_at = state.content_zone_indexed_at.get(effective_zone).copied();
            ContentZoneStatus {
                input_path: crate::common::normalize_display_path(path),
                workspace_root: Some(crate::common::normalize_display_path(&workspace_root)),
                zone: Some(effective_zone.to_string()),
                status,
                ready,
                warming,
                indexed,
                partial: state.content_index_partial,
                indexed_at,
                index_age_secs: indexed_at
                    .map(|timestamp| current_unix_timestamp().saturating_sub(timestamp)),
                last_error: state.last_error.clone(),
            }
        }
        None => ContentZoneStatus {
            input_path: crate::common::normalize_display_path(path),
            workspace_root: Some(crate::common::normalize_display_path(&workspace_root)),
            zone: Some(zone),
            status: "workspace_runtime_unavailable".to_string(),
            ready: false,
            warming: false,
            indexed: false,
            partial: false,
            indexed_at: None,
            index_age_secs: None,
            last_error: None,
        },
    }
}

pub fn stale_index_after_secs() -> u64 {
    DEFAULT_STALE_INDEX_SECS
}

pub fn schedule_workspace_index(workspace_root: PathBuf, workspace_source: String) {
    let workspace_root = canonicalize_or_original(workspace_root);
    if !workspace_root.exists() || !workspace_root.is_dir() {
        return;
    }
    let workspace_key = normalize_path_for_identity(&workspace_root);
    DISABLED_WORKSPACES.remove(&workspace_key);
    record_runtime_access(&workspace_key);
    if ACTIVE_INDEX_LOADS
        .insert(workspace_key.clone(), ())
        .is_some()
    {
        return;
    }

    let worker_key = workspace_key.clone();
    let spawn_result = thread::Builder::new()
        .name("codeloupe-index-load".to_string())
        .spawn(move || {
            let _guard = ActiveIndexLoadGuard(worker_key);
            ensure_workspace_index(workspace_root, workspace_source);
        });
    if let Err(error) = spawn_result {
        ACTIVE_INDEX_LOADS.remove(&workspace_key);
        warn!(error = %error, "Failed to schedule workspace index loading");
    }
}

pub fn ensure_workspace_index(workspace_root: PathBuf, workspace_source: String) {
    let mut workspace_root = canonicalize_or_original(workspace_root);
    if !workspace_root.exists() || !workspace_root.is_dir() {
        return;
    }

    if let Some((_, registered_root)) = registered_workspace_for_path(&workspace_root) {
        workspace_root = registered_root;
    }

    let workspace_key = normalize_path_for_identity(&workspace_root);
    if DISABLED_WORKSPACES.contains_key(&workspace_key) {
        return;
    }
    set_active_workspace(&workspace_key);

    let first_seen = ensure_runtime_loaded(&workspace_key, &workspace_root, &workspace_source);
    record_request_source(&workspace_key, &workspace_source);
    let storage_dir = index_storage_dir_for_workspace(&workspace_root);
    if storage_dir.is_dir()
        && let Err(err) = touch_index_usage(&storage_dir)
    {
        warn!(
            workspace = %workspace_root.display(),
            error = %err,
            "Failed to update index last-used timestamp"
        );
    }

    let now = current_unix_timestamp();
    let should_refresh = match INDEX_RUNTIMES.read() {
        Ok(guard) => guard.get(&workspace_key).is_none_or(|state| {
            if state.refresh_running {
                return false;
            }
            if !state.scan_complete || state.indexed_entries_count == 0 {
                return true;
            }
            state
                .last_scan_completed_at
                .map(|timestamp| now.saturating_sub(timestamp) >= stale_index_after_secs())
                .unwrap_or(true)
        }),
        Err(_) => first_seen,
    };

    if should_refresh && refresh_interval_elapsed(&workspace_key, now) {
        record_refresh_request(&workspace_key, &workspace_source, now);
        spawn_full_metadata_refresh(workspace_root, workspace_key);
    }
}

fn runtime_snapshot_from_state(state: IndexRuntime) -> IndexRuntimeSnapshot {
    let metadata_status = if state.refresh_running {
        "refreshing".to_string()
    } else if state.scan_complete {
        "complete".to_string()
    } else if state.indexed_entries_count > 0 {
        "partial".to_string()
    } else if state.last_error.is_some() {
        "error".to_string()
    } else {
        "idle".to_string()
    };

    IndexRuntimeSnapshot {
        workspace_root: crate::common::normalize_display_path(&state.workspace_root),
        workspace_source: state.workspace_source,
        index_file: crate::common::normalize_display_path(&state.index_file),
        loaded_from_disk: state.loaded_from_disk,
        scan_complete: state.scan_complete,
        last_loaded_entries: state.last_loaded_entries,
        last_persisted_entries: state.last_persisted_entries,
        last_persisted_at: state.last_persisted_at,
        last_scan_completed_at: state.last_scan_completed_at,
        last_refresh_requested_at: state.last_refresh_requested_at,
        last_request_source: state.last_request_source,
        last_error: state.last_error,
        indexed_entries_count: state.indexed_entries_count,
        cached_files_count: state.indexed_entries_count,
        index_kind: "path",
        index_status: if state.indexed_entries_count > 0 {
            "active".to_string()
        } else {
            "idle".to_string()
        },
        metadata_index_backend: "heed_lmdb",
        content_index_backend: if state.content_index_enabled {
            "tantivy"
        } else {
            "disabled"
        },
        metadata_index_status: metadata_status,
        content_index_status: state.content_index_status,
        content_index_zones: state.content_index_zones,
        content_zone_indexed_at: state.content_zone_indexed_at,
        content_index_partial: state.content_index_partial,
        indexed_content_files: state.indexed_content_files,
        indexed_content_bytes: state.indexed_content_bytes,
        index_storage_dir: crate::common::normalize_display_path(&state.storage_dir),
        index_map_size_bytes: state.index_map_size_bytes,
        index_size_bytes: state.index_size_bytes,
        indexed_files_count: state.indexed_files_count,
        indexed_dirs_count: state.indexed_dirs_count,
        refresh_running: state.refresh_running,
        last_refresh_started_at: state.last_refresh_started_at,
        last_refresh_completed_at: state.last_refresh_completed_at,
    }
}

fn ensure_runtime_loaded(
    workspace_key: &str,
    workspace_root: &Path,
    workspace_source: &str,
) -> bool {
    let _load_guard = match RUNTIME_LOAD_LOCK.lock() {
        Ok(guard) => guard,
        Err(_) => {
            warn!(workspace = %workspace_root.display(), "Runtime load lock is poisoned");
            return false;
        }
    };
    if DISABLED_WORKSPACES.contains_key(workspace_key) {
        return false;
    }
    if INDEX_RUNTIMES
        .read()
        .ok()
        .is_some_and(|guard| guard.contains_key(workspace_key))
    {
        record_runtime_access(workspace_key);
        return false;
    }

    if !evict_lru_runtime_for_new_workspace(workspace_key) {
        warn!(
            workspace = %workspace_root.display(),
            max_loaded_workspaces = index_max_loaded_workspaces(),
            "Deferred index loading because all runtime cache slots are busy"
        );
        return false;
    }

    #[cfg(debug_assertions)]
    if let Ok(delay_ms) = std::env::var("CODELOUPE_MCP_TEST_INDEX_LOAD_DELAY_MS")
        && let Ok(delay_ms) = delay_ms.parse::<u64>()
        && delay_ms > 0
    {
        thread::sleep(Duration::from_millis(delay_ms));
    }

    let storage_dir = index_storage_dir_for_workspace(workspace_root);
    let index_file = storage_dir.join("data.mdb");
    let map_size_bytes = index_map_size_bytes();
    let active_lock = match acquire_index_active_lock(&storage_dir) {
        Ok(lock) => lock,
        Err(err) => {
            warn!(
                workspace = %workspace_root.display(),
                error = %err,
                "Failed to acquire shared index activity lock"
            );
            return false;
        }
    };

    PATH_INDEXES.insert(
        workspace_key.to_string(),
        Arc::new(RwLock::new(PathIndex::default())),
    );

    let mut runtime = IndexRuntime {
        workspace_root: workspace_root.to_path_buf(),
        workspace_source: workspace_source.to_string(),
        storage_dir: storage_dir.clone(),
        index_file,
        loaded_from_disk: false,
        scan_complete: false,
        refresh_running: false,
        last_loaded_entries: 0,
        last_persisted_entries: 0,
        last_persisted_at: None,
        last_scan_completed_at: None,
        last_refresh_requested_at: None,
        last_refresh_started_at: None,
        last_refresh_completed_at: None,
        last_access_sequence: next_runtime_access_sequence(),
        last_request_source: Some(workspace_source.to_string()),
        last_error: None,
        indexed_entries_count: 0,
        indexed_files_count: 0,
        indexed_dirs_count: 0,
        content_index_enabled: tantivy_enabled(),
        content_index_status: if tantivy_enabled() {
            "idle".to_string()
        } else {
            "disabled".to_string()
        },
        content_index_zones: Vec::new(),
        content_zone_indexed_at: BTreeMap::new(),
        content_index_partial: false,
        indexed_content_files: 0,
        indexed_content_bytes: 0,
        index_map_size_bytes: map_size_bytes,
        index_size_bytes: 0,
        _active_lock: active_lock,
    };
    let mut sanitized_content_state = None;

    match storage_dir
        .is_dir()
        .then(|| load_existing_index(workspace_key, workspace_root))
        .transpose()
        .map(|loaded| loaded.flatten())
    {
        Ok(Some((meta, index))) => {
            let live_entries = index.live_entries;
            let original_content_index_status = meta.content_index_status;
            let original_content_index_zones = meta.content_index_zones;
            let content_index_zones =
                sanitize_content_zones(workspace_root, original_content_index_zones.clone());
            let content_zones_changed = content_index_zones != original_content_index_zones;
            let content_index_status = if matches!(
                original_content_index_status.as_str(),
                "warming" | "writer_busy"
            ) {
                "stale".to_string()
            } else if content_index_zones.is_empty() && original_content_index_status == "ready" {
                "idle".to_string()
            } else {
                original_content_index_status.clone()
            };
            let content_status_changed = content_index_status != original_content_index_status;
            if let Some(slot) = PATH_INDEXES.get(workspace_key)
                && let Ok(mut guard) = slot.value().write()
            {
                *guard = index;
            }
            let (indexed_content_files, indexed_content_bytes) = if content_zones_changed {
                recalc_content_totals_for_zones(workspace_key, workspace_root, &content_index_zones)
                    .unwrap_or((0, 0))
            } else {
                (meta.indexed_content_files, meta.indexed_content_bytes)
            };
            runtime.loaded_from_disk = true;
            runtime.scan_complete = meta.scan_complete;
            runtime.last_loaded_entries = live_entries;
            runtime.last_persisted_entries = live_entries;
            runtime.last_persisted_at = Some(meta.saved_at);
            runtime.last_scan_completed_at = meta.last_full_scan_at;
            runtime.indexed_entries_count = live_entries;
            runtime.indexed_files_count = meta.indexed_files_count;
            runtime.indexed_dirs_count = meta.indexed_dirs_count;
            runtime.content_index_enabled = meta.content_index_enabled;
            runtime.content_index_status = content_index_status;
            runtime.content_index_zones = content_index_zones;
            runtime.content_zone_indexed_at = BTreeMap::new();
            runtime.content_index_partial = if runtime.content_index_zones.is_empty() {
                false
            } else {
                meta.content_index_partial || content_zones_changed
            };
            runtime.indexed_content_files = indexed_content_files;
            runtime.indexed_content_bytes = indexed_content_bytes;
            if content_zones_changed || content_status_changed {
                sanitized_content_state = Some((
                    runtime.content_index_status.clone(),
                    runtime.content_index_zones.clone(),
                    runtime.content_index_partial,
                    runtime.indexed_content_files,
                    runtime.indexed_content_bytes,
                ));
            }
        }
        Ok(None) => {}
        Err(err) => {
            runtime.last_error = Some(err);
        }
    }

    with_runtime_map_write(|state| {
        state.insert(workspace_key.to_string(), runtime);
    });
    if let Some((status, zones, partial, files, bytes)) = sanitized_content_state
        && let Err(err) =
            persist_content_status(workspace_root, status, zones, partial, files, bytes)
    {
        warn!(
            workspace = %workspace_root.display(),
            error = %err,
            "Failed to persist sanitized content index zones"
        );
    }

    true
}

fn evict_lru_runtime_for_new_workspace(incoming_key: &str) -> bool {
    let max_loaded = index_max_loaded_workspaces();
    loop {
        let candidate = match INDEX_RUNTIMES.read() {
            Ok(guard) => {
                if guard.contains_key(incoming_key) || guard.len() < max_loaded {
                    return true;
                }
                guard
                    .iter()
                    .filter(|(key, runtime)| {
                        (*key).as_str() != incoming_key
                            && !runtime.refresh_running
                            && !ACTIVE_REFRESHES.contains_key((*key).as_str())
                            && !ACTIVE_CONTENT_REFRESHES
                                .iter()
                                .any(|entry| entry.key().starts_with(&((*key).to_string() + "|")))
                            && !runtime_cache_in_use(key, runtime)
                    })
                    .min_by_key(|(_, runtime)| runtime.last_access_sequence)
                    .map(|(key, _)| key.clone())
            }
            Err(_) => return false,
        };
        let Some(candidate) = candidate else {
            return false;
        };
        if !evict_runtime_cache(&candidate) {
            return false;
        }
    }
}

fn evict_runtime_cache(workspace_key: &str) -> bool {
    let runtime = match INDEX_RUNTIMES.write() {
        Ok(mut guard) => guard.remove(workspace_key),
        Err(_) => None,
    };
    let Some(runtime) = runtime else {
        return false;
    };

    remove_runtime_cache_entries(workspace_key, &runtime.storage_dir);
    if let Ok(mut active) = ACTIVE_WORKSPACE_KEY.write()
        && active.as_deref() == Some(workspace_key)
    {
        *active = None;
    }
    info!(
        workspace = %runtime.workspace_root.display(),
        max_loaded_workspaces = index_max_loaded_workspaces(),
        "Evicted inactive workspace index runtime from memory"
    );
    true
}

fn remove_runtime_cache_entries(workspace_key: &str, storage_dir: &Path) {
    PATH_INDEXES.remove(workspace_key);
    INDEX_STORES.remove(&normalize_path_for_identity(storage_dir));
    TANTIVY_SEARCHERS.remove(&normalize_path_for_identity(
        &storage_dir.join("tantivy-content"),
    ));
    TANTIVY_WRITE_LOCKS.remove(workspace_key);
}

fn runtime_cache_in_use(workspace_key: &str, runtime: &IndexRuntime) -> bool {
    let path_index_in_use = PATH_INDEXES
        .get(workspace_key)
        .is_some_and(|entry| Arc::strong_count(entry.value()) > 1);
    let store_key = normalize_path_for_identity(&runtime.storage_dir);
    let store_in_use = INDEX_STORES
        .get(&store_key)
        .is_some_and(|entry| Arc::strong_count(entry.value()) > 1);
    let search_key = normalize_path_for_identity(&runtime.storage_dir.join("tantivy-content"));
    let searcher_in_use = TANTIVY_SEARCHERS
        .get(&search_key)
        .is_some_and(|entry| Arc::strong_count(entry.value()) > 1);
    let writer_lock_in_use = TANTIVY_WRITE_LOCKS
        .get(workspace_key)
        .is_some_and(|entry| Arc::strong_count(entry.value()) > 1);
    path_index_in_use || store_in_use || searcher_in_use || writer_lock_in_use
}

fn next_runtime_access_sequence() -> u64 {
    RUNTIME_ACCESS_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn record_runtime_access(workspace_key: &str) {
    with_runtime_write(workspace_key, |runtime| {
        runtime.last_access_sequence = next_runtime_access_sequence();
    });
}

fn load_existing_index(
    workspace_key: &str,
    workspace_root: &Path,
) -> Result<Option<(WorkspaceMeta, PathIndex)>, String> {
    let store = open_store(workspace_root)?;
    let rtxn = store
        .env
        .read_txn()
        .map_err(|e| format!("read_txn_failed: {e}"))?;
    let meta = match store.meta_db.get(&rtxn, "workspace") {
        Ok(Some(meta)) => meta,
        Ok(None) => return Ok(None),
        Err(_) => return load_existing_json_index(&store, &rtxn, workspace_root),
    };

    if meta.schema_version != INDEX_SCHEMA_VERSION {
        return Err(format!(
            "unsupported_schema_version: got={}, expected={}",
            meta.schema_version, INDEX_SCHEMA_VERSION
        ));
    }
    if meta.workspace_root != normalize_path_for_identity(workspace_root) {
        return Err("workspace_mismatch".to_string());
    }

    let mut entries = Vec::new();
    let path_iter = store
        .path_db
        .iter(&rtxn)
        .map_err(|e| format!("path_iter_failed: {e}"))?;
    for item in path_iter {
        match item {
            Ok((_, entry)) => entries.push(entry),
            Err(_) => return load_existing_json_index(&store, &rtxn, workspace_root),
        }
    }

    let index = PathIndex::from_entries(workspace_root, entries);
    info!(
        workspace_key,
        entries = index.live_entries,
        "Loaded LMDB metadata index"
    );
    Ok(Some((meta, index)))
}

fn load_existing_json_index(
    store: &IndexStore,
    rtxn: &heed::RoTxn<'_>,
    workspace_root: &Path,
) -> Result<Option<(WorkspaceMeta, PathIndex)>, String> {
    let meta_db = store.meta_db.remap_data_type::<SerdeJson<WorkspaceMeta>>();
    let path_db = store
        .path_db
        .remap_data_type::<SerdeJson<IndexedPathEntry>>();
    let Some(meta) = meta_db
        .get(rtxn, "workspace")
        .map_err(|e| format!("meta_json_read_failed: {e}"))?
    else {
        return Ok(None);
    };

    if meta.schema_version != INDEX_SCHEMA_VERSION {
        return Err(format!(
            "unsupported_schema_version: got={}, expected={}",
            meta.schema_version, INDEX_SCHEMA_VERSION
        ));
    }
    if meta.workspace_root != normalize_path_for_identity(workspace_root) {
        return Err("workspace_mismatch".to_string());
    }

    let mut entries = Vec::new();
    for item in path_db
        .iter(rtxn)
        .map_err(|e| format!("path_json_iter_failed: {e}"))?
    {
        let (_, entry) = item.map_err(|e| format!("path_json_decode_failed: {e}"))?;
        entries.push(entry);
    }

    Ok(Some((
        meta,
        PathIndex::from_entries(workspace_root, entries),
    )))
}

fn spawn_full_metadata_refresh(workspace_root: PathBuf, workspace_key: String) {
    if ACTIVE_REFRESHES.insert(workspace_key.clone(), ()).is_some() {
        return;
    }

    record_refresh_started(&workspace_key);
    thread::spawn(move || {
        let result = refresh_metadata_index(&workspace_root, &workspace_key);
        match result {
            Ok(summary) => record_refresh_success(&workspace_key, summary),
            Err(err) => {
                record_runtime_error(&workspace_key, err.clone());
                if err.starts_with("too_large:") {
                    crate::workspace_control::record_rejected_candidate(
                        workspace_root.clone(),
                        "metadata_scan",
                        "too_large",
                        err.clone(),
                        scan_limit_count(&err),
                        None,
                    );
                } else if err.starts_with("disk_budget:") {
                    crate::workspace_control::record_rejected_candidate(
                        workspace_root.clone(),
                        "metadata_scan",
                        "disk_budget",
                        err.clone(),
                        None,
                        None,
                    );
                }
                error!(workspace = %workspace_root.display(), error = %err, "Metadata index refresh failed");
            }
        }
        ACTIVE_REFRESHES.remove(&workspace_key);
    });
}

fn refresh_metadata_index(
    workspace_root: &Path,
    workspace_key: &str,
) -> Result<RefreshSummary, String> {
    let indexed_at = current_unix_timestamp();
    let entries = collect_workspace_entries(workspace_root, indexed_at)?;
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut files = 0usize;
    let mut dirs = 0usize;

    for entry in &entries {
        if entry.is_dir {
            dirs += 1;
        } else {
            files += 1;
        }
        if let Some(name) = file_name_from_relative_path(&entry.relative_path) {
            children
                .entry(entry.parent_relative_path.clone())
                .or_default()
                .push(name.to_string());
        }
    }
    for names in children.values_mut() {
        names.sort();
        names.dedup();
    }

    ensure_index_write_budget((entries.len() as u64).saturating_mul(512))?;
    let store = open_store(workspace_root)?;

    if current_path_index_matches(workspace_key, &entries) {
        return Ok(RefreshSummary {
            entries: entries.len(),
            files,
            dirs,
            completed_at: indexed_at,
        });
    }

    let mut wtxn = store
        .env
        .write_txn()
        .map_err(|e| format!("write_txn_failed: {e}"))?;
    store
        .path_db
        .clear(&mut wtxn)
        .map_err(|e| format!("path_clear_failed: {e}"))?;
    store
        .children_db
        .clear(&mut wtxn)
        .map_err(|e| format!("children_clear_failed: {e}"))?;

    for entry in &entries {
        store
            .path_db
            .put(&mut wtxn, entry.relative_path.as_str(), entry)
            .map_err(|e| format!("path_put_failed: {e}"))?;
    }
    for (parent, names) in &children {
        let parent_key = if parent.is_empty() {
            ROOT_CHILDREN_KEY
        } else {
            parent.as_str()
        };
        store
            .children_db
            .put(&mut wtxn, parent_key, names)
            .map_err(|e| format!("children_put_failed: {e}"))?;
    }

    let meta = WorkspaceMeta {
        schema_version: INDEX_SCHEMA_VERSION,
        workspace_root: normalize_path_for_identity(workspace_root),
        saved_at: indexed_at,
        scan_complete: true,
        indexed_entries_count: entries.len(),
        indexed_files_count: files,
        indexed_dirs_count: dirs,
        last_full_scan_at: Some(indexed_at),
        content_index_enabled: tantivy_enabled(),
        content_index_status: runtime_content_status(workspace_key),
        content_index_zones: runtime_content_zones(workspace_key),
        content_index_partial: runtime_content_partial(workspace_key),
        indexed_content_files: runtime_indexed_content_files(workspace_key),
        indexed_content_bytes: runtime_indexed_content_bytes(workspace_key),
    };
    store
        .meta_db
        .put(&mut wtxn, "workspace", &meta)
        .map_err(|e| format!("meta_put_failed: {e}"))?;
    wtxn.commit().map_err(|e| format!("commit_failed: {e}"))?;

    write_sidecar_meta(&store.storage_dir, &meta)?;

    let index = PathIndex::from_entries(workspace_root, entries);
    let live_entries = index.live_entries;
    if let Some(slot) = PATH_INDEXES.get(workspace_key)
        && let Ok(mut guard) = slot.value().write()
    {
        *guard = index;
    }

    Ok(RefreshSummary {
        entries: live_entries,
        files,
        dirs,
        completed_at: indexed_at,
    })
}

fn collect_workspace_entries(
    workspace_root: &Path,
    indexed_at: u64,
) -> Result<Vec<IndexedPathEntry>, String> {
    let entry_batches = Arc::new(Mutex::new(Vec::<Vec<IndexedPathEntry>>::new()));
    let errors = Arc::new(AtomicUsize::new(0));
    let entries_seen = Arc::new(AtomicUsize::new(0));
    let limit_reason = Arc::new(Mutex::new(None::<String>));
    let root = workspace_root.to_path_buf();
    let started = Instant::now();
    let max_entries = index_max_entries();
    let max_duration = Duration::from_secs(index_max_scan_seconds());
    let mut walk = WalkBuilder::new(workspace_root);
    walk.hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .threads(bounded_walk_threads());

    walk.build_parallel().run(|| {
        let entry_batches = Arc::clone(&entry_batches);
        let errors = Arc::clone(&errors);
        let entries_seen = Arc::clone(&entries_seen);
        let limit_reason = Arc::clone(&limit_reason);
        let root = root.clone();
        let mut batch = EntryBatch::new(Arc::clone(&entry_batches));
        Box::new(move |result| {
            let entry = match result {
                Ok(entry) => entry,
                Err(_) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                }
            };
            let Some(file_type) = entry.file_type() else {
                return WalkState::Continue;
            };
            if !file_type.is_file() && !file_type.is_dir() {
                return WalkState::Continue;
            }
            let current = entries_seen
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            if current > max_entries {
                if let Ok(mut reason) = limit_reason.lock() {
                    *reason = Some(format!(
                        "too_large:entry_budget_exceeded:{current}>{max_entries}"
                    ));
                }
                return WalkState::Quit;
            }
            if started.elapsed() >= max_duration {
                if let Ok(mut reason) = limit_reason.lock() {
                    *reason = Some(format!(
                        "too_large:time_budget_exceeded:{}s:entries_seen={current}",
                        max_duration.as_secs()
                    ));
                }
                return WalkState::Quit;
            }
            let Ok(metadata) = entry.metadata() else {
                errors.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            };
            let Some(indexed) = indexed_entry_from_metadata(
                &root,
                entry.path(),
                file_type.is_dir(),
                &metadata,
                indexed_at,
            ) else {
                return WalkState::Continue;
            };
            batch.push(indexed);
            WalkState::Continue
        })
    });

    if let Ok(reason) = limit_reason.lock()
        && let Some(reason) = reason.clone()
    {
        return Err(reason);
    }

    if errors.load(Ordering::Relaxed) > 0 {
        warn!(
            workspace = %workspace_root.display(),
            errors = errors.load(Ordering::Relaxed),
            "Metadata refresh skipped some entries"
        );
    }

    let batches = Arc::try_unwrap(entry_batches)
        .map_err(|_| "entry_batch_collector_still_shared".to_string())?
        .into_inner()
        .map_err(|_| "entry_batch_collector_poisoned".to_string())?;
    let total_entries = batches.iter().map(Vec::len).sum();
    let mut entries = Vec::with_capacity(total_entries);
    for mut batch in batches {
        entries.append(&mut batch);
    }
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(entries)
}

fn current_path_index_matches(workspace_key: &str, entries: &[IndexedPathEntry]) -> bool {
    let Some(slot) = PATH_INDEXES.get(workspace_key) else {
        return false;
    };
    let Ok(index) = slot.value().read() else {
        return false;
    };
    if index.live_entries != entries.len() {
        return false;
    }
    entries.iter().all(|entry| {
        let key = entry.relative_path.to_ascii_lowercase();
        let Some(existing_index) = index.path_lookup.get(&key).copied() else {
            return false;
        };
        let Some(existing) = index
            .entries
            .get(existing_index)
            .and_then(|entry| entry.as_ref())
        else {
            return false;
        };
        existing.relative_path == entry.relative_path
            && existing.is_dir == entry.is_dir
            && existing.size == entry.size
            && existing.modified_at == entry.modified_at
            && existing.extension_lower == entry.extension_lower
    })
}

struct EntryBatch {
    batches: Arc<Mutex<Vec<Vec<IndexedPathEntry>>>>,
    local: Vec<IndexedPathEntry>,
}

impl EntryBatch {
    fn new(batches: Arc<Mutex<Vec<Vec<IndexedPathEntry>>>>) -> Self {
        Self {
            batches,
            local: Vec::with_capacity(512),
        }
    }

    fn push(&mut self, entry: IndexedPathEntry) {
        self.local.push(entry);
        if self.local.len() >= 512 {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.local.is_empty() {
            return;
        }
        if let Ok(mut batches) = self.batches.lock() {
            batches.push(std::mem::take(&mut self.local));
        }
    }
}

impl Drop for EntryBatch {
    fn drop(&mut self) {
        self.flush();
    }
}

fn indexed_entry_from_metadata(
    workspace_root: &Path,
    path: &Path,
    is_dir: bool,
    metadata: &fs::Metadata,
    indexed_at: u64,
) -> Option<IndexedPathEntry> {
    if path == workspace_root {
        return None;
    }
    let relative_path = path
        .strip_prefix(workspace_root)
        .ok()
        .map(normalize_path)
        .filter(|relative| !relative.is_empty())?;
    let parent_relative_path = path
        .parent()
        .and_then(|parent| parent.strip_prefix(workspace_root).ok())
        .map(normalize_path)
        .unwrap_or_default();
    let extension_lower = if is_dir {
        String::new()
    } else {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .unwrap_or_default()
    };
    Some(IndexedPathEntry {
        relative_path,
        is_dir,
        size: if is_dir { 0 } else { metadata.len() },
        modified_at: metadata_modified_secs(metadata),
        extension_lower,
        parent_relative_path,
        indexed_at,
    })
}

fn open_store(workspace_root: &Path) -> Result<Arc<IndexStore>, String> {
    let storage_dir = index_storage_dir_for_workspace(workspace_root);
    let store_key = normalize_path_for_identity(&storage_dir);
    if let Some(store) = INDEX_STORES.get(&store_key) {
        return Ok(Arc::clone(store.value()));
    }

    let _open_guard = INDEX_STORE_OPEN_LOCK
        .lock()
        .map_err(|_| "index_store_open_lock_poisoned".to_string())?;
    if let Some(store) = INDEX_STORES.get(&store_key) {
        return Ok(Arc::clone(store.value()));
    }

    fs::create_dir_all(&storage_dir).map_err(|e| format!("create_index_dir_failed: {e}"))?;
    let env = unsafe {
        EnvOpenOptions::new()
            .map_size(index_map_size_bytes() as usize)
            .max_dbs(16)
            .open(&storage_dir)
            .map_err(|e| format!("lmdb_open_failed: {e}"))?
    };
    let mut wtxn = env
        .write_txn()
        .map_err(|e| format!("write_txn_failed: {e}"))?;
    let meta_db: MetaDb = env
        .create_database(&mut wtxn, Some("workspace_meta"))
        .map_err(|e| format!("meta_db_open_failed: {e}"))?;
    let path_db: PathDb = env
        .create_database(&mut wtxn, Some("path_by_rel"))
        .map_err(|e| format!("path_db_open_failed: {e}"))?;
    let children_db: ChildrenDb = env
        .create_database(&mut wtxn, Some("children_by_parent"))
        .map_err(|e| format!("children_db_open_failed: {e}"))?;
    wtxn.commit()
        .map_err(|e| format!("db_open_commit_failed: {e}"))?;
    let tantivy_dir = storage_dir.join("tantivy-content");
    let store = Arc::new(IndexStore {
        env,
        meta_db,
        path_db,
        children_db,
        storage_dir,
        tantivy_dir,
    });
    INDEX_STORES.insert(store_key.clone(), Arc::clone(&store));
    Ok(INDEX_STORES
        .get(&store_key)
        .map(|entry| Arc::clone(entry.value()))
        .unwrap_or(store))
}

fn content_refresh_key(workspace_key: &str, zone: &str) -> String {
    format!("{workspace_key}\n{zone}")
}

fn covering_active_content_zone(workspace_key: &str, zone: &str) -> Option<String> {
    let key_prefix = format!("{workspace_key}\n");
    ACTIVE_CONTENT_REFRESHES
        .iter()
        .filter_map(|entry| {
            entry
                .key()
                .strip_prefix(&key_prefix)
                .map(ToString::to_string)
        })
        .filter(|active_zone| zone_is_within(zone, active_zone))
        .max_by_key(|active_zone| active_zone.len())
}

fn schedule_content_zone_refresh(
    workspace_root: PathBuf,
    workspace_key: String,
    zone: String,
    include_ignored: bool,
) {
    if !tantivy_enabled() {
        return;
    }
    let refresh_key = content_refresh_key(&workspace_key, &zone);
    if ACTIVE_CONTENT_REFRESHES
        .insert(refresh_key.clone(), ())
        .is_some()
    {
        return;
    }
    record_content_status(&workspace_key, "warming", None, false, 0, 0);
    thread::spawn(move || {
        let result = refresh_tantivy_zone(&workspace_root, &workspace_key, &zone, include_ignored);
        match result {
            Ok(refresh) => record_content_status(
                &workspace_key,
                "ready",
                Some(zone.clone()),
                refresh.summary.partial,
                refresh.summary.files,
                refresh.summary.bytes,
            ),
            Err(ContentRefreshError::WriterBusy(err)) => {
                record_content_error(&workspace_key, err.clone());
                record_content_status(&workspace_key, "writer_busy", None, true, 0, 0);
                warn!(workspace = %workspace_root.display(), zone, error = %err, "Tantivy writer remained busy; refresh can be retried");
            }
            Err(ContentRefreshError::Fatal(err)) => {
                record_content_error(&workspace_key, err.clone());
                record_content_status(&workspace_key, "error", None, true, 0, 0);
                error!(workspace = %workspace_root.display(), zone, error = %err, "Tantivy zone refresh failed");
            }
        }
        ACTIVE_CONTENT_REFRESHES.remove(&refresh_key);
    });
}

fn tantivy_write_lock(workspace_key: &str) -> Arc<Mutex<()>> {
    TANTIVY_WRITE_LOCKS
        .entry(workspace_key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn refresh_tantivy_zone(
    workspace_root: &Path,
    workspace_key: &str,
    zone: &str,
    include_ignored: bool,
) -> Result<ContentRefreshSuccess, ContentRefreshError> {
    let writer_lock = tantivy_write_lock(workspace_key);
    let _writer_guard = writer_lock
        .lock()
        .map_err(|_| ContentRefreshError::Fatal("tantivy_writer_lock_poisoned".to_string()))?;
    ensure_index_write_budget(DEFAULT_TANTIVY_MAX_ZONE_BYTES)
        .map_err(ContentRefreshError::Fatal)?;
    let store = open_store(workspace_root).map_err(ContentRefreshError::Fatal)?;
    let _file_guard = acquire_content_writer_file_lock(&store.storage_dir)?;
    let retry_deadline = Instant::now() + Duration::from_secs(TANTIVY_WRITER_RETRY_TIMEOUT_SECS);
    let mut last_busy_error = None;
    loop {
        match refresh_tantivy_zone_once(
            &store,
            workspace_root,
            workspace_key,
            zone,
            include_ignored,
        ) {
            Ok(summary) => {
                return Ok(ContentRefreshSuccess {
                    summary,
                    _file_guard,
                });
            }
            Err(ContentRefreshError::WriterBusy(err)) if Instant::now() < retry_deadline => {
                last_busy_error = Some(err);
                thread::sleep(Duration::from_millis(TANTIVY_WRITER_RETRY_DELAY_MS));
            }
            Err(ContentRefreshError::WriterBusy(err)) => {
                return Err(ContentRefreshError::WriterBusy(
                    last_busy_error.unwrap_or(err),
                ));
            }
            Err(err) => return Err(err),
        }
    }
}

fn refresh_tantivy_zone_once(
    store: &IndexStore,
    workspace_root: &Path,
    workspace_key: &str,
    zone: &str,
    include_ignored: bool,
) -> Result<ContentRefreshSummary, ContentRefreshError> {
    let index = open_or_create_tantivy(&store.tantivy_dir).map_err(ContentRefreshError::Fatal)?;
    let fields = tantivy_fields(&index.schema()).map_err(ContentRefreshError::Fatal)?;
    let writer_threads = bounded_walk_threads();
    let writer_memory = writer_threads.saturating_mul(TANTIVY_WRITER_MEMORY_BYTES_PER_THREAD);
    let mut writer: IndexWriter<TantivyDocument> = index
        .writer_with_num_threads(writer_threads, writer_memory)
        .map_err(|error| classify_tantivy_write_error("tantivy_writer_failed", error))?;
    let entries =
        indexed_entries_for_content_zone(workspace_key, zone, workspace_root, include_ignored)
            .map_err(ContentRefreshError::Fatal)?;
    let mut indexed_files = 0usize;
    let mut indexed_bytes = 0u64;
    let mut partial = false;
    let limits = ContentPolicy::default();
    let workspace_budget =
        content_workspace_budget_for_zone(workspace_key, workspace_root, zone, &limits)
            .map_err(ContentRefreshError::Fatal)?;
    let zone_is_workspace_root = zone.is_empty();
    let allow_third_party = zone == "third_party" || zone.starts_with("third_party/");
    let exclude_managed_bin = crate::common::workspace_uses_managed_build_outputs(workspace_root);

    for entry in entries {
        if entry.is_dir {
            continue;
        }
        writer.delete_term(Term::from_field_text(
            fields.relative_path,
            &entry.relative_path,
        ));
        if !content_policy_allows(&entry, &limits, allow_third_party, exclude_managed_bin) {
            continue;
        }
        if indexed_bytes.saturating_add(entry.size) > workspace_budget
            || indexed_bytes.saturating_add(entry.size) > limits.max_zone_bytes
        {
            partial = true;
            break;
        }

        let content = if zone_is_workspace_root {
            String::new()
        } else {
            read_indexable_content(&entry.path, limits.max_file_bytes)
                .map_err(ContentRefreshError::Fatal)?
        };
        let mut document = TantivyDocument::new();
        document.add_text(fields.relative_path, &entry.relative_path);
        document.add_text(fields.file_name, &entry.file_name);
        document.add_text(
            fields.path_tokens,
            path_tokens_for_tantivy(&entry.relative_path),
        );
        document.add_text(fields.content, &content);
        document.add_text(fields.extension, &entry.extension_lower);
        document.add_u64(fields.size, entry.size);
        document.add_u64(fields.modified_at, entry.modified_at);
        writer.add_document(document).map_err(|error| {
            ContentRefreshError::Fatal(format!("tantivy_add_document_failed: {error}"))
        })?;
        indexed_files += 1;
        if !zone_is_workspace_root {
            indexed_bytes = indexed_bytes.saturating_add(entry.size);
        }
    }

    ensure_index_write_budget(indexed_bytes.saturating_add((indexed_files as u64) * 512))
        .map_err(ContentRefreshError::Fatal)?;
    writer
        .commit()
        .map_err(|error| classify_tantivy_write_error("tantivy_commit_failed", error))?;

    Ok(ContentRefreshSummary {
        files: indexed_files,
        bytes: indexed_bytes,
        partial,
    })
}

#[derive(Debug)]
enum ContentRefreshError {
    WriterBusy(String),
    Fatal(String),
}

fn acquire_content_writer_file_lock(storage_dir: &Path) -> Result<File, ContentRefreshError> {
    let lock_path = storage_dir.join(CONTENT_WRITER_LOCK_FILE);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            ContentRefreshError::Fatal(format!(
                "content_writer_lock_open_failed: {}: {error}",
                lock_path.display()
            ))
        })?;
    lock_file.lock().map_err(|error| {
        ContentRefreshError::Fatal(format!(
            "content_writer_lock_failed: {}: {error}",
            lock_path.display()
        ))
    })?;
    Ok(lock_file)
}

fn classify_tantivy_write_error(context: &str, error: TantivyError) -> ContentRefreshError {
    let message = format!("{context}: {error}");
    match error {
        TantivyError::LockFailure(LockError::LockBusy, _)
        | TantivyError::OpenWriteError(OpenWriteError::FileAlreadyExists(_)) => {
            ContentRefreshError::WriterBusy(message)
        }
        TantivyError::OpenWriteError(OpenWriteError::IoError { io_error, .. })
            if matches!(
                io_error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
            ) =>
        {
            ContentRefreshError::WriterBusy(message)
        }
        _ => ContentRefreshError::Fatal(message),
    }
}

fn search_tantivy_zone(
    workspace_root: &Path,
    query: &str,
    limit: usize,
) -> Result<TantivyZoneSearchResult, String> {
    if limit == 0 {
        return Ok(TantivyZoneSearchResult {
            paths: Vec::new(),
            truncated: false,
        });
    }
    let store = open_store(workspace_root)?;
    if !store.tantivy_dir.exists() {
        return Ok(TantivyZoneSearchResult {
            paths: Vec::new(),
            truncated: false,
        });
    }
    let search_store = open_tantivy_search_store(&store.tantivy_dir)?;
    search_store
        .reader
        .reload()
        .map_err(|e| format!("tantivy_reader_reload_failed: {e}"))?;
    let searcher = search_store.reader.searcher();
    let fields = search_store.fields;
    let mut parser = QueryParser::for_index(
        &search_store.index,
        vec![
            fields.content,
            fields.path_tokens,
            fields.file_name,
            fields.relative_path,
        ],
    );
    parser.set_conjunction_by_default();
    let parsed = parser
        .parse_query(&escape_tantivy_literal_query(query))
        .map_err(|e| format!("tantivy_query_parse_failed: {e}"))?;
    let search_limit = limit.saturating_add(1);
    let top_docs = searcher
        .search(&parsed, &TopDocs::with_limit(search_limit).order_by_score())
        .map_err(|e| format!("tantivy_search_failed: {e}"))?;
    let mut paths = Vec::new();
    let truncated = top_docs.len() > limit;
    for (_, address) in top_docs.into_iter().take(limit) {
        let doc = searcher
            .doc::<TantivyDocument>(address)
            .map_err(|e| format!("tantivy_doc_fetch_failed: {e}"))?;
        if let Some(relative_path) = doc
            .get_first(fields.relative_path)
            .and_then(|value| value.as_str())
        {
            paths.push(
                workspace_root.join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR)),
            );
        }
    }
    Ok(TantivyZoneSearchResult { paths, truncated })
}

fn candidate_belongs_to_search_path(candidate: &Path, search_path: &Path) -> bool {
    if search_path.is_file() {
        candidate == search_path
    } else {
        candidate.starts_with(search_path)
    }
}

fn path_index_contains(workspace_key: &str, workspace_root: &Path, path: &Path) -> bool {
    let Ok(relative_path) = path.strip_prefix(workspace_root) else {
        return false;
    };
    let key = normalize_path(relative_path).to_ascii_lowercase();
    let Some(index) = PATH_INDEXES
        .get(workspace_key)
        .map(|entry| entry.value().clone())
    else {
        return true;
    };
    index
        .read()
        .map(|guard| guard.path_lookup.contains_key(&key))
        .unwrap_or(true)
}

fn escape_tantivy_literal_query(query: &str) -> String {
    query
        .chars()
        .flat_map(|ch| match ch {
            '-' => ['\\', '-'].into_iter().collect::<Vec<_>>(),
            _ => [ch].into_iter().collect(),
        })
        .collect()
}

fn open_tantivy_search_store(tantivy_dir: &Path) -> Result<Arc<TantivySearchStore>, String> {
    let cache_key = normalize_path_for_identity(tantivy_dir);
    if let Some(store) = TANTIVY_SEARCHERS.get(&cache_key) {
        return Ok(Arc::clone(store.value()));
    }

    let index = Index::open_in_dir(tantivy_dir).map_err(|e| format!("tantivy_open_failed: {e}"))?;
    let fields = tantivy_fields(&index.schema())?;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .map_err(|e| format!("tantivy_reader_failed: {e}"))?;
    let store = Arc::new(TantivySearchStore {
        index,
        reader,
        fields,
    });
    let cached = TANTIVY_SEARCHERS
        .entry(cache_key)
        .or_insert_with(|| Arc::clone(&store));
    Ok(Arc::clone(cached.value()))
}

fn invalidate_tantivy_search_store(tantivy_dir: &Path) {
    let cache_key = normalize_path_for_identity(tantivy_dir);
    TANTIVY_SEARCHERS.remove(&cache_key);
}

fn open_or_create_tantivy(tantivy_dir: &Path) -> Result<Index, String> {
    if tantivy_dir.exists()
        && let Ok(index) = Index::open_in_dir(tantivy_dir)
        && tantivy_schema_is_current(&index.schema())
    {
        return Ok(index);
    }

    if tantivy_dir.exists() {
        invalidate_tantivy_search_store(tantivy_dir);
        let _ = fs::remove_dir_all(tantivy_dir);
    }
    fs::create_dir_all(tantivy_dir).map_err(|e| format!("tantivy_dir_create_failed: {e}"))?;
    Index::create_in_dir(tantivy_dir, tantivy_schema())
        .map_err(|e| format!("tantivy_create_failed: {e}"))
}

fn tantivy_schema() -> Schema {
    let mut schema_builder = Schema::builder();
    let text_options = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default().set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    schema_builder.add_text_field("relative_path", STRING | STORED);
    schema_builder.add_text_field("file_name", STRING | STORED);
    schema_builder.add_text_field("path_tokens", text_options.clone());
    schema_builder.add_text_field("content", text_options);
    schema_builder.add_text_field("extension", STRING | STORED);
    schema_builder.add_u64_field("size", STORED);
    schema_builder.add_u64_field("modified_at", STORED);
    schema_builder.build()
}

fn tantivy_schema_is_current(schema: &Schema) -> bool {
    let Ok(fields) = tantivy_fields(schema) else {
        return false;
    };
    !schema.get_field_entry(fields.path_tokens).is_stored()
        && !schema.get_field_entry(fields.content).is_stored()
}

fn tantivy_fields(schema: &Schema) -> Result<TantivyFields, String> {
    Ok(TantivyFields {
        relative_path: schema
            .get_field("relative_path")
            .map_err(|_| "tantivy_schema_missing_relative_path".to_string())?,
        file_name: schema
            .get_field("file_name")
            .map_err(|_| "tantivy_schema_missing_file_name".to_string())?,
        path_tokens: schema
            .get_field("path_tokens")
            .map_err(|_| "tantivy_schema_missing_path_tokens".to_string())?,
        content: schema
            .get_field("content")
            .map_err(|_| "tantivy_schema_missing_content".to_string())?,
        extension: schema
            .get_field("extension")
            .map_err(|_| "tantivy_schema_missing_extension".to_string())?,
        size: schema
            .get_field("size")
            .map_err(|_| "tantivy_schema_missing_size".to_string())?,
        modified_at: schema
            .get_field("modified_at")
            .map_err(|_| "tantivy_schema_missing_modified_at".to_string())?,
    })
}

fn indexed_entries_for_content_zone(
    workspace_key: &str,
    zone: &str,
    workspace_root: &Path,
    include_ignored: bool,
) -> Result<Vec<IndexedPathRecord>, String> {
    let zone_path = workspace_root.join(zone.replace('/', std::path::MAIN_SEPARATOR_STR));
    if include_ignored {
        return fallback_walk_records(workspace_root, &zone_path, true);
    }
    let Some(index) = PATH_INDEXES
        .get(workspace_key)
        .map(|entry| entry.value().clone())
    else {
        return Ok(Vec::new());
    };
    let guard = index
        .read()
        .map_err(|_| "path_index_read_failed".to_string())?;
    let relative_root = if zone.is_empty() { None } else { Some(zone) };
    let mut records = guard.records_under(relative_root);
    if records.is_empty() && !zone.is_empty() {
        records = fallback_walk_records(workspace_root, &zone_path, false)?;
    }
    Ok(records)
}

fn fallback_walk_records(
    workspace_root: &Path,
    zone_path: &Path,
    include_ignored: bool,
) -> Result<Vec<IndexedPathRecord>, String> {
    let mut records = Vec::new();
    let mut walk = WalkBuilder::new(zone_path);
    walk.hidden(true)
        .ignore(!include_ignored)
        .git_ignore(!include_ignored)
        .git_global(!include_ignored)
        .git_exclude(!include_ignored)
        .require_git(false);
    for entry in walk.build().flatten() {
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() && !file_type.is_dir() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if let Some(indexed) = indexed_entry_from_metadata(
            workspace_root,
            entry.path(),
            file_type.is_dir(),
            &metadata,
            current_unix_timestamp(),
        ) {
            records.push(indexed.to_record(workspace_root));
        }
    }
    Ok(records)
}

fn content_policy_allows(
    entry: &IndexedPathRecord,
    limits: &ContentPolicy,
    allow_third_party: bool,
    exclude_managed_bin: bool,
) -> bool {
    if entry.is_dir || entry.size > limits.max_file_bytes {
        return false;
    }
    if !CONTENT_EXTENSIONS.contains(&entry.extension_lower.as_str()) {
        return false;
    }
    let relative = entry.relative_path.replace('\\', "/").to_ascii_lowercase();
    if relative.split('/').any(|part| {
        crate::common::is_default_excluded_directory_name(part, exclude_managed_bin)
            && !(allow_third_party && part == "third_party")
    }) {
        return false;
    }
    true
}

pub fn content_policy_allows_path(path: &Path) -> bool {
    let canonical_path = canonicalize_or_original(path.to_path_buf());
    let Ok(metadata) = fs::metadata(&canonical_path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let indexed_workspace = indexed_workspace_for_path(&canonical_path);
    let relative_path = indexed_workspace
        .as_ref()
        .and_then(|(_, workspace_root)| {
            canonical_path
                .strip_prefix(workspace_root)
                .ok()
                .map(normalize_path)
        })
        .unwrap_or_else(|| crate::common::normalize_display_path(&canonical_path));
    let extension_lower = canonical_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let record = IndexedPathRecord {
        path: canonical_path.clone(),
        relative_path,
        file_name: canonical_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string(),
        extension_lower,
        is_dir: false,
        size: metadata.len(),
        modified_at: metadata_modified_secs(&metadata),
    };
    let exclude_managed_bin = indexed_workspace
        .as_ref()
        .is_some_and(|(_, workspace_root)| {
            crate::common::workspace_uses_managed_build_outputs(workspace_root)
        });
    content_policy_allows(
        &record,
        &ContentPolicy::default(),
        false,
        exclude_managed_bin,
    )
}

const CONTENT_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cs", "cshtml", "csproj", "css", "gn", "gni", "go", "h", "hpp", "html",
    "java", "js", "json", "jsonl", "jsx", "kt", "kts", "m", "md", "mm", "php", "props", "proto",
    "ps1", "py", "razor", "rb", "rs", "scss", "sh", "sql", "svelte", "swift", "targets", "toml",
    "ts", "tsx", "txt", "vue", "xaml", "xml", "yaml", "yml",
];

struct ContentPolicy {
    max_file_bytes: u64,
    max_zone_bytes: u64,
    max_workspace_bytes: u64,
}

impl ContentPolicy {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_TANTIVY_MAX_FILE_BYTES,
            max_zone_bytes: DEFAULT_TANTIVY_MAX_ZONE_BYTES,
            max_workspace_bytes: DEFAULT_TANTIVY_MAX_WORKSPACE_BYTES,
        }
    }
}

fn read_indexable_content(path: &Path, max_file_bytes: u64) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("content_open_failed: {e}"))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_file_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("content_read_failed: {e}"))?;
    if bytes.len() as u64 > max_file_bytes || bytes.contains(&0) {
        return Ok(String::new());
    }
    Ok(String::from_utf8(bytes).unwrap_or_default())
}

fn path_tokens_for_tantivy(relative_path: &str) -> String {
    relative_path
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_tantivy_query_compatible(query: &str) -> bool {
    !query.trim().is_empty()
        && query.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || ch.is_ascii_whitespace() || "_-./:".contains(ch)
        })
}

fn content_zone_for_path(workspace_root: &Path, path: &Path) -> String {
    let workspace_root = crate::common::canonicalize_with_existing_ancestor(workspace_root);
    let path = crate::common::canonicalize_with_existing_ancestor(path);
    let target = if path.is_file() {
        path.parent().unwrap_or(&path)
    } else {
        &path
    };
    target
        .strip_prefix(&workspace_root)
        .ok()
        .map(normalize_path)
        .unwrap_or_default()
}

fn path_is_ignored_by_workspace_rules(workspace_root: &Path, path: &Path) -> bool {
    let workspace_root = canonicalize_or_original(workspace_root.to_path_buf());
    let path = canonicalize_or_original(path.to_path_buf());
    let Ok(relative_path) = path.strip_prefix(&workspace_root) else {
        return false;
    };
    if relative_path.as_os_str().is_empty() {
        return false;
    }

    let components = relative_path.components().collect::<Vec<_>>();
    let target_is_dir = path.is_dir();
    let mut ignore_matchers = Vec::<Gitignore>::new();
    let mut gitignore_matchers = Vec::<Gitignore>::new();
    push_ignore_matcher(
        &mut ignore_matchers,
        &workspace_root,
        &workspace_root.join(".ignore"),
    );
    push_ignore_matcher(
        &mut gitignore_matchers,
        &workspace_root,
        &workspace_root.join(".gitignore"),
    );
    let git_exclude =
        build_ignore_matcher(&workspace_root, &workspace_root.join(".git/info/exclude"));
    let (global_ignore, _) = Gitignore::global();

    let mut candidate = workspace_root.clone();
    let mut relative_candidate = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        candidate.push(component.as_os_str());
        relative_candidate.push(component.as_os_str());
        let is_dir = index + 1 < components.len() || target_is_dir;
        let decision = match_ignore_stack(&ignore_matchers, &candidate, is_dir)
            .or_else(|| match_ignore_stack(&gitignore_matchers, &candidate, is_dir))
            .or_else(|| {
                git_exclude
                    .as_ref()
                    .and_then(|matcher| ignore_match_decision(matcher, &candidate, is_dir))
            })
            .or_else(|| ignore_match_decision(&global_ignore, &relative_candidate, is_dir));
        if decision == Some(true) {
            return true;
        }

        if is_dir && index + 1 < components.len() {
            push_ignore_matcher(&mut ignore_matchers, &candidate, &candidate.join(".ignore"));
            push_ignore_matcher(
                &mut gitignore_matchers,
                &candidate,
                &candidate.join(".gitignore"),
            );
        }
    }
    false
}

fn push_ignore_matcher(matchers: &mut Vec<Gitignore>, root: &Path, rules_path: &Path) {
    if let Some(matcher) = build_ignore_matcher(root, rules_path) {
        matchers.push(matcher);
    }
}

fn build_ignore_matcher(root: &Path, rules_path: &Path) -> Option<Gitignore> {
    if !rules_path.is_file() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(root);
    let _ = builder.add(rules_path);
    builder.build().ok()
}

fn match_ignore_stack(matchers: &[Gitignore], path: &Path, is_dir: bool) -> Option<bool> {
    matchers
        .iter()
        .rev()
        .find_map(|matcher| ignore_match_decision(matcher, path, is_dir))
}

fn ignore_match_decision(matcher: &Gitignore, path: &Path, is_dir: bool) -> Option<bool> {
    let matched = matcher.matched_path_or_any_parents(path, is_dir);
    if matched.is_ignore() {
        Some(true)
    } else if matched.is_whitelist() {
        Some(false)
    } else {
        None
    }
}

fn sanitize_content_zones(workspace_root: &Path, zones: Vec<String>) -> Vec<String> {
    let valid_zones = zones
        .into_iter()
        .filter(|zone| {
            if zone.is_empty() || crate::common::contains_path_glob(zone) {
                return false;
            }
            let zone_path = Path::new(zone);
            if zone_path.is_absolute()
                || zone_path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return false;
            }
            workspace_root.join(zone_path).is_dir()
        })
        .collect::<Vec<_>>();
    collapse_content_zones(valid_zones)
}

fn collapse_content_zones(mut zones: Vec<String>) -> Vec<String> {
    zones.sort_by(|left, right| {
        left.matches('/')
            .count()
            .cmp(&right.matches('/').count())
            .then_with(|| left.cmp(right))
    });
    zones.dedup();
    let mut collapsed = Vec::<String>::new();
    for zone in zones {
        if collapsed.iter().any(|parent| zone_is_within(&zone, parent)) {
            continue;
        }
        collapsed.push(zone);
    }
    collapsed
}

fn covering_content_zone(workspace_key: &str, zone: &str) -> Option<String> {
    INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| guard.get(workspace_key).cloned())
        .and_then(|state| covering_content_zone_in_state(zone, &state))
}

fn ready_content_zone(workspace_key: &str, zone: &str) -> Option<String> {
    INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| guard.get(workspace_key).cloned())
        .and_then(|state| ready_content_zone_in_state(workspace_key, zone, &state))
}

fn covering_content_zone_in_state(zone: &str, state: &IndexRuntime) -> Option<String> {
    state
        .content_index_zones
        .iter()
        .filter(|existing| zone_is_within(zone, existing))
        .max_by_key(|existing| existing.len())
        .cloned()
}

fn ready_content_zone_in_state(
    workspace_key: &str,
    zone: &str,
    state: &IndexRuntime,
) -> Option<String> {
    if matches!(state.content_index_status.as_str(), "error" | "stale") {
        return None;
    }
    let covering = covering_content_zone_in_state(zone, state)?;
    covering_active_content_zone(workspace_key, zone)
        .is_none()
        .then_some(covering)
}

fn zone_is_within(zone: &str, parent: &str) -> bool {
    zone == parent
        || zone
            .strip_prefix(parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[derive(Debug)]
struct RefreshSummary {
    entries: usize,
    files: usize,
    dirs: usize,
    completed_at: u64,
}

#[derive(Debug)]
struct ContentRefreshSummary {
    files: usize,
    bytes: u64,
    partial: bool,
}

#[derive(Debug)]
struct ContentRefreshSuccess {
    summary: ContentRefreshSummary,
    _file_guard: File,
}

impl PathIndex {
    fn from_entries(workspace_root: &Path, entries: Vec<IndexedPathEntry>) -> Self {
        let mut index = Self::default();
        for entry in entries {
            index.insert_persisted_entry(workspace_root, entry);
        }
        index
    }

    fn insert_persisted_entry(&mut self, workspace_root: &Path, entry: IndexedPathEntry) {
        let normalized_key = entry.relative_path.to_ascii_lowercase();
        if let Some(existing) = self.path_lookup.get(&normalized_key).copied() {
            self.entries[existing] = None;
        }

        let absolute_path = workspace_root.join(
            entry
                .relative_path
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        let runtime_entry = PathIndexEntry {
            absolute_path,
            relative_path: entry.relative_path,
            extension_lower: entry.extension_lower,
            is_dir: entry.is_dir,
            size: entry.size,
            modified_at: entry.modified_at,
        };

        let index = self.entries.len();
        self.path_lookup.insert(normalized_key, index);
        for term in index_terms(&runtime_entry) {
            self.term_postings.entry(term).or_default().push(index);
        }
        self.entries.push(Some(runtime_entry));
        self.live_entries += 1;
    }

    fn shortlist_candidates(
        &self,
        anchor_terms: &[String],
        relative_root: Option<&str>,
        limit: usize,
    ) -> Option<Vec<PathQueryCandidate>> {
        if anchor_terms.is_empty() {
            return None;
        }

        let mut workset: Option<Vec<usize>> = None;
        for term in anchor_terms {
            let postings = self.term_postings.get(term)?;
            workset = Some(match workset {
                Some(existing) => intersect_sorted_indexes(&existing, postings),
                None => postings.clone(),
            });
        }

        let relative_root_lower = relative_root.map(|root| root.to_ascii_lowercase());
        let indexes = workset.unwrap_or_default();

        let mut candidates = Vec::new();
        for index in indexes {
            if candidates.len() >= limit.min(MAX_SHORTLIST_CANDIDATES) {
                break;
            }
            let Some(entry) = self.entries.get(index).and_then(|entry| entry.as_ref()) else {
                continue;
            };
            if !relative_root_matches(&entry.relative_path, relative_root_lower.as_deref()) {
                continue;
            }
            candidates.push(PathQueryCandidate {
                path: entry.absolute_path.clone(),
                is_dir: entry.is_dir,
                size: entry.size,
                modified_at: entry.modified_at,
            });
        }

        Some(candidates)
    }

    fn records_under(&self, relative_root: Option<&str>) -> Vec<IndexedPathRecord> {
        let relative_root_lower = relative_root.map(|root| root.to_ascii_lowercase());
        let mut records = Vec::new();
        for entry in self.entries.iter().filter_map(|entry| entry.as_ref()) {
            if !relative_root_matches(&entry.relative_path, relative_root_lower.as_deref()) {
                continue;
            }
            records.push(entry.to_record());
        }
        records
    }

    fn visit_records_under(
        &self,
        relative_root: Option<&str>,
        mut visitor: impl FnMut(IndexedPathRecord) -> bool,
    ) -> usize {
        let relative_root_lower = relative_root.map(|root| root.to_ascii_lowercase());
        let mut visited = 0usize;
        for entry in self.entries.iter().filter_map(|entry| entry.as_ref()) {
            if !relative_root_matches(&entry.relative_path, relative_root_lower.as_deref()) {
                continue;
            }
            visited += 1;
            if !visitor(entry.to_record()) {
                break;
            }
        }
        visited
    }
}

impl PathIndexEntry {
    fn to_record(&self) -> IndexedPathRecord {
        IndexedPathRecord {
            path: self.absolute_path.clone(),
            relative_path: self.relative_path.clone(),
            file_name: file_name_from_relative_path(&self.relative_path)
                .unwrap_or_default()
                .to_string(),
            extension_lower: self.extension_lower.clone(),
            is_dir: self.is_dir,
            size: self.size,
            modified_at: self.modified_at,
        }
    }
}

impl IndexedPathEntry {
    fn to_record(&self, workspace_root: &Path) -> IndexedPathRecord {
        IndexedPathRecord {
            path: workspace_root.join(
                self.relative_path
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            ),
            relative_path: self.relative_path.clone(),
            file_name: file_name_from_relative_path(&self.relative_path)
                .unwrap_or_default()
                .to_string(),
            extension_lower: self.extension_lower.clone(),
            is_dir: self.is_dir,
            size: self.size,
            modified_at: self.modified_at,
        }
    }
}

fn index_terms(entry: &PathIndexEntry) -> Vec<String> {
    let mut terms = entry
        .relative_path
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= MIN_INDEX_TERM_LEN)
        .map(|term| term.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if entry.extension_lower.len() >= MIN_INDEX_TERM_LEN {
        terms.push(entry.extension_lower.clone());
    }
    terms.sort();
    terms.dedup();
    terms
}

fn query_anchor_terms(pattern: &str) -> Vec<String> {
    let mut terms = pattern
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= MIN_INDEX_TERM_LEN)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    terms
}

fn relative_root_matches(relative_path: &str, relative_root_lower: Option<&str>) -> bool {
    let Some(root) = relative_root_lower else {
        return true;
    };
    root.is_empty()
        || relative_path.eq_ignore_ascii_case(root)
        || relative_path_starts_with(relative_path, root)
}

fn relative_path_starts_with(relative_path: &str, root_lower: &str) -> bool {
    relative_path
        .get(..root_lower.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(root_lower))
        && relative_path.as_bytes().get(root_lower.len()) == Some(&b'/')
}

fn intersect_sorted_indexes(left: &[usize], right: &[usize]) -> Vec<usize> {
    let mut result = Vec::with_capacity(left.len().min(right.len()));
    let (mut left_index, mut right_index) = (0usize, 0usize);
    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Equal => {
                result.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
        }
    }
    result
}

fn indexed_workspace_for_path(path: &Path) -> Option<(String, PathBuf)> {
    let canonical_path = canonicalize_or_original(path.to_path_buf());
    let result = match INDEX_RUNTIMES.read() {
        Ok(guard) => guard
            .iter()
            .filter(|(_, state)| {
                (state.scan_complete || state.indexed_entries_count > 0)
                    && path_belongs_to_workspace(&state.workspace_root, &canonical_path)
            })
            .min_by_key(|(_, state)| state.workspace_root.components().count())
            .map(|(key, state)| (key.clone(), state.workspace_root.clone())),
        Err(_) => None,
    };
    if let Some((workspace_key, _)) = result.as_ref() {
        record_runtime_access(workspace_key);
    }
    result
}

fn registered_workspace_for_path(path: &Path) -> Option<(String, PathBuf)> {
    let canonical_path = canonicalize_or_original(path.to_path_buf());
    let result = match INDEX_RUNTIMES.read() {
        Ok(guard) => guard
            .iter()
            .filter(|(_, state)| path_belongs_to_workspace(&state.workspace_root, &canonical_path))
            .min_by_key(|(_, state)| state.workspace_root.components().count())
            .map(|(key, state)| (key.clone(), state.workspace_root.clone())),
        Err(_) => None,
    };
    if let Some((workspace_key, _)) = result.as_ref() {
        record_runtime_access(workspace_key);
    }
    result
}

fn relative_root_prefix(workspace_root: &Path, search_root: &Path) -> Option<Option<String>> {
    if workspace_root == search_root {
        return Some(None);
    }
    search_root
        .strip_prefix(workspace_root)
        .ok()
        .map(normalize_path)
        .map(Some)
}

fn refresh_interval_elapsed(workspace_key: &str, now: u64) -> bool {
    INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| guard.get(workspace_key).cloned())
        .and_then(|state| state.last_refresh_requested_at)
        .is_none_or(|timestamp| now.saturating_sub(timestamp) >= MIN_REFRESH_INTERVAL_SECS)
}

fn set_active_workspace(workspace_key: &str) {
    if let Ok(mut guard) = ACTIVE_WORKSPACE_KEY.write() {
        *guard = Some(workspace_key.to_string());
    }
}

fn record_request_source(workspace_key: &str, workspace_source: &str) {
    with_runtime_write(workspace_key, |state| {
        if state.workspace_source != "client_initialize" || workspace_source == "client_initialize"
        {
            state.workspace_source = workspace_source.to_string();
        }
        state.last_access_sequence = next_runtime_access_sequence();
        state.last_request_source = Some(workspace_source.to_string());
    });
}

fn record_refresh_request(workspace_key: &str, workspace_source: &str, now: u64) {
    with_runtime_write(workspace_key, |state| {
        state.last_refresh_requested_at = Some(now);
        state.last_request_source = Some(workspace_source.to_string());
    });
}

fn record_refresh_started(workspace_key: &str) {
    with_runtime_write(workspace_key, |state| {
        state.refresh_running = true;
        state.last_refresh_started_at = Some(current_unix_timestamp());
        state.last_error = None;
    });
}

fn record_refresh_success(workspace_key: &str, summary: RefreshSummary) {
    with_runtime_write(workspace_key, |state| {
        state.refresh_running = false;
        state.loaded_from_disk = true;
        state.scan_complete = true;
        state.indexed_entries_count = summary.entries;
        state.indexed_files_count = summary.files;
        state.indexed_dirs_count = summary.dirs;
        state.last_persisted_entries = summary.entries;
        state.last_loaded_entries = summary.entries;
        state.last_persisted_at = Some(summary.completed_at);
        state.last_scan_completed_at = Some(summary.completed_at);
        state.last_refresh_completed_at = Some(summary.completed_at);
        state.index_size_bytes = 0;
        state.last_error = None;
    });
}

fn record_runtime_error(workspace_key: &str, err: String) {
    with_runtime_write(workspace_key, |state| {
        state.refresh_running = false;
        state.last_error = Some(err);
    });
}

fn record_content_error(workspace_key: &str, err: String) {
    with_runtime_write(workspace_key, |state| {
        state.last_error = Some(err);
    });
}

fn record_content_status(
    workspace_key: &str,
    status: &str,
    zone: Option<String>,
    partial: bool,
    files: usize,
    bytes: u64,
) {
    let mut persist_state = None;
    let recorded_at = current_unix_timestamp();
    with_runtime_map_write(|state| {
        if let Some(runtime) = state.get_mut(workspace_key) {
            runtime.content_index_status = status.to_string();
            runtime.content_index_partial = partial;
            if status == "ready" {
                runtime.last_error = None;
            }
            if let Some(zone) = zone.as_ref()
                && !runtime.content_index_zones.contains(zone)
            {
                runtime.content_index_zones.push(zone.clone());
            }
            runtime.content_index_zones =
                collapse_content_zones(std::mem::take(&mut runtime.content_index_zones));
            runtime
                .content_zone_indexed_at
                .retain(|existing, _| runtime.content_index_zones.contains(existing));
            if status == "ready"
                && let Some(zone) = zone.as_ref()
                && runtime.content_index_zones.contains(zone)
            {
                runtime
                    .content_zone_indexed_at
                    .insert(zone.clone(), recorded_at);
            }
            persist_state = Some((
                runtime.workspace_root.clone(),
                runtime.content_index_status.clone(),
                runtime.content_index_zones.clone(),
                runtime.content_index_partial,
                runtime.indexed_content_files,
                runtime.indexed_content_bytes,
            ));
        }
    });

    if let Some((workspace_root, status, zones, partial, current_files, current_bytes)) =
        persist_state
    {
        let (total_files, total_bytes) = if status == "ready" {
            recalc_content_totals_for_zones(workspace_key, &workspace_root, &zones)
                .unwrap_or((files, bytes))
        } else if files > 0 || bytes > 0 {
            (files, bytes)
        } else {
            (current_files, current_bytes)
        };

        with_runtime_write(workspace_key, |runtime| {
            runtime.indexed_content_files = total_files;
            runtime.indexed_content_bytes = total_bytes;
        });

        if let Err(err) = persist_content_status(
            &workspace_root,
            status,
            zones,
            partial,
            total_files,
            total_bytes,
        ) {
            warn!(
                workspace = %workspace_root.display(),
                error = %err,
                "Failed to persist Tantivy content index status"
            );
        }
    }
}

fn recalc_content_totals_for_zones(
    workspace_key: &str,
    workspace_root: &Path,
    zones: &[String],
) -> Result<(usize, u64), String> {
    let limits = ContentPolicy::default();
    recalc_content_totals_for_zones_with_policy(workspace_key, workspace_root, zones, &limits)
}

fn recalc_content_totals_for_zones_with_policy(
    workspace_key: &str,
    workspace_root: &Path,
    zones: &[String],
    limits: &ContentPolicy,
) -> Result<(usize, u64), String> {
    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    for zone in collapse_content_zones(zones.to_vec()) {
        let allow_third_party = zone == "third_party" || zone.starts_with("third_party/");
        let exclude_managed_bin =
            crate::common::workspace_uses_managed_build_outputs(workspace_root);
        let mut zone_bytes = 0u64;
        for entry in indexed_entries_for_content_zone(workspace_key, &zone, workspace_root, false)?
        {
            if content_policy_allows(&entry, limits, allow_third_party, exclude_managed_bin) {
                if zone_bytes.saturating_add(entry.size) > limits.max_zone_bytes
                    || total_bytes.saturating_add(entry.size) > limits.max_workspace_bytes
                {
                    break;
                }
                total_files += 1;
                if !zone.is_empty() {
                    zone_bytes = zone_bytes.saturating_add(entry.size);
                    total_bytes = total_bytes.saturating_add(entry.size);
                }
            }
        }
    }
    Ok((total_files, total_bytes))
}

fn content_workspace_budget_for_zone(
    workspace_key: &str,
    workspace_root: &Path,
    zone: &str,
    limits: &ContentPolicy,
) -> Result<u64, String> {
    let other_zones = collapse_content_zones(
        runtime_content_zones(workspace_key)
            .into_iter()
            .filter(|existing| !zone_is_within(existing, zone) && !zone_is_within(zone, existing))
            .collect(),
    );
    let (_, other_bytes) = recalc_content_totals_for_zones_with_policy(
        workspace_key,
        workspace_root,
        &other_zones,
        limits,
    )?;
    Ok(limits.max_workspace_bytes.saturating_sub(other_bytes))
}

fn persist_content_status(
    workspace_root: &Path,
    status: String,
    zones: Vec<String>,
    partial: bool,
    files: usize,
    bytes: u64,
) -> Result<(), String> {
    let store = open_store(workspace_root)?;
    let mut wtxn = store
        .env
        .write_txn()
        .map_err(|e| format!("write_txn_failed: {e}"))?;
    let Some(mut meta) = store
        .meta_db
        .get(&wtxn, "workspace")
        .map_err(|e| format!("meta_read_failed: {e}"))?
    else {
        return Ok(());
    };

    meta.content_index_enabled = tantivy_enabled();
    meta.content_index_status = status;
    meta.content_index_zones = zones;
    meta.content_index_partial = partial;
    meta.indexed_content_files = files;
    meta.indexed_content_bytes = bytes;
    meta.saved_at = current_unix_timestamp();

    store
        .meta_db
        .put(&mut wtxn, "workspace", &meta)
        .map_err(|e| format!("meta_put_failed: {e}"))?;
    wtxn.commit()
        .map_err(|e| format!("content_meta_commit_failed: {e}"))?;
    write_sidecar_meta(&store.storage_dir, &meta)
}

fn with_runtime_write(workspace_key: &str, f: impl FnOnce(&mut IndexRuntime)) {
    with_runtime_map_write(|state| {
        if let Some(runtime) = state.get_mut(workspace_key) {
            f(runtime);
        }
    });
}

fn with_runtime_map_write(f: impl FnOnce(&mut BTreeMap<String, IndexRuntime>)) {
    if let Ok(mut guard) = INDEX_RUNTIMES.write() {
        f(&mut guard);
    }
}

fn runtime_content_zones(workspace_key: &str) -> Vec<String> {
    INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| {
            guard
                .get(workspace_key)
                .map(|state| state.content_index_zones.clone())
        })
        .unwrap_or_default()
}

fn runtime_content_status(workspace_key: &str) -> String {
    INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| {
            guard
                .get(workspace_key)
                .map(|state| state.content_index_status.clone())
        })
        .unwrap_or_else(|| {
            if tantivy_enabled() {
                "idle".to_string()
            } else {
                "disabled".to_string()
            }
        })
}

fn runtime_content_partial(workspace_key: &str) -> bool {
    INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| {
            guard
                .get(workspace_key)
                .map(|state| state.content_index_partial)
        })
        .unwrap_or(false)
}

fn runtime_indexed_content_files(workspace_key: &str) -> usize {
    INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| {
            guard
                .get(workspace_key)
                .map(|state| state.indexed_content_files)
        })
        .unwrap_or(0)
}

fn runtime_indexed_content_bytes(workspace_key: &str) -> u64 {
    INDEX_RUNTIMES
        .read()
        .ok()
        .and_then(|guard| {
            guard
                .get(workspace_key)
                .map(|state| state.indexed_content_bytes)
        })
        .unwrap_or(0)
}

pub fn default_index_max_age_secs() -> u64 {
    DEFAULT_INDEX_MAX_AGE_SECS
}

pub fn default_index_orphan_grace_secs() -> u64 {
    DEFAULT_INDEX_ORPHAN_GRACE_SECS
}

pub fn default_index_max_total_bytes() -> u64 {
    env_u64(
        "CODELOUPE_MCP_INDEX_MAX_TOTAL_BYTES",
        DEFAULT_INDEX_MAX_TOTAL_BYTES,
    )
}

pub fn index_min_free_bytes() -> u64 {
    env_u64(
        "CODELOUPE_MCP_INDEX_MIN_FREE_BYTES",
        DEFAULT_INDEX_MIN_FREE_BYTES,
    )
}

pub fn index_max_entries() -> usize {
    std::env::var("CODELOUPE_MCP_INDEX_MAX_ENTRIES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_INDEX_MAX_ENTRIES)
}

pub fn index_max_scan_seconds() -> u64 {
    env_u64(
        "CODELOUPE_MCP_INDEX_MAX_SCAN_SECONDS",
        DEFAULT_INDEX_MAX_SCAN_SECONDS,
    )
}

pub fn index_max_loaded_workspaces() -> usize {
    std::env::var("CODELOUPE_MCP_INDEX_MAX_LOADED_WORKSPACES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_INDEX_MAX_LOADED_WORKSPACES)
}

pub fn workspace_decisions_path() -> PathBuf {
    index_storage_root().join("workspaces.json")
}

pub fn remove_workspace_index(workspace_root: &Path) -> Result<(), String> {
    let workspace_root = canonicalize_or_original(workspace_root.to_path_buf());
    let workspace_key = normalize_path_for_identity(&workspace_root);
    DISABLED_WORKSPACES.insert(workspace_key.clone(), ());
    let _load_guard = RUNTIME_LOAD_LOCK
        .lock()
        .map_err(|_| "runtime_load_lock_poisoned".to_string())?;
    if ACTIVE_REFRESHES.contains_key(&workspace_key)
        || ACTIVE_CONTENT_REFRESHES
            .iter()
            .any(|entry| entry.key().starts_with(&(workspace_key.clone() + "|")))
    {
        return Err("index_refresh_active".to_string());
    }
    let storage_dir = index_storage_dir_for_workspace(&workspace_root);
    with_runtime_map_write(|state| {
        state.remove(&workspace_key);
    });
    remove_runtime_cache_entries(&workspace_key, &storage_dir);
    if let Ok(mut active) = ACTIVE_WORKSPACE_KEY.write()
        && active.as_deref() == Some(workspace_key.as_str())
    {
        *active = None;
    }

    if !storage_dir.exists() {
        return Ok(());
    }
    let Some(_lock) = try_acquire_index_gc_lock(&storage_dir)? else {
        return Err("index_in_use".to_string());
    };
    fs::remove_dir_all(&storage_dir)
        .map_err(|error| format!("index_remove_failed:{}:{error}", storage_dir.display()))
}

pub fn index_storage_snapshot() -> IndexStorageSnapshot {
    inspect_index_storage_root(&index_storage_root())
}

pub fn run_startup_index_gc() -> IndexGcReport {
    run_index_gc(IndexGcOptions {
        apply: true,
        remove_orphans: true,
        max_age_secs: Some(DEFAULT_INDEX_MAX_AGE_SECS),
        max_total_bytes: Some(default_index_max_total_bytes()),
        workspace_roots: Vec::new(),
        max_results: 100,
    })
}

pub fn run_index_gc(options: IndexGcOptions) -> IndexGcReport {
    run_index_gc_at_root(&index_storage_root(), options)
}

fn inspect_index_storage_root(root: &Path) -> IndexStorageSnapshot {
    let now = current_unix_timestamp();
    let loaded_storage_dirs = INDEX_RUNTIMES
        .read()
        .ok()
        .map(|guard| {
            guard
                .values()
                .map(|runtime| normalize_path_for_identity(&runtime.storage_dir))
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    let mut entries = Vec::new();

    if let Ok(read_dir) = fs::read_dir(root) {
        for entry in read_dir.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || entry.file_name() == INDEX_ACTIVE_LOCK_DIR {
                continue;
            }

            let storage_dir = entry.path();
            let meta = read_sidecar_meta(&storage_dir);
            let workspace_root = meta.as_ref().map(|value| value.workspace_root.clone());
            let workspace_exists = workspace_root
                .as_ref()
                .is_some_and(|path| Path::new(path).is_dir());
            let last_used_at = read_index_last_used(&storage_dir)
                .or_else(|| meta.as_ref().map(|value| value.saved_at))
                .or_else(|| {
                    entry
                        .metadata()
                        .ok()
                        .map(|metadata| metadata_modified_secs(&metadata))
                })
                .unwrap_or(0);
            let storage_key = normalize_path_for_identity(&storage_dir);
            entries.push(IndexStorageEntry {
                workspace_root,
                storage_dir: crate::common::normalize_display_path(&storage_dir),
                size_bytes: directory_size(&storage_dir),
                last_used_at,
                age_secs: now.saturating_sub(last_used_at),
                workspace_exists,
                orphaned: !workspace_exists,
                loaded: loaded_storage_dirs.contains(&storage_key),
            });
        }
    }

    entries.sort_by(|left, right| {
        left.last_used_at
            .cmp(&right.last_used_at)
            .then_with(|| left.storage_dir.cmp(&right.storage_dir))
    });
    let loaded_count = entries.iter().filter(|entry| entry.loaded).count();
    let orphaned_count = entries.iter().filter(|entry| entry.orphaned).count();
    IndexStorageSnapshot {
        root: crate::common::normalize_display_path(root),
        total_size_bytes: directory_size(root),
        entry_count: entries.len(),
        loaded_count,
        unloaded_count: entries.len().saturating_sub(loaded_count),
        orphaned_count,
        entries,
    }
}

fn run_index_gc_at_root(root: &Path, options: IndexGcOptions) -> IndexGcReport {
    let snapshot = inspect_index_storage_root(root);
    let requested_roots = options
        .workspace_roots
        .iter()
        .map(|path| normalize_path_for_identity(&canonicalize_or_original(path.clone())))
        .collect::<HashSet<_>>();
    let mut selected = HashMap::<String, Vec<String>>::new();

    for entry in &snapshot.entries {
        let mut reasons = Vec::new();
        if entry.workspace_root.as_ref().is_some_and(|workspace_root| {
            requested_roots.contains(&normalize_path_for_identity(Path::new(workspace_root)))
        }) {
            reasons.push("requested_workspace".to_string());
        }
        if options.remove_orphans
            && entry.orphaned
            && entry.age_secs >= DEFAULT_INDEX_ORPHAN_GRACE_SECS
        {
            reasons.push("orphaned_workspace".to_string());
        }
        if options
            .max_age_secs
            .is_some_and(|max_age_secs| entry.age_secs >= max_age_secs)
        {
            reasons.push("max_age_exceeded".to_string());
        }
        if !reasons.is_empty() {
            selected.insert(entry.storage_dir.clone(), reasons);
        }
    }

    let mut projected_size = snapshot.total_size_bytes.saturating_sub(
        snapshot
            .entries
            .iter()
            .filter(|entry| selected.contains_key(&entry.storage_dir) && !entry.loaded)
            .map(|entry| entry.size_bytes)
            .sum::<u64>(),
    );
    if let Some(max_total_bytes) = options.max_total_bytes {
        for entry in &snapshot.entries {
            if projected_size <= max_total_bytes {
                break;
            }
            if entry.loaded || selected.contains_key(&entry.storage_dir) {
                continue;
            }
            selected.insert(
                entry.storage_dir.clone(),
                vec!["storage_limit_lru".to_string()],
            );
            projected_size = projected_size.saturating_sub(entry.size_bytes);
        }
    }

    let selected_entries = selected.len();
    let mut deleted_entries = 0usize;
    let mut freed_bytes = 0u64;
    let mut skipped_loaded = 0usize;
    let mut skipped_locked = 0usize;
    let mut errors = Vec::new();
    let mut candidates = Vec::new();
    let max_results = options.max_results.max(1);

    for entry in &snapshot.entries {
        let Some(reasons) = selected.get(&entry.storage_dir).cloned() else {
            continue;
        };
        let storage_dir = PathBuf::from(&entry.storage_dir);
        let mut outcome = if options.apply {
            "pending".to_string()
        } else {
            "would_delete".to_string()
        };
        let mut candidate_error = None;

        if entry.loaded {
            skipped_loaded += 1;
            outcome = "skipped_loaded".to_string();
        } else if options.apply {
            match try_acquire_index_gc_lock(&storage_dir) {
                Ok(Some(_lock)) => match fs::remove_dir_all(&storage_dir) {
                    Ok(()) => {
                        deleted_entries += 1;
                        freed_bytes = freed_bytes.saturating_add(entry.size_bytes);
                        outcome = "deleted".to_string();
                    }
                    Err(error) => {
                        let message =
                            format!("index_gc_delete_failed: {}: {error}", storage_dir.display());
                        errors.push(message.clone());
                        candidate_error = Some(message);
                        outcome = "error".to_string();
                    }
                },
                Ok(None) => {
                    skipped_locked += 1;
                    outcome = "skipped_locked".to_string();
                }
                Err(error) => {
                    errors.push(error.clone());
                    candidate_error = Some(error);
                    outcome = "error".to_string();
                }
            }
        }

        if candidates.len() < max_results {
            candidates.push(IndexGcCandidate {
                workspace_root: entry.workspace_root.clone(),
                storage_dir: entry.storage_dir.clone(),
                size_bytes: entry.size_bytes,
                last_used_at: entry.last_used_at,
                reasons,
                outcome,
                error: candidate_error,
            });
        }
    }

    let storage_size_after_bytes = if options.apply {
        directory_size(root)
    } else {
        snapshot.total_size_bytes
    };
    IndexGcReport {
        apply: options.apply,
        storage_root: snapshot.root,
        storage_size_before_bytes: snapshot.total_size_bytes,
        projected_size_after_bytes: projected_size,
        storage_size_after_bytes,
        scanned_entries: snapshot.entry_count,
        selected_entries,
        deleted_entries,
        freed_bytes,
        skipped_loaded,
        skipped_locked,
        errors,
        candidates_truncated: selected_entries > candidates.len(),
        candidates,
    }
}

fn read_sidecar_meta(storage_dir: &Path) -> Option<WorkspaceMeta> {
    let payload = fs::read(storage_dir.join("meta.json")).ok()?;
    serde_json::from_slice(&payload).ok()
}

fn read_index_last_used(storage_dir: &Path) -> Option<u64> {
    fs::read_to_string(storage_dir.join(INDEX_LAST_USED_FILE))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

fn touch_index_usage(storage_dir: &Path) -> Result<(), String> {
    if !storage_dir.is_dir() {
        return Err(format!(
            "index_storage_dir_missing: {}",
            storage_dir.display()
        ));
    }
    fs::write(
        storage_dir.join(INDEX_LAST_USED_FILE),
        current_unix_timestamp().to_string(),
    )
    .map_err(|error| format!("index_last_used_write_failed: {error}"))
}

fn index_active_lock_path(storage_dir: &Path) -> Result<PathBuf, String> {
    let root = storage_dir
        .parent()
        .ok_or_else(|| "index_storage_root_missing".to_string())?;
    let name = storage_dir
        .file_name()
        .ok_or_else(|| "index_storage_name_missing".to_string())?;
    Ok(root
        .join(INDEX_ACTIVE_LOCK_DIR)
        .join(Path::new(name).with_extension("lock")))
}

fn open_index_active_lock(storage_dir: &Path) -> Result<File, String> {
    let lock_path = index_active_lock_path(storage_dir)?;
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "index_lock_dir_create_failed: {}: {error}",
                parent.display()
            )
        })?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("index_lock_open_failed: {}: {error}", lock_path.display()))
}

fn acquire_index_active_lock(storage_dir: &Path) -> Result<Arc<File>, String> {
    let lock_file = open_index_active_lock(storage_dir)?;
    lock_file.lock_shared().map_err(|error| {
        format!(
            "index_shared_lock_failed: {}: {error}",
            storage_dir.display()
        )
    })?;
    Ok(Arc::new(lock_file))
}

fn try_acquire_index_gc_lock(storage_dir: &Path) -> Result<Option<File>, String> {
    let lock_file = open_index_active_lock(storage_dir)?;
    match lock_file.try_lock() {
        Ok(()) => Ok(Some(lock_file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(format!(
            "index_gc_lock_failed: {}: {error}",
            storage_dir.display()
        )),
    }
}

fn index_storage_dir_for_workspace(workspace_root: &Path) -> PathBuf {
    let storage_root = index_storage_root();
    let storage_dir = storage_root.join(hash_workspace_root(workspace_root));
    migrate_legacy_workspace_storage(&storage_root, workspace_root, &storage_dir);
    storage_dir
}

fn index_storage_root() -> PathBuf {
    if let Some(custom_dir) = env_var_os(INDEX_DIR_ENV_VARS) {
        return PathBuf::from(custom_dir).join("index-v2");
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(test_root) = test_index_storage_root(&executable, std::process::id())
    {
        return test_root;
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("codeloupe-mcp")
            .join("index-v2");
    }
    if let Some(xdg_cache_home) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg_cache_home)
            .join("codeloupe-mcp")
            .join("index-v2");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("codeloupe-mcp")
            .join("index-v2");
    }
    std::env::temp_dir().join("codeloupe-mcp").join("index-v2")
}

fn ensure_index_write_budget(additional_bytes: u64) -> Result<(), String> {
    let root = index_storage_root();
    let current_size = directory_size(&root);
    let max_total = default_index_max_total_bytes();
    if current_size.saturating_add(additional_bytes) > max_total {
        return Err(format!(
            "disk_budget:max_total_bytes:{}+{}>{max_total}",
            current_size, additional_bytes
        ));
    }

    let probe = existing_storage_probe(&root);
    let free =
        available_space(&probe).map_err(|error| format!("disk_budget:free_space:{error}"))?;
    let total = total_space(&probe).map_err(|error| format!("disk_budget:total_space:{error}"))?;
    let min_free = index_min_free_bytes().max(total / 10);
    if free.saturating_sub(additional_bytes) < min_free {
        return Err(format!(
            "disk_budget:min_free_bytes:{}-{}<{min_free}",
            free, additional_bytes
        ));
    }
    Ok(())
}

fn existing_storage_probe(path: &Path) -> PathBuf {
    let mut current = path;
    while !current.exists() {
        let Some(parent) = current.parent() else {
            return PathBuf::from(".");
        };
        current = parent;
    }
    current.to_path_buf()
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn scan_limit_count(error: &str) -> Option<usize> {
    error
        .split("entries_seen=")
        .nth(1)
        .and_then(|value| value.parse::<usize>().ok())
        .or_else(|| {
            error
                .split("entry_budget_exceeded:")
                .nth(1)
                .and_then(|value| value.split('>').next())
                .and_then(|value| value.parse::<usize>().ok())
        })
}

fn test_index_storage_root(executable: &Path, process_id: u32) -> Option<PathBuf> {
    let parent = executable.parent()?;
    if parent.file_name()?.to_string_lossy() != "deps" {
        return None;
    }

    let stem = executable.file_stem()?.to_string_lossy();
    if !stem.starts_with("test_")
        && !stem.starts_with("codeloupe_mcp-")
        && !stem.starts_with("codeloupe-mcp-")
    {
        return None;
    }

    Some(
        parent
            .join(".codeloupe-mcp-test-indexes")
            .join(process_id.to_string())
            .join("index-v2"),
    )
}

fn hash_workspace_root(workspace_root: &Path) -> String {
    let normalized = normalize_path_for_identity(workspace_root);
    let mut hash = 0xcbf29ce484222325u64;
    for byte in normalized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn migrate_legacy_workspace_storage(storage_root: &Path, workspace_root: &Path, stable_dir: &Path) {
    if stable_dir.exists() || !storage_root.is_dir() {
        return;
    }

    let workspace_key = normalize_path_for_identity(workspace_root);
    let Some(legacy_dir) = fs::read_dir(storage_root)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .find(|candidate| {
            candidate != stable_dir
                && candidate.is_dir()
                && candidate.file_name().is_some_and(|name| name != ".locks")
                && read_sidecar_meta(candidate)
                    .is_some_and(|meta| meta.workspace_root == workspace_key)
        })
    else {
        return;
    };

    match fs::rename(&legacy_dir, stable_dir) {
        Ok(()) => info!(
            workspace = %workspace_root.display(),
            from = %legacy_dir.display(),
            to = %stable_dir.display(),
            "Migrated workspace index to stable storage hash"
        ),
        Err(_error) if stable_dir.exists() => {}
        Err(error) => warn!(
            workspace = %workspace_root.display(),
            from = %legacy_dir.display(),
            to = %stable_dir.display(),
            error = %error,
            "Failed to migrate workspace index to stable storage hash"
        ),
    }
}

fn write_sidecar_meta(storage_dir: &Path, meta: &WorkspaceMeta) -> Result<(), String> {
    let payload =
        serde_json::to_vec_pretty(meta).map_err(|e| format!("meta_serialize_failed: {e}"))?;
    fs::write(storage_dir.join("meta.json"), payload).map_err(|e| format!("meta_write_failed: {e}"))
}

/// Total size of the files under `path`, including subdirectories such as the
/// Tantivy segment directory. Symlinks are not followed.
pub fn directory_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(read_dir) = fs::read_dir(path) else {
        return 0;
    };
    for entry in read_dir.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total = total.saturating_add(directory_size(&entry.path()));
        } else if file_type.is_file()
            && let Ok(metadata) = entry.metadata()
        {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

fn canonicalize_or_original(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn path_belongs_to_workspace(workspace_root: &Path, path: &Path) -> bool {
    let workspace_key = normalize_path_for_identity(workspace_root);
    let path_key = normalize_path_for_identity(path);
    path_key == workspace_key || path_key.starts_with(&(workspace_key + "/"))
}

fn normalize_path_for_identity(path: &Path) -> String {
    let normalized = normalize_path(path);
    #[cfg(windows)]
    {
        normalized.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

fn normalize_path(path: &Path) -> String {
    crate::common::normalize_display_path(path)
}

fn file_name_from_relative_path(relative_path: &str) -> Option<&str> {
    relative_path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
}

fn metadata_modified_secs(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn index_map_size_bytes() -> u64 {
    DEFAULT_INDEX_MAP_SIZE_MB
        .saturating_mul(1024)
        .saturating_mul(1024)
}

fn tantivy_enabled() -> bool {
    env_var_os(TANTIVY_ENABLED_ENV_VARS)
        .map(|value| {
            !matches!(
                value.to_string_lossy().to_ascii_lowercase().as_str(),
                "0" | "false" | "off"
            )
        })
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    fn wait_for_content_ready(path: &Path) -> ContentZoneStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = content_status_for_path(path);
            if status.ready {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "content index did not become ready: {status:?}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn canonical_test_root(dir: &tempfile::TempDir) -> PathBuf {
        canonicalize_or_original(dir.path().to_path_buf())
    }

    #[test]
    fn workspace_hash_is_stable() {
        assert_eq!(
            hash_workspace_root(Path::new("workspace")),
            "40e26138f4336c36"
        );
    }

    #[test]
    fn legacy_workspace_storage_is_migrated_to_stable_hash() {
        let index_root = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let legacy_dir = index_root.path().join("legacy-default-hasher");
        let stable_dir = index_root
            .path()
            .join(hash_workspace_root(workspace.path()));
        fs::create_dir_all(&legacy_dir).unwrap();
        write_sidecar_meta(
            &legacy_dir,
            &WorkspaceMeta {
                schema_version: INDEX_SCHEMA_VERSION,
                workspace_root: normalize_path_for_identity(workspace.path()),
                ..WorkspaceMeta::default()
            },
        )
        .unwrap();
        fs::write(legacy_dir.join("payload.bin"), b"legacy").unwrap();

        migrate_legacy_workspace_storage(index_root.path(), workspace.path(), &stable_dir);

        assert!(!legacy_dir.exists());
        assert_eq!(fs::read(stable_dir.join("payload.bin")).unwrap(), b"legacy");
    }

    #[test]
    fn children_records_store_names_only() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let key = normalize_path_for_identity(root);
        refresh_metadata_index(root, &key).unwrap();
        let store = open_store(root).unwrap();
        let rtxn = store.env.read_txn().unwrap();
        let children = store.children_db.get(&rtxn, "src").unwrap().unwrap();
        assert_eq!(children, vec!["lib.rs".to_string()]);
    }

    #[test]
    fn lmdb_upsert_load_delete_roundtrip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let key = normalize_path_for_identity(root);
        let summary = refresh_metadata_index(root, &key).unwrap();
        assert_eq!(summary.files, 1);

        let (_meta, index) = load_existing_index(&key, root).unwrap().unwrap();
        assert_eq!(index.live_entries, 1);

        fs::remove_file(root.join("a.rs")).unwrap();
        let summary = refresh_metadata_index(root, &key).unwrap();
        assert_eq!(summary.files, 0);
        let (_meta, index) = load_existing_index(&key, root).unwrap().unwrap();
        assert_eq!(index.live_entries, 0);
    }

    #[test]
    fn remove_workspace_index_releases_cached_handles_before_deleting_storage() {
        let workspace = tempdir().unwrap();
        let root = canonical_test_root(&workspace);
        fs::write(root.join("lib.rs"), "fn sample() {}\n").unwrap();
        let workspace_key = normalize_path_for_identity(&root);
        assert!(ensure_runtime_loaded(
            &workspace_key,
            &root,
            "remove_workspace_index_test"
        ));
        refresh_metadata_index(&root, &workspace_key).unwrap();

        let storage_dir = index_storage_dir_for_workspace(&root);
        let store_key = normalize_path_for_identity(&storage_dir);
        let tantivy_dir = storage_dir.join("tantivy-content");
        open_or_create_tantivy(&tantivy_dir).unwrap();
        let search_store = open_tantivy_search_store(&tantivy_dir).unwrap();
        drop(search_store);
        let search_key = normalize_path_for_identity(&tantivy_dir);

        assert!(INDEX_STORES.contains_key(&store_key));
        assert!(TANTIVY_SEARCHERS.contains_key(&search_key));
        remove_workspace_index(&root).unwrap();

        assert!(!storage_dir.exists());
        assert!(!INDEX_STORES.contains_key(&store_key));
        assert!(!TANTIVY_SEARCHERS.contains_key(&search_key));
        assert!(!PATH_INDEXES.contains_key(&workspace_key));
        assert!(INDEX_RUNTIMES.read().unwrap().get(&workspace_key).is_none());
    }

    #[test]
    fn remove_workspace_index_prevents_delayed_runtime_registration() {
        let workspace = tempdir().unwrap();
        let root = canonical_test_root(&workspace);
        fs::write(root.join("lib.rs"), "fn sample() {}\n").unwrap();
        let workspace_key = normalize_path_for_identity(&root);

        let load_guard = RUNTIME_LOAD_LOCK.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        schedule_workspace_index(root.clone(), "delayed_remove_test".to_string());
        while !ACTIVE_INDEX_LOADS.contains_key(&workspace_key) {
            assert!(Instant::now() < deadline, "index loader did not start");
            thread::yield_now();
        }

        let remove_root = root.clone();
        let remover = thread::spawn(move || remove_workspace_index(&remove_root));
        while !DISABLED_WORKSPACES.contains_key(&workspace_key) {
            assert!(Instant::now() < deadline, "index removal did not start");
            thread::yield_now();
        }
        drop(load_guard);

        remover.join().unwrap().unwrap();
        while ACTIVE_INDEX_LOADS.contains_key(&workspace_key) {
            assert!(Instant::now() < deadline, "index loader did not finish");
            thread::yield_now();
        }

        assert!(INDEX_RUNTIMES.read().unwrap().get(&workspace_key).is_none());
        assert!(!index_storage_dir_for_workspace(&root).exists());
        DISABLED_WORKSPACES.remove(&workspace_key);
    }

    #[test]
    fn tantivy_schema_can_index_and_delete_path_doc() {
        let dir = tempdir().unwrap();
        let index = open_or_create_tantivy(dir.path()).unwrap();
        let schema = index.schema();
        let fields = tantivy_fields(&schema).unwrap();
        assert!(!schema.get_field_entry(fields.path_tokens).is_stored());
        assert!(!schema.get_field_entry(fields.content).is_stored());
        {
            let mut writer: tantivy::IndexWriter<TantivyDocument> = index
                .writer_with_num_threads(1, TANTIVY_WRITER_MEMORY_BYTES_PER_THREAD)
                .unwrap();
            writer.delete_term(Term::from_field_text(fields.relative_path, "src/lib.rs"));
            writer
                .add_document(doc!(
                    fields.relative_path => "src/lib.rs",
                    fields.file_name => "lib.rs",
                    fields.path_tokens => "src lib rs",
                    fields.content => "fn indexed_symbol() {}",
                    fields.extension => "rs",
                    fields.size => 22u64,
                    fields.modified_at => 1u64
                ))
                .unwrap();
            writer.commit().unwrap();
        }

        {
            let reader: tantivy::IndexReader = index
                .reader_builder()
                .reload_policy(ReloadPolicy::Manual)
                .try_into()
                .unwrap();
            let searcher = reader.searcher();
            let parser = QueryParser::for_index(&index, vec![fields.content]);
            let parsed = parser.parse_query("indexed_symbol").unwrap();
            let top_docs = searcher
                .search(&parsed, &TopDocs::with_limit(10).order_by_score())
                .unwrap();
            assert_eq!(top_docs.len(), 1);
            let doc = searcher.doc::<TantivyDocument>(top_docs[0].1).unwrap();
            assert_eq!(
                doc.get_first(fields.relative_path)
                    .and_then(|value| value.as_str()),
                Some("src/lib.rs")
            );
        }

        {
            let mut writer: tantivy::IndexWriter<TantivyDocument> = index
                .writer_with_num_threads(1, TANTIVY_WRITER_MEMORY_BYTES_PER_THREAD)
                .unwrap();
            writer.delete_term(Term::from_field_text(fields.relative_path, "src/lib.rs"));
            writer.commit().unwrap();
        }

        {
            let reader: tantivy::IndexReader = index
                .reader_builder()
                .reload_policy(ReloadPolicy::Manual)
                .try_into()
                .unwrap();
            let searcher = reader.searcher();
            let parser = QueryParser::for_index(&index, vec![fields.content]);
            let parsed = parser.parse_query("indexed_symbol").unwrap();
            let top_docs = searcher
                .search(&parsed, &TopDocs::with_limit(10).order_by_score())
                .unwrap();
            assert!(top_docs.is_empty());
        }
    }

    #[test]
    fn path_shortlist_candidates_are_deterministic() {
        let root = PathBuf::from("C:/workspace");
        let entries = vec![
            IndexedPathEntry {
                relative_path: "src/zebra_main.rs".to_string(),
                is_dir: false,
                size: 1,
                modified_at: 1,
                extension_lower: "rs".to_string(),
                parent_relative_path: "src".to_string(),
                indexed_at: 1,
            },
            IndexedPathEntry {
                relative_path: "src/alpha_main.rs".to_string(),
                is_dir: false,
                size: 1,
                modified_at: 1,
                extension_lower: "rs".to_string(),
                parent_relative_path: "src".to_string(),
                indexed_at: 1,
            },
        ];
        let index = PathIndex::from_entries(&root, entries);

        let first = index
            .shortlist_candidates(&["main".to_string()], None, 1)
            .unwrap();
        let second = index
            .shortlist_candidates(&["main".to_string()], None, 1)
            .unwrap();

        assert_eq!(first[0].path, second[0].path);
        assert_eq!(first[0].path, root.join("src/zebra_main.rs"));
    }

    #[test]
    fn stale_content_zones_are_removed_when_metadata_is_loaded() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/live")).unwrap();
        fs::write(root.join("src/file.rs"), "fn sample() {}\n").unwrap();
        let workspace_key = normalize_path_for_identity(root);
        refresh_metadata_index(root, &workspace_key).unwrap();
        let store = open_store(root).unwrap();
        let stale_zones = vec![
            "src/live".to_string(),
            "src/live".to_string(),
            "src/file.rs".to_string(),
            "missing".to_string(),
            "src/**/*.rs".to_string(),
            "../outside".to_string(),
        ];
        {
            let mut wtxn = store.env.write_txn().unwrap();
            let mut meta = store.meta_db.get(&wtxn, "workspace").unwrap().unwrap();
            meta.content_index_status = "ready".to_string();
            meta.content_index_zones = stale_zones;
            meta.indexed_content_files = 99;
            meta.indexed_content_bytes = 999;
            store.meta_db.put(&mut wtxn, "workspace", &meta).unwrap();
            wtxn.commit().unwrap();
            write_sidecar_meta(&store.storage_dir, &meta).unwrap();
        }

        assert!(ensure_runtime_loaded(
            &workspace_key,
            root,
            "metadata_reload_test"
        ));
        let runtime = get_runtime_snapshots()
            .into_iter()
            .find(|runtime| runtime.workspace_root == normalize_path(root))
            .unwrap();
        assert_eq!(runtime.content_index_zones, vec!["src/live".to_string()]);
        assert_eq!(runtime.indexed_content_files, 0);
        assert_eq!(runtime.indexed_content_bytes, 0);

        let rtxn = store.env.read_txn().unwrap();
        let persisted = store.meta_db.get(&rtxn, "workspace").unwrap().unwrap();
        assert_eq!(persisted.content_index_zones, vec!["src/live".to_string()]);
        drop(rtxn);
        let sidecar: WorkspaceMeta =
            serde_json::from_slice(&fs::read(store.storage_dir.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(sidecar.content_index_zones, vec!["src/live".to_string()]);
    }

    #[test]
    fn content_warm_works_before_initial_metadata_scan_finishes() {
        let dir = tempdir().unwrap();
        let root = canonical_test_root(&dir);
        let zone_path = root.join("src");
        fs::create_dir_all(&zone_path).unwrap();
        fs::write(zone_path.join("a.rs"), "fn indexed_symbol() {}\n").unwrap();
        let key = normalize_path_for_identity(&root);

        assert!(ensure_runtime_loaded(&key, &root, "first_warm_test"));
        let scanning = content_status_for_path(&zone_path);
        assert_eq!(scanning.workspace_root, Some(normalize_path(&root)));
        assert_eq!(scanning.zone.as_deref(), Some("src"));
        assert_eq!(scanning.status, "workspace_scanning");
        assert!(!scanning.ready);
        assert!(!scanning.warming);

        let scheduled = warm_content_index_paths(std::slice::from_ref(&zone_path), false, false);
        assert_eq!(scheduled.len(), 1);
        assert_eq!(scheduled[0].workspace_root, Some(normalize_path(&root)));
        assert_eq!(scheduled[0].status, "warming");
        assert!(scheduled[0].warming);

        let ready = wait_for_content_ready(&zone_path);
        assert_eq!(ready.status, "ready");
        assert!(ready.indexed);
    }

    #[test]
    fn persisted_warming_status_is_reloaded_as_stale_and_can_rewarm() {
        let dir = tempdir().unwrap();
        let root = canonical_test_root(&dir);
        let zone_path = root.join("src");
        fs::create_dir_all(&zone_path).unwrap();
        fs::write(zone_path.join("a.rs"), "fn indexed_symbol() {}\n").unwrap();
        let key = normalize_path_for_identity(&root);
        refresh_metadata_index(&root, &key).unwrap();
        let store = open_store(&root).unwrap();
        {
            let mut wtxn = store.env.write_txn().unwrap();
            let mut meta = store.meta_db.get(&wtxn, "workspace").unwrap().unwrap();
            meta.content_index_status = "warming".to_string();
            meta.content_index_zones = vec!["src".to_string()];
            store.meta_db.put(&mut wtxn, "workspace", &meta).unwrap();
            wtxn.commit().unwrap();
            write_sidecar_meta(&store.storage_dir, &meta).unwrap();
        }

        assert!(ensure_runtime_loaded(&key, &root, "stale_warming_test"));
        let stale = content_status_for_path(&zone_path);
        assert_eq!(stale.status, "stale");
        assert!(stale.indexed);
        assert!(!stale.ready);
        assert!(!stale.warming);
        let (persisted, _) = load_existing_index(&key, &root).unwrap().unwrap();
        assert_eq!(persisted.content_index_status, "stale");

        let scheduled = warm_content_index_paths(std::slice::from_ref(&zone_path), false, false);
        assert_eq!(scheduled[0].status, "warming");
        let ready = wait_for_content_ready(&zone_path);
        assert_eq!(ready.status, "ready");
    }

    #[test]
    fn content_refresh_retries_while_tantivy_writer_lock_is_busy() {
        let dir = tempdir().unwrap();
        let root = canonical_test_root(&dir);
        let zone_path = root.join("src");
        fs::create_dir_all(&zone_path).unwrap();
        fs::write(zone_path.join("a.rs"), "fn indexed_symbol() {}\n").unwrap();
        let key = normalize_path_for_identity(&root);
        refresh_metadata_index(&root, &key).unwrap();
        ensure_runtime_loaded(&key, &root, "writer_busy_test");

        let store = open_store(&root).unwrap();
        let index = open_or_create_tantivy(&store.tantivy_dir).unwrap();
        let held_writer: tantivy::IndexWriter<TantivyDocument> = index
            .writer_with_num_threads(1, TANTIVY_WRITER_MEMORY_BYTES_PER_THREAD)
            .unwrap();

        let scheduled = warm_content_index_paths(std::slice::from_ref(&zone_path), false, false);
        assert_eq!(scheduled[0].status, "warming");
        thread::sleep(Duration::from_millis(300));
        let waiting = content_status_for_path(&zone_path);
        assert_eq!(waiting.status, "warming");
        assert!(waiting.warming);

        drop(held_writer);
        let ready = wait_for_content_ready(&zone_path);
        assert_eq!(ready.status, "ready");
    }

    #[test]
    fn content_status_persists_to_lmdb_meta() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("base")).unwrap();
        fs::write(root.join("base/a.rs"), "fn indexed_symbol() {}\n").unwrap();
        let key = normalize_path_for_identity(root);
        refresh_metadata_index(root, &key).unwrap();
        ensure_runtime_loaded(&key, root, "test");

        record_content_status(&key, "ready", Some("base".to_string()), false, 1, 24);

        let (meta, _) = load_existing_index(&key, root).unwrap().unwrap();
        assert_eq!(meta.content_index_status, "ready");
        assert_eq!(meta.content_index_zones, vec!["base".to_string()]);
        assert!(!meta.content_index_partial);
        assert_eq!(meta.indexed_content_files, 1);
        assert_eq!(meta.indexed_content_bytes, 23);

        record_runtime_error(&key, "stale content refresh error".to_string());
        record_content_status(&key, "ready", Some("base".to_string()), false, 2, 48);

        let runtime = INDEX_RUNTIMES.read().unwrap().get(&key).cloned().unwrap();
        assert_eq!(runtime.content_index_status, "ready");
        assert_eq!(runtime.last_error, None);

        let (meta, _) = load_existing_index(&key, root).unwrap().unwrap();
        assert_eq!(meta.content_index_zones, vec!["base".to_string()]);
        assert_eq!(meta.indexed_content_files, 1);
        assert_eq!(meta.indexed_content_bytes, 23);
    }

    #[test]
    fn metadata_refresh_preserves_runtime_content_status() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "fn indexed_symbol() {}\n").unwrap();
        let key = normalize_path_for_identity(root);
        refresh_metadata_index(root, &key).unwrap();
        ensure_runtime_loaded(&key, root, "test");
        record_content_status(&key, "ready", Some("src".to_string()), false, 1, 24);

        fs::write(root.join("src/b.rs"), "fn added_symbol() {}\n").unwrap();
        refresh_metadata_index(root, &key).unwrap();

        let (meta, _) = load_existing_index(&key, root).unwrap().unwrap();
        assert_eq!(meta.content_index_status, "ready");
        assert_eq!(meta.content_index_zones, vec!["src".to_string()]);
    }

    #[test]
    fn content_status_reports_refreshing_for_previously_indexed_zone() {
        let dir = tempdir().unwrap();
        let root = canonical_test_root(&dir);
        let zone_path = root.join("base");
        let other_path = root.join("other");
        fs::create_dir_all(&zone_path).unwrap();
        fs::create_dir_all(&other_path).unwrap();
        fs::write(zone_path.join("a.rs"), "fn indexed_symbol() {}\n").unwrap();
        fs::write(other_path.join("b.rs"), "fn other_symbol() {}\n").unwrap();
        let key = normalize_path_for_identity(&root);
        refresh_metadata_index(&root, &key).unwrap();
        ensure_runtime_loaded(&key, &root, "test");
        record_content_status(&key, "ready", Some("base".to_string()), false, 1, 24);
        record_content_status(&key, "ready", Some("other".to_string()), false, 1, 22);

        let refresh_key = content_refresh_key(&key, "base");
        ACTIVE_CONTENT_REFRESHES.insert(refresh_key.clone(), ());
        record_content_status(&key, "warming", None, false, 0, 0);
        let warming = content_status_for_path(&zone_path);
        assert!(warming.indexed);
        assert!(warming.warming);
        assert!(!warming.ready);
        assert_eq!(warming.status, "warming");
        let unrelated = content_status_for_path(&other_path);
        assert!(unrelated.indexed);
        assert!(!unrelated.warming);
        assert!(unrelated.ready);
        assert_eq!(unrelated.status, "ready");

        ACTIVE_CONTENT_REFRESHES.remove(&refresh_key);
        let ready = content_status_for_path(&zone_path);
        assert!(ready.indexed);
        assert!(!ready.warming);
        assert!(ready.ready);
        assert_eq!(ready.status, "ready");
    }

    #[test]
    fn parent_content_zone_covers_nested_paths() {
        let dir = tempdir().unwrap();
        let root = canonical_test_root(&dir);
        let nested_path = root.join("src/nested");
        fs::create_dir_all(&nested_path).unwrap();
        fs::write(nested_path.join("lib.rs"), "fn nested_symbol() {}\n").unwrap();
        let key = normalize_path_for_identity(&root);
        refresh_metadata_index(&root, &key).unwrap();
        ensure_runtime_loaded(&key, &root, "test");
        record_content_status(&key, "ready", Some("src".to_string()), false, 1, 22);

        let status = content_status_for_path(&nested_path);
        assert!(status.ready);
        assert!(status.indexed);
        assert_eq!(status.zone.as_deref(), Some("src"));
        assert!(ready_content_zone(&key, "src/nested").is_some());

        let refresh_key = content_refresh_key(&key, "src");
        ACTIVE_CONTENT_REFRESHES.insert(refresh_key.clone(), ());
        record_content_status(&key, "warming", None, false, 0, 0);
        let warming = content_status_for_path(&nested_path);
        assert!(warming.indexed);
        assert!(warming.warming);
        assert!(!warming.ready);
        assert_eq!(warming.zone.as_deref(), Some("src"));
        assert_eq!(warming.status, "warming");
        ACTIVE_CONTENT_REFRESHES.remove(&refresh_key);
    }

    #[test]
    fn content_zone_sanitization_collapses_nested_children() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();

        let zones = sanitize_content_zones(
            dir.path(),
            vec!["src/nested".to_string(), "src".to_string()],
        );

        assert_eq!(zones, vec!["src".to_string()]);

        let key = normalize_path_for_identity(dir.path());
        refresh_metadata_index(dir.path(), &key).unwrap();
        ensure_runtime_loaded(&key, dir.path(), "test");
        record_content_status(&key, "ready", Some("src/nested".to_string()), false, 0, 0);
        record_content_status(&key, "ready", Some("src".to_string()), false, 0, 0);

        assert_eq!(runtime_content_zones(&key), vec!["src".to_string()]);
    }

    #[test]
    fn ignore_rule_detection_respects_supported_sources_and_precedence() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::create_dir_all(root.join("shared")).unwrap();
        fs::create_dir_all(root.join("gitignored")).unwrap();
        fs::create_dir_all(root.join("excluded")).unwrap();
        fs::create_dir_all(root.join("visible")).unwrap();
        fs::write(root.join(".gitignore"), "!shared/\ngitignored/\n").unwrap();
        fs::write(root.join(".ignore"), "shared/\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "excluded/\n").unwrap();

        assert!(path_is_ignored_by_workspace_rules(
            root,
            &root.join("shared")
        ));
        assert!(path_is_ignored_by_workspace_rules(
            root,
            &root.join("gitignored")
        ));
        assert!(path_is_ignored_by_workspace_rules(
            root,
            &root.join("excluded")
        ));
        assert!(!path_is_ignored_by_workspace_rules(
            root,
            &root.join("visible")
        ));
    }

    #[test]
    fn content_workspace_budget_counts_other_ready_zones() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("base")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(root.join("base/a.rs"), vec![b'a'; 60]).unwrap();
        fs::write(root.join("other/b.rs"), vec![b'b'; 60]).unwrap();
        let key = normalize_path_for_identity(root);
        refresh_metadata_index(root, &key).unwrap();
        ensure_runtime_loaded(&key, root, "test");
        record_content_status(&key, "ready", Some("base".to_string()), false, 1, 60);
        let policy = ContentPolicy {
            max_file_bytes: 1_000,
            max_zone_bytes: 100,
            max_workspace_bytes: 100,
        };

        let remaining = content_workspace_budget_for_zone(&key, root, "other", &policy).unwrap();

        assert_eq!(remaining, 40);
    }

    #[test]
    fn content_policy_default_bounds_workspace_for_dev_use() {
        let policy = ContentPolicy::default();

        assert!(policy.max_zone_bytes <= 128 * 1024 * 1024);
        assert!(policy.max_workspace_bytes <= 256 * 1024 * 1024);
    }

    #[test]
    fn content_policy_respects_extension_size_and_generated_scopes() {
        let policy = ContentPolicy {
            max_file_bytes: 100,
            max_zone_bytes: 1_000,
            max_workspace_bytes: 1_000,
        };
        let mut record = IndexedPathRecord {
            path: PathBuf::from("src/lib.rs"),
            relative_path: "src/lib.rs".to_string(),
            file_name: "lib.rs".to_string(),
            extension_lower: "rs".to_string(),
            is_dir: false,
            size: 50,
            modified_at: 1,
        };

        assert!(content_policy_allows(&record, &policy, false, false));
        record.size = 101;
        assert!(!content_policy_allows(&record, &policy, false, false));

        record.size = 50;
        record.extension_lower = "png".to_string();
        assert!(!content_policy_allows(&record, &policy, false, false));

        record.extension_lower = "cc".to_string();
        record.relative_path = "third_party/lib/a.cc".to_string();
        assert!(!content_policy_allows(&record, &policy, false, false));
        assert!(content_policy_allows(&record, &policy, true, false));

        record.relative_path = "obj/Debug/generated.cc".to_string();
        assert!(!content_policy_allows(&record, &policy, false, false));
        record.relative_path = "build/generated.cc".to_string();
        assert!(!content_policy_allows(&record, &policy, false, false));
        record.relative_path = "bin/Debug/generated.cc".to_string();
        assert!(!content_policy_allows(&record, &policy, false, true));
        assert!(content_policy_allows(&record, &policy, false, false));
    }

    #[test]
    fn test_executables_use_isolated_index_storage() {
        let deps = PathBuf::from("target").join("debug").join("deps");
        let test_executable = deps.join(format!(
            "test_api_cleanup-abcd{}",
            std::env::consts::EXE_SUFFIX
        ));
        let expected = deps
            .join(".codeloupe-mcp-test-indexes")
            .join("42")
            .join("index-v2");
        assert_eq!(
            test_index_storage_root(&test_executable, 42),
            Some(expected)
        );

        let server_executable = PathBuf::from("target")
            .join("debug")
            .join(format!("codeloupe-mcp{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(test_index_storage_root(&server_executable, 42), None);
    }

    fn write_gc_fixture(
        index_root: &Path,
        name: &str,
        workspace_root: &Path,
        last_used_at: u64,
        payload_bytes: usize,
    ) -> PathBuf {
        let storage_dir = index_root.join(name);
        fs::create_dir_all(&storage_dir).unwrap();
        let workspace_root = canonicalize_or_original(workspace_root.to_path_buf());
        let meta = WorkspaceMeta {
            schema_version: INDEX_SCHEMA_VERSION,
            workspace_root: normalize_path_for_identity(&workspace_root),
            saved_at: last_used_at,
            scan_complete: true,
            indexed_entries_count: 1,
            indexed_files_count: 1,
            indexed_dirs_count: 0,
            last_full_scan_at: Some(last_used_at),
            content_index_enabled: false,
            content_index_status: "disabled".to_string(),
            content_index_zones: Vec::new(),
            content_index_partial: false,
            indexed_content_files: 0,
            indexed_content_bytes: 0,
        };
        write_sidecar_meta(&storage_dir, &meta).unwrap();
        fs::write(
            storage_dir.join(INDEX_LAST_USED_FILE),
            last_used_at.to_string(),
        )
        .unwrap();
        fs::write(storage_dir.join("payload.bin"), vec![0u8; payload_bytes]).unwrap();
        storage_dir
    }

    #[test]
    fn index_gc_removes_old_orphans_and_reports_unloaded_storage() {
        let fixture = tempdir().unwrap();
        let index_root = fixture.path().join("index-v2");
        let live_workspace = fixture.path().join("live-workspace");
        fs::create_dir_all(&live_workspace).unwrap();
        let missing_workspace = fixture.path().join("missing-workspace");
        let now = current_unix_timestamp();
        let live_storage = write_gc_fixture(&index_root, "live", &live_workspace, now, 64);
        let orphan_storage = write_gc_fixture(&index_root, "orphan", &missing_workspace, 1, 128);

        let before = inspect_index_storage_root(&index_root);
        assert_eq!(before.entry_count, 2);
        assert_eq!(before.unloaded_count, 2);
        assert_eq!(before.orphaned_count, 1);

        let report = run_index_gc_at_root(
            &index_root,
            IndexGcOptions {
                apply: true,
                remove_orphans: true,
                max_age_secs: None,
                max_total_bytes: None,
                workspace_roots: Vec::new(),
                max_results: 10,
            },
        );

        assert_eq!(report.selected_entries, 1);
        assert_eq!(report.deleted_entries, 1);
        assert!(live_storage.is_dir());
        assert!(!orphan_storage.exists());
    }

    #[test]
    fn index_gc_enforces_storage_cap_by_lru() {
        let fixture = tempdir().unwrap();
        let index_root = fixture.path().join("index-v2");
        let old_workspace = fixture.path().join("old-workspace");
        let new_workspace = fixture.path().join("new-workspace");
        fs::create_dir_all(&old_workspace).unwrap();
        fs::create_dir_all(&new_workspace).unwrap();
        let old_storage = write_gc_fixture(&index_root, "old", &old_workspace, 10, 256);
        let new_storage = write_gc_fixture(&index_root, "new", &new_workspace, 20, 256);
        let before = inspect_index_storage_root(&index_root);
        let old_size = before
            .entries
            .iter()
            .find(|entry| entry.storage_dir.ends_with("old"))
            .unwrap()
            .size_bytes;

        let report = run_index_gc_at_root(
            &index_root,
            IndexGcOptions {
                apply: true,
                remove_orphans: false,
                max_age_secs: None,
                max_total_bytes: Some(before.total_size_bytes.saturating_sub(old_size)),
                workspace_roots: Vec::new(),
                max_results: 10,
            },
        );

        assert_eq!(report.deleted_entries, 1);
        assert!(!old_storage.exists());
        assert!(new_storage.is_dir());
        assert_eq!(report.candidates[0].reasons, ["storage_limit_lru"]);
    }

    #[test]
    fn index_gc_skips_indexes_locked_by_another_runtime() {
        let fixture = tempdir().unwrap();
        let index_root = fixture.path().join("index-v2");
        let workspace = fixture.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let storage = write_gc_fixture(&index_root, "locked", &workspace, 1, 64);
        let _active_lock = acquire_index_active_lock(&storage).unwrap();

        let report = run_index_gc_at_root(
            &index_root,
            IndexGcOptions {
                apply: true,
                remove_orphans: false,
                max_age_secs: None,
                max_total_bytes: None,
                workspace_roots: vec![workspace],
                max_results: 10,
            },
        );

        assert_eq!(report.selected_entries, 1);
        assert_eq!(report.deleted_entries, 0);
        assert_eq!(report.skipped_locked, 1);
        assert!(storage.is_dir());
    }

    #[test]
    fn index_gc_dry_run_never_deletes_selected_index() {
        let fixture = tempdir().unwrap();
        let index_root = fixture.path().join("index-v2");
        let workspace = fixture.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let storage = write_gc_fixture(&index_root, "dry-run", &workspace, 1, 64);

        let report = run_index_gc_at_root(
            &index_root,
            IndexGcOptions {
                apply: false,
                remove_orphans: false,
                max_age_secs: None,
                max_total_bytes: None,
                workspace_roots: vec![workspace],
                max_results: 10,
            },
        );

        assert_eq!(report.selected_entries, 1);
        assert_eq!(report.deleted_entries, 0);
        assert_eq!(report.candidates[0].outcome, "would_delete");
        assert!(storage.is_dir());
    }
}

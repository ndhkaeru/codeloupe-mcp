use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

const INDEX_MODE_ENV: &str = "CODELOUPE_MCP_INDEX_MODE";
const INDEX_ADVICE_ENV: &str = "CODELOUPE_MCP_INDEX_ADVICE";
const INDEX_ADVICE_MIN_FILES_ENV: &str = "CODELOUPE_MCP_INDEX_ADVICE_MIN_FILES";
const DEFAULT_INDEX_ADVICE_MIN_FILES: usize = 20_000;
const DECLINE_TTL_SECS: u64 = 30 * 24 * 60 * 60;
const DECISIONS_VERSION: u32 = 1;
const CLIENT_ROOTS_LIST_SOURCE: &str = "client_roots_list";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexMode {
    Ask,
    Auto,
    Off,
}

impl IndexMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ConfiguredWorkspaceSnapshot {
    pub workspace_root: String,
    pub declared_roots: Vec<String>,
    pub write_roots: Vec<String>,
    pub source: String,
    pub index_approved: bool,
    pub write_allowed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceEstimate {
    pub workspace_root: String,
    pub recommendation: String,
    pub reasons: Vec<String>,
    pub entries_seen: usize,
    pub files: usize,
    pub total_file_bytes: u64,
    pub estimated_index_bytes: u64,
    pub content_index: bool,
    pub complete: bool,
    pub elapsed_ms: u64,
    pub limit_reason: Option<String>,
    pub narrower_candidates: Vec<WorkspaceNarrowerCandidate>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceNarrowerCandidate {
    pub path: String,
    pub entries: usize,
    pub files: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceCandidateSnapshot {
    pub workspace_root: String,
    pub source: String,
    pub status: String,
    pub reason: Option<String>,
    pub entries_seen: Option<usize>,
    pub files_seen: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct WriteWorkspaceScope {
    pub workspace_root: PathBuf,
    pub declared_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WriteSessionSnapshot {
    pub write_scope: &'static str,
    pub elicitation_supported: bool,
    pub approved_directories: Vec<String>,
    pub declined_directories: Vec<String>,
}

#[derive(Clone)]
struct ConfiguredWorkspace {
    root: PathBuf,
    source: String,
    index_approved: bool,
    declared_roots: BTreeMap<String, DeclaredWorkspaceRoot>,
}

#[derive(Clone)]
struct DeclaredWorkspaceRoot {
    root: PathBuf,
    persistent: bool,
    persistent_write_allowed: bool,
    roots_list: bool,
    roots_list_write_allowed: bool,
}

impl DeclaredWorkspaceRoot {
    fn write_allowed(&self) -> bool {
        self.persistent_write_allowed || self.roots_list_write_allowed
    }
}

#[derive(Clone)]
struct WorkspaceCandidate {
    root: PathBuf,
    source: String,
    status: String,
    reason: Option<String>,
    entries_seen: Option<usize>,
    files_seen: Option<usize>,
    scan_count: usize,
    advice_emitted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkspaceDecision {
    root: String,
    decision: String,
    by: String,
    decided_at: u64,
    estimate: Option<PersistedEstimate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedEstimate {
    entries_seen: usize,
    files: usize,
    estimated_index_bytes: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspaceDecisionFile {
    version: u32,
    workspaces: Vec<WorkspaceDecision>,
}

#[derive(Default)]
struct WorkspaceState {
    configured: BTreeMap<String, ConfiguredWorkspace>,
    roots_list_keys: BTreeSet<String>,
    candidates: BTreeMap<String, WorkspaceCandidate>,
    decisions: BTreeMap<String, WorkspaceDecision>,
    decisions_loaded: bool,
    active_root: Option<PathBuf>,
    active_source: Option<String>,
}

lazy_static::lazy_static! {
    static ref WORKSPACE_STATE: RwLock<WorkspaceState> = RwLock::new(WorkspaceState::default());
}

pub fn index_mode() -> IndexMode {
    match std::env::var(INDEX_MODE_ENV)
        .unwrap_or_else(|_| "ask".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "auto" => IndexMode::Auto,
        "off" => IndexMode::Off,
        _ => IndexMode::Ask,
    }
}

pub fn register_configured_workspace(
    root: PathBuf,
    source: impl Into<String>,
    index_approved: bool,
) -> Option<PathBuf> {
    register_workspace(root, source, index_approved, true, true)
}

fn register_index_workspace(
    root: PathBuf,
    source: impl Into<String>,
    index_approved: bool,
) -> Option<PathBuf> {
    register_workspace(root, source, index_approved, false, false)
}

fn register_workspace(
    root: PathBuf,
    source: impl Into<String>,
    index_approved: bool,
    write_allowed: bool,
    record_declared_root: bool,
) -> Option<PathBuf> {
    let declared_root = normalize_declared_root(root)?;
    let root = normalize_workspace_root(declared_root.clone())?;
    let source = source.into();
    let key = normalize_key(&root);
    let declared_key = normalize_key(&declared_root);
    let incoming_roots_list = source == CLIENT_ROOTS_LIST_SOURCE;
    if let Ok(mut state) = WORKSPACE_STATE.write() {
        let workspace = state
            .configured
            .entry(key)
            .and_modify(|existing| {
                existing.index_approved |= index_approved;
                if existing.source == CLIENT_ROOTS_LIST_SOURCE
                    || (!incoming_roots_list
                        && (index_approved
                            || write_allowed
                            || existing.source == "client_initialize"))
                {
                    existing.source = source.clone();
                }
            })
            .or_insert_with(|| ConfiguredWorkspace {
                root: root.clone(),
                source: source.clone(),
                index_approved,
                declared_roots: BTreeMap::new(),
            });
        if record_declared_root {
            let declared = workspace
                .declared_roots
                .entry(declared_key)
                .or_insert_with(|| DeclaredWorkspaceRoot {
                    root: declared_root,
                    persistent: false,
                    persistent_write_allowed: false,
                    roots_list: false,
                    roots_list_write_allowed: false,
                });
            if incoming_roots_list {
                declared.roots_list = true;
                declared.roots_list_write_allowed |= write_allowed;
            } else {
                declared.persistent = true;
                declared.persistent_write_allowed |= write_allowed;
            }
        }
        if state.active_root.is_none() {
            state.active_root = Some(root.clone());
            state.active_source = Some(source);
        }
    }
    Some(root)
}

fn set_configured_workspace_index_approval(root: &Path, index_approved: bool) {
    let key = normalize_key(root);
    if let Ok(mut state) = WORKSPACE_STATE.write()
        && let Some(workspace) = state.configured.get_mut(&key)
    {
        workspace.index_approved = index_approved;
    }
}

pub fn configure_and_maybe_index(root: PathBuf, source: impl Into<String>) -> Option<PathBuf> {
    let source = source.into();
    let approved = index_mode() != IndexMode::Off;
    let root = register_configured_workspace(root, source.clone(), approved)?;
    if approved {
        request_index(root.clone(), source, false);
    } else {
        record_rejected_candidate(
            root.clone(),
            source,
            "mode_off",
            "Indexing is disabled by CODELOUPE_MCP_INDEX_MODE=off",
            None,
            None,
        );
    }
    Some(root)
}

pub fn register_client_workspace(root: PathBuf, source: impl Into<String>) -> Option<PathBuf> {
    let source = source.into();
    let declared_root = normalize_declared_root(root)?;
    let write_allowed = crate::common::implicit_write_root_allowed(&declared_root);
    let root = register_workspace(declared_root, source.clone(), false, write_allowed, true)?;
    observe_workspace(root.clone(), source);
    Some(root)
}

pub fn replace_client_roots_list(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let normalized = roots
        .into_iter()
        .filter_map(normalize_declared_root)
        .map(|root| (normalize_key(&root), root))
        .collect::<BTreeMap<_, _>>();
    let new_keys = normalized.keys().cloned().collect::<BTreeSet<_>>();

    if let Ok(mut state) = WORKSPACE_STATE.write() {
        for workspace in state.configured.values_mut() {
            for (key, declared) in &mut workspace.declared_roots {
                if declared.roots_list && !new_keys.contains(key) {
                    declared.roots_list = false;
                    declared.roots_list_write_allowed = false;
                }
            }
            workspace
                .declared_roots
                .retain(|_, declared| declared.persistent || declared.roots_list);
        }
        let removed_workspace_keys = state
            .configured
            .iter()
            .filter(|(_, workspace)| {
                !workspace.index_approved && workspace.declared_roots.is_empty()
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in removed_workspace_keys {
            let removed_root = state
                .configured
                .remove(&key)
                .map(|workspace| workspace.root);
            if removed_root.as_ref().is_some_and(|removed| {
                state
                    .active_root
                    .as_ref()
                    .is_some_and(|active| normalize_key(active) == normalize_key(removed))
            }) {
                state.active_root = None;
                state.active_source = None;
            }
        }
        state.roots_list_keys = new_keys;
    }

    let mut registered = Vec::<PathBuf>::new();
    for root in normalized.into_values() {
        if let Some(root) = register_client_workspace(root, CLIENT_ROOTS_LIST_SOURCE)
            && !registered
                .iter()
                .any(|existing| normalize_key(existing) == normalize_key(&root))
        {
            registered.push(root);
        }
    }
    registered
}

pub fn observe_workspace(root: PathBuf, source: impl Into<String>) -> Option<PathBuf> {
    ensure_decisions_loaded();
    let root = normalize_workspace_root(root)?;
    let source = source.into();
    activate_workspace(root.clone(), source.clone());

    match index_mode() {
        IndexMode::Off => {
            record_rejected_candidate(
                root.clone(),
                source,
                "mode_off",
                "Indexing is disabled by CODELOUPE_MCP_INDEX_MODE=off",
                None,
                None,
            );
        }
        IndexMode::Auto => match decision_for_root(&root) {
            Some(decision) if decision.decision == "too_large" => {
                record_rejected_candidate(
                    root.clone(),
                    source,
                    "too_large",
                    "Previous metadata scan exceeded the configured budget",
                    decision
                        .estimate
                        .as_ref()
                        .map(|estimate| estimate.entries_seen),
                    decision.estimate.as_ref().map(|estimate| estimate.files),
                );
            }
            _ => request_index(root.clone(), source, false),
        },
        IndexMode::Ask => match decision_for_root(&root) {
            Some(decision) if decision.decision == "enabled" => {
                request_index(root.clone(), format!("persisted:{}", decision.by), false)
            }
            Some(decision) if decision.decision == "declined" => {
                record_rejected_candidate(
                    root.clone(),
                    source,
                    "declined",
                    "Indexing was declined for this workspace",
                    decision
                        .estimate
                        .as_ref()
                        .map(|estimate| estimate.entries_seen),
                    decision.estimate.as_ref().map(|estimate| estimate.files),
                );
            }
            Some(decision) if decision.decision == "too_large" => {
                record_rejected_candidate(
                    root.clone(),
                    source,
                    "too_large",
                    "Previous metadata scan exceeded the configured budget",
                    decision
                        .estimate
                        .as_ref()
                        .map(|estimate| estimate.entries_seen),
                    decision.estimate.as_ref().map(|estimate| estimate.files),
                );
            }
            _ => {
                record_candidate(root.clone(), source);
            }
        },
    }
    Some(root)
}

pub fn activate_workspace(root: PathBuf, source: impl Into<String>) -> Option<PathBuf> {
    let root = normalize_workspace_root(root)?;
    if let Ok(mut state) = WORKSPACE_STATE.write() {
        state.active_root = Some(root.clone());
        state.active_source = Some(source.into());
    }
    Some(root)
}

pub fn active_workspace() -> Option<(PathBuf, String)> {
    let state = WORKSPACE_STATE.read().ok()?;
    Some((
        state.active_root.clone()?,
        state
            .active_source
            .clone()
            .unwrap_or_else(|| "workspace_context".to_string()),
    ))
}

pub fn workspace_root_for_path(path: &Path) -> Option<PathBuf> {
    let path = crate::common::canonicalize_if_exists(path.to_path_buf());
    let state = WORKSPACE_STATE.read().ok()?;
    state
        .configured
        .values()
        .map(|workspace| workspace.root.clone())
        .chain(state.active_root.iter().cloned())
        .filter(|root| crate::common::path_is_within(&path, root))
        .max_by_key(|root| root.components().count())
}

pub fn write_workspace_scope_for_path(path: &Path) -> Option<WriteWorkspaceScope> {
    let state = WORKSPACE_STATE.read().ok()?;
    state
        .configured
        .values()
        .filter_map(|workspace| {
            let declared_roots = workspace
                .declared_roots
                .values()
                .filter(|declared| declared.write_allowed())
                .map(|declared| declared.root.clone())
                .collect::<Vec<_>>();
            (!declared_roots.is_empty() && crate::common::path_is_within(path, &workspace.root))
                .then(|| WriteWorkspaceScope {
                    workspace_root: workspace.root.clone(),
                    declared_roots,
                })
        })
        .max_by_key(|scope| scope.workspace_root.components().count())
}

pub fn write_workspace_scope_for_lexical_path(path: &Path) -> Option<WriteWorkspaceScope> {
    let state = WORKSPACE_STATE.read().ok()?;
    state
        .configured
        .values()
        .filter_map(|workspace| {
            let declared_roots = workspace
                .declared_roots
                .values()
                .filter(|declared| declared.write_allowed())
                .map(|declared| declared.root.clone())
                .collect::<Vec<_>>();
            (!declared_roots.is_empty()
                && crate::common::lexical_path_is_within(path, &workspace.root))
            .then(|| WriteWorkspaceScope {
                workspace_root: workspace.root.clone(),
                declared_roots,
            })
        })
        .max_by_key(|scope| scope.workspace_root.components().count())
}

pub fn write_session_snapshot() -> WriteSessionSnapshot {
    WriteSessionSnapshot {
        write_scope: "warning_only",
        elicitation_supported: false,
        approved_directories: Vec::new(),
        declined_directories: Vec::new(),
    }
}

pub fn configured_workspace_roots() -> Vec<PathBuf> {
    WORKSPACE_STATE
        .read()
        .map(|state| {
            state
                .configured
                .values()
                .map(|workspace| workspace.root.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub fn has_declared_workspace_roots() -> bool {
    WORKSPACE_STATE
        .read()
        .map(|state| {
            state
                .configured
                .values()
                .any(|workspace| !workspace.declared_roots.is_empty())
        })
        .unwrap_or(false)
}

pub fn writable_workspace_roots() -> Vec<PathBuf> {
    WORKSPACE_STATE
        .read()
        .map(|state| {
            let mut roots = BTreeMap::new();
            for workspace in state.configured.values() {
                for (key, declared) in &workspace.declared_roots {
                    if declared.write_allowed() {
                        roots
                            .entry(key.clone())
                            .or_insert_with(|| declared.root.clone());
                    }
                }
            }
            roots.into_values().collect()
        })
        .unwrap_or_default()
}

pub fn configured_workspace_snapshots() -> Vec<ConfiguredWorkspaceSnapshot> {
    WORKSPACE_STATE
        .read()
        .map(|state| {
            state
                .configured
                .values()
                .map(|workspace| {
                    let declared_roots = workspace
                        .declared_roots
                        .values()
                        .map(|declared| crate::common::normalize_display_path(&declared.root))
                        .collect::<Vec<_>>();
                    let write_roots = workspace
                        .declared_roots
                        .values()
                        .filter(|declared| declared.write_allowed())
                        .map(|declared| crate::common::normalize_display_path(&declared.root))
                        .collect::<Vec<_>>();
                    ConfiguredWorkspaceSnapshot {
                        workspace_root: crate::common::normalize_display_path(&workspace.root),
                        declared_roots,
                        write_allowed: !write_roots.is_empty(),
                        write_roots,
                        source: workspace.source.clone(),
                        index_approved: workspace.index_approved,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn approved_workspace_roots() -> Vec<(PathBuf, String)> {
    WORKSPACE_STATE
        .read()
        .map(|state| {
            state
                .configured
                .values()
                .filter(|workspace| workspace.index_approved)
                .map(|workspace| (workspace.root.clone(), workspace.source.clone()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn record_candidate(root: PathBuf, source: impl Into<String>) -> Option<PathBuf> {
    ensure_decisions_loaded();
    let root = normalize_workspace_root(root)?;
    let source = source.into();
    let decision = decision_for_root(&root);
    let (status, reason) = match index_mode() {
        IndexMode::Off => (
            "mode_off".to_string(),
            Some("Indexing is disabled by CODELOUPE_MCP_INDEX_MODE=off".to_string()),
        ),
        IndexMode::Ask
            if decision
                .as_ref()
                .is_some_and(|decision| decision.decision == "declined") =>
        {
            (
                "declined".to_string(),
                Some("Indexing was declined for this workspace".to_string()),
            )
        }
        _ if decision
            .as_ref()
            .is_some_and(|decision| decision.decision == "too_large") =>
        {
            (
                "too_large".to_string(),
                Some("Previous metadata scan exceeded the configured budget".to_string()),
            )
        }
        _ => {
            if let Some(reason) = blocked_workspace_reason_with_approval(&root, true) {
                ("blocked_path".to_string(), Some(reason))
            } else if let Some(reason) = approval_required_workspace_reason(&root) {
                ("approval_required".to_string(), Some(reason))
            } else {
                ("candidate".to_string(), None)
            }
        }
    };
    upsert_candidate(root.clone(), source, status, reason, None, None);
    Some(root)
}

pub fn record_rejected_candidate(
    root: PathBuf,
    source: impl Into<String>,
    status: &str,
    reason: impl Into<String>,
    entries_seen: Option<usize>,
    files_seen: Option<usize>,
) -> Option<PathBuf> {
    let root = normalize_workspace_root(root)?;
    let reason = reason.into();
    upsert_candidate(
        root.clone(),
        source.into(),
        status.to_string(),
        Some(reason.clone()),
        entries_seen,
        files_seen,
    );
    if status == "too_large" {
        let estimate = WorkspaceEstimate {
            workspace_root: crate::common::normalize_display_path(&root),
            recommendation: "too_large".to_string(),
            reasons: vec![reason],
            entries_seen: entries_seen.unwrap_or(0),
            files: files_seen.unwrap_or(0),
            total_file_bytes: 0,
            estimated_index_bytes: (entries_seen.unwrap_or(0) as u64).saturating_mul(512),
            content_index: false,
            complete: false,
            elapsed_ms: 0,
            limit_reason: Some("scan_budget".to_string()),
            narrower_candidates: Vec::new(),
        };
        let _ = persist_decision(&root, "too_large", "server", Some(&estimate));
    }
    Some(root)
}

pub fn candidate_snapshots() -> Vec<WorkspaceCandidateSnapshot> {
    WORKSPACE_STATE
        .read()
        .map(|state| {
            state
                .candidates
                .values()
                .map(|candidate| WorkspaceCandidateSnapshot {
                    workspace_root: crate::common::normalize_display_path(&candidate.root),
                    source: candidate.source.clone(),
                    status: candidate.status.clone(),
                    reason: candidate.reason.clone(),
                    entries_seen: candidate.entries_seen,
                    files_seen: candidate.files_seen,
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn start_persisted_and_configured_indexes() {
    ensure_decisions_loaded();
    if index_mode() == IndexMode::Off {
        return;
    }

    for (root, source) in approved_workspace_roots() {
        request_index(root, source, false);
    }

    let persisted = WORKSPACE_STATE
        .read()
        .map(|state| {
            state
                .decisions
                .values()
                .filter(|decision| decision.decision == "enabled")
                .map(|decision| (PathBuf::from(&decision.root), decision.by.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (root, by) in persisted {
        request_index(root, format!("persisted:{by}"), false);
    }
}

pub fn estimate_workspace(path: PathBuf, full_budget: bool) -> Result<WorkspaceEstimate, String> {
    estimate_workspace_with_policy(path, full_budget, false)
}

fn estimate_workspace_with_policy(
    path: PathBuf,
    full_budget: bool,
    explicitly_approved: bool,
) -> Result<WorkspaceEstimate, String> {
    let root = normalize_workspace_root(path).ok_or_else(|| "workspace_not_found".to_string())?;
    let hard_block = blocked_workspace_reason_with_approval(&root, true);
    let approval_required = if explicitly_approved {
        None
    } else {
        approval_required_workspace_reason(&root)
    };
    if let Some(reason) = hard_block.or(approval_required.clone()) {
        let recommendation = if approval_required.is_some() {
            "approval_required"
        } else {
            "blocked"
        };
        return Ok(WorkspaceEstimate {
            workspace_root: crate::common::normalize_display_path(&root),
            recommendation: recommendation.to_string(),
            reasons: vec![format!("{recommendation}_path")],
            entries_seen: 0,
            files: 0,
            total_file_bytes: 0,
            estimated_index_bytes: 0,
            content_index: false,
            complete: false,
            elapsed_ms: 0,
            limit_reason: Some(reason),
            narrower_candidates: Vec::new(),
        });
    }

    let started = Instant::now();
    let max_entries = crate::indexer::index_max_entries();
    let max_duration = if full_budget {
        Duration::from_secs(crate::indexer::index_max_scan_seconds())
    } else {
        Duration::from_secs(2)
    };
    let mut entries_seen = 0usize;
    let mut files = 0usize;
    let mut total_file_bytes = 0u64;
    let mut source_like_files = 0usize;
    let mut child_counts: BTreeMap<PathBuf, (usize, usize)> = BTreeMap::new();
    let mut limit_reason = None;
    let mut walk = WalkBuilder::new(&root);
    walk.hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false);

    for entry in walk.build().flatten() {
        if entry.path() == root {
            continue;
        }
        entries_seen = entries_seen.saturating_add(1);
        if let Ok(relative) = entry.path().strip_prefix(&root)
            && let Some(Component::Normal(child)) = relative.components().next()
        {
            let counts = child_counts.entry(root.join(child)).or_default();
            counts.0 = counts.0.saturating_add(1);
            if entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                counts.1 = counts.1.saturating_add(1);
            }
        }
        if entries_seen > max_entries {
            limit_reason = Some(format!("entry_budget_exceeded:{max_entries}"));
            break;
        }
        if started.elapsed() >= max_duration {
            limit_reason = Some(format!("time_budget_exceeded:{}s", max_duration.as_secs()));
            break;
        }
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            files = files.saturating_add(1);
            if let Ok(metadata) = entry.metadata() {
                total_file_bytes = total_file_bytes.saturating_add(metadata.len());
            }
            if is_source_like(entry.path()) {
                source_like_files = source_like_files.saturating_add(1);
            }
        }
    }

    let complete = limit_reason.is_none();
    let real_workspace = crate::common::looks_like_workspace_root(&root);
    let min_files = index_advice_min_files();
    let content_index =
        files > 0 && source_like_files.saturating_mul(100) >= files.saturating_mul(30);
    let recommendation = if !complete {
        "too_large"
    } else if !real_workspace || files < min_files {
        "not_needed"
    } else {
        "index"
    };
    let mut reasons = Vec::new();
    reasons.push(if real_workspace {
        "workspace_marker".to_string()
    } else {
        "no_workspace_marker".to_string()
    });
    reasons.push(format!("files={files}"));
    if let Some(reason) = &limit_reason {
        reasons.push(reason.clone());
    }
    let narrower_candidates = if complete {
        Vec::new()
    } else {
        let mut candidates = child_counts
            .into_iter()
            .filter(|(path, (entries, _))| path.is_dir() && *entries <= max_entries)
            .map(|(path, (entries, files))| WorkspaceNarrowerCandidate {
                path: crate::common::normalize_display_path(&path),
                entries,
                files,
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            right
                .entries
                .cmp(&left.entries)
                .then_with(|| left.path.cmp(&right.path))
        });
        candidates.truncate(5);
        candidates
    };

    Ok(WorkspaceEstimate {
        workspace_root: crate::common::normalize_display_path(&root),
        recommendation: recommendation.to_string(),
        reasons,
        entries_seen,
        files,
        total_file_bytes,
        estimated_index_bytes: (entries_seen as u64)
            .saturating_mul(512)
            .saturating_add(if content_index { total_file_bytes } else { 0 }),
        content_index,
        complete,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        limit_reason,
        narrower_candidates,
    })
}

pub fn enable_workspace(path: PathBuf) -> Result<WorkspaceEstimate, String> {
    if index_mode() == IndexMode::Off {
        return Err("index_mode_off".to_string());
    }
    let estimate = estimate_workspace_with_policy(path, true, true)?;
    if matches!(estimate.recommendation.as_str(), "blocked" | "too_large") {
        if estimate.recommendation == "too_large" {
            let root = PathBuf::from(&estimate.workspace_root);
            record_rejected_candidate(
                root.clone(),
                "workspace_index:enable",
                "too_large",
                estimate
                    .limit_reason
                    .clone()
                    .unwrap_or_else(|| "scan_budget".to_string()),
                Some(estimate.entries_seen),
                Some(estimate.files),
            );
            let _ = persist_decision(&root, "too_large", "server", Some(&estimate));
        }
        return Err(serde_json::to_string(&estimate).unwrap_or(estimate.recommendation));
    }
    let root = PathBuf::from(&estimate.workspace_root);
    persist_decision(&root, "enabled", "agent", Some(&estimate))?;
    register_index_workspace(root.clone(), "agent", true);
    request_index(root, "agent", false);
    Ok(estimate)
}

pub fn disable_workspace(path: PathBuf) -> Result<PathBuf, String> {
    let root = normalize_workspace_root(path).ok_or_else(|| "workspace_not_found".to_string())?;
    persist_decision(&root, "declined", "agent", None)?;
    set_configured_workspace_index_approval(&root, false);
    record_rejected_candidate(
        root.clone(),
        "agent",
        "declined",
        "Indexing was declined for this workspace",
        None,
        None,
    );
    crate::indexer::remove_workspace_index(&root)?;
    Ok(root)
}

pub fn maybe_attach_index_advice(
    tool_name: &str,
    args: &Value,
    result: &mut Value,
    elapsed_ms: u64,
) {
    if index_mode() != IndexMode::Ask || !is_scan_tool(tool_name) {
        return;
    }
    if std::env::var(INDEX_ADVICE_ENV)
        .ok()
        .is_some_and(|value| value.eq_ignore_ascii_case("timeout_only"))
    {
        return;
    }

    let Some(root) = workspace_from_tool_args(args) else {
        return;
    };
    let source = format!("tool_call:{tool_name}");
    let Some(root) = record_candidate(root, source.clone()) else {
        return;
    };
    let progress_files_seen = args
        .get(crate::cancellation::ARG_KEY)
        .and_then(Value::as_str)
        .map(crate::cancellation::progress)
        .map(|progress| progress.entries_processed)
        .unwrap_or(0);
    let files_seen = result_scan_count(result).max(progress_files_seen);
    let refused = result
        .get("search_strategy")
        .and_then(Value::as_str)
        .is_some_and(|strategy| strategy == "refused_large_scope");
    let advice = update_scan_and_build_advice(&root, &source, files_seen, elapsed_ms, refused);
    if let Some(advice) = advice
        && let Some(object) = result.as_object_mut()
    {
        object.insert("index_advice".to_string(), advice);
    }
}

pub fn index_advice_for_timeout(
    tool_name: &str,
    args: &Value,
    files_seen: usize,
    elapsed_ms: u64,
    progress_workspace: Option<&str>,
) -> Option<Value> {
    if index_mode() != IndexMode::Ask || !is_scan_tool(tool_name) {
        return None;
    }
    let root = progress_workspace
        .map(PathBuf::from)
        .or_else(|| workspace_from_tool_args(args))?;
    let root = record_candidate(root, format!("tool_call:{tool_name}"))?;
    update_scan_and_build_advice(
        &root,
        &format!("tool_call:{tool_name}"),
        files_seen,
        elapsed_ms,
        true,
    )
}

pub fn blocked_workspace_reason(path: &Path) -> Option<String> {
    blocked_workspace_reason_with_approval(path, false)
}

pub fn workspace_requires_explicit_approval(path: &Path) -> bool {
    approval_required_workspace_reason(path).is_some()
}

fn blocked_workspace_reason_with_approval(
    path: &Path,
    explicitly_approved: bool,
) -> Option<String> {
    let path = crate::common::canonicalize_if_exists(path.to_path_buf());
    if path.parent().is_none() {
        return Some("Filesystem roots cannot be indexed".to_string());
    }

    let temp_root = crate::common::canonicalize_if_exists(std::env::temp_dir());
    let mut exact_blocked = vec![temp_root.clone()];
    for variable in ["USERPROFILE", "HOME"] {
        if let Some(home) = std::env::var_os(variable).map(PathBuf::from) {
            let home = crate::common::canonicalize_if_exists(home);
            exact_blocked.push(home.clone());
            if let Some(parent) = home.parent() {
                exact_blocked.push(crate::common::canonicalize_if_exists(parent.to_path_buf()));
            }
        }
    }
    if exact_blocked
        .iter()
        .any(|blocked| normalize_key(blocked) == normalize_key(&path))
    {
        return Some("Path is a protected root, home, home parent, or temp directory".to_string());
    }

    let mut blocked_trees = Vec::new();
    #[cfg(windows)]
    {
        for variable in ["WINDIR", "ProgramFiles", "ProgramFiles(x86)", "ProgramData"] {
            if let Some(value) = std::env::var_os(variable) {
                blocked_trees.push(crate::common::canonicalize_if_exists(PathBuf::from(value)));
            }
        }
    }
    #[cfg(not(windows))]
    {
        blocked_trees.extend(
            ["/usr", "/etc", "/var", "/System", "/Library"]
                .into_iter()
                .map(PathBuf::from),
        );
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            blocked_trees.push(home.join("Library"));
        }
    }
    #[cfg(target_os = "macos")]
    let is_macos_temp_descendant = {
        let temp_container = PathBuf::from("/var/folders");
        normalize_key(&path) != normalize_key(&temp_root)
            && crate::common::path_is_within(&path, &temp_root)
            && crate::common::path_is_within(&temp_root, &temp_container)
    };
    if blocked_trees.iter().any(|blocked| {
        if !crate::common::path_is_within(&path, blocked) {
            return false;
        }

        #[cfg(target_os = "macos")]
        if is_macos_temp_descendant {
            let blocked = crate::common::canonicalize_if_exists(blocked.clone());
            let var_root = crate::common::canonicalize_if_exists(PathBuf::from("/var"));
            if normalize_key(&blocked) == normalize_key(&var_root) {
                return false;
            }
        }

        true
    }) {
        return Some("Path is inside a protected operating-system tree".to_string());
    }
    if !explicitly_approved {
        return approval_required_workspace_reason(&path);
    }

    None
}

fn approval_required_workspace_reason(path: &Path) -> Option<String> {
    let path = crate::common::canonicalize_if_exists(path.to_path_buf());
    for variable in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(value) = std::env::var_os(variable) {
            let root = crate::common::canonicalize_if_exists(PathBuf::from(value));
            if crate::common::path_is_within(&path, &root) {
                return Some(
                    "Path is inside an application-data tree; enable only when the user explicitly requested work at this exact path"
                        .to_string(),
                );
            }
        }
    }
    None
}

fn request_index(root: PathBuf, source: impl Into<String>, persist_config: bool) {
    let source = source.into();
    let explicitly_approved = is_workspace_explicitly_approved(&root);
    if let Some(reason) = blocked_workspace_reason_with_approval(&root, explicitly_approved) {
        let status = if workspace_requires_explicit_approval(&root) && !explicitly_approved {
            "approval_required"
        } else {
            "blocked_path"
        };
        record_rejected_candidate(root, source, status, reason, None, None);
        return;
    }
    if persist_config {
        let _ = persist_decision(&root, "enabled", "config", None);
    }
    upsert_candidate(
        root.clone(),
        source.clone(),
        "indexing".to_string(),
        None,
        None,
        None,
    );
    crate::indexer::schedule_workspace_index(root, source);
}

fn is_workspace_explicitly_approved(root: &Path) -> bool {
    if decision_for_root(root).is_some_and(|decision| decision.decision == "enabled") {
        return true;
    }
    WORKSPACE_STATE
        .read()
        .ok()
        .and_then(|state| state.configured.get(&normalize_key(root)).cloned())
        .is_some_and(|workspace| workspace.index_approved)
}

fn update_scan_and_build_advice(
    root: &Path,
    source: &str,
    files_seen: usize,
    elapsed_ms: u64,
    high_value_moment: bool,
) -> Option<Value> {
    if blocked_workspace_reason_with_approval(root, true).is_some()
        || !crate::common::looks_like_workspace_root(root)
    {
        return None;
    }
    if decision_for_root(root).is_some() {
        return None;
    }

    let key = normalize_key(root);
    let mut state = WORKSPACE_STATE.write().ok()?;
    let candidate = state
        .candidates
        .entry(key)
        .or_insert_with(|| WorkspaceCandidate {
            root: root.to_path_buf(),
            source: source.to_string(),
            status: "candidate".to_string(),
            reason: None,
            entries_seen: None,
            files_seen: None,
            scan_count: 0,
            advice_emitted: false,
        });
    candidate.scan_count = candidate.scan_count.saturating_add(1);
    candidate.files_seen = Some(candidate.files_seen.unwrap_or(0).max(files_seen));
    if candidate.advice_emitted {
        return None;
    }
    let observed_files = candidate.files_seen.unwrap_or(0);
    if observed_files < index_advice_min_files() {
        return None;
    }
    let early_signal = elapsed_ms >= 5_000
        || observed_files >= index_advice_min_files()
        || candidate.scan_count >= 3;
    if !high_value_moment && !early_signal {
        return None;
    }
    candidate.advice_emitted = true;
    let too_large = observed_files > crate::indexer::index_max_entries();
    if too_large {
        candidate.status = "too_large".to_string();
        candidate.reason = Some("Observed scan exceeded the configured entry budget".to_string());
    }
    let approval_required = workspace_requires_explicit_approval(root);
    Some(json!({
        "workspace_root": crate::common::normalize_display_path(root),
        "recommendation": if too_large {
            "too_large"
        } else if approval_required {
            "approval_required"
        } else {
            "index"
        },
        "reasons": [format!("files>={observed_files}"), "workspace_marker", format!("search_ms:{elapsed_ms}")],
        "estimate": {
            "files": observed_files,
            "index_bytes": (observed_files as u64).saturating_mul(512),
            "content_index": true
        },
        "next": if too_large {
            "Ask the user to raise the server budget or choose a narrower candidate.".to_string()
        } else if approval_required {
            format!(
                "Only if the user explicitly requested work at this exact path, call workspace_index(action=\"enable\", path=\"{}\").",
                crate::common::normalize_display_path(root)
            )
        } else {
            format!(
                "workspace_index(action=\"enable\", path=\"{}\")",
                crate::common::normalize_display_path(root)
            )
        }
    }))
}

fn upsert_candidate(
    root: PathBuf,
    source: String,
    status: String,
    reason: Option<String>,
    entries_seen: Option<usize>,
    files_seen: Option<usize>,
) {
    let key = normalize_key(&root);
    if let Ok(mut state) = WORKSPACE_STATE.write() {
        state
            .candidates
            .entry(key)
            .and_modify(|candidate| {
                candidate.source = source.clone();
                candidate.status = status.clone();
                candidate.reason = reason.clone();
                candidate.entries_seen = entries_seen.or(candidate.entries_seen);
                candidate.files_seen = files_seen.or(candidate.files_seen);
            })
            .or_insert(WorkspaceCandidate {
                root,
                source,
                status,
                reason,
                entries_seen,
                files_seen,
                scan_count: 0,
                advice_emitted: false,
            });
    }
}

fn workspace_from_tool_args(args: &Value) -> Option<PathBuf> {
    let mut paths = Vec::new();
    collect_path_values(args, None, &mut paths);
    paths
        .into_iter()
        .filter_map(|raw| {
            if raw.is_empty() || crate::common::contains_path_glob(&raw) {
                return None;
            }
            let resolved = crate::common::resolve_tool_path(&raw);
            crate::common::discover_workspace_root(&resolved)
                .or_else(|| workspace_root_for_path(&resolved))
        })
        .next()
        .or_else(|| active_workspace().map(|(root, _)| root))
}

fn collect_path_values(value: &Value, parent_key: Option<&str>, paths: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                collect_path_values(child, Some(key), paths);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_path_values(child, parent_key, paths);
            }
        }
        Value::String(raw) if parent_key.is_some_and(is_workspace_path_key) => {
            paths.push(raw.clone());
        }
        _ => {}
    }
}

fn is_workspace_path_key(key: &str) -> bool {
    matches!(
        key,
        "path"
            | "paths"
            | "file_path"
            | "repo_path"
            | "archive_path"
            | "input_file"
            | "output_file"
            | "file_hint"
            | "left_path"
            | "right_path"
    )
}

fn result_scan_count(result: &Value) -> usize {
    fn count_from_object(value: &Value) -> usize {
        [
            "files_considered",
            "files_searched",
            "files_scanned",
            "files_walked",
            "entries_scanned",
            "total_files",
        ]
        .into_iter()
        .filter_map(|key| value.get(key).and_then(Value::as_u64))
        .max()
        .unwrap_or(0) as usize
    }

    count_from_object(result).max(
        result
            .get("diagnostics")
            .map(count_from_object)
            .unwrap_or(0),
    )
}

fn is_scan_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "text_search"
            | "fuzzy_find"
            | "project_map"
            | "workspace_stats"
            | "find_definition"
            | "find_references"
    )
}

fn index_advice_min_files() -> usize {
    std::env::var(INDEX_ADVICE_MIN_FILES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_INDEX_ADVICE_MIN_FILES)
}

fn is_source_like(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "c" | "cc"
                    | "cpp"
                    | "cs"
                    | "css"
                    | "go"
                    | "h"
                    | "hpp"
                    | "html"
                    | "java"
                    | "js"
                    | "json"
                    | "jsx"
                    | "kt"
                    | "md"
                    | "php"
                    | "py"
                    | "rb"
                    | "rs"
                    | "sh"
                    | "sql"
                    | "swift"
                    | "toml"
                    | "ts"
                    | "tsx"
                    | "xml"
                    | "yaml"
                    | "yml"
            )
        })
}

fn persist_decision(
    root: &Path,
    decision: &str,
    by: &str,
    estimate: Option<&WorkspaceEstimate>,
) -> Result<(), String> {
    ensure_decisions_loaded();
    let key = normalize_key(root);
    let value = WorkspaceDecision {
        root: crate::common::normalize_display_path(root),
        decision: decision.to_string(),
        by: by.to_string(),
        decided_at: crate::common::unix_timestamp_secs(),
        estimate: estimate.map(|estimate| PersistedEstimate {
            entries_seen: estimate.entries_seen,
            files: estimate.files,
            estimated_index_bytes: estimate.estimated_index_bytes,
        }),
    };
    if let Ok(mut state) = WORKSPACE_STATE.write() {
        state.decisions.insert(key, value);
    }
    save_decisions()
}

fn decision_for_root(root: &Path) -> Option<WorkspaceDecision> {
    ensure_decisions_loaded();
    let decision = WORKSPACE_STATE
        .read()
        .ok()?
        .decisions
        .get(&normalize_key(root))
        .cloned()?;
    if decision.decision == "declined"
        && crate::common::unix_timestamp_secs().saturating_sub(decision.decided_at)
            > DECLINE_TTL_SECS
    {
        return None;
    }
    Some(decision)
}

fn ensure_decisions_loaded() {
    if WORKSPACE_STATE
        .read()
        .ok()
        .is_some_and(|state| state.decisions_loaded)
    {
        return;
    }
    let file = fs::read(crate::indexer::workspace_decisions_path())
        .ok()
        .and_then(|payload| serde_json::from_slice::<WorkspaceDecisionFile>(&payload).ok())
        .unwrap_or_default();
    if let Ok(mut state) = WORKSPACE_STATE.write() {
        if state.decisions_loaded {
            return;
        }
        for decision in file.workspaces {
            state
                .decisions
                .insert(normalize_key(Path::new(&decision.root)), decision);
        }
        state.decisions_loaded = true;
    }
}

fn save_decisions() -> Result<(), String> {
    let path = crate::indexer::workspace_decisions_path();
    let workspaces = WORKSPACE_STATE
        .read()
        .map_err(|_| "workspace_state_poisoned".to_string())?
        .decisions
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let payload = serde_json::to_vec_pretty(&WorkspaceDecisionFile {
        version: DECISIONS_VERSION,
        workspaces,
    })
    .map_err(|error| format!("workspace_decisions_serialize_failed: {error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("workspace_decisions_dir_failed: {error}"))?;
    }
    fs::write(&path, payload).map_err(|error| {
        format!(
            "workspace_decisions_write_failed:{}:{error}",
            path.display()
        )
    })
}

fn normalize_workspace_root(path: PathBuf) -> Option<PathBuf> {
    let path = normalize_declared_root(path)?;
    Some(crate::common::discover_workspace_root(&path).unwrap_or(path))
}

fn normalize_declared_root(path: PathBuf) -> Option<PathBuf> {
    let path = crate::common::canonicalize_if_exists(path);
    if !path.exists() || !path.is_dir() {
        return None;
    }
    Some(path)
}

fn normalize_key(path: &Path) -> String {
    let normalized = crate::common::normalize_display_path(path);
    #[cfg(windows)]
    {
        normalized.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_scan_count_reads_compact_and_diagnostic_counters() {
        assert_eq!(
            result_scan_count(&json!({
                "files_walked": 12,
                "diagnostics": {"entries_scanned": 21_000}
            })),
            21_000
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_temp_workspace_descendant_is_not_blocked_as_system_tree() {
        let temp_root = crate::common::canonicalize_if_exists(std::env::temp_dir());
        let workspace = tempfile::tempdir_in(&temp_root).expect("create temp workspace");

        assert_eq!(
            blocked_workspace_reason_with_approval(workspace.path(), true),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_temp_root_remains_blocked() {
        assert_eq!(
            blocked_workspace_reason_with_approval(&std::env::temp_dir(), true).as_deref(),
            Some("Path is a protected root, home, home parent, or temp directory")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_protected_var_subtree_remains_blocked() {
        assert_eq!(
            blocked_workspace_reason_with_approval(Path::new("/private/var/db"), true).as_deref(),
            Some("Path is inside a protected operating-system tree")
        );
    }
}

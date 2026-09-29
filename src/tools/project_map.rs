use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tokio::task;

use super::path_filters::{
    Pattern, apply_walk_overrides, compile_patterns, configure_walk_filters, is_vcs_metadata_dir,
    matches_patterns, parse_pattern_strings, passes_patterns,
};
use crate::indexer::{is_path_index_available, visit_indexed_entries_under};

const DEFAULT_MAX_CHILDREN_PER_DIR: usize = 250;
const MAX_CHILDREN_PER_DIR_LIMIT: usize = 5_000;
const MAX_TOTAL_ENTRIES: usize = 20_000;

#[derive(Default, Serialize)]
struct ProjectMapChildren {
    dirs: Vec<ProjectMapEntry>,
    files: Vec<ProjectMapEntry>,
}

#[derive(Serialize)]
struct ProjectMapEntry {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

impl ProjectMapChildren {
    fn push(&mut self, name: String, is_dir: bool, size_bytes: Option<u64>) {
        let entry = ProjectMapEntry { name, size_bytes };
        if is_dir {
            self.dirs.push(entry);
        } else {
            self.files.push(entry);
        }
    }
}
const DIAGNOSTIC_FIELDS: &[&str] = &[
    "max_depth",
    "entries_seen",
    "entries_skipped_by_patterns",
    "max_children_per_dir",
    "max_total_entries",
    "truncated_directory_count",
    "truncated_directories",
    "metadata_index_used",
    "include_ignored",
    "include_hidden",
    "indexed_entries_available",
    "warnings",
    "note",
    "indexed_at",
    "no_fallback_reason",
];

pub fn schema() -> Value {
    json!({
        "name": "project_map",
        "title": "Map project tree",
        "description": "Build a bounded tree view of a directory with optional size metadata. Use first for repository orientation and to choose precise text_search scopes; keep depth/children low for large repos.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to map. Prefer a subsystem path over workspace root in large repos." },
                "max_depth": { "type": "integer", "description": "Maximum tree depth; use 1-3 for reconnaissance." },
                "show_sizes": { "type": "boolean", "description": "Include file sizes when useful; omit for compact structure scans." },
                "max_children_per_dir": { "type": "integer", "description": "Per-directory child cap; lower this for very large directories." },
                "include_ignored": { "type": "boolean", "description": "Include files ignored by .gitignore, .git/info/exclude, global gitignore, or .ignore files." },
                "include_hidden": { "type": "boolean", "description": "Include hidden files and directories except VCS metadata directories such as .git, unless scoped directly." },
                "includes": { "type": "array", "items": { "type": "string" }, "description": "Optional glob include filters." },
                "excludes": { "type": "array", "items": { "type": "string" }, "description": "Optional glob exclude filters for generated/build/vendor areas." },
                "verbose": { "type": "boolean", "description": "Include detailed traversal and index diagnostics even when the map is complete. Defaults to false." }
            },
            "required": ["path"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("project_map background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
        .context("Missing path")?;
    let path = crate::common::resolve_tool_path(path_str);

    if !path.exists() || !path.is_dir() {
        return Err(anyhow::anyhow!(
            "Path is not a valid directory: '{}' (resolved to {})",
            path_str,
            crate::common::normalize_display_path(&path)
        ));
    }

    let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
    let show_sizes = args
        .get("show_sizes")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_children_per_dir = parse_usize_arg(
        &args,
        "max_children_per_dir",
        DEFAULT_MAX_CHILDREN_PER_DIR,
        1,
        MAX_CHILDREN_PER_DIR_LIMIT,
    );
    let include_globs = parse_pattern_strings(args.get("includes"));
    let exclude_globs = parse_pattern_strings(args.get("excludes"));
    let include_ignored = args
        .get("include_ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_hidden = args
        .get("include_hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let verbose = args
        .get("verbose")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let includes = compile_patterns(&include_globs)?;
    let excludes = compile_patterns(&exclude_globs)?;
    let canonical_path = path.canonicalize().unwrap_or_else(|_| path.clone());
    let cancellation =
        crate::cancellation::token_for_scan(&args, std::slice::from_ref(&canonical_path));

    if !include_ignored
        && !include_hidden
        && max_depth > 3
        && is_path_index_available(&canonical_path)
    {
        let indexed_map = build_project_map_from_index(
            &path,
            &canonical_path,
            max_depth,
            show_sizes,
            max_children_per_dir,
            &includes,
            &excludes,
            cancellation.as_ref(),
        );
        if indexed_map
            .get("indexed_entries_available")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
        {
            let response =
                attach_index_diagnostics(indexed_map, std::slice::from_ref(&canonical_path), true);
            return Ok(finalize_project_map_response(response, verbose));
        }
    }

    let mut walker = WalkBuilder::new(&path);
    configure_walk_filters(&mut walker, include_ignored, include_hidden);
    walker.max_depth(Some(max_depth));
    apply_walk_overrides(&mut walker, &canonical_path, &include_globs, &exclude_globs)?;
    let filter_root = canonical_path.clone();
    let filter_excludes = excludes.clone();
    walker.filter_entry(move |entry| {
        if entry.path() == filter_root {
            return true;
        }
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir())
            && is_vcs_metadata_dir(entry.path(), &filter_root)
        {
            return false;
        }
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir())
        {
            return true;
        }
        if filter_excludes.is_empty() {
            return true;
        }
        let relative_path = relative_path(entry.path(), &filter_root);
        !matches_patterns(entry.path(), &relative_path, &filter_excludes)
    });

    let mut dir_map: HashMap<String, ProjectMapChildren> = HashMap::new();
    let mut children_seen: HashMap<String, usize> = HashMap::new();
    let mut truncated_dirs = HashSet::new();
    let mut entries_seen = 0usize;
    let mut entries_returned = 0usize;
    let mut entries_skipped_by_patterns = 0usize;
    let mut total_entries_truncated = false;
    let mut scanned_entries = 0usize;
    let has_patterns = !includes.is_empty() || !excludes.is_empty();

    for entry in walker.build().flatten() {
        if entry.path() == path {
            continue;
        }
        scanned_entries = scanned_entries.saturating_add(1);
        if crate::cancellation::report_scan_progress(cancellation.as_ref(), scanned_entries) {
            break;
        }

        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        if has_patterns
            && !passes_patterns(
                entry.path(),
                &relative_path(entry.path(), &canonical_path),
                &includes,
                &excludes,
            )
        {
            entries_skipped_by_patterns += 1;
            continue;
        }

        entries_seen += 1;
        if entries_returned >= MAX_TOTAL_ENTRIES {
            total_entries_truncated = true;
            break;
        }
        let parent_path = entry
            .path()
            .parent()
            .map(|parent| tree_directory_key(parent, &canonical_path))
            .unwrap_or_else(|| ".".to_string());

        let child_count = children_seen.entry(parent_path.clone()).or_insert(0);
        *child_count += 1;
        if *child_count > max_children_per_dir {
            truncated_dirs.insert(parent_path);
            continue;
        }

        let size_bytes = if show_sizes && !is_dir {
            Some(entry.metadata().map(|m| m.len()).unwrap_or(0))
        } else {
            None
        };

        dir_map.entry(parent_path).or_default().push(
            entry.file_name().to_string_lossy().to_string(),
            is_dir,
            size_bytes,
        );
        entries_returned += 1;
    }
    crate::cancellation::finish_scan_progress(cancellation.as_ref(), scanned_entries);

    let mut truncated_dirs = truncated_dirs.into_iter().collect::<Vec<_>>();
    let truncated_directory_count = truncated_dirs.len();
    truncated_dirs.sort();
    truncated_dirs.truncate(50);

    let complete = truncated_directory_count == 0
        && !total_entries_truncated
        && !cancellation
            .as_ref()
            .is_some_and(crate::cancellation::CancellationToken::is_cancelled);
    let response = json!({
        "root": normalize_path(&path),
        "canonical_path": normalize_path(&canonical_path),
        "max_depth": max_depth,
        "tree_representation": dir_map,
        "complete": complete,
        "entries_seen": entries_seen,
        "entries_returned": entries_returned,
        "entries_skipped_by_patterns": entries_skipped_by_patterns,
        "max_children_per_dir": max_children_per_dir,
        "limit_reached": truncated_directory_count > 0 || total_entries_truncated,
        "limit_reason": if total_entries_truncated { Some("max_total_entries") } else if truncated_directory_count > 0 { Some("max_children_per_dir") } else { None },
        "max_total_entries": MAX_TOTAL_ENTRIES,
        "truncated_directory_count": truncated_directory_count,
        "truncated_directories": truncated_dirs,
        "search_strategy": "filesystem_walk",
        "metadata_index_used": false,
        "include_ignored": include_ignored,
        "include_hidden": include_hidden,
        "note": if include_ignored || include_hidden {
            "Inclusive filesystem walk enabled; hidden VCS metadata directories remain excluded unless scoped directly."
        } else {
            "Skipped hidden and ignored paths to preserve token context."
        }
    });
    Ok(finalize_project_map_response(
        attach_index_diagnostics(response, std::slice::from_ref(&canonical_path), false),
        verbose,
    ))
}

#[allow(clippy::too_many_arguments)]
fn build_project_map_from_index(
    path: &Path,
    canonical_path: &Path,
    max_depth: usize,
    show_sizes: bool,
    max_children_per_dir: usize,
    includes: &[Pattern],
    excludes: &[Pattern],
    cancellation: Option<&crate::cancellation::CancellationToken>,
) -> Value {
    let mut indexed_entry_count = 0usize;
    let mut warnings = Vec::new();

    let mut dir_map: HashMap<String, ProjectMapChildren> = HashMap::new();
    let mut children_seen: HashMap<String, usize> = HashMap::new();
    let mut truncated_dirs = HashSet::new();
    let mut entries_seen = 0usize;
    let mut entries_returned = 0usize;
    let mut entries_skipped_by_patterns = 0usize;
    let mut total_entries_truncated = false;

    visit_indexed_entries_under(canonical_path, |entry| {
        indexed_entry_count += 1;
        if crate::cancellation::report_scan_progress(cancellation, indexed_entry_count) {
            return false;
        }
        if entry.path == canonical_path {
            return true;
        }

        let rel = relative_path(&entry.path, canonical_path);
        if rel.is_empty() || relative_depth(&rel) > max_depth {
            return true;
        }

        if !passes_patterns(&entry.path, &rel, includes, excludes) {
            entries_skipped_by_patterns += 1;
            return true;
        }

        entries_seen += 1;
        if entries_returned >= MAX_TOTAL_ENTRIES {
            total_entries_truncated = true;
            return false;
        }
        let parent_path = entry
            .path
            .parent()
            .map(|parent| tree_directory_key(parent, canonical_path))
            .unwrap_or_else(|| ".".to_string());

        let child_count = children_seen.entry(parent_path.clone()).or_insert(0);
        *child_count += 1;
        if *child_count > max_children_per_dir {
            truncated_dirs.insert(parent_path);
            return true;
        }

        let size_bytes = if show_sizes && !entry.is_dir {
            Some(entry.size)
        } else {
            None
        };

        dir_map
            .entry(parent_path)
            .or_default()
            .push(entry.file_name, entry.is_dir, size_bytes);
        entries_returned += 1;
        true
    });
    crate::cancellation::finish_scan_progress(cancellation, indexed_entry_count);

    if indexed_entry_count >= 200_000 && (max_depth > 2 || max_children_per_dir > 100) {
        warnings.push(
            "Large indexed tree detected; prefer a module path plus max_depth <= 2 and max_children_per_dir <= 100 for Chromium-sized workspaces."
                .to_string(),
        );
    }

    let mut truncated_dirs = truncated_dirs.into_iter().collect::<Vec<_>>();
    let truncated_directory_count = truncated_dirs.len();
    truncated_dirs.sort();
    truncated_dirs.truncate(50);

    let complete = truncated_directory_count == 0
        && !total_entries_truncated
        && !cancellation.is_some_and(crate::cancellation::CancellationToken::is_cancelled);
    json!({
        "root": normalize_path(path),
        "canonical_path": normalize_path(canonical_path),
        "max_depth": max_depth,
        "tree_representation": dir_map,
        "complete": complete,
        "entries_seen": entries_seen,
        "entries_returned": entries_returned,
        "entries_skipped_by_patterns": entries_skipped_by_patterns,
        "max_children_per_dir": max_children_per_dir,
        "limit_reached": truncated_directory_count > 0 || total_entries_truncated,
        "limit_reason": if total_entries_truncated { Some("max_total_entries") } else if truncated_directory_count > 0 { Some("max_children_per_dir") } else { None },
        "max_total_entries": MAX_TOTAL_ENTRIES,
        "truncated_directory_count": truncated_directory_count,
        "truncated_directories": truncated_dirs,
        "search_strategy": "lmdb_metadata",
        "metadata_index_used": true,
        "include_ignored": false,
        "include_hidden": false,
        "indexed_entries_available": indexed_entry_count,
        "warnings": warnings,
        "note": "Read from LMDB metadata index; hidden/git ignore rules were applied during index refresh."
    })
}

fn attach_index_diagnostics(
    mut response: Value,
    paths: &[std::path::PathBuf],
    index_used: bool,
) -> Value {
    let indexed_at = crate::indexer::path_index_ages_for_paths(paths);
    let index_complete = !indexed_at.is_empty() && indexed_at.iter().all(|item| item.complete);
    let index_age_secs = indexed_at
        .iter()
        .filter_map(|item| item.index_age_secs)
        .max();
    crate::common::insert_object_field(&mut response, "index_used", json!(index_used));
    crate::common::insert_object_field(&mut response, "index_complete", json!(index_complete));
    crate::common::insert_object_field(&mut response, "indexed_at", json!(indexed_at));
    crate::common::insert_object_field(&mut response, "index_age_secs", json!(index_age_secs));
    crate::common::insert_object_field(
        &mut response,
        "no_fallback_reason",
        json!(if index_used && index_complete {
            Some("index_complete")
        } else {
            None
        }),
    );
    response
}

fn finalize_project_map_response(response: Value, verbose: bool) -> Value {
    let complete = response
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_warnings = response
        .get("warnings")
        .and_then(Value::as_array)
        .is_some_and(|warnings| !warnings.is_empty());
    super::nest_diagnostics(
        response,
        DIAGNOSTIC_FIELDS,
        verbose || !complete || has_warnings,
    )
}

fn tree_directory_key(path: &Path, root: &Path) -> String {
    crate::common::relative_display_path(path, root)
        .filter(|relative| !relative.is_empty())
        .unwrap_or_else(|| ".".to_string())
}

fn parse_usize_arg(args: &Value, name: &str, default: usize, min: usize, max: usize) -> usize {
    args.get(name)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .map(normalize_path)
        .filter(|relative| !relative.is_empty())
        .unwrap_or_else(|| normalize_path(path))
}

fn relative_depth(relative_path: &str) -> usize {
    relative_path
        .split('/')
        .filter(|part| !part.is_empty())
        .count()
}

fn normalize_path(path: &Path) -> String {
    crate::common::normalize_display_path(path)
}

use anyhow::{Context, Result};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ignore::WalkBuilder;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use tokio::task;

use super::path_filters::{
    Pattern, apply_walk_overrides, compile_pattern, configure_walk_filters,
    filtered_scope_warnings, is_vcs_metadata_dir,
};

const MAX_FUZZY_RESULTS: usize = 500;

pub fn schema() -> Value {
    json!({
        "name": "fuzzy_find",
        "title": "Fuzzy find paths",
        "description": "Perform fast fuzzy path and file-name search using the metadata/path index when available. Use before text_search to discover precise directories/files in large repositories; prefer concrete basename/path tokens and scoped paths over broad prose.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Fuzzy path/name pattern. For best results in large repos, use distinctive path tokens such as browser net url_loader rather than a prose query." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Search roots or files. Defaults to the active workspace; scope this when you already know a subsystem." },
                "target_type": { "type": "string", "enum": ["file", "dir", "any"], "description": "Limit matches to files, directories, or both. Use dir to find a good text_search scope." },
                "extensions": { "type": "array", "items": { "type": "string" }, "description": "Optional file extensions without dots, e.g. rs or cc." },
                "max_depth": { "type": "integer", "description": "Optional traversal depth for filesystem fallback." },
                "include_ignored": { "type": "boolean", "description": "Include files ignored by .gitignore, .git/info/exclude, global gitignore, or .ignore files." },
                "include_hidden": { "type": "boolean", "description": "Include hidden files and directories except VCS metadata directories such as .git, unless scoped directly." },
                "max_results": { "type": "integer", "description": "Maximum ranked matches to return; defaults are capped to keep responses usable." },
                "verbose": { "type": "boolean", "description": "Include detailed scan and index diagnostics. Incomplete results include diagnostics automatically. Defaults to false." }
            },
            "required": ["pattern"]
        }
    })
}

#[derive(Clone, Debug)]
struct RankedMatch {
    path: String,
    relative_path: String,
    score: i64,
    entry_type: &'static str,
    size: u64,
    modified_at: u64,
}

#[derive(Default, Debug)]
struct FuzzyStats {
    entries_scanned: usize,
    indexed_candidates_considered: usize,
    indexed_candidates_accepted: usize,
    indexed_roots_used: usize,
    partial_index_roots_used: usize,
    filesystem_roots_walked: usize,
    broad_query_roots_skipped: usize,
    warnings: Vec<String>,
}

struct FuzzyIndexMetadata {
    index_used: bool,
    index_complete: bool,
    indexed_at: Vec<crate::indexer::PathIndexAge>,
    index_age_secs: Option<u64>,
    fallback_reason: Vec<&'static str>,
    no_fallback_reason: Option<&'static str>,
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("fuzzy_find background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let pattern = args
        .get("pattern")
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
        .context("Missing/empty pattern")?;
    let paths: Vec<PathBuf> = if let Some(paths) = args.get("paths").and_then(|v| v.as_array()) {
        paths
            .iter()
            .filter_map(|path| path.as_str())
            .map(crate::common::resolve_existing_tool_path)
            .collect::<Result<Vec<_>>>()?
    } else {
        vec![crate::common::default_tool_root()]
    };

    let target_type = args
        .get("target_type")
        .and_then(|v| v.as_str())
        .unwrap_or("any");
    let verbose = args
        .get("verbose")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_results = parse_usize_arg(&args, "max_results", 50, 0, MAX_FUZZY_RESULTS);
    let max_depth = args.get("max_depth").and_then(|v| v.as_u64());
    let include_ignored = args
        .get("include_ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_hidden = args
        .get("include_hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let suppress_filter_hints = args
        .get("_suppress_filter_hints")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let extensions: Vec<String> = args
        .get("extensions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    p.as_str().and_then(|s| {
                        let extension = s.trim().trim_start_matches('.').to_ascii_lowercase();
                        (!extension.is_empty()).then_some(extension)
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let search_roots: Vec<PathBuf> = paths
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect();
    let result_root = crate::common::common_path_root(&search_roots);
    let display_roots = result_root
        .as_ref()
        .map(|root| vec![root.clone()])
        .unwrap_or_else(|| search_roots.clone());
    if max_results == 0 {
        return Ok(empty_response(
            pattern,
            "none",
            None,
            result_root.as_deref(),
            &search_roots,
            verbose,
        ));
    }
    let cancellation = crate::cancellation::token_for_scan(&args, &search_roots);
    if let Some(glob_pattern) = compile_glob_pattern(pattern)? {
        return execute_glob_find(
            pattern,
            &glob_pattern,
            &search_roots,
            &display_roots,
            target_type,
            &extensions,
            max_depth,
            max_results,
            include_ignored,
            include_hidden,
            suppress_filter_hints,
            cancellation.as_ref(),
            verbose,
        );
    }

    let matcher = SkimMatcherV2::default();
    let filter_terms = pattern_filter_terms(pattern);
    let mut ranked = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut stats = FuzzyStats::default();

    for root in &search_roots {
        process_search_root(
            root,
            &display_roots,
            pattern,
            target_type,
            &extensions,
            &filter_terms,
            max_depth,
            max_results,
            &matcher,
            &mut ranked,
            &mut seen_paths,
            &mut stats,
            include_ignored,
            include_hidden,
            cancellation.as_ref(),
        )?;
    }
    crate::cancellation::finish_scan_progress(cancellation.as_ref(), stats.entries_scanned);

    ranked.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
            .then_with(|| left.path.cmp(&right.path))
    });

    let results: Vec<Value> = ranked
        .into_iter()
        .take(max_results)
        .map(|item| {
            json!({
                "path": item.relative_path,
                "score": item.score,
                "type": item.entry_type,
                "size_bytes": item.size,
                "modified_at": item.modified_at
            })
        })
        .collect();
    if results.is_empty() && !suppress_filter_hints {
        stats.warnings.extend(filtered_scope_warnings(
            &search_roots,
            include_ignored,
            include_hidden,
        ));
    }

    let search_strategy = match (
        stats.indexed_roots_used > 0,
        stats.filesystem_roots_walked > 0,
    ) {
        (true, true) => "mixed",
        (true, false) => "index",
        (false, true) => "filesystem_walk",
        (false, false) => "none",
    };
    let limit_reached = max_results > 0 && results.len() >= max_results;

    let index_metadata = fuzzy_index_metadata(&search_roots, &stats);
    let complete = !limit_reached
        && stats.broad_query_roots_skipped == 0
        && !cancellation
            .as_ref()
            .is_some_and(crate::cancellation::CancellationToken::is_cancelled);
    let response = json!({
        "root": result_root.as_deref().map(normalize_path),
        "results": results,
        "total_returned": results.len(),
        "complete": complete,
        "limit_reached": limit_reached,
        "limit_reason": if limit_reached { Some("max_results") } else { None },
        "search_strategy": search_strategy,
        "entries_scanned": stats.entries_scanned,
        "indexed_candidates_considered": stats.indexed_candidates_considered,
        "indexed_candidates_accepted": stats.indexed_candidates_accepted,
        "indexed_roots_used": stats.indexed_roots_used,
        "partial_index_roots_used": stats.partial_index_roots_used,
        "filesystem_roots_walked": stats.filesystem_roots_walked,
        "broad_query_roots_skipped": stats.broad_query_roots_skipped,
        "warnings": stats.warnings,
        "include_ignored": include_ignored,
        "include_hidden": include_hidden,
        "index_used": index_metadata.index_used,
        "index_complete": index_metadata.index_complete,
        "indexed_at": index_metadata.indexed_at,
        "index_age_secs": index_metadata.index_age_secs,
        "fallback_reason": index_metadata.fallback_reason,
        "no_fallback_reason": index_metadata.no_fallback_reason
    });
    Ok(finalize_fuzzy_response(response, verbose, complete))
}

fn empty_response(
    pattern: &str,
    search_strategy: &str,
    note: Option<String>,
    result_root: Option<&Path>,
    search_roots: &[PathBuf],
    verbose: bool,
) -> Value {
    let index_metadata = fuzzy_index_metadata(search_roots, &FuzzyStats::default());
    let response = json!({
        "root": result_root.map(normalize_path),
        "results": [],
        "total_returned": 0,
        "complete": true,
        "limit_reached": false,
        "limit_reason": None::<String>,
        "search_strategy": search_strategy,
        "entries_scanned": 0,
        "indexed_candidates_considered": 0,
        "indexed_candidates_accepted": 0,
        "indexed_roots_used": 0,
        "partial_index_roots_used": 0,
        "filesystem_roots_walked": 0,
        "broad_query_roots_skipped": 0,
        "warnings": Vec::<String>::new(),
        "index_used": index_metadata.index_used,
        "index_complete": index_metadata.index_complete,
        "indexed_at": index_metadata.indexed_at,
        "index_age_secs": index_metadata.index_age_secs,
        "fallback_reason": index_metadata.fallback_reason,
        "no_fallback_reason": index_metadata.no_fallback_reason,
        "pattern": pattern,
        "note": note
    });
    finalize_fuzzy_response(response, verbose, true)
}

#[allow(clippy::too_many_arguments)]
fn execute_glob_find(
    pattern: &str,
    glob_pattern: &Pattern,
    search_roots: &[PathBuf],
    display_roots: &[PathBuf],
    target_type: &str,
    extensions: &[String],
    max_depth: Option<u64>,
    max_results: usize,
    include_ignored: bool,
    include_hidden: bool,
    suppress_filter_hints: bool,
    cancellation: Option<&crate::cancellation::CancellationToken>,
    verbose: bool,
) -> Result<Value> {
    let mut ranked = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut stats = FuzzyStats::default();

    for root in search_roots {
        process_glob_root(
            root,
            display_roots,
            glob_pattern,
            target_type,
            extensions,
            max_depth,
            max_results,
            &mut ranked,
            &mut seen_paths,
            &mut stats,
            include_ignored,
            include_hidden,
            cancellation,
        )?;
    }
    crate::cancellation::finish_scan_progress(cancellation, stats.entries_scanned);

    ranked.sort_by(compare_ranked_match);
    let results: Vec<Value> = ranked
        .into_iter()
        .take(max_results)
        .map(|item| {
            json!({
                "path": item.relative_path,
                "score": item.score,
                "type": item.entry_type,
                "size_bytes": item.size,
                "modified_at": item.modified_at
            })
        })
        .collect();
    if results.is_empty() && !suppress_filter_hints {
        stats.warnings.extend(filtered_scope_warnings(
            search_roots,
            include_ignored,
            include_hidden,
        ));
    }
    let search_strategy = match (
        stats.indexed_roots_used > 0,
        stats.filesystem_roots_walked > 0,
    ) {
        (true, true) => "mixed",
        (true, false) => "index",
        (false, true) => "filesystem_walk",
        (false, false) => "none",
    };
    let limit_reached = max_results > 0 && results.len() >= max_results;

    let index_metadata = fuzzy_index_metadata(search_roots, &stats);
    let result_root = crate::common::common_path_root(search_roots);
    let complete = !limit_reached
        && stats.broad_query_roots_skipped == 0
        && !cancellation.is_some_and(crate::cancellation::CancellationToken::is_cancelled);
    let response = json!({
        "root": result_root.as_deref().map(normalize_path),
        "results": results,
        "total_returned": results.len(),
        "complete": complete,
        "limit_reached": limit_reached,
        "limit_reason": if limit_reached { Some("max_results") } else { None },
        "search_strategy": search_strategy,
        "pattern_kind": "glob",
        "entries_scanned": stats.entries_scanned,
        "indexed_candidates_considered": stats.indexed_candidates_considered,
        "indexed_candidates_accepted": stats.indexed_candidates_accepted,
        "indexed_roots_used": stats.indexed_roots_used,
        "partial_index_roots_used": stats.partial_index_roots_used,
        "filesystem_roots_walked": stats.filesystem_roots_walked,
        "broad_query_roots_skipped": stats.broad_query_roots_skipped,
        "warnings": stats.warnings,
        "include_ignored": include_ignored,
        "include_hidden": include_hidden,
        "index_used": index_metadata.index_used,
        "index_complete": index_metadata.index_complete,
        "indexed_at": index_metadata.indexed_at,
        "index_age_secs": index_metadata.index_age_secs,
        "fallback_reason": index_metadata.fallback_reason,
        "no_fallback_reason": index_metadata.no_fallback_reason,
        "note": format!("Pattern '{}' was treated as a glob; match is applied to relative path and file name.", pattern)
    });
    Ok(finalize_fuzzy_response(response, verbose, complete))
}

fn fuzzy_index_metadata(search_roots: &[PathBuf], stats: &FuzzyStats) -> FuzzyIndexMetadata {
    let indexed_at = crate::indexer::path_index_ages_for_paths(search_roots);
    let index_used = stats.indexed_roots_used > 0;
    let index_complete = !indexed_at.is_empty()
        && indexed_at.iter().all(|item| item.complete)
        && stats.partial_index_roots_used == 0;
    let index_age_secs = indexed_at
        .iter()
        .filter_map(|item| item.index_age_secs)
        .max();
    let fallback_reason = if stats.filesystem_roots_walked > 0 {
        if index_used {
            vec!["path_index_incomplete_or_unselective"]
        } else {
            vec!["path_index_unavailable_or_bypassed"]
        }
    } else {
        Vec::new()
    };
    let no_fallback_reason = (index_used && index_complete && stats.filesystem_roots_walked == 0)
        .then_some("index_complete");

    FuzzyIndexMetadata {
        index_used,
        index_complete,
        indexed_at,
        index_age_secs,
        fallback_reason,
        no_fallback_reason,
    }
}

fn fuzzy_diagnostics(response: &Value) -> Value {
    let mut diagnostics = serde_json::Map::new();
    for key in [
        "entries_scanned",
        "indexed_candidates_considered",
        "indexed_candidates_accepted",
        "indexed_roots_used",
        "partial_index_roots_used",
        "filesystem_roots_walked",
        "broad_query_roots_skipped",
        "warnings",
        "indexed_at",
        "fallback_reason",
        "no_fallback_reason",
        "include_ignored",
        "include_hidden",
    ] {
        if let Some(value) = response.get(key)
            && !value.is_null()
            && !matches!(value, Value::Array(items) if items.is_empty())
        {
            diagnostics.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(diagnostics)
}

fn finalize_fuzzy_response(mut response: Value, verbose: bool, complete: bool) -> Value {
    let include_diagnostics = verbose
        || !complete
        || response
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| !warnings.is_empty());
    let diagnostics = fuzzy_diagnostics(&response);
    if let Some(object) = response.as_object_mut() {
        for key in [
            "entries_scanned",
            "indexed_candidates_considered",
            "indexed_candidates_accepted",
            "indexed_roots_used",
            "partial_index_roots_used",
            "filesystem_roots_walked",
            "broad_query_roots_skipped",
            "warnings",
            "indexed_at",
            "fallback_reason",
            "no_fallback_reason",
            "include_ignored",
            "include_hidden",
        ] {
            object.remove(key);
        }
    }
    if include_diagnostics {
        crate::common::insert_object_field(&mut response, "diagnostics", diagnostics);
    }
    response
}

#[allow(clippy::too_many_arguments)]
fn process_glob_root(
    root: &Path,
    all_roots: &[PathBuf],
    glob_pattern: &Pattern,
    target_type: &str,
    extensions: &[String],
    max_depth: Option<u64>,
    max_results: usize,
    ranked: &mut Vec<RankedMatch>,
    seen_paths: &mut HashSet<String>,
    stats: &mut FuzzyStats,
    include_ignored: bool,
    include_hidden: bool,
    cancellation: Option<&crate::cancellation::CancellationToken>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let index_ready =
        !include_ignored && !include_hidden && crate::indexer::is_path_index_ready(&canonical_root);
    let has_index = !include_ignored
        && !include_hidden
        && crate::indexer::is_path_index_available(&canonical_root);
    let has_glob_anchors = !glob_anchor_terms(glob_pattern.as_str()).is_empty();
    if has_index
        && let Some(candidates) = crate::indexer::query_path_candidates(
            &canonical_root,
            glob_pattern.as_str(),
            indexed_shortlist_limit(max_results),
        )
    {
        stats.indexed_roots_used += 1;
        if !index_ready {
            stats.partial_index_roots_used += 1;
        }
        let mut indexed_accepted = 0usize;
        for candidate in candidates {
            stats.entries_scanned = stats.entries_scanned.saturating_add(1);
            if crate::cancellation::report_scan_progress(cancellation, stats.entries_scanned) {
                return Ok(());
            }
            stats.indexed_candidates_considered =
                stats.indexed_candidates_considered.saturating_add(1);
            let Ok(metadata) = std::fs::metadata(&candidate.path) else {
                continue;
            };
            let is_dir = metadata.is_dir();
            if !is_dir && !metadata.is_file() {
                continue;
            }
            if consider_glob_record(
                &candidate.path,
                is_dir,
                all_roots,
                glob_pattern,
                target_type,
                extensions,
                max_depth,
                max_results,
                ranked,
                seen_paths,
                Some(metadata_parts(&metadata)),
            ) {
                indexed_accepted = indexed_accepted.saturating_add(1);
            }
        }
        stats.indexed_candidates_accepted = stats
            .indexed_candidates_accepted
            .saturating_add(indexed_accepted);
        if indexed_accepted > 0 || (index_ready && has_glob_anchors) {
            return Ok(());
        }
    }

    if has_index {
        stats.indexed_roots_used += 1;
        if !index_ready {
            stats.partial_index_roots_used += 1;
        }
        let indexed_entries =
            crate::indexer::visit_indexed_entries_under(&canonical_root, |entry| {
                stats.entries_scanned = stats.entries_scanned.saturating_add(1);
                if crate::cancellation::report_scan_progress(cancellation, stats.entries_scanned) {
                    return false;
                }
                stats.indexed_candidates_considered =
                    stats.indexed_candidates_considered.saturating_add(1);
                if consider_glob_record(
                    &entry.path,
                    entry.is_dir,
                    all_roots,
                    glob_pattern,
                    target_type,
                    extensions,
                    max_depth,
                    max_results,
                    ranked,
                    seen_paths,
                    Some((entry.size, entry.modified_at)),
                ) {
                    stats.indexed_candidates_accepted =
                        stats.indexed_candidates_accepted.saturating_add(1);
                }
                true
            });
        if indexed_entries.unwrap_or(0) > 0 {
            crate::cancellation::finish_scan_progress(cancellation, stats.entries_scanned);
            return Ok(());
        }
    }

    let mut walk = WalkBuilder::new(&canonical_root);
    configure_walk_filters(&mut walk, include_ignored, include_hidden);
    walk.threads(crate::common::bounded_walk_threads());
    if let Some(depth) = max_depth {
        walk.max_depth(Some(depth as usize));
    }
    let extension_globs = extension_override_globs(target_type, extensions);
    if !extension_globs.is_empty() {
        apply_walk_overrides(&mut walk, &canonical_root, &extension_globs, &[])?;
    }
    let filter_root = canonical_root.clone();
    walk.filter_entry(move |entry| {
        !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir())
            || !is_vcs_metadata_dir(entry.path(), &filter_root)
    });

    stats.filesystem_roots_walked += 1;
    let local_ranked = Arc::new(Mutex::new(Vec::<RankedMatch>::new()));
    let local_seen = Arc::new(Mutex::new(HashSet::<String>::new()));
    let entries_scanned = Arc::new(AtomicUsize::new(stats.entries_scanned));
    let closure_cancellation = cancellation.cloned();
    let closure_roots = all_roots.to_vec();
    let closure_pattern = glob_pattern.clone();
    let closure_target_type = target_type.to_string();
    let closure_extensions = extensions.to_vec();
    let closure_max_depth = max_depth;
    let closure_max_results = max_results;

    walk.build_parallel().run(|| {
        let local_ranked = Arc::clone(&local_ranked);
        let local_seen = Arc::clone(&local_seen);
        let entries_scanned = Arc::clone(&entries_scanned);
        let roots = closure_roots.clone();
        let glob_pattern = closure_pattern.clone();
        let target_type = closure_target_type.clone();
        let extensions = closure_extensions.clone();
        let cancellation = closure_cancellation.clone();

        Box::new(move |entry| {
            let entries_seen = entries_scanned
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            if crate::cancellation::report_scan_progress(cancellation.as_ref(), entries_seen) {
                return ignore::WalkState::Quit;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return ignore::WalkState::Continue,
            };
            let Some(file_type) = entry.file_type() else {
                return ignore::WalkState::Continue;
            };

            let mut ranked = match local_ranked.lock() {
                Ok(guard) => guard,
                Err(_) => return ignore::WalkState::Quit,
            };
            let mut seen = match local_seen.lock() {
                Ok(guard) => guard,
                Err(_) => return ignore::WalkState::Quit,
            };
            consider_glob_record(
                entry.path(),
                file_type.is_dir(),
                &roots,
                &glob_pattern,
                &target_type,
                &extensions,
                closure_max_depth,
                closure_max_results,
                &mut ranked,
                &mut seen,
                None,
            );
            ignore::WalkState::Continue
        })
    });

    stats.entries_scanned = entries_scanned.load(Ordering::Relaxed);
    let local_ranked = match Arc::try_unwrap(local_ranked) {
        Ok(mutex) => mutex
            .into_inner()
            .map_err(|_| anyhow::anyhow!("fuzzy_find glob collector is unavailable"))?,
        Err(shared) => shared
            .lock()
            .map_err(|_| anyhow::anyhow!("fuzzy_find glob collector is unavailable"))?
            .clone(),
    };
    for candidate in local_ranked {
        if !seen_paths.insert(candidate.path.clone()) {
            continue;
        }
        push_ranked_match(ranked, candidate, max_results);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn consider_glob_record(
    path: &Path,
    is_dir: bool,
    roots: &[PathBuf],
    glob_pattern: &Pattern,
    target_type: &str,
    extensions: &[String],
    max_depth: Option<u64>,
    max_results: usize,
    ranked: &mut Vec<RankedMatch>,
    seen_paths: &mut HashSet<String>,
    cached_meta: Option<(u64, u64)>,
) -> bool {
    if target_type == "file" && is_dir {
        return false;
    }
    if target_type == "dir" && !is_dir {
        return false;
    }
    if is_dir && !extensions.is_empty() {
        return false;
    }
    if !extensions.is_empty() {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        if !extensions
            .iter()
            .any(|extension| ext.eq_ignore_ascii_case(extension))
        {
            return false;
        }
    }

    let relative_path = compute_relative_path(path, roots);
    if !within_max_depth(&relative_path, max_depth) {
        return false;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !glob_pattern.matches(&relative_path) && !glob_pattern.matches(file_name) {
        return false;
    }

    let normalized_path = normalize_path(path);
    if !seen_paths.insert(normalized_path.clone()) {
        return false;
    }
    let (size, modified_at) = cached_meta.unwrap_or_else(|| read_entry_metadata(path));
    let depth_penalty = relative_path.split('/').count() as i64 * 10;
    let length_penalty = relative_path.len().min(9_000) as i64;
    push_ranked_match(
        ranked,
        RankedMatch {
            path: normalized_path,
            relative_path,
            score: 10_000 - depth_penalty - length_penalty,
            entry_type: if is_dir { "dir" } else { "file" },
            size,
            modified_at,
        },
        max_results,
    );
    true
}

#[allow(clippy::too_many_arguments)]
fn process_search_root(
    root: &Path,
    all_roots: &[PathBuf],
    pattern: &str,
    target_type: &str,
    extensions: &[String],
    filter_terms: &[String],
    max_depth: Option<u64>,
    max_results: usize,
    matcher: &SkimMatcherV2,
    ranked: &mut Vec<RankedMatch>,
    seen_paths: &mut HashSet<String>,
    stats: &mut FuzzyStats,
    include_ignored: bool,
    include_hidden: bool,
    cancellation: Option<&crate::cancellation::CancellationToken>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    if canonical_root.is_file() {
        stats.entries_scanned = stats.entries_scanned.saturating_add(1);
        if crate::cancellation::report_scan_progress(cancellation, stats.entries_scanned) {
            return Ok(());
        }
        consider_candidate(
            &canonical_root,
            false,
            all_roots,
            pattern,
            target_type,
            extensions,
            filter_terms,
            None,
            max_results,
            matcher,
            ranked,
            seen_paths,
            None,
        );
        return Ok(());
    }

    if !canonical_root.is_dir() {
        return Ok(());
    }

    let index_complete =
        !include_ignored && !include_hidden && crate::indexer::is_path_index_ready(&canonical_root);
    let small_indexed_workspace = crate::indexer::indexed_workspace_file_count(&canonical_root)
        .is_some_and(|count| count <= crate::indexer::LARGE_WORKSPACE_FILE_THRESHOLD);
    if !include_ignored
        && !include_hidden
        && crate::indexer::is_path_index_available(&canonical_root)
        && let Some(candidates) = crate::indexer::query_path_candidates(
            &canonical_root,
            pattern,
            indexed_shortlist_limit(max_results),
        )
    {
        stats.indexed_roots_used += 1;
        if !index_complete {
            stats.partial_index_roots_used += 1;
        }

        if target_type != "file" {
            consider_candidate(
                &canonical_root,
                true,
                all_roots,
                pattern,
                target_type,
                extensions,
                filter_terms,
                max_depth,
                max_results,
                matcher,
                ranked,
                seen_paths,
                None,
            );
        }

        for candidate in candidates {
            stats.entries_scanned = stats.entries_scanned.saturating_add(1);
            if crate::cancellation::report_scan_progress(cancellation, stats.entries_scanned) {
                return Ok(());
            }
            stats.indexed_candidates_considered =
                stats.indexed_candidates_considered.saturating_add(1);
            if consider_indexed_candidate(
                &candidate,
                all_roots,
                pattern,
                target_type,
                extensions,
                filter_terms,
                max_depth,
                max_results,
                matcher,
                ranked,
                seen_paths,
            ) {
                stats.indexed_candidates_accepted =
                    stats.indexed_candidates_accepted.saturating_add(1);
            }
        }

        if index_complete
            && !small_indexed_workspace
            && should_skip_broad_filesystem_fallback(pattern, filter_terms, extensions, max_depth)
        {
            stats.broad_query_roots_skipped += 1;
            stats.warnings.push(format!(
                "Skipped filesystem fallback for broad fuzzy pattern '{}' under indexed root '{}'; use a concrete basename/path token, extensions, max_depth, or a narrower path.",
                pattern,
                crate::common::normalize_display_path(&canonical_root)
            ));
            return Ok(());
        }
    }

    let mut walk = WalkBuilder::new(&canonical_root);
    configure_walk_filters(&mut walk, include_ignored, include_hidden);
    walk.threads(crate::common::bounded_walk_threads());
    if let Some(depth) = max_depth {
        walk.max_depth(Some(depth as usize));
    }
    let extension_globs = extension_override_globs(target_type, extensions);
    if !extension_globs.is_empty() {
        apply_walk_overrides(&mut walk, &canonical_root, &extension_globs, &[])?;
    }
    let filter_root = canonical_root.clone();
    walk.filter_entry(move |entry| {
        !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir())
            || !is_vcs_metadata_dir(entry.path(), &filter_root)
    });

    stats.filesystem_roots_walked += 1;
    let local_ranked = Arc::new(Mutex::new(Vec::<RankedMatch>::new()));
    let local_seen = Arc::new(Mutex::new(HashSet::<String>::new()));
    let entries_scanned = Arc::new(AtomicUsize::new(stats.entries_scanned));
    let closure_cancellation = cancellation.cloned();
    let closure_roots = all_roots.to_vec();
    let closure_pattern = pattern.to_string();
    let closure_target_type = target_type.to_string();
    let closure_extensions = extensions.to_vec();
    let closure_filter_terms = filter_terms.to_vec();
    let closure_max_depth = max_depth;
    let closure_max_results = max_results;

    walk.build_parallel().run(|| {
        let local_ranked = Arc::clone(&local_ranked);
        let local_seen = Arc::clone(&local_seen);
        let entries_scanned = Arc::clone(&entries_scanned);
        let roots = closure_roots.clone();
        let pattern = closure_pattern.clone();
        let target_type = closure_target_type.clone();
        let extensions = closure_extensions.clone();
        let filter_terms = closure_filter_terms.clone();
        let cancellation = closure_cancellation.clone();
        let matcher = SkimMatcherV2::default();

        Box::new(move |entry| {
            let entries_seen = entries_scanned
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            if crate::cancellation::report_scan_progress(cancellation.as_ref(), entries_seen) {
                return ignore::WalkState::Quit;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return ignore::WalkState::Continue,
            };

            let Some(file_type) = entry.file_type() else {
                return ignore::WalkState::Continue;
            };

            if target_type == "file" && !file_type.is_file() {
                return ignore::WalkState::Continue;
            }
            if target_type == "dir" && !file_type.is_dir() {
                return ignore::WalkState::Continue;
            }

            let Some(candidate) = score_candidate(
                entry.path(),
                file_type.is_dir(),
                &roots,
                &pattern,
                &target_type,
                &extensions,
                &filter_terms,
                closure_max_depth,
                &matcher,
                None,
            ) else {
                return ignore::WalkState::Continue;
            };

            {
                let mut seen = match local_seen.lock() {
                    Ok(guard) => guard,
                    Err(_) => return ignore::WalkState::Quit,
                };
                if !seen.insert(candidate.path.clone()) {
                    return ignore::WalkState::Continue;
                }
            }

            let mut ranked = match local_ranked.lock() {
                Ok(guard) => guard,
                Err(_) => return ignore::WalkState::Quit,
            };
            push_ranked_match(&mut ranked, candidate, closure_max_results);
            ignore::WalkState::Continue
        })
    });

    stats.entries_scanned = entries_scanned.load(Ordering::Relaxed);

    let local_ranked = match Arc::try_unwrap(local_ranked) {
        Ok(mutex) => mutex
            .into_inner()
            .map_err(|_| anyhow::anyhow!("fuzzy_find ranked collector is unavailable"))?,
        Err(shared) => shared
            .lock()
            .map_err(|_| anyhow::anyhow!("fuzzy_find ranked collector is unavailable"))?
            .clone(),
    };
    for candidate in local_ranked {
        if !seen_paths.insert(candidate.path.clone()) {
            continue;
        }
        push_ranked_match(ranked, candidate, max_results);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn consider_candidate(
    path: &Path,
    is_dir: bool,
    roots: &[PathBuf],
    pattern: &str,
    target_type: &str,
    extensions: &[String],
    filter_terms: &[String],
    max_depth: Option<u64>,
    max_results: usize,
    matcher: &SkimMatcherV2,
    ranked: &mut Vec<RankedMatch>,
    seen_paths: &mut HashSet<String>,
    cached_meta: Option<(u64, u64)>,
) {
    let Some(candidate) = score_candidate(
        path,
        is_dir,
        roots,
        pattern,
        target_type,
        extensions,
        filter_terms,
        max_depth,
        matcher,
        cached_meta,
    ) else {
        return;
    };
    if !seen_paths.insert(candidate.path.clone()) {
        return;
    }

    push_ranked_match(ranked, candidate, max_results);
}

#[allow(clippy::too_many_arguments)]
fn consider_indexed_candidate(
    candidate: &crate::indexer::PathQueryCandidate,
    roots: &[PathBuf],
    pattern: &str,
    target_type: &str,
    extensions: &[String],
    filter_terms: &[String],
    max_depth: Option<u64>,
    max_results: usize,
    matcher: &SkimMatcherV2,
    ranked: &mut Vec<RankedMatch>,
    seen_paths: &mut HashSet<String>,
) -> bool {
    let Ok(metadata) = std::fs::metadata(&candidate.path) else {
        return false;
    };
    let is_dir = metadata.is_dir();
    if !is_dir && !metadata.is_file() {
        return false;
    }
    let Some(candidate) = score_candidate_from_parts(
        &candidate.path,
        is_dir,
        compute_relative_path(&candidate.path, roots),
        pattern,
        target_type,
        extensions,
        filter_terms,
        max_depth,
        matcher,
        Some(metadata_parts(&metadata)),
    ) else {
        return false;
    };
    if !seen_paths.insert(candidate.path.clone()) {
        return false;
    }

    push_ranked_match(ranked, candidate, max_results);
    true
}

fn push_ranked_match(ranked: &mut Vec<RankedMatch>, candidate: RankedMatch, max_results: usize) {
    if max_results == 0 {
        return;
    }

    if ranked.len() >= max_results
        && ranked
            .last()
            .is_some_and(|worst| !compare_ranked_match(&candidate, worst).is_lt())
    {
        return;
    }

    let insert_at = ranked
        .binary_search_by(|existing| compare_ranked_match(existing, &candidate))
        .unwrap_or_else(|index| index);
    ranked.insert(insert_at, candidate);
    if ranked.len() > max_results {
        ranked.truncate(max_results);
    }
}

#[allow(clippy::too_many_arguments)]
fn score_candidate(
    path: &Path,
    is_dir: bool,
    roots: &[PathBuf],
    pattern: &str,
    target_type: &str,
    extensions: &[String],
    filter_terms: &[String],
    max_depth: Option<u64>,
    matcher: &SkimMatcherV2,
    cached_meta: Option<(u64, u64)>,
) -> Option<RankedMatch> {
    let relative_path = compute_relative_path(path, roots);
    score_candidate_from_parts(
        path,
        is_dir,
        relative_path,
        pattern,
        target_type,
        extensions,
        filter_terms,
        max_depth,
        matcher,
        cached_meta,
    )
}

#[allow(clippy::too_many_arguments)]
fn score_candidate_from_parts(
    path: &Path,
    is_dir: bool,
    relative_path: String,
    pattern: &str,
    target_type: &str,
    extensions: &[String],
    filter_terms: &[String],
    max_depth: Option<u64>,
    matcher: &SkimMatcherV2,
    cached_meta: Option<(u64, u64)>,
) -> Option<RankedMatch> {
    if target_type == "file" && is_dir {
        return None;
    }
    if target_type == "dir" && !is_dir {
        return None;
    }
    if is_dir && !extensions.is_empty() {
        return None;
    }

    if !extensions.is_empty() {
        let ext = path.extension().and_then(|e| e.to_str())?;
        if !extensions
            .iter()
            .any(|extension| ext.eq_ignore_ascii_case(extension))
        {
            return None;
        }
    }

    if max_depth.is_some() && !within_max_depth(&relative_path, max_depth) {
        return None;
    }

    let score_target_lower = if !filter_terms.is_empty() {
        let filter_target = relative_path.to_ascii_lowercase();
        if !filter_terms.iter().all(|term| filter_target.contains(term)) {
            return None;
        }
        Some(filter_target)
    } else {
        None
    };

    let normalized_path;
    let score_target = if relative_path.is_empty() {
        normalized_path = normalize_path(path);
        normalized_path.as_str()
    } else {
        relative_path.as_str()
    };
    let score = matcher.fuzzy_match(score_target, pattern).or_else(|| {
        score_target_lower
            .as_deref()
            .map(|lower| term_containment_score(lower, filter_terms))
    })?;
    if score <= 0 {
        return None;
    }

    let (size, modified_at) = cached_meta.unwrap_or_else(|| read_entry_metadata(path));

    Some(RankedMatch {
        path: normalize_path(path),
        relative_path,
        score,
        entry_type: if is_dir { "dir" } else { "file" },
        size,
        modified_at,
    })
}

fn read_entry_metadata(path: &Path) -> (u64, u64) {
    std::fs::metadata(path)
        .map(|meta| metadata_parts(&meta))
        .unwrap_or((0, 0))
}

fn metadata_parts(meta: &Metadata) -> (u64, u64) {
    let modified_at = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    (meta.len(), modified_at)
}

fn within_max_depth(relative_path: &str, max_depth: Option<u64>) -> bool {
    let Some(max_depth) = max_depth else {
        return true;
    };
    if relative_path.is_empty() {
        return true;
    }

    relative_path.split('/').count() as u64 <= max_depth
}

fn term_containment_score(score_target_lower: &str, filter_terms: &[String]) -> i64 {
    let position_penalty = filter_terms
        .iter()
        .filter_map(|term| score_target_lower.find(term))
        .sum::<usize>() as i64;
    let length_penalty = score_target_lower.len().min(2_000) as i64;
    2_000 - length_penalty - position_penalty
}

fn compute_relative_path(path: &Path, roots: &[PathBuf]) -> String {
    for root in roots {
        if let Some(stripped) = crate::common::relative_display_path(path, root) {
            if stripped.is_empty() {
                return path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string();
            }
            return stripped;
        }
    }

    normalize_path(path)
}

fn compare_ranked_match(left: &RankedMatch, right: &RankedMatch) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.relative_path.cmp(&right.relative_path))
        .then_with(|| left.path.cmp(&right.path))
}

fn indexed_shortlist_limit(max_results: usize) -> usize {
    max_results.saturating_mul(64).clamp(256, 4096)
}

fn compile_glob_pattern(pattern: &str) -> Result<Option<Pattern>> {
    if !pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | '{'))
    {
        return Ok(None);
    }
    compile_pattern(pattern).map(Some)
}

fn glob_anchor_terms(pattern: &str) -> Vec<String> {
    let mut terms = pattern
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 3)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    terms
}

fn pattern_filter_terms(pattern: &str) -> Vec<String> {
    let normalized = pattern.to_ascii_lowercase();
    let mut terms = normalized
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 2)
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    if terms.is_empty() {
        return Vec::new();
    }

    terms.sort();
    terms.dedup();
    terms
}

fn should_skip_broad_filesystem_fallback(
    pattern: &str,
    filter_terms: &[String],
    extensions: &[String],
    max_depth: Option<u64>,
) -> bool {
    !filter_terms.is_empty()
        && !pattern.contains('/')
        && !pattern.contains('\\')
        && !pattern.contains('.')
        && extensions.is_empty()
        && max_depth.is_none()
}

fn extension_override_globs(target_type: &str, extensions: &[String]) -> Vec<String> {
    if target_type != "file" {
        return Vec::new();
    }

    extensions
        .iter()
        .filter(|extension| !extension.is_empty())
        .map(|extension| format!("*.{}", extension))
        .collect()
}

fn parse_usize_arg(args: &Value, name: &str, default: usize, min: usize, max: usize) -> usize {
    args.get(name)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn normalize_path(path: &Path) -> String {
    crate::common::normalize_display_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(relative_path: &str, score: i64) -> RankedMatch {
        RankedMatch {
            path: relative_path.to_string(),
            relative_path: relative_path.to_string(),
            score,
            entry_type: "file",
            size: 0,
            modified_at: 0,
        }
    }

    #[test]
    fn push_ranked_match_replaces_worst_with_better_candidate() {
        let mut ranked_matches = Vec::new();
        push_ranked_match(&mut ranked_matches, ranked("z.rs", 1), 1);
        push_ranked_match(&mut ranked_matches, ranked("a.rs", 10), 1);

        assert_eq!(ranked_matches.len(), 1);
        assert_eq!(ranked_matches[0].relative_path, "a.rs");
        assert_eq!(ranked_matches[0].score, 10);
    }

    #[test]
    fn indexed_candidates_reject_directories_with_extension_filter() {
        let matcher = SkimMatcherV2::default();
        let extensions = vec!["rs".to_string()];
        assert!(
            score_candidate_from_parts(
                Path::new("src/example.rs"),
                true,
                "src/example.rs".to_string(),
                "example",
                "any",
                &extensions,
                &[],
                None,
                &matcher,
                Some((0, 0))
            )
            .is_none()
        );
        assert!(
            score_candidate_from_parts(
                Path::new("src/example"),
                true,
                "src/example".to_string(),
                "example",
                "any",
                &extensions,
                &[],
                None,
                &matcher,
                Some((0, 0))
            )
            .is_none()
        );
        assert!(
            score_candidate_from_parts(
                Path::new("src/example.rs"),
                false,
                "src/example.rs".to_string(),
                "example",
                "any",
                &extensions,
                &[],
                None,
                &matcher,
                Some((0, 0))
            )
            .is_some()
        );
    }
}

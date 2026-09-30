use anyhow::{Context, Result};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::{WalkBuilder, WalkState};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use tokio::task;

use super::output_format::{OutputFormat, parse_output_format};
use super::path_filters::{
    Pattern, apply_walk_overrides, compile_patterns, configure_walk_filters,
    default_generated_vendor_globs, filtered_scope_warnings_with_defaults,
    is_direct_vendor_or_generated_scope, is_vcs_metadata_dir, matches_patterns_or_ancestors,
    parse_pattern_strings, passes_patterns as path_passes_patterns,
};
use super::search_snippet::{render_line_start, render_match_line};
use crate::cancellation::CancellationToken;
use crate::common::insert_object_field;
use crate::indexer::{content_policy_allows_path, query_tantivy_content_candidates};

const DEFAULT_MAX_RESULTS: usize = 100;
const MAX_RETURNED_MATCHES: usize = 1_000;
const DEFAULT_MAX_LINE_LENGTH: usize = 240;
const MAX_LINE_LENGTH: usize = 4_000;
const MAX_UNINDEXED_FILES_REPORTED: usize = 100;
static TOTAL_TEXT_SEARCHES: AtomicU64 = AtomicU64::new(0);
static TOTAL_GREP_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static TOTAL_REFUSED_LARGE_SCOPE: AtomicU64 = AtomicU64::new(0);
static LAST_SEARCH_DURATION_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Serialize, Debug)]
pub struct SearchMatch {
    pub file: String,
    pub line: u64,
    pub line_text: String,
    pub match_column: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub line_truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_before: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_after: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SearchMode {
    Literal,
    Regex,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum CaseMode {
    Insensitive,
    Sensitive,
    Smart,
}

#[derive(Clone, Debug)]
struct CandidateFile {
    path: PathBuf,
    relative_path: String,
}

#[derive(Default, Debug)]
struct SearchStats {
    paths_received: usize,
    valid_paths: usize,
    invalid_paths: Vec<String>,
    files_considered: usize,
    files_searched: usize,
    files_skipped_large: usize,
}

#[derive(Default)]
struct SharedSearchState {
    matches: Mutex<Vec<SearchMatch>>,
    seen: Mutex<HashSet<PathBuf>>,
    unindexed_seen: Mutex<HashSet<PathBuf>>,
    unindexed_files: Mutex<Vec<String>>,
    unindexed_files_count: AtomicUsize,
    files_considered: AtomicUsize,
    files_searched: AtomicUsize,
    search_errors: AtomicUsize,
    files_skipped_large: AtomicUsize,
    stop: AtomicBool,
    cancellation: Option<CancellationToken>,
}

impl SharedSearchState {
    fn cancellation_requested(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }
}

#[derive(Debug)]
struct FallbackPlan {
    allow_grep: bool,
    reason: Option<&'static str>,
}

const DIAGNOSTIC_FIELDS: &[&str] = &[
    "files_searched",
    "files_considered",
    "files_skipped_large",
    "search_errors",
    "duration_ms",
    "case_mode",
    "content_index_used",
    "content_index_partial",
    "content_index_zones",
    "indexed_at",
    "zone_indexed_at",
    "warming_zones",
    "fallback_reason",
    "grep_fallback_performed",
    "no_fallback_reason",
    "unindexed_files_in_scope",
    "unindexed_files_in_scope_count",
    "unindexed_files_complete",
    "unindexed_files_scope_complete",
    "default_excludes_applied",
    "include_ignored",
    "include_hidden",
    "candidate_count",
    "candidate_limit",
    "candidates_complete",
    "no_results",
    "suggested_next_query",
    "warnings",
    "cancelled",
];

pub fn schema() -> Value {
    json!({
        "name": "text_search",
        "title": "Search text",
        "description": "Search file contents with exact literal or regex verification. Literal queries use Tantivy only for shortlisting and grep for correctness. Inspect search_strategy, fallback_reason, zone age, and unindexed-file diagnostics.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query. Interpreted as literal text unless mode is regex." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Files or directories to search. Defaults to the active workspace root. In large repositories, provide the narrowest directory or file scope; avoid ['.'] unless allow_expensive_fallback is intentional." },
                "mode": { "type": "string", "enum": ["literal", "regex"], "description": "Search mode. Defaults to literal. Literal queries can use Tantivy to shortlist files before exact grep verification; regex always needs grep verification and should be scoped narrowly." },
                "case_mode": { "type": "string", "enum": ["insensitive", "sensitive", "smart"], "description": "Case handling. smart is case-insensitive unless the query contains uppercase. If case_sensitive is provided, it overrides case_mode for backward compatibility." },
                "case_sensitive": { "type": "boolean", "description": "Legacy override for case matching. When set, true forces sensitive and false forces insensitive, taking precedence over case_mode." },
                "max_results": { "type": "integer", "minimum": 0, "maximum": 1000, "description": "Maximum matches to return. Defaults to 100; 0 returns no matches." },
                "includes": { "type": "array", "items": { "type": "string" }, "description": "Glob include filters relative to searched roots, e.g. **/*.rs." },
                "excludes": { "type": "array", "items": { "type": "string" }, "description": "Glob exclude filters relative to searched roots. Grep fallback also applies shared generated/vendor excludes (including build, dist, obj, out, target, node_modules, vendor, and third_party) unless the user directly scopes into them. bin is excluded only for recognized .NET/Java workspaces; default_excludes_applied reports this." },
                "context_lines": { "type": "integer", "minimum": 0, "maximum": 10, "description": "Number of before/after context lines per match, returned as context_before/context_after. Maximum 10." },
                "max_line_length": { "type": "integer", "minimum": 1, "maximum": 4000, "description": "Maximum displayed characters per matched line before ellipsis markers. Defaults to 240." },
                "explain_no_results": { "type": "boolean", "description": "When true, include diagnostics explaining why no matches were found, including fallback/index context." },
                "include_ignored": { "type": "boolean", "description": "Include files ignored by .gitignore, .git/info/exclude, global gitignore, or .ignore files during grep fallback." },
                "include_hidden": { "type": "boolean", "description": "Include hidden files and directories except VCS metadata directories such as .git, unless scoped directly." },
                "allow_expensive_fallback": { "type": "boolean", "description": "Set true to permit root-wide grep fallback in very large indexed workspaces. Default false protects agents from Chromium-scale timeouts; prefer scoping paths first." }
                ,"output_format": { "type": "string", "enum": ["json", "markdown", "compact"], "description": "Output shape. All formats group detailed diagnostics only when results are incomplete, explicitly requested, or verbose." }
                ,"verbose": { "type": "boolean", "description": "Include detailed search and index diagnostics even when the result is complete. Defaults to false." }
            },
            "required": ["query"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("text_search background task failed to join")?
}

pub fn search_telemetry() -> Value {
    json!({
        "total_text_searches": TOTAL_TEXT_SEARCHES.load(Ordering::Relaxed),
        "total_grep_fallbacks": TOTAL_GREP_FALLBACKS.load(Ordering::Relaxed),
        "total_refused_large_scope": TOTAL_REFUSED_LARGE_SCOPE.load(Ordering::Relaxed),
        "last_search_duration_ms": LAST_SEARCH_DURATION_MS.load(Ordering::Relaxed)
    })
}

fn execute_blocking(args: Value) -> Result<Value> {
    let started_at = Instant::now();
    TOTAL_TEXT_SEARCHES.fetch_add(1, Ordering::Relaxed);
    let cancellation = crate::cancellation::token_from_args(&args);
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_end();
    if query.is_empty() {
        return Err(anyhow::anyhow!("Query cannot be empty"));
    }
    let output_format = parse_output_format(args.get("output_format"), true)?;
    let verbose = args
        .get("verbose")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if cancellation
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Ok(format_text_search_response(
            cancelled_search_response(started_at),
            output_format,
            verbose,
        ));
    }

    let input_paths: Vec<PathBuf> =
        if let Some(paths) = args.get("paths").and_then(|v| v.as_array()) {
            paths
                .iter()
                .filter_map(|path| path.as_str())
                .map(crate::common::resolve_existing_tool_path)
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![crate::common::default_tool_root()]
        };
    let display_root = crate::common::common_path_root(&input_paths);
    if let Some(cancellation) = cancellation.as_ref() {
        let workspace_root = input_paths
            .first()
            .and_then(|path| crate::common::discover_workspace_root(path))
            .or_else(|| crate::workspace_control::active_workspace().map(|(root, _)| root))
            .map(|root| crate::common::normalize_display_path(&root));
        cancellation.report_progress(0, workspace_root.as_deref());
    }

    let mode = parse_mode(args.get("mode").and_then(|v| v.as_str()))?;
    if args.get("case_sensitive").is_some() && args.get("case_mode").is_some() {
        anyhow::bail!("Conflicting case_sensitive and case_mode: choose only one case option");
    }
    let case_mode = parse_case_mode(
        args.get("case_mode").and_then(|v| v.as_str()),
        args.get("case_sensitive").and_then(|v| v.as_bool()),
    )?;
    let case_sensitive_effective = resolve_case_sensitive(case_mode, query);
    let max_results = parse_usize_arg(
        &args,
        "max_results",
        DEFAULT_MAX_RESULTS,
        0,
        MAX_RETURNED_MATCHES,
    );
    let context_lines = args
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(10) as usize;
    let max_line_length = parse_usize_arg(
        &args,
        "max_line_length",
        DEFAULT_MAX_LINE_LENGTH,
        1,
        MAX_LINE_LENGTH,
    );
    let explain_no_results = args
        .get("explain_no_results")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let allow_expensive_fallback = args
        .get("allow_expensive_fallback")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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

    let include_globs = parse_pattern_strings(args.get("includes"));
    let user_exclude_globs = parse_pattern_strings(args.get("excludes"));
    let default_exclude_globs = default_fallback_excludes(&input_paths, &user_exclude_globs);
    let mut exclude_globs = user_exclude_globs.clone();
    exclude_globs.extend(default_exclude_globs.iter().cloned());
    let includes = Arc::new(compile_patterns(&include_globs)?);
    let user_excludes = Arc::new(compile_patterns(&user_exclude_globs)?);
    let excludes = Arc::new(compile_patterns(&exclude_globs)?);
    let pattern = Arc::new(match mode {
        SearchMode::Literal => regex::escape(query),
        SearchMode::Regex => query.to_string(),
    });
    build_matcher(&pattern, case_sensitive_effective)
        .map_err(|error| anyhow::anyhow!("Invalid search query: {error:#}"))?;

    let includes_applied = !includes.is_empty();
    let excludes_applied = !excludes.is_empty();
    let default_excludes_applied = !default_exclude_globs.is_empty();
    let shared = Arc::new(SharedSearchState {
        cancellation,
        ..Default::default()
    });
    let mut stats = SearchStats {
        paths_received: input_paths.len(),
        ..Default::default()
    };
    let mut search_strategy = "grep_fallback";
    let mut content_index_used = false;
    let mut content_index_partial = false;
    let mut content_index_zones = Vec::<String>::new();
    let mut warming_zones = Vec::<String>::new();
    let mut zone_indexed_at = Vec::new();
    let mut index_age_secs = None;
    let mut fallback_reasons = Vec::<String>::new();
    let mut candidate_count = 0usize;
    let mut candidate_limit = 0usize;
    let mut candidates_truncated = false;
    let run_grep_fallback = max_results > 0;
    let mut grep_fallback_performed = false;
    let mut no_fallback_reason = None::<String>;
    let mut index_candidates_searched = false;

    if include_ignored {
        fallback_reasons.push("include_ignored_requires_filesystem".to_string());
    }
    if include_hidden {
        fallback_reasons.push("include_hidden_requires_filesystem".to_string());
    }
    if !include_ignored && !include_hidden && mode == SearchMode::Literal && max_results > 0 {
        let requested_candidate_limit = max_results.saturating_mul(64).max(256);
        let index_result =
            query_tantivy_content_candidates(&input_paths, query, requested_candidate_limit);
        content_index_used = index_result.content_index_used;
        content_index_partial = index_result.content_index_partial;
        content_index_zones = index_result.zones.clone();
        warming_zones = index_result.warming_zones.clone();
        fallback_reasons.extend(index_result.fallback_reasons.clone());
        candidate_count = index_result.candidate_count;
        candidate_limit = index_result.candidate_limit;
        candidates_truncated = index_result.candidates_truncated;
        zone_indexed_at = index_result.zone_indexed_at;
        index_age_secs = index_result.index_age_secs;

        if content_index_used {
            let matcher = build_matcher(&pattern, case_sensitive_effective)
                .map_err(|error| anyhow::anyhow!("Invalid search query: {error:#}"))?;
            index_candidates_searched = true;
            search_index_candidate_paths(
                index_result.paths,
                &input_paths,
                includes.as_ref(),
                user_excludes.as_ref(),
                &matcher,
                context_lines,
                max_results,
                max_line_length,
                Arc::clone(&shared),
                &mut stats,
            );

            fallback_reasons.push("literal_verification_requires_grep".to_string());
        }
    } else if mode == SearchMode::Regex {
        fallback_reasons.push("regex_mode_requires_grep".to_string());
    } else if max_results == 0 {
        fallback_reasons.push("max_results_zero".to_string());
    }

    if run_grep_fallback {
        if content_index_used {
            search_strategy = "mixed";
        }
        let fallback_plan = plan_grep_fallback(&input_paths, allow_expensive_fallback);
        if !fallback_plan.allow_grep {
            search_strategy = "refused_large_scope";
            TOTAL_REFUSED_LARGE_SCOPE.fetch_add(1, Ordering::Relaxed);
            if let Some(reason) = fallback_plan.reason {
                fallback_reasons.push(reason.to_string());
                no_fallback_reason = Some(reason.to_string());
            }
            record_input_path_validity(&input_paths, &mut stats);
        } else {
            grep_fallback_performed = true;
            TOTAL_GREP_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            let dedup_fallback_candidates = index_candidates_searched || input_paths.len() > 1;
            for input_path in &input_paths {
                if shared.cancellation_requested() {
                    shared.stop.store(true, Ordering::Relaxed);
                    break;
                }
                process_input_path(
                    input_path,
                    &include_globs,
                    &exclude_globs,
                    Arc::clone(&includes),
                    Arc::clone(&excludes),
                    Arc::clone(&pattern),
                    case_sensitive_effective,
                    context_lines,
                    search_collection_limit(max_results),
                    max_line_length,
                    dedup_fallback_candidates,
                    include_ignored,
                    include_hidden,
                    Arc::clone(&shared),
                    &mut stats,
                )?;
            }
        }
    } else {
        no_fallback_reason = Some("max_results_zero".to_string());
        record_input_path_validity(&input_paths, &mut stats);
    }
    fallback_reasons.sort();
    fallback_reasons.dedup();

    let search_errors = shared.search_errors.load(Ordering::Relaxed);
    let unindexed_files_count = shared.unindexed_files_count.load(Ordering::Relaxed);
    let mut unindexed_files_in_scope = shared
        .unindexed_files
        .lock()
        .map_err(|_| anyhow::anyhow!("text_search unindexed-file collector is unavailable"))?
        .clone();
    relativize_display_paths(&mut unindexed_files_in_scope, display_root.as_deref());
    let unindexed_files_truncated = unindexed_files_count > unindexed_files_in_scope.len();
    let unindexed_files_scope_complete = grep_fallback_performed && search_errors == 0;
    let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    LAST_SEARCH_DURATION_MS.store(duration_ms, Ordering::Relaxed);
    let cancelled = shared.cancellation_requested();
    let mut matches = match Arc::try_unwrap(shared) {
        Ok(state) => state
            .matches
            .into_inner()
            .map_err(|_| anyhow::anyhow!("text_search result collector is unavailable"))?,
        Err(state) => state
            .matches
            .lock()
            .map_err(|_| anyhow::anyhow!("text_search result collector is unavailable"))?
            .clone(),
    };
    for search_match in &mut matches {
        search_match.file = relative_display_path(&search_match.file, display_root.as_deref());
    }
    matches.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then_with(|| left.line.cmp(&right.line))
    });
    matches.truncate(max_results);

    let total_returned = matches.len();
    let limit_reached = max_results > 0 && total_returned >= max_results;
    let complete = !limit_reached
        && stats.files_skipped_large == 0
        && search_errors == 0
        && !cancelled
        && search_strategy != "refused_large_scope"
        && (!content_index_partial || grep_fallback_performed);
    let no_results = if explain_no_results && total_returned == 0 {
        Some(json!({
            "reason": no_results_reason(&stats),
            "mode": mode,
            "paths_received": stats.paths_received,
            "valid_paths": stats.valid_paths,
            "invalid_paths": stats.invalid_paths,
            "files_considered": stats.files_considered,
            "files_searched": stats.files_searched,
            "files_skipped_large": stats.files_skipped_large,
            "includes_applied": includes_applied,
            "excludes_applied": excludes_applied,
            "default_excludes_applied": default_excludes_applied,
            "default_excludes": default_exclude_globs.iter()
                .filter_map(|pattern| pattern.strip_suffix("/**"))
                .filter(|name| !name.contains('/'))
                .collect::<Vec<_>>(),
            "fallback_reason": fallback_reasons
        }))
    } else {
        None
    };

    let mut response = json!({
        "root": display_root.as_deref().map(normalize_path),
        "matches": matches,
        "total_returned": total_returned,
        "complete": complete,
        "limit_reached": limit_reached,
        "limit_reason": if limit_reached { Some("max_results") } else { None },
        "files_considered": stats.files_considered,
        "files_searched": stats.files_searched,
        "files_skipped_large": stats.files_skipped_large,
        "search_errors": search_errors,
        "duration_ms": duration_ms,
        "case_mode": case_mode,
        "search_strategy": search_strategy,
        "content_index_used": content_index_used,
        "content_index_partial": content_index_partial,
        "index_used": content_index_used,
        "index_complete": !content_index_partial,
        "content_index_zones": content_index_zones,
        "indexed_at": zone_indexed_at,
        "zone_indexed_at": zone_indexed_at,
        "index_age_secs": index_age_secs,
        "warming_zones": warming_zones,
        "fallback_reason": fallback_reasons,
        "grep_fallback_performed": grep_fallback_performed,
        "no_fallback_reason": no_fallback_reason,
        "unindexed_files_in_scope": unindexed_files_in_scope,
        "unindexed_files_in_scope_count": unindexed_files_count,
        "unindexed_files_complete": !unindexed_files_truncated,
        "unindexed_files_scope_complete": unindexed_files_scope_complete,
        "default_excludes_applied": default_excludes_applied,
        "include_ignored": include_ignored,
        "include_hidden": include_hidden
    });

    // Candidate stats only mean something when the content index was queried.
    if candidate_limit > 0 {
        insert_object_field(&mut response, "candidate_count", json!(candidate_count));
        insert_object_field(&mut response, "candidate_limit", json!(candidate_limit));
        insert_object_field(
            &mut response,
            "candidates_complete",
            json!(!candidates_truncated),
        );
    }
    if let Some(no_results) = no_results {
        insert_object_field(&mut response, "no_results", no_results);
    }
    if let Some(suggestion) = suggested_next_query(&input_paths, search_strategy) {
        insert_object_field(&mut response, "suggested_next_query", json!(suggestion));
    }
    let mut warnings = search_scope_warnings(&input_paths, search_strategy);
    if max_results > 0 && total_returned == 0 && !suppress_filter_hints {
        warnings.extend(filtered_scope_warnings_with_defaults(
            &input_paths,
            include_ignored,
            include_hidden,
            default_excludes_applied,
        ));
        warnings.sort();
        warnings.dedup();
    }
    if !warnings.is_empty() {
        insert_object_field(&mut response, "warnings", json!(warnings));
    }
    if cancelled {
        insert_object_field(&mut response, "cancelled", json!(true));
    }

    Ok(format_text_search_response(
        response,
        output_format,
        verbose,
    ))
}

fn cancelled_search_response(started_at: Instant) -> Value {
    json!({
        "root": Value::Null,
        "matches": [],
        "total_returned": 0,
        "complete": false,
        "limit_reached": false,
        "files_considered": 0,
        "files_searched": 0,
        "search_errors": 0,
        "duration_ms": started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        "cancelled": true
    })
}

fn format_text_search_response(
    response: Value,
    output_format: OutputFormat,
    verbose: bool,
) -> Value {
    match output_format {
        OutputFormat::Json => {
            let include_diagnostics = should_render_diagnostics(&response, verbose);
            let diagnostic_fields =
                if response.get("total_returned").and_then(Value::as_u64) == Some(0) {
                    &DIAGNOSTIC_FIELDS[1..]
                } else {
                    DIAGNOSTIC_FIELDS
                };
            super::nest_diagnostics(response, diagnostic_fields, include_diagnostics)
        }
        OutputFormat::Markdown => {
            json!({ "__mcp_raw_text": render_markdown_report(&response, verbose) })
        }
        OutputFormat::Compact => {
            json!({ "__mcp_raw_text": render_compact_report(&response, verbose) })
        }
    }
}

fn render_compact_report(response: &Value, verbose: bool) -> String {
    let mut lines = vec![
        format!(
            "root: {}",
            response.get("root").and_then(Value::as_str).unwrap_or(".")
        ),
        format!(
            "total_returned: {}",
            response
                .get("total_returned")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        ),
        format!(
            "complete: {}",
            response
                .get("complete")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        ),
    ];
    if response.get("total_returned").and_then(Value::as_u64) == Some(0) {
        lines.insert(
            2,
            format!(
                "files_searched: {}",
                response
                    .get("files_searched")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
        );
    }
    push_compact_matches(&mut lines, response.get("matches"));
    if should_render_diagnostics(response, verbose) {
        lines.push(format!(
            "diagnostics: {}",
            serde_json::to_string(&search_diagnostics(response)).unwrap_or_default()
        ));
    }
    lines.join("\n")
}

fn render_markdown_report(response: &Value, verbose: bool) -> String {
    let mut lines = vec![
        "# Text Search".to_string(),
        String::new(),
        format!(
            "- Root: `{}`",
            response.get("root").and_then(Value::as_str).unwrap_or(".")
        ),
        format!(
            "- Matches: {}",
            response
                .get("total_returned")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        ),
        format!(
            "- Complete: {}",
            response
                .get("complete")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        ),
    ];
    if response.get("total_returned").and_then(Value::as_u64) == Some(0) {
        lines.insert(
            4,
            format!(
                "- Files searched: {}",
                response
                    .get("files_searched")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
        );
    }
    push_markdown_matches(&mut lines, response.get("matches"));
    if should_render_diagnostics(response, verbose) {
        lines.extend([
            String::new(),
            "## Diagnostics".to_string(),
            String::new(),
            "```json".to_string(),
            serde_json::to_string(&search_diagnostics(response)).unwrap_or_default(),
            "```".to_string(),
        ]);
    }
    lines.join("\n")
}

fn push_compact_matches(lines: &mut Vec<String>, matches: Option<&Value>) {
    let mut current_file = None::<&str>;
    for item in matches.and_then(Value::as_array).into_iter().flatten() {
        let file = item.get("file").and_then(Value::as_str).unwrap_or("");
        if current_file != Some(file) {
            lines.push(format!("file: {}", file));
            current_file = Some(file);
        }
        lines.push(format_match_summary(item, false));
    }
}

fn push_markdown_matches(lines: &mut Vec<String>, matches: Option<&Value>) {
    let mut current_file = None::<&str>;
    for item in matches.and_then(Value::as_array).into_iter().flatten() {
        let file = item.get("file").and_then(Value::as_str).unwrap_or("");
        if current_file != Some(file) {
            lines.extend([String::new(), format!("## `{}`", file.replace('`', "\\`"))]);
            current_file = Some(file);
        }
        lines.push(format_match_summary(item, true));
    }
}

fn format_match_summary(item: &Value, markdown: bool) -> String {
    let line = item.get("line").and_then(Value::as_u64).unwrap_or(0);
    let column = item
        .get("match_column")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let text = item
        .get("line_text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace(['\r', '\n'], " ");
    if markdown {
        format!("- `{}:{}` {}", line, column, text)
    } else {
        format!("{}:{}: {}", line, column, text)
    }
}

fn should_render_diagnostics(response: &Value, verbose: bool) -> bool {
    verbose
        || !response
            .get("complete")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || response.get("no_results").is_some()
        || response
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| !warnings.is_empty())
}

fn search_diagnostics(response: &Value) -> Value {
    let keys = [
        "limit_reached",
        "limit_reason",
        "search_strategy",
        "index_used",
        "index_complete",
        "index_age_secs",
    ];
    let mut diagnostics = serde_json::Map::new();
    for key in keys.into_iter().chain(DIAGNOSTIC_FIELDS.iter().copied()) {
        if key == "files_searched"
            && response.get("total_returned").and_then(Value::as_u64) == Some(0)
        {
            continue;
        }
        if let Some(value) = response.get(key)
            && !value.is_null()
            && !matches!(value, Value::Array(items) if items.is_empty())
        {
            diagnostics.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(diagnostics)
}

fn relativize_display_paths(paths: &mut [String], root: Option<&Path>) {
    for path in paths {
        *path = relative_display_path(path, root);
    }
}

fn relative_display_path(path: &str, root: Option<&Path>) -> String {
    crate::common::display_path_relative_to(Path::new(path), root)
}

#[allow(clippy::too_many_arguments)]
fn search_index_candidate_paths(
    paths: Vec<PathBuf>,
    input_paths: &[PathBuf],
    includes: &[Pattern],
    excludes: &[Pattern],
    matcher: &RegexMatcher,
    context_lines: usize,
    max_results: usize,
    max_line_length: usize,
    shared: Arc<SharedSearchState>,
    stats: &mut SearchStats,
) {
    if paths.is_empty() {
        return;
    }

    let roots = input_paths
        .iter()
        .map(|path| canonicalize_existing_path(path))
        .collect::<Vec<_>>();

    let mut candidates = paths
        .into_iter()
        .filter(|path| path.is_file())
        .map(|path| {
            let canonical_path = canonicalize_existing_path(&path);
            let relative_path = relative_path_for_roots(&canonical_path, &roots);
            CandidateFile {
                path: canonical_path,
                relative_path,
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    candidates.dedup_by(|left, right| left.path == right.path);

    let worker_count = crate::common::bounded_walk_threads()
        .min(candidates.len().max(1))
        .max(1);
    let chunk_size = candidates.len().div_ceil(worker_count).max(1);
    thread::scope(|scope| {
        for chunk in candidates.chunks(chunk_size) {
            let shared = Arc::clone(&shared);
            scope.spawn(move || {
                let mut searcher = build_searcher(context_lines);
                for candidate in chunk {
                    search_candidate(
                        candidate.clone(),
                        includes,
                        excludes,
                        matcher,
                        &mut searcher,
                        search_collection_limit(max_results),
                        max_line_length,
                        true,
                        &shared,
                    );
                }
            });
        }
    });

    merge_shared_stats(&shared, stats);
}

#[allow(clippy::too_many_arguments)]
fn process_input_path(
    input_path: &Path,
    _include_globs: &[String],
    exclude_globs: &[String],
    includes: Arc<Vec<Pattern>>,
    excludes: Arc<Vec<Pattern>>,
    pattern: Arc<String>,
    case_sensitive: bool,
    context_lines: usize,
    max_results: usize,
    max_line_length: usize,
    dedup_candidates: bool,
    include_ignored: bool,
    include_hidden: bool,
    shared: Arc<SharedSearchState>,
    stats: &mut SearchStats,
) -> Result<()> {
    if !input_path.exists() {
        stats.invalid_paths.push(normalize_path(input_path));
        return Ok(());
    }
    if shared.cancellation_requested() {
        shared.stop.store(true, Ordering::Relaxed);
        return Ok(());
    }

    let canonical_path = canonicalize_existing_path(input_path);

    if canonical_path.is_file() {
        stats.valid_paths += 1;
        let relative_path = canonical_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_string())
            .unwrap_or_else(|| normalize_path(&canonical_path).to_string());
        let matcher = build_matcher(&pattern, case_sensitive)?;
        let mut searcher = build_searcher(context_lines);
        search_candidate(
            CandidateFile {
                path: canonical_path,
                relative_path,
            },
            includes.as_ref(),
            excludes.as_ref(),
            &matcher,
            &mut searcher,
            max_results,
            max_line_length,
            dedup_candidates,
            &shared,
        );
        merge_shared_stats(&shared, stats);
        return Ok(());
    }

    if !canonical_path.is_dir() {
        stats
            .invalid_paths
            .push(normalize_path(&canonical_path).to_string());
        return Ok(());
    }

    stats.valid_paths += 1;

    let mut walk = WalkBuilder::new(&canonical_path);
    configure_walk_filters(&mut walk, include_ignored, include_hidden);
    walk.threads(crate::common::bounded_walk_threads());
    // Includes are enforced by `passes_patterns` below. Applying them as
    // ignore overrides makes `includes: ["src"]` match only the directory
    // itself and silently drop descendants such as `src/lib.rs`.
    apply_walk_overrides(&mut walk, &canonical_path, &[], exclude_globs)?;
    let filter_root = canonical_path.clone();
    let filter_excludes = Arc::clone(&excludes);
    walk.filter_entry(move |entry| {
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
        let relative_path = entry
            .path()
            .strip_prefix(&filter_root)
            .ok()
            .map(normalize_path)
            .filter(|relative| !relative.is_empty())
            .unwrap_or_else(|| normalize_path(entry.path()));
        let candidate = CandidateFile {
            path: entry.path().to_path_buf(),
            relative_path,
        };
        !matches_excludes(&candidate, filter_excludes.as_ref())
    });

    walk.build_parallel().run(|| {
        let includes = Arc::clone(&includes);
        let excludes = Arc::clone(&excludes);
        let pattern = Arc::clone(&pattern);
        let shared = Arc::clone(&shared);
        let root = canonical_path.clone();
        let matcher = build_matcher(&pattern, case_sensitive).ok();
        let mut searcher = build_searcher(context_lines);

        Box::new(move |entry| {
            if shared.cancellation_requested() {
                shared.stop.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let Some(matcher) = matcher.as_ref() else {
                shared.search_errors.fetch_add(1, Ordering::Relaxed);
                shared.stop.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            };

            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    shared.search_errors.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                }
            };

            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                return WalkState::Continue;
            }

            let path = entry.path().to_path_buf();
            let relative_path = path
                .strip_prefix(&root)
                .ok()
                .map(normalize_path)
                .filter(|relative| !relative.is_empty())
                .unwrap_or_else(|| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name.to_string())
                        .unwrap_or_else(|| normalize_path(&path).to_string())
                });

            search_candidate(
                CandidateFile {
                    path,
                    relative_path,
                },
                includes.as_ref(),
                excludes.as_ref(),
                matcher,
                &mut searcher,
                max_results,
                max_line_length,
                dedup_candidates,
                &shared,
            );

            WalkState::Continue
        })
    });

    merge_shared_stats(&shared, stats);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn search_candidate(
    candidate: CandidateFile,
    includes: &[Pattern],
    excludes: &[Pattern],
    matcher: &RegexMatcher,
    searcher: &mut Searcher,
    max_results: usize,
    max_line_length: usize,
    dedup_candidates: bool,
    shared: &SharedSearchState,
) {
    if max_results == 0 || shared.stop.load(Ordering::Relaxed) || shared.cancellation_requested() {
        if shared.cancellation_requested() {
            shared.stop.store(true, Ordering::Relaxed);
        }
        return;
    }

    if !passes_patterns(&candidate, includes, excludes) {
        return;
    }

    if dedup_candidates {
        let mut seen = match shared.seen.lock() {
            Ok(guard) => guard,
            Err(_) => {
                shared.search_errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        if !seen.insert(candidate.path.clone()) {
            return;
        }
    }

    record_unindexed_file(&candidate.path, shared);

    let files_considered = shared
        .files_considered
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if files_considered.is_multiple_of(64)
        && let Some(cancellation) = shared.cancellation.as_ref()
    {
        cancellation.report_progress(files_considered, None);
    }

    if candidate.path.metadata().is_err() {
        shared.search_errors.fetch_add(1, Ordering::Relaxed);
        return;
    }

    shared.files_searched.fetch_add(1, Ordering::Relaxed);
    let candidate_path = candidate.path.clone();
    let display_path = normalize_path(&candidate_path);

    let mut decoded_searcher = match file_needs_windows_1252(&candidate_path) {
        Ok(true) => Some(build_windows_1252_searcher(searcher.before_context())),
        Ok(false) => None,
        Err(_) => {
            shared.search_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let active_searcher = decoded_searcher.as_mut().unwrap_or(searcher);

    let mut sink = MatchSink::new(
        matcher,
        max_results,
        max_line_length,
        active_searcher.before_context(),
        active_searcher.after_context(),
        shared.cancellation.clone(),
    );
    if active_searcher
        .search_path(matcher, &candidate_path, &mut sink)
        .is_err()
    {
        shared.search_errors.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let local_matches = sink.into_matches(&display_path);

    if local_matches.is_empty() {
        return;
    }

    let mut matches = match shared.matches.lock() {
        Ok(guard) => guard,
        Err(_) => {
            shared.search_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    for search_match in local_matches {
        if matches.len() >= max_results {
            break;
        }
        matches.push(search_match);
    }
}

fn record_unindexed_file(path: &Path, shared: &SharedSearchState) {
    if content_policy_allows_path(path) {
        return;
    }
    let Ok(mut seen) = shared.unindexed_seen.lock() else {
        shared.search_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if !seen.insert(path.to_path_buf()) {
        return;
    }
    shared.unindexed_files_count.fetch_add(1, Ordering::Relaxed);
    let Ok(mut files) = shared.unindexed_files.lock() else {
        shared.search_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if files.len() < MAX_UNINDEXED_FILES_REPORTED {
        files.push(crate::common::normalize_display_path(path));
    }
}

/// Collects matching lines together with the before/after context lines the
/// searcher reports. The `sinks::UTF8` convenience sink only receives matches,
/// so `context_lines` was silently ignored when it was used.
struct MatchSink<'matcher> {
    matcher: &'matcher RegexMatcher,
    max_results: usize,
    max_line_length: usize,
    before_context: usize,
    after_context: usize,
    matches: Vec<(u64, String, bool, usize)>,
    lines: BTreeMap<u64, String>,
    limit_reached: bool,
    trailing_after_lines: usize,
    cancellation: Option<CancellationToken>,
}

impl<'matcher> MatchSink<'matcher> {
    fn new(
        matcher: &'matcher RegexMatcher,
        max_results: usize,
        max_line_length: usize,
        before_context: usize,
        after_context: usize,
        cancellation: Option<CancellationToken>,
    ) -> Self {
        Self {
            matcher,
            max_results,
            max_line_length,
            before_context,
            after_context,
            matches: Vec::new(),
            lines: BTreeMap::new(),
            limit_reached: false,
            trailing_after_lines: 0,
            cancellation,
        }
    }

    fn into_matches(self, file: &str) -> Vec<SearchMatch> {
        let context = |range: std::ops::RangeInclusive<u64>| -> Vec<String> {
            range
                .filter_map(|line| self.lines.get(&line).cloned())
                .collect()
        };
        self.matches
            .iter()
            .map(
                |(line, line_text, line_truncated, match_column)| SearchMatch {
                    file: file.to_string(),
                    line: *line,
                    line_text: line_text.clone(),
                    match_column: *match_column,
                    line_truncated: *line_truncated,
                    context_before: if self.before_context == 0 {
                        Vec::new()
                    } else {
                        let first = line.saturating_sub(self.before_context as u64).max(1);
                        context(first..=line.saturating_sub(1))
                    },
                    context_after: if self.after_context == 0 {
                        Vec::new()
                    } else {
                        context(line + 1..=line + self.after_context as u64)
                    },
                },
            )
            .collect()
    }
}

impl Sink for MatchSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Ok(false);
        }
        if self.limit_reached {
            return Ok(false);
        }
        let line = mat.line_number().unwrap_or(0);
        let Some(rendered) = render_match_line(self.matcher, mat.bytes(), self.max_line_length)?
        else {
            return Ok(true);
        };
        let line_text = rendered.text;
        let line_truncated = rendered.line_truncated;
        self.lines.insert(line, line_text.clone());
        self.matches
            .push((line, line_text, line_truncated, rendered.match_column));
        if self.matches.len() >= self.max_results {
            self.limit_reached = true;
            // Keep reading only to collect the last match's after-context.
            return Ok(self.after_context > 0);
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Ok(false);
        }
        if let Some(line) = context.line_number() {
            let (text, _) = render_line_start(context.bytes(), self.max_line_length);
            self.lines.insert(line, text);
        }
        if self.limit_reached && *context.kind() == SinkContextKind::After {
            self.trailing_after_lines += 1;
            return Ok(self.trailing_after_lines < self.after_context);
        }
        Ok(true)
    }
}

fn search_collection_limit(max_results: usize) -> usize {
    if max_results == 0 { 0 } else { usize::MAX }
}

fn build_matcher(pattern: &str, case_sensitive: bool) -> Result<RegexMatcher> {
    RegexMatcherBuilder::new()
        .case_insensitive(!case_sensitive)
        .build(pattern)
        .map_err(Into::into)
}

fn build_searcher(context_lines: usize) -> Searcher {
    SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .before_context(context_lines)
        .after_context(context_lines)
        .build()
}

fn build_windows_1252_searcher(context_lines: usize) -> Searcher {
    SearcherBuilder::new()
        .encoding(Some(
            grep_searcher::Encoding::new("windows-1252").expect("valid encoding"),
        ))
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .before_context(context_lines)
        .after_context(context_lines)
        .build()
}

fn file_needs_windows_1252(path: &Path) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 8192];
    let mut pending = Vec::new();
    let mut first_chunk = true;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(!pending.is_empty());
        }
        if first_chunk {
            first_chunk = false;
            if buffer[..count].starts_with(&[0xef, 0xbb, 0xbf])
                || buffer[..count].starts_with(&[0xff, 0xfe])
                || buffer[..count].starts_with(&[0xfe, 0xff])
            {
                return Ok(false);
            }
        }
        if buffer[..count].contains(&0) {
            return Ok(false);
        }
        pending.extend_from_slice(&buffer[..count]);
        match std::str::from_utf8(&pending) {
            Ok(_) => pending.clear(),
            Err(error) if error.error_len().is_some() => return Ok(true),
            Err(error) => {
                pending.drain(..error.valid_up_to());
            }
        }
    }
}

fn merge_shared_stats(shared: &SharedSearchState, stats: &mut SearchStats) {
    stats.files_considered = shared.files_considered.load(Ordering::Relaxed);
    stats.files_searched = shared.files_searched.load(Ordering::Relaxed);
    stats.files_skipped_large = shared.files_skipped_large.load(Ordering::Relaxed);
}

fn parse_mode(raw: Option<&str>) -> Result<SearchMode> {
    match raw.unwrap_or("literal") {
        "literal" => Ok(SearchMode::Literal),
        "regex" => Ok(SearchMode::Regex),
        other => Err(anyhow::anyhow!("Unsupported mode '{}'", other)),
    }
}

fn parse_case_mode(raw: Option<&str>, legacy_case_sensitive: Option<bool>) -> Result<CaseMode> {
    if let Some(mode) = raw {
        return match mode {
            "insensitive" => Ok(CaseMode::Insensitive),
            "sensitive" => Ok(CaseMode::Sensitive),
            "smart" => Ok(CaseMode::Smart),
            other => Err(anyhow::anyhow!("Unsupported case_mode '{}'", other)),
        };
    }

    Ok(match legacy_case_sensitive {
        Some(true) => CaseMode::Sensitive,
        _ => CaseMode::Insensitive,
    })
}

fn resolve_case_sensitive(case_mode: CaseMode, query: &str) -> bool {
    match case_mode {
        CaseMode::Insensitive => false,
        CaseMode::Sensitive => true,
        CaseMode::Smart => query.chars().any(|c| c.is_uppercase()),
    }
}

fn parse_usize_arg(args: &Value, name: &str, default: usize, min: usize, max: usize) -> usize {
    args.get(name)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn passes_patterns(candidate: &CandidateFile, includes: &[Pattern], excludes: &[Pattern]) -> bool {
    path_passes_patterns(
        &candidate.path,
        &candidate.relative_path,
        includes,
        excludes,
    )
}

fn matches_excludes(candidate: &CandidateFile, excludes: &[Pattern]) -> bool {
    matches_patterns_or_ancestors(&candidate.path, &candidate.relative_path, excludes)
}

fn no_results_reason(stats: &SearchStats) -> &'static str {
    if stats.valid_paths == 0 {
        "no_valid_paths"
    } else if stats.files_searched == 0 {
        "no_candidate_files"
    } else {
        "no_match_found"
    }
}

fn record_input_path_validity(input_paths: &[PathBuf], stats: &mut SearchStats) {
    for input_path in input_paths {
        if input_path.exists() {
            stats.valid_paths += 1;
        } else {
            stats.invalid_paths.push(normalize_path(input_path));
        }
    }
}

fn search_scope_warnings(input_paths: &[PathBuf], search_strategy: &str) -> Vec<String> {
    if search_strategy == "tantivy" {
        return Vec::new();
    }

    let mut warnings = Vec::new();
    for input_path in input_paths {
        let canonical_path = canonicalize_existing_path(input_path);
        if !canonical_path.is_dir() {
            continue;
        }

        let Some(indexed_root) = crate::indexer::indexed_workspace_root_for_path(&canonical_path)
        else {
            continue;
        };
        if indexed_root == canonical_path
            && indexed_file_count_is_large(crate::indexer::indexed_workspace_file_count(
                &indexed_root,
            ))
        {
            warnings.push(format!(
                "Search used {} at indexed workspace root '{}'. For large repos, retry with a narrower paths value (for example a component directory), use a literal query when possible, or set allow_expensive_fallback=true only when a full grep scan is intentional.",
                search_strategy,
                normalize_path(&canonical_path)
            ));
        }
    }

    warnings.sort();
    warnings.dedup();
    warnings
}

fn plan_grep_fallback(input_paths: &[PathBuf], allow_expensive_fallback: bool) -> FallbackPlan {
    if allow_expensive_fallback {
        return FallbackPlan {
            allow_grep: true,
            reason: None,
        };
    }

    for input_path in input_paths {
        let canonical_path = canonicalize_existing_path(input_path);
        if !canonical_path.is_dir() {
            continue;
        }

        let Some(indexed_root) = crate::indexer::indexed_workspace_root_for_path(&canonical_path)
        else {
            continue;
        };
        if indexed_root == canonical_path
            && indexed_file_count_is_large(crate::indexer::indexed_workspace_file_count(
                &indexed_root,
            ))
        {
            return FallbackPlan {
                allow_grep: false,
                reason: Some("large_scope_requires_explicit_fallback"),
            };
        }
    }

    FallbackPlan {
        allow_grep: true,
        reason: None,
    }
}

fn indexed_file_count_is_large(indexed_files: Option<usize>) -> bool {
    indexed_files.is_some_and(|count| count > crate::indexer::LARGE_WORKSPACE_FILE_THRESHOLD)
}

pub(super) fn default_fallback_excludes(
    input_paths: &[PathBuf],
    user_excludes: &[String],
) -> Vec<String> {
    if input_paths
        .iter()
        .any(|path| is_direct_vendor_or_generated_scope(path.as_path()))
    {
        return Vec::new();
    }

    default_generated_vendor_globs(input_paths)
        .into_iter()
        .filter(|pattern| !user_excludes.iter().any(|existing| existing == pattern))
        .collect()
}

fn suggested_next_query(input_paths: &[PathBuf], search_strategy: &str) -> Option<String> {
    if search_strategy != "refused_large_scope" {
        return None;
    }

    input_paths.first().map(|path| {
        format!(
            "Retry with a narrower paths value under '{}' or set allow_expensive_fallback=true for an intentional full grep scan.",
            normalize_path(path)
        )
    })
}

fn relative_path_for_roots(path: &Path, roots: &[PathBuf]) -> String {
    for root in roots {
        let target_root = if root.is_file() {
            root.parent().unwrap_or(root)
        } else {
            root.as_path()
        };
        if let Ok(relative) = path.strip_prefix(target_root) {
            let normalized = normalize_path(relative);
            if !normalized.is_empty() {
                return normalized;
            }
        }
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .unwrap_or_else(|| normalize_path(path).to_string())
}

fn canonicalize_existing_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn normalize_path(path: &Path) -> String {
    crate::common::normalize_display_path(path)
}

#[cfg(test)]
mod tests {
    use super::indexed_file_count_is_large;

    #[test]
    fn root_grep_is_refused_only_above_large_workspace_threshold() {
        assert!(!indexed_file_count_is_large(None));
        assert!(!indexed_file_count_is_large(Some(50_000)));
        assert!(indexed_file_count_is_large(Some(50_001)));
    }
}

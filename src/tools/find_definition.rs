use anyhow::{Context, Result};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::task;

use crate::limits::{DEFAULT_AST_FILE_SIZE_BYTES, MAX_SKIPPED_FILE_DETAILS};
use crate::tools::{
    ast_support::{
        find_symbol_candidates, parse_supported_file, symbol_basename,
        visit_candidate_code_files_with_stats_and_options as visit_code_files,
    },
    path_filters::filtered_scope_warnings,
    search_snippet::{LossyMatchSink, render_match_range},
};

const MAX_FILE_SIZE_BYTES: u64 = DEFAULT_AST_FILE_SIZE_BYTES;
const MAX_RESULTS: usize = 20;
const DEFAULT_MAX_LINE_LENGTH: usize = 240;
const MAX_LINE_LENGTH: usize = 4_000;

pub fn schema() -> Value {
    json!({
        "name": "find_definition",
        "title": "Find definition",
        "description": "Find symbol definitions with Tree-sitter for Rust, JavaScript/TypeScript, Python, C, C++, Go, Java, C#, PHP, Ruby, Swift, and Objective-C. Unsupported code extensions use a case-sensitive regex fallback labeled as heuristic.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Case-sensitive symbol name to locate, such as a function, method, type, class, or qualified name." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Search roots or files. Defaults to the active workspace. Scope this for large repositories." },
                "max_line_length": { "type": "integer", "minimum": 1, "maximum": 4000, "description": "Maximum snippet characters before ellipsis markers. Defaults to 240." },
                "include_ignored": { "type": "boolean", "description": "Include files filtered by ignore rules. Uses a filesystem walk instead of the path index." },
                "include_hidden": { "type": "boolean", "description": "Include hidden files and directories except VCS metadata directories. Uses a filesystem walk instead of the path index." },
                "verbose": { "type": "boolean", "description": "Include detailed scan diagnostics even when the result is complete. Defaults to false." }
            },
            "required": ["symbol"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("find_definition background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .context("Missing symbol")?;

    let search_paths: Vec<PathBuf> =
        if let Some(paths) = args.get("paths").and_then(|v| v.as_array()) {
            paths
                .iter()
                .filter_map(|path| path.as_str())
                .map(crate::common::resolve_existing_tool_path)
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![crate::common::default_tool_root()]
        };

    if !search_paths.iter().any(|path| path.exists()) {
        return Err(anyhow::anyhow!(
            "No valid search path found for find_definition"
        ));
    }
    let display_root = crate::common::common_path_root(&search_paths);
    let cancellation = crate::cancellation::token_for_scan(&args, &search_paths);
    let max_line_length = args
        .get("max_line_length")
        .and_then(Value::as_u64)
        .map(|value| (value as usize).clamp(1, MAX_LINE_LENGTH))
        .unwrap_or(DEFAULT_MAX_LINE_LENGTH);
    let verbose = args
        .get("verbose")
        .and_then(Value::as_bool)
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

    let escaped_symbol = regex::escape(symbol_basename(symbol));
    let pattern_str = format!(
        r"(?:\b(?i:fn|pub\s+fn|def|class|struct|enum|trait|interface|protocol|actor|extension|type|function|func|const|let|var|void|int|bool|auto|static)\s+{escaped_symbol}\b|@(?i:interface|implementation|protocol)\s+{escaped_symbol}\b)"
    );

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(false)
        .build(&pattern_str)
        .context("Invalid regex pattern")?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();

    let mut definitions = Vec::new();
    let mut files_scanned = 0usize;
    let mut files_with_read_errors = 0usize;
    let mut files_skipped_large = 0usize;
    let mut skipped_large_files = Vec::new();
    let mut limit_reached = false;
    let mut ast_files_scanned = 0usize;
    let mut heuristic_files_scanned = 0usize;

    let visit_stats = visit_code_files(
        &search_paths,
        None,
        None,
        include_ignored,
        include_hidden,
        |candidate| {
            if limit_reached {
                return Ok(false);
            }

            let meta = match std::fs::metadata(candidate) {
                Ok(meta) => meta,
                Err(_) => {
                    files_with_read_errors += 1;
                    return Ok(true);
                }
            };

            if meta.len() > MAX_FILE_SIZE_BYTES {
                files_skipped_large += 1;
                if skipped_large_files.len() < MAX_SKIPPED_FILE_DETAILS {
                    skipped_large_files.push(json!({
                    "path": crate::common::display_path_relative_to(candidate, display_root.as_deref()),
                    "size_bytes": meta.len(),
                    "limit_bytes": MAX_FILE_SIZE_BYTES
                }));
                }
                return Ok(true);
            }

            files_scanned += 1;
            if crate::cancellation::report_scan_progress(cancellation.as_ref(), files_scanned) {
                return Ok(false);
            }

            let path_str =
                crate::common::display_path_relative_to(candidate, display_root.as_deref());
            if definitions.len() >= MAX_RESULTS {
                limit_reached = true;
                return Ok(false);
            }

            match parse_supported_file(candidate, MAX_FILE_SIZE_BYTES, None) {
                Ok(Some(parsed)) => {
                    ast_files_scanned += 1;
                    for matched in find_symbol_candidates(
                        parsed.tree.root_node(),
                        &parsed.source,
                        symbol,
                        None,
                    ) {
                        let declaration_start = matched.node.start_byte();
                        let line_start = parsed.source[..declaration_start]
                            .iter()
                            .rposition(|byte| *byte == b'\n')
                            .map(|offset| offset + 1)
                            .unwrap_or(0);
                        let line_end = parsed.source[declaration_start..]
                            .iter()
                            .position(|byte| *byte == b'\n')
                            .map(|offset| declaration_start + offset)
                            .unwrap_or(parsed.source.len());
                        let line_bytes = &parsed.source[line_start..line_end];
                        let relative_declaration_start = declaration_start - line_start;
                        let match_start = line_bytes[relative_declaration_start..]
                            .windows(matched.name.len())
                            .position(|window| window == matched.name.as_bytes())
                            .map(|offset| relative_declaration_start + offset)
                            .unwrap_or(relative_declaration_start);
                        let rendered = render_match_range(
                            line_bytes,
                            match_start,
                            match_start + matched.name.len(),
                            max_line_length,
                        );
                        definitions.push(json!({
                            "path": path_str,
                            "line": matched.node.start_position().row + 1,
                            "name": matched.name,
                            "qualified_name": matched.qualified_name,
                            "kind": matched.node.kind(),
                            "language": parsed.language_name,
                            "snippet": rendered.text,
                            "line_truncated": rendered.line_truncated,
                            "match_column": rendered.match_column,
                            "resolution": "ast"
                        }));
                        if definitions.len() >= MAX_RESULTS {
                            limit_reached = true;
                            break;
                        }
                    }
                }
                Ok(None) => {
                    heuristic_files_scanned += 1;
                    let mut sink = LossyMatchSink::new(
                        &matcher,
                        MAX_RESULTS - definitions.len(),
                        max_line_length,
                    );
                    if searcher
                        .search_path(&matcher, candidate, &mut sink)
                        .is_err()
                    {
                        files_with_read_errors += 1;
                        return Ok(true);
                    }
                    for matched in sink.into_matches() {
                        definitions.push(json!({
                            "path": path_str,
                            "line": matched.line,
                            "snippet": matched.text,
                            "line_truncated": matched.line_truncated,
                            "match_column": matched.match_column,
                            "resolution": "heuristic"
                        }));
                    }
                }
                Err(_) => {
                    files_with_read_errors += 1;
                    return Ok(true);
                }
            }
            limit_reached = definitions.len() >= MAX_RESULTS;

            Ok(!limit_reached)
        },
    )?;
    crate::cancellation::finish_scan_progress(cancellation.as_ref(), files_scanned);

    let skipped_large_files_omitted = files_skipped_large.saturating_sub(skipped_large_files.len());
    let cancelled = cancellation
        .as_ref()
        .is_some_and(|token| token.is_cancelled());
    let complete =
        !limit_reached && !cancelled && files_with_read_errors == 0 && files_skipped_large == 0;
    let indexed_at = crate::indexer::path_index_ages_for_paths(&search_paths);
    let index_used = visit_stats.indexed_roots_used > 0;
    let index_complete =
        index_used && !indexed_at.is_empty() && indexed_at.iter().all(|item| item.complete);
    let index_age_secs = indexed_at
        .iter()
        .filter_map(|item| item.index_age_secs)
        .max();
    let warnings = if definitions.is_empty() && !suppress_filter_hints {
        filtered_scope_warnings(&search_paths, include_ignored, include_hidden)
    } else {
        Vec::new()
    };
    let mut response = json!({
        "symbol": symbol,
        "root": display_root.as_deref().map(crate::common::normalize_display_path),
        "definitions": definitions,
        "total_returned": definitions.len(),
        "complete": complete,
        "search_strategy": visit_stats.search_strategy(),
        "index_used": index_used,
        "index_complete": index_complete,
        "index_age_secs": index_age_secs,
        "indexed_at": indexed_at,
        "direct_files_considered": visit_stats.direct_files_considered,
        "indexed_roots_used": visit_stats.indexed_roots_used,
        "filesystem_roots_walked": visit_stats.filesystem_roots_walked,
        "include_ignored": include_ignored,
        "include_hidden": include_hidden,
        "files_searched": files_scanned,
        "files_skipped_non_code": 0,
        "files_with_read_errors": files_with_read_errors,
        "files_skipped_large": files_skipped_large,
        "skipped_large_files": skipped_large_files,
        "skipped_large_files_omitted": skipped_large_files_omitted,
        "ast_files_scanned": ast_files_scanned,
        "heuristic_files_scanned": heuristic_files_scanned,
        "cancelled": cancelled,
        "limit_reached": limit_reached,
        "limit_reason": if limit_reached { Some("max_results") } else { None }
    });
    if !warnings.is_empty() {
        crate::common::insert_object_field(&mut response, "warnings", json!(warnings));
    }
    let diagnostic_fields = &[
        "files_searched",
        "files_skipped_non_code",
        "files_with_read_errors",
        "files_skipped_large",
        "skipped_large_files",
        "skipped_large_files_omitted",
        "ast_files_scanned",
        "heuristic_files_scanned",
        "search_strategy",
        "index_used",
        "index_complete",
        "index_age_secs",
        "indexed_at",
        "direct_files_considered",
        "indexed_roots_used",
        "filesystem_roots_walked",
        "include_ignored",
        "include_hidden",
        "cancelled",
    ];
    Ok(super::nest_diagnostics(
        response,
        if definitions.is_empty() {
            &diagnostic_fields[1..]
        } else {
            diagnostic_fields
        },
        verbose || !complete,
    ))
}

use anyhow::{Context, Result};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::task;

use crate::limits::{FIND_REFERENCES_FILE_SIZE_BYTES, MAX_SKIPPED_FILE_DETAILS};
use crate::tools::{
    ast_support::{
        classify_reference_match, parse_supported_file, symbol_segments,
        visit_candidate_code_files_with_options,
    },
    path_filters::filtered_scope_warnings,
    search_snippet::LossyMatchSink,
};

const MAX_FILE_SIZE_BYTES: u64 = FIND_REFERENCES_FILE_SIZE_BYTES;
const MAX_RESULTS: usize = 200;
const DEFAULT_MAX_LINE_LENGTH: usize = 240;
const MAX_LINE_LENGTH: usize = 4_000;

pub fn schema() -> Value {
    json!({
        "name": "find_references",
        "title": "Find references",
        "description": "Find token matches for a symbol across code files. Rust, JavaScript/TypeScript, Python, C, C++, Go, Java, C#, PHP, Ruby, Swift, and Objective-C matches are classified with Tree-sitter as code, comment, or string; unsupported code extensions use an explicitly labeled text fallback, not semantic reference resolution.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol text/name to search for token references. AST-supported languages classify each textual match but do not perform type or binding resolution." },
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
        .context("find_references background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .context("Missing/empty symbol")?;

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
            "No valid search path found for find_references"
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

    let segments = symbol_segments(symbol);
    let reference = if segments.len() > 1 {
        segments
            .iter()
            .map(|segment| regex::escape(segment))
            .collect::<Vec<_>>()
            .join(r"(?:::|\.)")
    } else {
        regex::escape(symbol)
    };
    let pattern_str = format!(r"\b{reference}\b");
    let matcher = RegexMatcherBuilder::new()
        .build(&pattern_str)
        .context("Invalid regex pattern")?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();

    let mut references = Vec::new();
    let mut files_scanned = 0usize;
    let mut files_with_read_errors = 0usize;
    let mut files_skipped_large = 0usize;
    let mut skipped_large_files = Vec::new();
    let mut limit_reached = false;
    let mut ast_classified_files = 0usize;
    let mut text_fallback_files = 0usize;

    visit_candidate_code_files_with_options(
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
            let parsed = match parse_supported_file(candidate, MAX_FILE_SIZE_BYTES, None) {
                Ok(parsed) => parsed,
                Err(_) => {
                    files_with_read_errors += 1;
                    return Ok(true);
                }
            };
            if parsed.is_some() {
                ast_classified_files += 1;
            } else {
                text_fallback_files += 1;
            }
            let mut sink =
                LossyMatchSink::new(&matcher, MAX_RESULTS - references.len(), max_line_length);
            let search_result = searcher.search_path(&matcher, candidate, &mut sink);

            if search_result.is_err() {
                files_with_read_errors += 1;
                return Ok(true);
            }

            for matched in sink.into_matches() {
                let (match_kind, classification, language) = match parsed.as_ref() {
                    Some(parsed) => (
                        classify_reference_match(
                            parsed.tree.root_node(),
                            matched.absolute_byte_offset as usize,
                        ),
                        "ast",
                        Some(parsed.language_name),
                    ),
                    None => ("unknown", "text_fallback", None),
                };
                let mut reference = json!({
                    "path": path_str,
                    "line": matched.line,
                    "snippet": matched.text,
                    "match_column": matched.match_column,
                    "language": language
                });
                if matched.line_truncated {
                    crate::common::insert_object_field(
                        &mut reference,
                        "line_truncated",
                        Value::Bool(true),
                    );
                }
                if match_kind != "code" {
                    crate::common::insert_object_field(
                        &mut reference,
                        "match_kind",
                        json!(match_kind),
                    );
                }
                if classification != "ast" {
                    crate::common::insert_object_field(
                        &mut reference,
                        "classification",
                        json!(classification),
                    );
                }
                references.push(reference);
            }
            limit_reached = references.len() >= MAX_RESULTS;

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
    let warnings = if references.is_empty() {
        filtered_scope_warnings(&search_paths, include_ignored, include_hidden)
    } else {
        Vec::new()
    };
    let mut response = json!({
        "symbol": symbol,
        "root": display_root.as_deref().map(crate::common::normalize_display_path),
        "references": references,
        "reference_defaults": {
            "classification": "ast",
            "match_kind": "code",
            "line_truncated": false
        },
        "total_returned": references.len(),
        "complete": complete,
        "files_searched": files_scanned,
        "files_skipped_non_code": 0,
        "files_with_read_errors": files_with_read_errors,
        "files_skipped_large": files_skipped_large,
        "skipped_large_files": skipped_large_files,
        "skipped_large_files_omitted": skipped_large_files_omitted,
        "ast_classified_files": ast_classified_files,
        "text_fallback_files": text_fallback_files,
        "include_ignored": include_ignored,
        "include_hidden": include_hidden,
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
        "ast_classified_files",
        "text_fallback_files",
        "include_ignored",
        "include_hidden",
        "cancelled",
    ];
    Ok(super::nest_diagnostics(
        response,
        if references.is_empty() {
            &diagnostic_fields[1..]
        } else {
            diagnostic_fields
        },
        verbose || !complete,
    ))
}

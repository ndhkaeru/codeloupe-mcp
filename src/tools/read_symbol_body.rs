use anyhow::{Context, Result};
use regex::Regex;
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use tokio::task;

use crate::limits::MAX_IN_MEMORY_TEXT_FILE_BYTES;
use crate::tools::ast_support::{
    DEFAULT_AST_FILE_SIZE_LIMIT, detect_language, find_symbol_candidates, parse_language_filter,
    parse_supported_file, visit_candidate_code_files,
};
use crate::tools::read_file::decode_fuzzy;

const READ_FILE_SIZE_LIMIT: u64 = MAX_IN_MEMORY_TEXT_FILE_BYTES;
const HEURISTIC_WINDOW_LINES: usize = 80;

pub fn schema() -> Value {
    json!({
        "name": "read_symbol_body",
        "title": "Read symbol body",
        "description": "Read one symbol body with AST-first resolution, then heuristic fallback for other code-like files. Use when a function/type name is known and you need focused implementation context.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol name to resolve, such as a function, method, class, type, or qualified name." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Search roots or files for symbol resolution. Defaults to the active workspace. Use this to scope large repositories." },
                "file_hint": { "type": "string", "description": "Preferred file to check first. It narrows and prioritizes resolution but does not replace paths." },
                "language": { "type": "string", "description": "Optional language filter. Accepted values include rust/rs, python/py, javascript/js/jsx/typescript/ts/tsx, c, cpp/c++, go, java, csharp/c#/cs, php, ruby/rb, swift, objc/objective-c." },
                "include_signature": { "type": "boolean", "description": "Include the symbol signature/header when true. Defaults to true. AST parsing skips files larger than 2 MB." },
                "line": { "type": "integer", "minimum": 1, "description": "Optional 1-based declaration line used to select one candidate when names are duplicated." }
            },
            "required": ["symbol"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("read_symbol_body background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .context("Missing symbol")?;
    let include_signature = args
        .get("include_signature")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let line = args
        .get("line")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    if line == Some(0) {
        return Err(anyhow::anyhow!("line must be >= 1"));
    }
    let language_filter = parse_language_filter(args.get("language").and_then(|v| v.as_str()))?;

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

    let file_hint = args
        .get("file_hint")
        .and_then(|v| v.as_str())
        .map(|raw| resolve_file_hint(raw, &search_paths))
        .transpose()?;

    let mut ast_matches = Vec::new();
    visit_candidate_code_files(
        &search_paths,
        file_hint.as_deref(),
        language_filter,
        |candidate| {
            if detect_language(candidate).is_none() {
                return Ok(true);
            }
            for ast_match in try_ast_matches(candidate, symbol, line, include_signature)? {
                ast_matches.push(AstSymbolMatch {
                    path: crate::common::normalize_display_path(candidate),
                    body: ast_match,
                });
            }

            Ok(true)
        },
    )?;

    if let Some(file_hint) = &file_hint {
        let hinted_file = crate::common::normalize_display_path(file_hint);
        if ast_matches
            .iter()
            .any(|matched| matched.path == hinted_file)
        {
            ast_matches.retain(|matched| matched.path == hinted_file);
        }
    }

    if ast_matches.len() > 1 {
        let candidates = ast_matches
            .iter()
            .map(ast_candidate_payload)
            .collect::<Vec<_>>();
        return Ok(json!({
            "symbol": symbol,
            "ambiguous": true,
            "match_source": "ast",
            "total_candidates": candidates.len(),
            "candidates": candidates
        }));
    }

    if let Some(ast_match) = ast_matches.pop() {
        return Ok(json!({
            "symbol": symbol,
            "qualified_name": ast_match.body.qualified_name,
            "name": ast_match.body.name,
            "path": ast_match.path,
            "start_line": ast_match.body.start_line,
            "end_line": ast_match.body.end_line,
            "content": ast_match.body.content,
            "match_source": "ast",
            "confidence": "high"
        }));
    }

    let definition_pattern = Regex::new(&format!(
        r"(?i)\b(fn|pub\s+fn|func|def|class|struct|enum|trait|interface|type|function|const|let|var|void|int|bool|auto|static)\s+{}\b",
        regex::escape(crate::tools::ast_support::symbol_basename(symbol))
    ))
    .context("Invalid heuristic definition regex")?;

    let mut heuristic_result = None;
    visit_candidate_code_files(
        &search_paths,
        file_hint.as_deref(),
        language_filter,
        |candidate| {
            if let Some(heuristic_match) =
                try_heuristic_match(candidate, &definition_pattern, include_signature)?
            {
                heuristic_result = Some(json!({
                    "symbol": symbol,
                    "path": crate::common::normalize_display_path(candidate),
                    "start_line": heuristic_match.start_line,
                    "end_line": heuristic_match.end_line,
                    "content": heuristic_match.content,
                    "match_source": "heuristic",
                    "confidence": heuristic_match.confidence
                }));
                return Ok(false);
            }

            Ok(true)
        },
    )?;

    if let Some(heuristic_result) = heuristic_result {
        return Ok(heuristic_result);
    }

    Err(anyhow::anyhow!("Could not resolve symbol '{}'", symbol))
}

fn resolve_file_hint(raw: &str, search_paths: &[PathBuf]) -> Result<PathBuf> {
    let input = crate::common::path_from_input(raw);
    let mut candidates = Vec::new();

    if input.is_absolute() {
        push_file_hint_candidate(&mut candidates, input.clone());
    } else {
        for search_path in search_paths {
            let candidate = if search_path.is_file() {
                if search_path.ends_with(&input) {
                    search_path.clone()
                } else {
                    continue;
                }
            } else {
                search_path.join(&input)
            };
            push_file_hint_candidate(&mut candidates, candidate);
        }

        if candidates.is_empty() {
            push_file_hint_candidate(&mut candidates, crate::common::resolve_tool_path(raw));
        }
    }

    match candidates.as_slice() {
        [candidate] => Ok(candidate.clone()),
        [] => Err(anyhow::anyhow!(
            "file_hint is not a valid file: {}",
            crate::common::normalize_display_path(&input)
        )),
        _ => Err(anyhow::anyhow!(
            "file_hint is ambiguous across search paths: {}",
            candidates
                .iter()
                .map(|candidate| crate::common::normalize_display_path(candidate))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn push_file_hint_candidate(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    let candidate = crate::common::canonicalize_if_exists(candidate);
    if !candidate.is_file() || candidates.iter().any(|existing| existing == &candidate) {
        return;
    }
    candidates.push(candidate);
}

struct SymbolBody {
    content: String,
    start_line: usize,
    end_line: usize,
    declaration_start_line: usize,
    declaration_end_line: usize,
    name: String,
    qualified_name: String,
}

struct AstSymbolMatch {
    path: String,
    body: SymbolBody,
}

struct HeuristicBody {
    content: String,
    start_line: usize,
    end_line: usize,
    confidence: &'static str,
}

fn try_ast_matches(
    path: &Path,
    symbol: &str,
    line: Option<usize>,
    include_signature: bool,
) -> Result<Vec<SymbolBody>> {
    let parsed = match parse_supported_file(path, DEFAULT_AST_FILE_SIZE_LIMIT, None)? {
        Some(parsed) => parsed,
        None => return Ok(Vec::new()),
    };

    Ok(
        find_symbol_candidates(parsed.tree.root_node(), &parsed.source, symbol, line)
            .into_iter()
            .map(|candidate| {
                let content_node = if include_signature {
                    candidate.node
                } else {
                    candidate
                        .node
                        .child_by_field_name("body")
                        .unwrap_or(candidate.node)
                };
                SymbolBody {
                    content: String::from_utf8_lossy(&parsed.source[content_node.byte_range()])
                        .to_string(),
                    start_line: content_node.start_position().row + 1,
                    end_line: content_node.end_position().row + 1,
                    declaration_start_line: candidate.node.start_position().row + 1,
                    declaration_end_line: candidate.node.end_position().row + 1,
                    name: candidate.name,
                    qualified_name: candidate.qualified_name,
                }
            })
            .collect(),
    )
}

fn ast_candidate_payload(candidate: &AstSymbolMatch) -> Value {
    json!({
        "path": candidate.path,
        "name": candidate.body.name,
        "qualified_name": candidate.body.qualified_name,
        "start_line": candidate.body.declaration_start_line,
        "end_line": candidate.body.declaration_end_line
    })
}

fn try_heuristic_match(
    path: &Path,
    definition_pattern: &Regex,
    include_signature: bool,
) -> Result<Option<HeuristicBody>> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    if meta.len() > READ_FILE_SIZE_LIMIT {
        return Ok(None);
    }

    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    let (content, _) = decode_fuzzy(&buffer);
    let lines: Vec<&str> = content.split('\n').collect();

    for (line_index, line) in lines.iter().enumerate() {
        if definition_pattern.is_match(line) {
            return Ok(Some(extract_heuristic_body(
                &lines,
                line_index,
                include_signature,
            )));
        }
    }

    Ok(None)
}

fn extract_heuristic_body(
    lines: &[&str],
    definition_index: usize,
    include_signature: bool,
) -> HeuristicBody {
    let definition_line = lines.get(definition_index).copied().unwrap_or("");
    let definition_indent = indentation_width(definition_line);
    let (body_start, body_end, confidence) =
        if let Some(end_line) = find_brace_delimited_end(lines, definition_index) {
            (definition_index, end_line, "medium")
        } else if let Some(end_line) =
            find_indentation_delimited_end(lines, definition_index, definition_indent)
        {
            (definition_index, end_line, "medium")
        } else {
            (
                definition_index,
                std::cmp::min(lines.len(), definition_index + HEURISTIC_WINDOW_LINES),
                "low",
            )
        };

    let mut content_start = if include_signature {
        body_start
    } else {
        std::cmp::min(body_start + 1, body_end)
    };
    if content_start == body_end {
        content_start = body_start;
    }

    let content = lines[content_start..body_end].join("\n");
    HeuristicBody {
        content,
        start_line: content_start + 1,
        end_line: body_end,
        confidence,
    }
}

fn find_brace_delimited_end(lines: &[&str], definition_index: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut seen_open = false;

    for (offset, line) in lines.iter().enumerate().skip(definition_index) {
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    seen_open = true;
                }
                '}' if seen_open => {
                    depth -= 1;
                    if depth <= 0 {
                        return Some(offset + 1);
                    }
                }
                _ => {}
            }
        }
    }

    None
}

fn find_indentation_delimited_end(
    lines: &[&str],
    definition_index: usize,
    definition_indent: usize,
) -> Option<usize> {
    let mut first_body_line: Option<usize> = None;

    for (index, line) in lines.iter().enumerate().skip(definition_index + 1) {
        if line.trim().is_empty() {
            continue;
        }

        let indent = indentation_width(line);
        if first_body_line.is_none() {
            if indent <= definition_indent {
                return None;
            }
            first_body_line = Some(index);
            continue;
        }

        if indent <= definition_indent {
            return Some(index);
        }
    }

    first_body_line.map(|_| lines.len())
}

fn indentation_width(line: &str) -> usize {
    line.chars().take_while(|ch| ch.is_whitespace()).count()
}

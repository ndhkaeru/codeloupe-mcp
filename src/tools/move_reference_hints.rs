use super::path_filters::{compile_patterns, parse_pattern_strings};
use super::tool_errors::error;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(crate) struct Request {
    paths: Vec<PathBuf>,
    includes: Vec<String>,
    excludes: Vec<String>,
    max_results: usize,
}

pub(crate) fn schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional pre-move lexical hints for old filename/module mentions. Requires an explicit search scope. Read-only; includes comments/strings and may have false positives. Uses text_search defaults and never enables expensive root fallback.",
        "properties": {
            "paths": {
                "type": "array", "minItems": 1, "maxItems": 8,
                "items": {"type": "string"}
            },
            "includes": {
                "type": "array", "maxItems": 16,
                "items": {"type": "string"}
            },
            "excludes": {
                "type": "array", "maxItems": 16,
                "items": {"type": "string"}
            },
            "max_results": {"type": "integer", "minimum": 1, "maximum": 100}
        },
        "required": ["paths"],
        "additionalProperties": false
    })
}

pub(crate) fn parse(args: &Value) -> Result<Option<Request>, Value> {
    let Some(raw) = args.get("reference_hints") else {
        return Ok(None);
    };
    let invalid = |message: &str| error("invalid_reference_hints", message);
    let Some(object) = raw.as_object() else {
        return Err(invalid("reference_hints must be an object"));
    };
    if object
        .keys()
        .any(|key| !["paths", "includes", "excludes", "max_results"].contains(&key.as_str()))
    {
        return Err(invalid("unknown reference_hints option"));
    }
    let Some(paths) = raw.get("paths").and_then(Value::as_array) else {
        return Err(invalid(
            "reference_hints.paths must contain an explicit search scope",
        ));
    };
    if paths.is_empty() || paths.len() > 8 {
        return Err(invalid("reference_hints.paths must contain 1..8 paths"));
    }
    let paths = paths
        .iter()
        .map(|path| {
            let Some(path) = path.as_str().filter(|path| !path.trim().is_empty()) else {
                return Err(invalid(
                    "reference_hints.paths must contain non-empty strings",
                ));
            };
            let resolved = crate::common::resolve_existing_tool_path(path)
                .map_err(|_| invalid("reference_hints search scope is unavailable"))?;
            if !resolved.is_file() && !resolved.is_dir() {
                return Err(invalid("reference_hints search scope must exist"));
            }
            Ok(resolved)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for field in ["includes", "excludes"] {
        if let Some(value) = raw.get(field) {
            let Some(values) = value.as_array() else {
                return Err(invalid("reference_hints filters must be arrays"));
            };
            if values.len() > 16
                || values
                    .iter()
                    .any(|item| item.as_str().is_none_or(|s| s.trim().is_empty()))
            {
                return Err(invalid(
                    "reference_hints filters support at most 16 non-empty strings",
                ));
            }
        }
    }
    let includes = parse_pattern_strings(raw.get("includes"));
    let excludes = parse_pattern_strings(raw.get("excludes"));
    compile_patterns(&includes).map_err(|_| invalid("invalid reference_hints include glob"))?;
    compile_patterns(&excludes).map_err(|_| invalid("invalid reference_hints exclude glob"))?;
    let max_results = match raw.get("max_results") {
        None => 20,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=100).contains(n))
            .ok_or_else(|| invalid("reference_hints.max_results must be 1..100"))?
            as usize,
    };
    Ok(Some(Request {
        paths,
        includes,
        excludes,
        max_results,
    }))
}

pub(crate) async fn collect(
    request: Request,
    source: &Path,
    destination: &Path,
    args: &Value,
) -> Value {
    let filename = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let stem = source
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let module = if ["mod", "index", "__init__"].contains(&stem) {
        source
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or(stem)
    } else {
        stem
    };
    let mut queries = vec![filename.to_owned()];
    if module != filename {
        queries.push(module.to_owned());
    }
    queries.retain(|query| {
        !query.is_empty() && query.len() <= 512 && !query.chars().any(char::is_control)
    });

    let root = crate::common::common_path_root(&request.paths);
    let source_canonical = crate::common::canonicalize_with_existing_ancestor(source);
    let destination_canonical = crate::common::canonicalize_with_existing_ancestor(destination);
    let mut matches = BTreeMap::new();
    let mut complete = !queries.is_empty();
    let mut limit_reached = false;
    let mut issues = BTreeSet::new();
    for query in &queries {
        let mut search_args = json!({
            "query": query,
            "paths": request.paths.iter().map(|path| crate::common::normalize_display_path(path)).collect::<Vec<_>>(),
            "includes": request.includes, "excludes": request.excludes,
            "mode": "literal", "case_mode": "sensitive",
            "max_results": request.max_results + 1,
            "max_line_length": 160, "context_lines": 0,
            "allow_expensive_fallback": false, "output_format": "json"
        });
        if let Some(key) = args.get(crate::cancellation::ARG_KEY) {
            search_args[crate::cancellation::ARG_KEY] = key.clone();
        }
        let result = match super::text_search::execute(&search_args).await {
            Ok(result) => result,
            Err(_) => {
                complete = false;
                issues.insert("search_unavailable");
                continue;
            }
        };
        let search_complete = result.get("complete").and_then(Value::as_bool) == Some(true);
        complete &= search_complete;
        limit_reached |= result.get("limit_reached").and_then(Value::as_bool) == Some(true);
        if !search_complete {
            issues.insert("search_incomplete");
        }
        let search_root = result
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        for item in result
            .get("matches")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(file) = item.get("file").and_then(Value::as_str) else {
                continue;
            };
            let path = PathBuf::from(file);
            let path = if path.is_absolute() {
                path
            } else if let Some(root) = &search_root {
                root.join(path)
            } else {
                complete = false;
                issues.insert("unresolved_search_path");
                continue;
            };
            let canonical = crate::common::canonicalize_with_existing_ancestor(&path);
            if canonical == source_canonical || canonical == destination_canonical {
                continue;
            }
            let display = root
                .as_deref()
                .and_then(|root| crate::common::relative_display_path(&path, root))
                .unwrap_or_else(|| crate::common::normalize_display_path(&path));
            let line = item.get("line").and_then(Value::as_u64).unwrap_or(0);
            matches.entry((display.clone(), line)).or_insert_with(|| {
                json!({
                    "file": display, "line": line,
                    "line_text": item["line_text"],
                    "line_truncated": item["line_truncated"],
                    "matched_query": query
                })
            });
        }
    }
    if matches.len() > request.max_results {
        complete = false;
        limit_reached = true;
    }
    let matches: Vec<_> = matches.into_values().take(request.max_results).collect();
    let files: BTreeSet<_> = matches
        .iter()
        .filter_map(|item| item.get("file").and_then(Value::as_str))
        .collect();
    json!({
        "kind": "lexical",
        "timing": "before_move",
        "root": root.as_deref().map(crate::common::normalize_display_path),
        "paths": request.paths.iter().map(|path| crate::common::normalize_display_path(path)).collect::<Vec<_>>(),
        "queries": queries,
        "files": files,
        "matches": matches,
        "max_results": request.max_results,
        "complete": complete,
        "limit_reached": limit_reached,
        "issues": issues,
        "note": "Text mentions only; review comments, strings, and unrelated identifiers. The transferred file is excluded."
    })
}

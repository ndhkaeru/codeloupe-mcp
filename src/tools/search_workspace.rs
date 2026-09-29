use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::future::Future;
use std::path::PathBuf;

const DEFAULT_MAX_RESULTS: usize = 20;
const MAX_RESULTS: usize = 100;
const DEFAULT_MAX_LINE_LENGTH: usize = 240;
const MAX_LINE_LENGTH: usize = 4_000;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 128 * 1024;
const MIN_MAX_OUTPUT_BYTES: usize = 4 * 1024;
const MAX_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_QUERY_CHARS: usize = 1024;
const GROUP_NAMES: [&str; 3] = ["path", "symbol", "text"];

pub fn schema() -> Value {
    json!({
        "name": "search_workspace",
        "title": "Search workspace",
        "description": "Run fuzzy path, symbol-definition, and exact text searches concurrently. Results stay grouped by engine with explicit source, strategy, completeness, and limit status; one engine failure does not discard the other groups.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "maxLength": 1024, "description": "Shared query sent to the path, symbol, and text engines without heuristic routing." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Search roots or files. Defaults to the active workspace." },
                "max_results": { "type": "integer", "minimum": 0, "maximum": 100, "description": "Maximum returned results per group. Defaults to 20." },
                "max_line_length": { "type": "integer", "minimum": 1, "maximum": 4000, "description": "Maximum text and symbol snippet characters. Defaults to 240." },
                "text_mode": { "type": "string", "enum": ["literal", "regex"], "description": "Text-search mode. Defaults to literal." },
                "case_mode": { "type": "string", "enum": ["insensitive", "sensitive", "smart"], "description": "Text-search case handling. Defaults to smart." },
                "includes": { "type": "array", "items": { "type": "string" }, "description": "Text-search include globs relative to the searched roots." },
                "excludes": { "type": "array", "items": { "type": "string" }, "description": "Text-search exclude globs relative to the searched roots." },
                "include_ignored": { "type": "boolean", "description": "Include files filtered by ignore rules in all three search engines." },
                "include_hidden": { "type": "boolean", "description": "Include hidden files and directories except VCS metadata directories such as .git, unless scoped directly." },
                "allow_expensive_fallback": { "type": "boolean", "description": "Allow root-wide grep fallback in very large workspaces. Defaults to false." },
                "max_output_bytes": { "type": "integer", "minimum": 4096, "maximum": 1048576, "description": "Maximum serialized response size. Defaults to 131072 bytes." },
                "verbose": { "type": "boolean", "description": "Include underlying engine diagnostics. Defaults to false; incomplete engines include diagnostics automatically." }
            },
            "required": ["query"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("Missing/empty query")?;
    if query.chars().count() > MAX_QUERY_CHARS {
        anyhow::bail!("Query exceeds maximum length of {MAX_QUERY_CHARS} characters");
    }
    let max_results = usize_arg(args, "max_results", DEFAULT_MAX_RESULTS, 0, MAX_RESULTS);
    let max_line_length = usize_arg(
        args,
        "max_line_length",
        DEFAULT_MAX_LINE_LENGTH,
        1,
        MAX_LINE_LENGTH,
    );
    let max_output_bytes = usize_arg(
        args,
        "max_output_bytes",
        DEFAULT_MAX_OUTPUT_BYTES,
        MIN_MAX_OUTPUT_BYTES,
        MAX_MAX_OUTPUT_BYTES,
    );
    let verbose = args
        .get("verbose")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut path_args = json!({
        "pattern": query,
        "max_results": max_results,
        "verbose": verbose,
        "_suppress_filter_hints": true
    });
    copy_fields(
        args,
        &mut path_args,
        &[
            "paths",
            "include_ignored",
            "include_hidden",
            crate::cancellation::ARG_KEY,
        ],
    );

    let mut symbol_args = json!({
        "symbol": query,
        "max_line_length": max_line_length,
        "verbose": true,
        "_suppress_filter_hints": true
    });
    copy_fields(
        args,
        &mut symbol_args,
        &[
            "paths",
            "include_ignored",
            "include_hidden",
            crate::cancellation::ARG_KEY,
        ],
    );

    let mut text_args = json!({
        "query": query,
        "mode": args.get("text_mode").cloned().unwrap_or_else(|| json!("literal")),
        "case_mode": args.get("case_mode").cloned().unwrap_or_else(|| json!("smart")),
        "max_results": max_results,
        "max_line_length": max_line_length,
        "verbose": verbose,
        "_suppress_filter_hints": true
    });
    copy_fields(
        args,
        &mut text_args,
        &[
            "paths",
            "includes",
            "excludes",
            "include_ignored",
            "include_hidden",
            "allow_expensive_fallback",
            crate::cancellation::ARG_KEY,
        ],
    );

    let (path_result, symbol_result, text_result) = join_engine_futures(
        super::fuzzy_find::execute(&path_args),
        super::find_definition::execute(&symbol_args),
        super::text_search::execute(&text_args),
    )
    .await;

    let mut groups = json!({
        "path": build_group("fuzzy_find", "results", path_result, max_results, verbose),
        "symbol": build_group("find_definition", "definitions", symbol_result, max_results, verbose),
        "text": build_group("text_search", "matches", text_result, max_results, verbose)
    });
    let root = lift_shared_root(&mut groups);
    let complete = GROUP_NAMES.iter().all(|name| {
        groups.get(*name).is_some_and(|group| {
            group.get("status").and_then(Value::as_str) == Some("ok")
                && group.get("complete").and_then(Value::as_bool) == Some(true)
        })
    });
    let mut response = json!({
        "query": query,
        "root": root,
        "groups": groups,
        "complete": complete,
        "partial": !complete,
        "max_output_bytes": max_output_bytes,
        "output_bytes_used": 0,
        "output_truncated": false
    });
    if max_results > 0
        && groups_have_no_results(
            response
                .get("groups")
                .expect("search_workspace response always contains groups"),
        )
    {
        let warnings = super::path_filters::filtered_scope_warnings(
            &resolved_search_paths(args),
            args.get("include_ignored")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            args.get("include_hidden")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        if !warnings.is_empty() {
            crate::common::insert_object_field(&mut response, "warnings", json!(warnings));
        }
    }

    Ok(bound_response(response, max_output_bytes))
}

fn groups_have_no_results(groups: &Value) -> bool {
    GROUP_NAMES.iter().all(|name| {
        groups
            .get(*name)
            .and_then(|group| group.get("results"))
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    })
}

fn resolved_search_paths(args: &Value) -> Vec<PathBuf> {
    args.get("paths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(crate::common::resolve_tool_path)
                .collect()
        })
        .unwrap_or_else(|| vec![crate::common::default_tool_root()])
}

fn lift_shared_root(groups: &mut Value) -> Value {
    let Some(groups_object) = groups.as_object_mut() else {
        return Value::Null;
    };
    let mut shared_root = None;
    for group in groups_object.values() {
        let Some(root) = group.get("root").filter(|root| !root.is_null()) else {
            continue;
        };
        if shared_root.as_ref().is_some_and(|known| known != root) {
            return Value::Null;
        }
        shared_root = Some(root.clone());
    }
    let Some(shared_root) = shared_root else {
        return Value::Null;
    };
    for group in groups_object.values_mut() {
        if group.get("root") == Some(&shared_root)
            && let Some(group) = group.as_object_mut()
        {
            group.remove("root");
        }
    }
    shared_root
}

async fn join_engine_futures<P, S, T>(
    path: P,
    symbol: S,
    text: T,
) -> (P::Output, S::Output, T::Output)
where
    P: Future,
    S: Future,
    T: Future,
{
    tokio::join!(path, symbol, text)
}

fn copy_fields(source: &Value, target: &mut Value, fields: &[&str]) {
    let Some(target) = target.as_object_mut() else {
        return;
    };
    for field in fields {
        if let Some(value) = source.get(*field) {
            target.insert((*field).to_string(), value.clone());
        }
    }
}

fn build_group(
    source: &str,
    result_field: &str,
    result: Result<Value>,
    max_results: usize,
    verbose: bool,
) -> Value {
    let mut payload = match result {
        Ok(payload) => payload,
        Err(engine_error) => {
            let error = super::structured_tool_error(&engine_error.to_string())
                .get("error")
                .cloned()
                .unwrap_or_else(
                    || json!({ "code": "tool_error", "message": "Search engine failed" }),
                );
            return json!({
                "source": source,
                "status": "error",
                "results": [],
                "total_returned": 0,
                "complete": false,
                "limit_reached": false,
                "strategy": {
                    "search_strategy": "not_completed",
                    "index_used": Value::Null,
                    "index_complete": Value::Null
                },
                "error": error
            });
        }
    };

    let root = payload.get("root").cloned().unwrap_or(Value::Null);
    let engine_complete = payload
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let engine_limit_reached = payload
        .get("limit_reached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut results = payload
        .get_mut(result_field)
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    let facade_omitted = results.len().saturating_sub(max_results);
    results.truncate(max_results);
    let limit_reached = engine_limit_reached || facade_omitted > 0;
    let complete = engine_complete && facade_omitted == 0;
    let limit_reason = if facade_omitted > 0 {
        Some("facade_max_results")
    } else {
        payload.get("limit_reason").and_then(Value::as_str)
    };
    let strategy = json!({
        "search_strategy": engine_field(&payload, "search_strategy")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        "index_used": engine_field(&payload, "index_used").cloned().unwrap_or(Value::Null),
        "index_complete": engine_field(&payload, "index_complete")
            .cloned()
            .unwrap_or(Value::Null),
        "index_age_secs": engine_field(&payload, "index_age_secs")
            .cloned()
            .unwrap_or(Value::Null)
    });
    let mut group = json!({
        "source": source,
        "status": "ok",
        "root": root,
        "results": results,
        "total_returned": results.len(),
        "complete": complete,
        "limit_reached": limit_reached,
        "limit_reason": limit_reason,
        "strategy": strategy
    });
    let Some(group_object) = group.as_object_mut() else {
        return group;
    };
    for field in ["warnings", "no_results", "suggested_next_query"] {
        if let Some(value) = payload.get(field)
            && !value.is_null()
            && !matches!(value, Value::Array(items) if items.is_empty())
        {
            group_object.insert(field.to_string(), value.clone());
        }
    }
    if (verbose || !complete)
        && let Some(diagnostics) = payload.get("diagnostics")
        && !diagnostics
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    {
        group_object.insert("diagnostics".to_string(), diagnostics.clone());
    }
    if facade_omitted > 0 {
        group_object.insert("results_omitted".to_string(), json!(facade_omitted));
    }
    group
}

fn engine_field<'a>(payload: &'a Value, field: &str) -> Option<&'a Value> {
    payload.get(field).or_else(|| {
        payload
            .get("diagnostics")
            .and_then(|value| value.get(field))
    })
}

fn bound_response(mut response: Value, max_bytes: usize) -> Value {
    if refresh_output_size(&mut response) <= max_bytes {
        return response;
    }

    if let Some(object) = response.as_object_mut() {
        object.insert("complete".to_string(), Value::Bool(false));
        object.insert("partial".to_string(), Value::Bool(true));
        object.insert("output_truncated".to_string(), Value::Bool(true));
    }
    for group_name in GROUP_NAMES {
        if let Some(group) = response
            .pointer_mut(&format!("/groups/{group_name}"))
            .and_then(Value::as_object_mut)
            && group.remove("diagnostics").is_some()
        {
            group.insert("diagnostics_omitted".to_string(), Value::Bool(true));
        }
    }

    let mut cursor = 0usize;
    while refresh_output_size(&mut response) > max_bytes {
        let mut removed = false;
        for offset in 0..GROUP_NAMES.len() {
            let group_name = GROUP_NAMES[(cursor + offset) % GROUP_NAMES.len()];
            if pop_group_result(&mut response, group_name) {
                cursor = (cursor + offset + 1) % GROUP_NAMES.len();
                removed = true;
                break;
            }
        }
        if !removed {
            break;
        }
    }

    if refresh_output_size(&mut response) > max_bytes {
        compact_group_metadata(&mut response);
    }

    refresh_output_size(&mut response);
    response
}

fn compact_group_metadata(response: &mut Value) {
    if let Some(object) = response.as_object_mut()
        && let Some(query) = object.get("query").and_then(Value::as_str)
        && query.chars().count() > 256
    {
        object.insert(
            "query".to_string(),
            Value::String(query.chars().take(256).collect()),
        );
        object.insert("query_truncated".to_string(), Value::Bool(true));
    }
    for group_name in GROUP_NAMES {
        let Some(group) = response
            .pointer_mut(&format!("/groups/{group_name}"))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        group.remove("root");
        for field in [
            "diagnostics",
            "warnings",
            "no_results",
            "suggested_next_query",
            "diagnostics_omitted",
        ] {
            group.remove(field);
        }
        if let Some(message) = group
            .get_mut("error")
            .and_then(Value::as_object_mut)
            .and_then(|error| error.get_mut("message"))
            .and_then(|message| message.as_str())
            .map(ToString::to_string)
        {
            group
                .get_mut("error")
                .and_then(Value::as_object_mut)
                .expect("error object was already checked")
                .insert(
                    "message".to_string(),
                    Value::String(message.chars().take(160).collect()),
                );
        }
    }
}

fn pop_group_result(response: &mut Value, group_name: &str) -> bool {
    let Some(group) = response
        .pointer_mut(&format!("/groups/{group_name}"))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let Some(results) = group.get_mut("results").and_then(Value::as_array_mut) else {
        return false;
    };
    if results.pop().is_none() {
        return false;
    }
    let remaining = results.len();
    let omitted = group
        .get("results_omitted")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(1);
    group.insert("total_returned".to_string(), json!(remaining));
    group.insert("results_omitted".to_string(), json!(omitted));
    group.insert("complete".to_string(), Value::Bool(false));
    group.insert("limit_reached".to_string(), Value::Bool(true));
    group.insert("limit_reason".to_string(), json!("output_budget"));
    group.insert("output_truncated".to_string(), Value::Bool(true));
    true
}

fn refresh_output_size(response: &mut Value) -> usize {
    let mut expected = 0usize;
    for _ in 0..8 {
        if let Some(object) = response.as_object_mut() {
            object.insert("output_bytes_used".to_string(), json!(expected));
        }
        let measured = serde_json::to_vec(response).map_or(0, |bytes| bytes.len());
        if measured == expected {
            return measured;
        }
        expected = measured;
    }
    serde_json::to_vec(response).map_or(0, |bytes| bytes.len())
}

fn usize_arg(args: &Value, key: &str, default: usize, minimum: usize, maximum: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|value| (value as usize).clamp(minimum, maximum))
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::join_engine_futures;
    use std::sync::Arc;
    use tokio::sync::Barrier;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn engine_futures_are_polled_concurrently() {
        let barrier = Arc::new(Barrier::new(3));
        let make_future = |value| {
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                value
            }
        };

        let result = timeout(
            Duration::from_secs(1),
            join_engine_futures(make_future(1), make_future(2), make_future(3)),
        )
        .await
        .expect("joined engines should reach the barrier together");
        assert_eq!(result, (1, 2, 3));
    }
}

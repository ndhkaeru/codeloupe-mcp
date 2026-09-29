use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::common::insert_object_field;
use crate::tools::read_file;

fn display_path(path: &str) -> String {
    if path == "<unknown>" {
        path.to_string()
    } else {
        crate::common::normalize_display_path(&crate::common::resolve_tool_path(path))
    }
}

pub fn schema() -> Value {
    json!({
        "name": "read_snippets",
        "title": "Read snippets",
        "description": "Read multiple focused file ranges in one request. Use to gather evidence from several files or call sites without loading whole files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "requests": { "description": "List of file range requests. Each range uses 1-indexed inclusive line numbers.",
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "File path to read. Relative paths resolve against the active workspace." },
                            "start_line": { "type": "integer", "minimum": 1, "description": "1-indexed inclusive start line. Defaults to 1." },
                            "end_line": { "type": "integer", "description": "1-indexed inclusive end line. Omit to read until max_lines/max_bytes or EOF." },
                            "tail": { "type": "integer", "minimum": 1, "description": "Read the final N text lines. Mutually exclusive with line ranges and start_byte." },
                            "start_byte": { "type": "integer", "minimum": 0, "description": "Read a bounded text window from this zero-based raw byte offset." },
                            "json_pointer": { "type": "string", "description": "Read one JSON branch selected by an RFC 6901 pointer." },
                            "max_lines": { "type": "integer", "description": "Maximum lines for this snippet." },
                            "max_bytes": { "type": "integer", "description": "Maximum UTF-8 output bytes for this snippet. If max_lines and max_bytes are both set, the first reached limit wins." },
                            "include_line_numbers": { "type": "boolean", "description": "Prefix returned lines with 1-indexed line numbers when true. Defaults to false." }
                        },
                        "required": ["path"]
                    }
                },
                "max_total_bytes": { "type": "integer", "description": "Optional total UTF-8 output budget across all snippets. Later requests are skipped when the budget is exhausted." }
            },
            "required": ["requests"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let requests = args
        .get("requests")
        .and_then(|v| v.as_array())
        .context("Missing/Invalid 'requests' field")?;
    let max_total_bytes = args
        .get("max_total_bytes")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize);

    let mut results = Vec::with_capacity(requests.len());
    let mut continuations = Vec::new();
    let mut total_content_bytes = 0usize;
    let mut completed_requests = 0usize;
    let mut errored_requests = 0usize;
    let mut skipped_requests = 0usize;
    let mut truncated_results = 0usize;

    for (request_index, request) in requests.iter().enumerate() {
        let path = request
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>")
            .to_string();

        let Some(remaining_budget) = remaining_budget(max_total_bytes, total_content_bytes) else {
            let skipped = skipped_result(
                request_index,
                &path,
                request,
                "batch_total_byte_limit_reached",
            );
            continuations.push(skipped_result_continuation(request_index, &path, request));
            results.push(skipped);
            skipped_requests += 1;
            continue;
        };

        let (effective_request, batch_limit_applied) =
            apply_batch_byte_limit(request, max_total_bytes, remaining_budget);
        let mut payload = execute_single_request(&effective_request, request_index);
        if batch_limit_applied
            && payload.get("status").and_then(|v| v.as_str()) == Some("success")
            && payload
                .get("returned_lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                == 0
            && payload.get("truncated").and_then(|v| v.as_bool()) == Some(true)
        {
            payload = skipped_result(
                request_index,
                &path,
                request,
                "batch_total_byte_limit_reached",
            );
            continuations.push(skipped_result_continuation(request_index, &path, request));
            skipped_requests += 1;
            results.push(payload);
            continue;
        }
        let content_bytes = payload
            .get("content")
            .and_then(|v| v.as_str())
            .map(|content| content.len())
            .unwrap_or(0);
        total_content_bytes += content_bytes;

        match payload.get("status").and_then(|v| v.as_str()) {
            Some("success") => {
                completed_requests += 1;
                let continuation =
                    build_truncation_continuation(request_index, &path, request, &payload);
                if continuation.is_some() {
                    truncated_results += 1;
                }

                if batch_limit_applied {
                    insert_object_field(&mut payload, "batch_limit_applied", json!(true));
                }

                if let Some(continuation) = continuation {
                    continuations.push(continuation.clone());
                    insert_object_field(&mut payload, "continuation", continuation);
                }
            }
            Some("error") => {
                errored_requests += 1;
            }
            Some("skipped") => {
                skipped_requests += 1;
            }
            _ => {}
        }

        compact_result_defaults(&mut payload);
        results.push(payload);
    }

    let complete = continuations.is_empty() && errored_requests == 0 && skipped_requests == 0;
    Ok(json!({
        "results": results,
        "result_defaults": {
            "status": "success",
            "encoding": "UTF-8",
            "is_binary": false
        },
        "total_requests": requests.len(),
        "completed_requests": completed_requests,
        "errored_requests": errored_requests,
        "skipped_requests": skipped_requests,
        "truncated_results": truncated_results,
        "complete": complete,
        "continuations": continuations,
        "batch_limits": {
            "max_total_bytes": max_total_bytes,
            "total_content_bytes": total_content_bytes
        }
    }))
}

fn compact_result_defaults(payload: &mut Value) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    if object.get("status").and_then(Value::as_str) == Some("success") {
        object.remove("status");
    }
    if object.get("encoding").and_then(Value::as_str) == Some("UTF-8") {
        object.remove("encoding");
    }
    if object.get("is_binary").and_then(Value::as_bool) == Some(false) {
        object.remove("is_binary");
    }
}

fn remaining_budget(max_total_bytes: Option<usize>, used_bytes: usize) -> Option<usize> {
    match max_total_bytes {
        Some(limit) if used_bytes >= limit => None,
        Some(limit) => Some(limit - used_bytes),
        None => Some(usize::MAX),
    }
}

fn apply_batch_byte_limit(
    request: &Value,
    max_total_bytes: Option<usize>,
    remaining_budget: usize,
) -> (Value, bool) {
    let Some(_) = max_total_bytes else {
        return (request.clone(), false);
    };

    let requested_max_bytes = request.get("max_bytes").and_then(|v| v.as_u64());
    let effective_max_bytes = requested_max_bytes
        .map(|value| (value as usize).min(remaining_budget))
        .unwrap_or(remaining_budget);

    let mut effective_request = request.clone();
    insert_object_field(
        &mut effective_request,
        "max_bytes",
        json!(effective_max_bytes.max(1)),
    );

    let batch_limit_applied = requested_max_bytes
        .map(|value| effective_max_bytes < value as usize)
        .unwrap_or(true);

    (effective_request, batch_limit_applied)
}

fn execute_single_request(request: &Value, request_index: usize) -> Value {
    let path = request
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>")
        .to_string();

    let display_path = display_path(&path);
    let mut requested_range = json!({
        "path": display_path,
        "start_line": request.get("start_line").cloned().unwrap_or(Value::Null),
        "end_line": request.get("end_line").cloned().unwrap_or(Value::Null),
        "tail": request.get("tail").cloned().unwrap_or(Value::Null),
        "start_byte": request.get("start_byte").cloned().unwrap_or(Value::Null),
        "json_pointer": request.get("json_pointer").cloned().unwrap_or(Value::Null),
        "max_lines": request.get("max_lines").cloned().unwrap_or(Value::Null),
        "max_bytes": request.get("max_bytes").cloned().unwrap_or(Value::Null),
        "include_line_numbers": request.get("include_line_numbers").cloned().unwrap_or(json!(false))
    });

    match read_file::execute_sync(request) {
        Ok(Value::Object(mut object)) => {
            // A successful read already reports path/start_line/end_line, so
            // `requested_range` is only echoed for errors and skips.
            object.insert("path".to_string(), json!(display_path));
            object.insert("status".to_string(), json!("success"));
            object.insert("request_index".to_string(), json!(request_index));
            Value::Object(object)
        }
        Ok(other) => {
            insert_object_field(&mut requested_range, "raw_result", other);
            json!({
                "path": display_path,
                "status": "success",
                "request_index": request_index,
                "requested_range": requested_range
            })
        }
        Err(err) => {
            let structured = super::structured_tool_error(&err.to_string());
            let error = structured
                .get("error")
                .cloned()
                .unwrap_or_else(|| json!({ "code": "tool_error", "message": err.to_string() }));
            json!({
                "path": display_path,
                "status": "error",
                "request_index": request_index,
                "requested_range": requested_range,
                "error": error
            })
        }
    }
}

fn build_truncation_continuation(
    request_index: usize,
    path: &str,
    original_request: &Value,
    payload: &Value,
) -> Option<Value> {
    if !payload
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return None;
    }

    let next_start_line = payload.get("next_start_line").and_then(|v| v.as_u64());
    let next_start_byte = payload.get("next_start_byte").and_then(|v| v.as_u64());
    if next_start_line.is_none() && next_start_byte.is_none() {
        return None;
    }
    let remaining_lines = payload
        .get("omitted_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut suggested_request = original_request.clone();
    let display_path = display_path(path);
    insert_object_field(&mut suggested_request, "path", json!(display_path));
    if let Some(object) = suggested_request.as_object_mut() {
        object.remove("tail");
        if let Some(next_start_byte) = next_start_byte {
            object.remove("start_line");
            object.remove("end_line");
            object.remove("max_lines");
            object.remove("include_line_numbers");
            object.insert("start_byte".to_string(), json!(next_start_byte));
        } else if let Some(next_start_line) = next_start_line {
            object.remove("start_byte");
            object.insert("start_line".to_string(), json!(next_start_line));
        }
    }

    Some(json!({
        "request_index": request_index,
        "path": display_path,
        "reason": "snippet_truncated",
        "next_start_line": next_start_line,
        "next_start_byte": next_start_byte,
        "remaining_lines": remaining_lines,
        "suggested_request": suggested_request
    }))
}

fn skipped_result(request_index: usize, path: &str, request: &Value, reason: &str) -> Value {
    let display_path = display_path(path);
    json!({
        "path": display_path,
        "status": "skipped",
        "request_index": request_index,
        "reason": reason,
        "requested_range": {
            "path": display_path,
            "start_line": request.get("start_line").cloned().unwrap_or(Value::Null),
            "end_line": request.get("end_line").cloned().unwrap_or(Value::Null),
            "tail": request.get("tail").cloned().unwrap_or(Value::Null),
            "start_byte": request.get("start_byte").cloned().unwrap_or(Value::Null),
            "json_pointer": request.get("json_pointer").cloned().unwrap_or(Value::Null),
            "max_lines": request.get("max_lines").cloned().unwrap_or(Value::Null),
            "max_bytes": request.get("max_bytes").cloned().unwrap_or(Value::Null),
            "include_line_numbers": request.get("include_line_numbers").cloned().unwrap_or(json!(false))
        }
    })
}

fn skipped_result_continuation(request_index: usize, path: &str, request: &Value) -> Value {
    let display_path = display_path(path);
    json!({
        "request_index": request_index,
        "path": display_path,
        "reason": "batch_total_byte_limit_reached",
        "suggested_request": request
    })
}

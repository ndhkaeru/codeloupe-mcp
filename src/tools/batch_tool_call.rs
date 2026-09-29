use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use tokio::time::{Duration, Instant, timeout};

const DEFAULT_SUBCALL_TIMEOUT_SECONDS: u64 = 60;
const MAX_SUBCALL_TIMEOUT_SECONDS: u64 = 300;
const DEFAULT_BATCH_DEADLINE_MS: u64 = 55_000;
const MAX_BATCH_DEADLINE_MS: u64 = 300_000;
const DEFAULT_TOTAL_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_CALL_OUTPUT_BYTES: usize = 256 * 1024;
const MIN_TOTAL_OUTPUT_BYTES: usize = 1024;
const MAX_TOOL_LABEL_BYTES: usize = 256;
const TRUNCATED_SUFFIX: &str = "... [truncated]";

#[derive(Serialize)]
struct BatchResponse {
    summary: Vec<Value>,
    results: Vec<Value>,
    total: usize,
    completed: usize,
    partial: bool,
    deadline_ms: u64,
    max_output_bytes: usize,
    output_bytes_used: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    index_advice: Option<Value>,
}

fn extract_text_payload(result: &Value) -> Option<&str> {
    result
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|v| v.as_str())
}

fn flatten_tool_result(result: Value) -> Result<Value> {
    if result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let payload = extract_text_payload(&result)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or_else(|| {
                super::structured_tool_error(
                    extract_text_payload(&result).unwrap_or("Unknown tool error"),
                )
            });
        return Err(anyhow::anyhow!(
            serde_json::to_string(&payload).unwrap_or_else(|_| "Unknown tool error".to_string())
        ));
    }

    if let Some(text) = extract_text_payload(&result) {
        return match serde_json::from_str::<Value>(text) {
            Ok(parsed) => Ok(parsed),
            Err(_) => Ok(json!({ "raw_text": text })),
        };
    }

    Ok(result)
}

pub fn schema() -> Value {
    json!({
        "name": "batch_tool_call",
        "title": "Batch tool calls",
        "description": "Run a short sequence of codeloupe-mcp tools in one request when later calls depend on earlier results. Uses a deadline and fair output budget so earlier large results do not hide later calls; maximum 20 calls and recursive batch_tool_call is rejected.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "calls": { "description": "Ordered list of tool calls to run sequentially. Maximum 20 calls per request.",
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": { "type": "string", "description": "Tool name to call. batch_tool_call is not allowed recursively." },
                            "args": { "type": "object", "description": "Arguments object for the selected tool." },
                            "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": 300, "description": "Timeout for this subcall. It is also capped by the remaining batch deadline." },
                            "max_output_bytes": { "type": "integer", "minimum": 64, "description": "Maximum result bytes for this subcall before a bounded preview is returned." }
                        },
                        "required": ["tool", "args"]
                    }
                },
                "deadline_ms": { "type": "integer", "minimum": 0, "maximum": 300000, "description": "Batch deadline in milliseconds. Calls not started before it expires are returned as skipped_deadline. Defaults to 55000." },
                "max_output_bytes": { "type": "integer", "minimum": 1024, "maximum": 8388608, "description": "Total result budget shared fairly across calls. Defaults to 1 MiB." }
            },
            "required": ["calls"]
        }
    })
}

pub fn execute(args: &Value) -> Pin<Box<dyn Future<Output = Result<Value>> + '_>> {
    Box::pin(async move {
        let calls = args
            .get("calls")
            .and_then(|v| v.as_array())
            .context("Missing 'calls' array")?;

        if calls.is_empty() {
            return encode_batch_response(BatchResponse {
                summary: Vec::new(),
                results: Vec::new(),
                total: 0,
                completed: 0,
                partial: false,
                deadline_ms: 0,
                max_output_bytes: DEFAULT_TOTAL_OUTPUT_BYTES,
                output_bytes_used: 0,
                index_advice: None,
            });
        }
        if calls.len() > 20 {
            return Err(anyhow::anyhow!(
                "batch_tool_call accepts at most 20 calls per request"
            ));
        }

        let deadline_ms = args
            .get("deadline_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_BATCH_DEADLINE_MS)
            .min(MAX_BATCH_DEADLINE_MS);
        let total_output_budget = args
            .get("max_output_bytes")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_TOTAL_OUTPUT_BYTES)
            .clamp(MIN_TOTAL_OUTPUT_BYTES, MAX_TOTAL_OUTPUT_BYTES);
        let cancellation_key = args
            .get(crate::cancellation::ARG_KEY)
            .and_then(Value::as_str);
        let started_at = Instant::now();
        let deadline = Duration::from_millis(deadline_ms);
        let mut remaining_output_budget = total_output_budget;
        let mut index_advice = None;
        let mut results = Vec::with_capacity(calls.len());
        let mut summary = Vec::with_capacity(calls.len());
        let mut completed = 0usize;
        let mut partial = false;

        for (index, call) in calls.iter().enumerate() {
            let tool_name = call
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool_label = bound_tool_label(&tool_name);

            if cancellation_key.is_some_and(crate::cancellation::is_cancelled) {
                partial = true;
                push_skipped(&mut results, &mut summary, tool_label, "skipped_cancelled");
                continue;
            }
            if started_at.elapsed() >= deadline {
                partial = true;
                push_skipped(&mut results, &mut summary, tool_label, "skipped_deadline");
                continue;
            }
            let remaining_calls = calls.len().saturating_sub(index).max(1);
            let fair_output_budget = remaining_output_budget / remaining_calls;
            if fair_output_budget < 64 {
                partial = true;
                push_skipped(
                    &mut results,
                    &mut summary,
                    tool_label,
                    "skipped_output_budget",
                );
                continue;
            }
            let requested_output_budget = call
                .get("max_output_bytes")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(DEFAULT_CALL_OUTPUT_BYTES)
                .max(64);
            let call_output_budget = requested_output_budget.min(fair_output_budget);
            if tool_name == "batch_tool_call" {
                partial = true;
                let returned_bytes = push_error(
                    &mut results,
                    &mut summary,
                    tool_label,
                    "Recursive batch_tool_call is not allowed".to_string(),
                    call_output_budget,
                );
                remaining_output_budget = remaining_output_budget.saturating_sub(returned_bytes);
                continue;
            }
            if !crate::security::rate_limiter::GLOBAL_LIMITER.allow() {
                partial = true;
                let returned_bytes = push_error(
                    &mut results,
                    &mut summary,
                    tool_label,
                    "Rate limit exceeded (max 50 req/s)".to_string(),
                    call_output_budget,
                );
                remaining_output_budget = remaining_output_budget.saturating_sub(returned_bytes);
                continue;
            }
            let remaining_deadline = deadline.saturating_sub(started_at.elapsed());
            let requested_timeout = call
                .get("timeout_seconds")
                .and_then(Value::as_u64)
                .or_else(|| {
                    call.get("args")
                        .and_then(|value| value.get("timeout_seconds"))
                        .and_then(Value::as_u64)
                })
                .unwrap_or(DEFAULT_SUBCALL_TIMEOUT_SECONDS)
                .clamp(1, MAX_SUBCALL_TIMEOUT_SECONDS);
            let subcall_timeout = Duration::from_secs(requested_timeout).min(remaining_deadline);
            let params = json!({
                "name": tool_name,
                "arguments": call.get("args").cloned().unwrap_or(json!({}))
            });

            let execution = timeout(
                subcall_timeout,
                super::call_tool_inner_with_cancellation(params, cancellation_key),
            )
            .await;

            match execution {
                Err(_) => {
                    partial = true;
                    let returned_bytes = push_error(
                        &mut results,
                        &mut summary,
                        tool_label,
                        format!(
                            "Tool call timed out after {}ms",
                            subcall_timeout.as_millis()
                        ),
                        call_output_budget,
                    );
                    remaining_output_budget =
                        remaining_output_budget.saturating_sub(returned_bytes);
                }
                Ok(Err(error)) => {
                    partial = true;
                    let returned_bytes = push_error(
                        &mut results,
                        &mut summary,
                        tool_label,
                        error.to_string(),
                        call_output_budget,
                    );
                    remaining_output_budget =
                        remaining_output_budget.saturating_sub(returned_bytes);
                }
                Ok(Ok(result)) => match flatten_tool_result(result) {
                    Err(error) => {
                        partial = true;
                        let returned_bytes = push_error(
                            &mut results,
                            &mut summary,
                            tool_label,
                            error.to_string(),
                            call_output_budget,
                        );
                        remaining_output_budget =
                            remaining_output_budget.saturating_sub(returned_bytes);
                    }
                    Ok(mut flattened) => {
                        if index_advice.is_none()
                            && let Some(object) = flattened.as_object_mut()
                        {
                            index_advice = object.remove("index_advice");
                        }
                        completed += 1;
                        let (bounded, original_bytes, returned_bytes, truncated) =
                            bound_result(flattened, call_output_budget);
                        partial |= truncated;
                        remaining_output_budget =
                            remaining_output_budget.saturating_sub(returned_bytes);
                        summary.push(json!({
                            "tool": tool_label,
                            "status": "ok",
                            "bytes": original_bytes,
                            "returned_bytes": returned_bytes,
                            "truncated": truncated
                        }));
                        results.push(json!({
                            "tool": tool_label,
                            "status": "ok",
                            "result": bounded,
                            "truncated": truncated
                        }));
                    }
                },
            }
        }

        encode_batch_response(BatchResponse {
            summary,
            results,
            total: calls.len(),
            completed,
            partial,
            deadline_ms,
            max_output_bytes: total_output_budget,
            output_bytes_used: total_output_budget.saturating_sub(remaining_output_budget),
            index_advice,
        })
    })
}

fn encode_batch_response(payload: BatchResponse) -> Result<Value> {
    let raw_text = serde_json::to_string(&payload).context("failed to serialize batch response")?;
    let mut value = serde_json::to_value(payload).context("failed to encode batch response")?;
    if let Some(object) = value.as_object_mut() {
        object.insert("__mcp_raw_text".to_string(), Value::String(raw_text));
    }
    Ok(value)
}

fn bound_result(value: Value, max_bytes: usize) -> (Value, usize, usize, bool) {
    let serialized = serde_json::to_vec(&value).unwrap_or_default();
    let original_bytes = serialized.len();
    if original_bytes <= max_bytes {
        return (value, original_bytes, original_bytes, false);
    }

    let preview_limit = max_bytes.saturating_sub(192) / 2;
    let preview = utf8_prefix(&serialized, preview_limit);
    let mut bounded = json!({
        "truncated": true,
        "original_bytes": original_bytes,
        "preview": preview,
        "retry_hint": "Call this tool directly with narrower scope or a larger per-call max_output_bytes."
    });
    let mut returned_bytes = serde_json::to_vec(&bounded).map_or(0, |bytes| bytes.len());
    if returned_bytes > max_bytes {
        bounded = json!({
            "truncated": true,
            "original_bytes": original_bytes
        });
        returned_bytes = serde_json::to_vec(&bounded).map_or(0, |bytes| bytes.len());
    }
    (bounded, original_bytes, returned_bytes, true)
}

fn utf8_prefix(bytes: &[u8], max_bytes: usize) -> String {
    let limit = bytes.len().min(max_bytes);
    let mut end = limit;
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn push_error(
    results: &mut Vec<Value>,
    summary: &mut Vec<Value>,
    tool_name: String,
    error: String,
    max_bytes: usize,
) -> usize {
    let (error, bytes, returned_bytes, truncated) = bound_error(error, max_bytes);
    summary.push(json!({
        "tool": tool_name,
        "status": "error",
        "bytes": bytes,
        "returned_bytes": returned_bytes,
        "truncated": truncated
    }));
    results.push(json!({
        "tool": tool_name,
        "status": "error",
        "error": error,
        "truncated": truncated
    }));
    returned_bytes
}

fn bound_error(error: String, max_bytes: usize) -> (Value, usize, usize, bool) {
    let structured = super::structured_tool_error(&error);
    let error = structured
        .get("error")
        .cloned()
        .unwrap_or_else(|| json!({ "code": "tool_error", "message": error }));
    let original_bytes = serde_json::to_vec(&error).map_or(0, |bytes| bytes.len());
    if original_bytes <= max_bytes {
        return (error, original_bytes, original_bytes, false);
    }

    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("tool_error");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Tool call failed");
    let mut prefix_limit = message.len().min(max_bytes);
    loop {
        let mut bounded_message = utf8_prefix(message.as_bytes(), prefix_limit);
        bounded_message.push_str(TRUNCATED_SUFFIX);
        let bounded = json!({ "code": code, "message": bounded_message });
        let returned_bytes = serde_json::to_vec(&bounded).map_or(0, |bytes| bytes.len());
        if returned_bytes <= max_bytes {
            return (bounded, original_bytes, returned_bytes, true);
        }
        if prefix_limit == 0 {
            let fallback = json!({ "code": "tool_error", "message": "" });
            let returned_bytes = serde_json::to_vec(&fallback).map_or(0, |bytes| bytes.len());
            return (fallback, original_bytes, returned_bytes, true);
        }
        let overflow = returned_bytes.saturating_sub(max_bytes).max(1);
        prefix_limit = prefix_limit.saturating_sub(overflow);
    }
}

fn bound_tool_label(tool_name: &str) -> String {
    if tool_name.len() <= MAX_TOOL_LABEL_BYTES {
        return tool_name.to_string();
    }
    let prefix_limit = MAX_TOOL_LABEL_BYTES.saturating_sub(TRUNCATED_SUFFIX.len());
    let mut bounded = utf8_prefix(tool_name.as_bytes(), prefix_limit);
    bounded.push_str(TRUNCATED_SUFFIX);
    bounded
}

fn push_skipped(
    results: &mut Vec<Value>,
    summary: &mut Vec<Value>,
    tool_name: String,
    status: &str,
) {
    summary.push(json!({
        "tool": tool_name,
        "status": status,
        "bytes": 0,
        "returned_bytes": 0,
        "truncated": false
    }));
    results.push(json!({
        "tool": tool_name,
        "status": status
    }));
}

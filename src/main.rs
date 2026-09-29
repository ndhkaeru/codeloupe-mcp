use codeloupe_mcp::{common, indexer, mcp, security, tools, version, workspace_control};
use mcp::{JsonRpcRequest, JsonRpcResponse};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, timeout};
use tracing::{Level, debug, error, info, warn};
use version::SERVER_VERSION;

const SERVER_NAME: &str = "codeloupe-mcp";
const SERVER_INSTRUCTIONS: &str = "Use codeloupe-mcp when you need local repository context without loading whole files or trees. Start with search_workspace for an unfamiliar query when path, symbol, and exact text evidence are all useful; inspect each group's source, status, strategy, and completeness instead of treating the facade as heuristic routing. Use project_map, workspace_stats, fuzzy_find, or compare_directories when you need one focused view of scope; use server_health to check workspace candidates and path/content index status in large repos; warm scoped content zones before repeated literal searches; then use text_search with the narrowest paths/includes possible and read_file_range, read_snippets, or read_symbol_body for focused evidence. Prefer symbol tools for definitions, references, imports/exports, and call graphs before editing. Prefer literal text_search so the content index can shortlist files; inspect search_strategy, fallback_reason, content_index_used, content_index_partial, content_index_zones, zone_indexed_at, index_age_secs, warming_zones, and unindexed_files_in_scope. If index_advice recommends indexing and repeated searches are expected, call workspace_index(action=\"enable\"); do not enable unrelated paths, and decline with workspace_index(action=\"disable\") when indexing is not needed. A too_large or blocked recommendation cannot be overridden by the agent. An approval_required recommendation may be enabled only when the user explicitly requested work at that exact path; scan and disk budgets still apply. Avoid workspace-root searches in large repos unless allow_expensive_fallback=true is intentional. Use file_hash or read_file_range sha256 values as expected_hash preconditions when edits must not overwrite concurrent changes, and use validate_json for streaming JSON syntax checks. For edits, use create_file/create_directory/edit_file/delete_file with exact paths and verify with focused reads or tests. Paths should be absolute or workspace-relative to the active workspace context.";
const WRITE_AUTH_INSTRUCTIONS: &str = "workspace_index controls indexing only and does not declare a write root. Write tools do not block paths by policy; they attach low, medium, high, or critical risk warnings based on location. Filesystem permissions and normal operation preconditions still apply.";
/// Default timeout for one tool call (seconds).
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 60;
/// Maximum timeout a client can request for one tool call (seconds).
const MAX_TOOL_TIMEOUT_SECS: u64 = 10 * 60;
const MAX_JSON_RPC_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_MCP_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_MCP_HEADER_BYTES: usize = 64 * 1024;
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    LATEST_PROTOCOL_VERSION,
];
const CANCEL_TOKEN_RETENTION_SECS: u64 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportMode {
    /// Legacy mode: one JSON message per line.
    Line,
    /// MCP stdio framing mode: Content-Length + JSON body.
    Framed,
}

#[derive(Debug)]
struct MessageReadError {
    message: String,
    transport_mode: TransportMode,
    bytes_read: usize,
    recoverable: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let log_level = Level::INFO;

    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_max_level(log_level)
        .with_target(false)
        .init();

    info!(
        "{} v{} starting (log_level={:?})",
        SERVER_NAME, SERVER_VERSION, log_level
    );

    register_explicit_workspaces()?;

    // Force-init START_TIME.
    lazy_static::initialize(&tools::server_health::START_TIME);

    let gc_report = indexer::run_startup_index_gc();
    if !gc_report.errors.is_empty() {
        warn!(
            errors = gc_report.errors.len(),
            "Index garbage collection completed with errors"
        );
    } else if gc_report.deleted_entries > 0 {
        info!(
            deleted_entries = gc_report.deleted_entries,
            freed_bytes = gc_report.freed_bytes,
            "Index garbage collection completed"
        );
    }

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let (response_tx, mut response_rx) = mpsc::unbounded_channel::<QueuedMessage>();
    let writer_handle = tokio::task::spawn_blocking(move || {
        let mut stdout = io::stdout();
        while let Some(queued) = response_rx.blocking_recv() {
            match write_message(&mut stdout, &queued.message, queued.transport_mode) {
                Ok(out_bytes) => debug!(req = queued.request_number, bytes = out_bytes, "-> send"),
                Err(err) => {
                    error!(req = queued.request_number, error = %err, "JSON-RPC write failed")
                }
            }
        }
    });
    let mut line_buf = Vec::new();
    let mut body_buf = Vec::new();
    let mut request_count: u64 = 0;
    let mut initialized = false;
    let mut client_supports_roots = false;
    let mut roots_request_sequence = 0u64;
    let mut pending_roots_requests = HashSet::new();
    let mut active_tool_requests = HashMap::<String, ActiveToolRequest>::new();
    let mut drain_active_requests = false;

    info!("Ready - waiting for JSON-RPC on stdin");

    loop {
        let (raw_request, transport_mode, bytes_read) =
            match read_next_message(&mut reader, &mut line_buf, &mut body_buf).await {
                Ok(Some(v)) => v,
                Ok(None) => {
                    info!("EOF on stdin, shutting down");
                    drain_active_requests = true;
                    break;
                }
                Err(read_error) => {
                    request_count += 1;
                    warn!(
                        req = request_count,
                        bytes = read_error.bytes_read,
                        error = %read_error.message,
                        "invalid transport message"
                    );
                    response_tx.send(QueuedMessage::response(
                        request_count,
                        read_error.transport_mode,
                        JsonRpcResponse::error(
                            Value::Null,
                            -32700,
                            format!("Parse error: {}", read_error.message),
                        ),
                    ))?;
                    if read_error.recoverable {
                        continue;
                    }
                    break;
                }
            };

        request_count += 1;
        active_tool_requests.retain(|_, active| !active.handle.is_finished());
        debug!(req = request_count, bytes = bytes_read, "<- recv");

        let message: Result<Value, _> = serde_json::from_str(&raw_request);
        match message {
            Ok(message) => {
                if message.get("method").is_none()
                    && message.get("id").is_some()
                    && (message.get("result").is_some() || message.get("error").is_some())
                {
                    match serde_json::from_value::<JsonRpcClientResponse>(message) {
                        Ok(response) => {
                            handle_client_response(response, &mut pending_roots_requests)
                        }
                        Err(error) => warn!(error = %error, "Invalid JSON-RPC client response"),
                    }
                    continue;
                }

                let request = match serde_json::from_value::<JsonRpcRequest>(message) {
                    Ok(request) => request,
                    Err(error) => {
                        error!(error = %error, raw_len = raw_request.len(), "Parse error");
                        response_tx.send(QueuedMessage::response(
                            request_count,
                            transport_mode,
                            JsonRpcResponse::error(Value::Null, -32700, "Parse error"),
                        ))?;
                        continue;
                    }
                };
                let is_notification = request.id.is_none();
                let id = request.id.clone().unwrap_or(serde_json::Value::Null);
                let method = request.method.clone();
                debug!(req = request_count, method = %method, "dispatching");

                if request.jsonrpc != "2.0" {
                    if !is_notification {
                        response_tx.send(QueuedMessage::response(
                            request_count,
                            transport_mode,
                            JsonRpcResponse::error(id, -32600, "Invalid Request"),
                        ))?;
                    }
                    continue;
                }

                if method != "initialize" && !initialized {
                    if request.id.is_none() {
                        continue;
                    }
                    response_tx.send(QueuedMessage::response(
                        request_count,
                        transport_mode,
                        JsonRpcResponse::error(id, -32002, "Server not initialized"),
                    ))?;
                    continue;
                }

                if is_notification {
                    match method.as_str() {
                        "notifications/initialized" => {
                            start_indexer_if_needed();
                            debug!("notifications/initialized - indexer triggered");
                            if client_supports_roots {
                                queue_roots_list_request(
                                    &response_tx,
                                    request_count,
                                    transport_mode,
                                    &mut roots_request_sequence,
                                    &mut pending_roots_requests,
                                )?;
                            }
                        }
                        "notifications/roots/list_changed" => {
                            if client_supports_roots {
                                queue_roots_list_request(
                                    &response_tx,
                                    request_count,
                                    transport_mode,
                                    &mut roots_request_sequence,
                                    &mut pending_roots_requests,
                                )?;
                            }
                        }
                        "notifications/cancelled" => {
                            cancel_active_tool_request(
                                request.params.as_ref(),
                                &mut active_tool_requests,
                            );
                        }
                        _ => debug!(method = %method, "Ignoring notification"),
                    }
                    continue;
                }

                let response = match method.as_str() {
                    "initialize" => {
                        let protocol_version = negotiated_protocol_version(request.params.as_ref());
                        if let Some(params) = &request.params {
                            client_supports_roots = params
                                .pointer("/capabilities/roots")
                                .is_some_and(Value::is_object);
                            maybe_set_index_roots(params);
                        }
                        initialized = true;
                        info!("Client initialized");
                        JsonRpcResponse::success(
                            id,
                            json!({
                                "protocolVersion": protocol_version,
                                "serverInfo": {
                                    "name": SERVER_NAME,
                                    "version": SERVER_VERSION
                                },
                                "capabilities": {
                                    "tools": {}
                                },
                                "instructions": format!("{SERVER_INSTRUCTIONS} {WRITE_AUTH_INSTRUCTIONS}")
                            }),
                        )
                    }
                    "ping" => JsonRpcResponse::success(id, json!({})),
                    "tools/list" => {
                        let tools = tools::list_tools();
                        debug!(count = tools.len(), "tools/list");
                        JsonRpcResponse::success(id, json!({ "tools": tools }))
                    }
                    "tools/call" => {
                        if !security::rate_limiter::GLOBAL_LIMITER.allow() {
                            warn!("Rate limit exceeded");
                            JsonRpcResponse::error(id, -32000, "Rate limit exceeded (max 50 req/s)")
                        } else {
                            let params = request.params.unwrap_or(json!({}));
                            let tool_name = params
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let tool_timeout_secs = match tool_call_timeout_secs(&params) {
                                Ok(timeout) => timeout,
                                Err(message) => {
                                    warn!(tool = %tool_name, error = %message, "invalid tool timeout");
                                    response_tx.send(QueuedMessage::response(
                                        request_count,
                                        transport_mode,
                                        JsonRpcResponse::error(id, -32602, message),
                                    ))?;
                                    continue;
                                }
                            };
                            let tool_call = PendingToolCall {
                                request_number: request_count,
                                transport_mode,
                                id,
                                params,
                                tool_name,
                                tool_timeout_secs,
                            };
                            start_tool_request(tool_call, &response_tx, &mut active_tool_requests);
                            continue;
                        }
                    }
                    "resources/list" => JsonRpcResponse::success(id, json!({ "resources": [] })),
                    "resources/templates/list" => {
                        JsonRpcResponse::success(id, json!({ "resourceTemplates": [] }))
                    }
                    "prompts/list" => JsonRpcResponse::success(id, json!({ "prompts": [] })),
                    _ => {
                        warn!(method = %method, "Unknown method");
                        JsonRpcResponse::error(id, -32601, "Method not found")
                    }
                };

                response_tx.send(QueuedMessage::response(
                    request_count,
                    transport_mode,
                    response,
                ))?;
            }
            Err(e) => {
                error!(error = %e, raw_len = raw_request.len(), "Parse error");
                let err = JsonRpcResponse::error(serde_json::Value::Null, -32700, "Parse error");
                response_tx.send(QueuedMessage::response(request_count, transport_mode, err))?;
            }
        }
    }

    for (_, active) in active_tool_requests {
        if drain_active_requests {
            if let Err(error) = active.handle.await {
                warn!(error = %error, "tool request failed while draining shutdown");
            }
        } else {
            codeloupe_mcp::cancellation::cancel(&active.cancellation_key);
            active.handle.abort();
        }
    }
    drop(response_tx);
    if let Err(err) = writer_handle.await {
        error!(error = %err, "response writer failed to join");
    }

    info!(total_requests = request_count, "Server shutdown");
    Ok(())
}

struct QueuedMessage {
    request_number: u64,
    transport_mode: TransportMode,
    message: Value,
}

impl QueuedMessage {
    fn response(
        request_number: u64,
        transport_mode: TransportMode,
        response: JsonRpcResponse,
    ) -> Self {
        Self {
            request_number,
            transport_mode,
            message: serde_json::to_value(response)
                .expect("JsonRpcResponse serialization should not fail"),
        }
    }

    fn roots_list_request(request_number: u64, transport_mode: TransportMode, id: Value) -> Self {
        Self {
            request_number,
            transport_mode,
            message: json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "roots/list",
                "params": {}
            }),
        }
    }
}

#[derive(serde::Deserialize)]
struct JsonRpcClientResponse {
    jsonrpc: String,
    id: Value,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

struct ActiveToolRequest {
    cancellation_key: String,
    handle: JoinHandle<()>,
}

struct PendingToolCall {
    request_number: u64,
    transport_mode: TransportMode,
    id: Value,
    params: Value,
    tool_name: String,
    tool_timeout_secs: u64,
}

fn start_tool_request(
    tool_call: PendingToolCall,
    response_tx: &mpsc::UnboundedSender<QueuedMessage>,
    active_tool_requests: &mut HashMap<String, ActiveToolRequest>,
) {
    let request_id_key = request_id_key(&tool_call.id);
    let cancellation_key = format!("{}:{request_id_key}", tool_call.request_number);
    let _ = codeloupe_mcp::cancellation::register(cancellation_key.clone());
    let tx = response_tx.clone();
    let task_cancellation_key = cancellation_key.clone();
    let observed_tool_name = tool_call.tool_name.clone();
    let observed_params = tool_call.params.clone();
    let handle = tokio::spawn(async move {
        if let Err(error) = tokio::task::spawn_blocking(move || {
            observe_tool_workspaces(&observed_tool_name, &observed_params);
        })
        .await
        {
            warn!(error = %error, "tool workspace observation task failed");
        }
        if codeloupe_mcp::cancellation::is_cancelled(&task_cancellation_key) {
            schedule_cancellation_cleanup(task_cancellation_key);
            return;
        }
        let response = run_tool_call(
            tool_call.id,
            tool_call.params,
            tool_call.tool_name,
            tool_call.tool_timeout_secs,
            task_cancellation_key.clone(),
        )
        .await;
        let _ = tx.send(QueuedMessage::response(
            tool_call.request_number,
            tool_call.transport_mode,
            response,
        ));
        if codeloupe_mcp::cancellation::is_cancelled(&task_cancellation_key) {
            schedule_cancellation_cleanup(task_cancellation_key);
        } else {
            codeloupe_mcp::cancellation::remove(&task_cancellation_key);
        }
    });
    if let Some(previous) = active_tool_requests.insert(
        request_id_key,
        ActiveToolRequest {
            cancellation_key,
            handle,
        },
    ) {
        codeloupe_mcp::cancellation::cancel(&previous.cancellation_key);
        previous.handle.abort();
        schedule_cancellation_cleanup(previous.cancellation_key);
    }
}

async fn run_tool_call(
    id: Value,
    params: Value,
    tool_name: String,
    tool_timeout_secs: u64,
    cancellation_key: String,
) -> JsonRpcResponse {
    debug!(
        tool = %tool_name,
        timeout_s = tool_timeout_secs,
        "-> tool call start"
    );
    let start = Instant::now();
    let advice_arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let timeout_duration = tool_timeout_duration(&tool_name, tool_timeout_secs);

    match timeout(
        timeout_duration,
        tools::call_tool_with_cancellation(params, Some(cancellation_key.clone())),
    )
    .await
    {
        Ok(Ok(result)) => {
            let elapsed = start.elapsed();
            debug!(
                tool = %tool_name,
                elapsed_ms = elapsed.as_millis() as u64,
                "OK tool call"
            );
            if elapsed.as_millis() > 5000 {
                warn!(
                    tool = %tool_name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "SLOW tool call (>5s)"
                );
            }
            JsonRpcResponse::success(id, result)
        }
        Ok(Err(e)) => {
            error!(
                tool = %tool_name,
                error = %e,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "tool call error"
            );
            JsonRpcResponse::success(
                id,
                json!({
                    "isError": true,
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string(&tools::structured_tool_error(&e.to_string()))
                            .unwrap_or_default()
                    }]
                }),
            )
        }
        Err(_) => {
            codeloupe_mcp::cancellation::cancel(&cancellation_key);
            let elapsed_ms = start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            let progress = codeloupe_mcp::cancellation::progress(&cancellation_key);
            error!(
                tool = %tool_name,
                timeout_s = tool_timeout_secs,
                "tool call timeout"
            );
            let mut timeout_details = json!({
                "error": "tool_timeout",
                "tool": tool_name,
                "timeout_seconds": tool_timeout_secs,
                "timeout_milliseconds": timeout_duration.as_millis().min(u128::from(u64::MAX)) as u64,
                "elapsed_ms": elapsed_ms,
                "files_scanned": progress.entries_processed,
                "narrower_candidates": timeout_narrower_candidates(
                    &advice_arguments,
                    progress.workspace_root.as_deref(),
                ),
                "suggestion": "Retry with a narrower paths value and use includes/excludes filters."
            });
            if let Some(advice) = workspace_control::index_advice_for_timeout(
                &tool_name,
                &advice_arguments,
                progress.entries_processed,
                elapsed_ms,
                progress.workspace_root.as_deref(),
            ) && let Some(object) = timeout_details.as_object_mut()
            {
                object.insert("index_advice".to_string(), advice);
            }
            JsonRpcResponse::success(
                id,
                json!({
                    "isError": true,
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string(&timeout_details).unwrap_or_default()
                    }]
                }),
            )
        }
    }
}

fn timeout_narrower_candidates(args: &Value, progress_workspace: Option<&str>) -> Vec<Value> {
    let root = progress_workspace
        .map(PathBuf::from)
        .or_else(|| {
            args.get("paths")
                .and_then(Value::as_array)
                .and_then(|paths| paths.first())
                .and_then(Value::as_str)
                .map(common::resolve_tool_path)
        })
        .or_else(|| {
            args.get("path")
                .and_then(Value::as_str)
                .map(common::resolve_tool_path)
        })
        .or_else(|| workspace_control::active_workspace().map(|(root, _)| root));
    let Some(root) = root else {
        return Vec::new();
    };
    let mut candidates = std::fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .is_some_and(|file_type| file_type.is_dir())
                .then(|| json!({ "path": common::normalize_display_path(&entry.path()) }))
        })
        .take(5)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.to_string());
    candidates
}

fn negotiated_protocol_version(params: Option<&Value>) -> &'static str {
    let requested = params
        .and_then(|value| value.get("protocolVersion"))
        .and_then(Value::as_str);
    requested
        .and_then(|version| {
            SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .find(|supported| *supported == version)
        })
        .unwrap_or(LATEST_PROTOCOL_VERSION)
}

fn request_id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| id.to_string())
}

fn queue_roots_list_request(
    response_tx: &mpsc::UnboundedSender<QueuedMessage>,
    request_number: u64,
    transport_mode: TransportMode,
    sequence: &mut u64,
    pending: &mut HashSet<String>,
) -> anyhow::Result<()> {
    *sequence = sequence.saturating_add(1);
    let id = Value::String(format!("codeloupe-roots-{sequence}"));
    pending.insert(request_id_key(&id));
    response_tx
        .send(QueuedMessage::roots_list_request(
            request_number,
            transport_mode,
            id,
        ))
        .map_err(|error| anyhow::anyhow!("failed to queue roots/list request: {error}"))
}

fn handle_client_response(
    response: JsonRpcClientResponse,
    pending_roots_requests: &mut HashSet<String>,
) {
    let id_key = request_id_key(&response.id);
    if !pending_roots_requests.remove(&id_key) {
        debug!(id = %id_key, "Ignoring response for unknown server request");
        return;
    }
    if response.jsonrpc != "2.0" {
        warn!(id = %id_key, "Ignoring roots/list response with invalid JSON-RPC version");
        return;
    }
    if let Some(error) = response.error {
        warn!(id = %id_key, error = %error, "Client rejected roots/list request");
        return;
    }
    let Some(result) = response.result else {
        warn!(id = %id_key, "roots/list response omitted result");
        return;
    };
    let Some(roots) = extract_roots_list_result(&result) else {
        warn!(id = %id_key, "roots/list response has invalid roots payload");
        return;
    };
    let registered = workspace_control::replace_client_roots_list(roots);
    info!(
        id = %id_key,
        workspace_count = registered.len(),
        "Updated MCP client roots"
    );
}

fn cancel_active_tool_request(
    params: Option<&Value>,
    active_tool_requests: &mut HashMap<String, ActiveToolRequest>,
) {
    let Some(request_id) = params.and_then(|value| value.get("requestId")) else {
        return;
    };
    let request_id_key = request_id_key(request_id);
    let Some(active) = active_tool_requests.remove(&request_id_key) else {
        return;
    };
    codeloupe_mcp::cancellation::cancel(&active.cancellation_key);
    active.handle.abort();
    schedule_cancellation_cleanup(active.cancellation_key);
}

fn schedule_cancellation_cleanup(cancellation_key: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(CANCEL_TOKEN_RETENTION_SECS)).await;
        codeloupe_mcp::cancellation::remove(&cancellation_key);
    });
}

fn parse_content_length_header(line: &str) -> Option<usize> {
    let (name, value) = line.split_once(':')?;
    if !name.trim().eq_ignore_ascii_case("content-length") {
        return None;
    }
    value.trim().parse::<usize>().ok()
}

async fn read_next_message(
    reader: &mut BufReader<tokio::io::Stdin>,
    line_buf: &mut Vec<u8>,
    body_buf: &mut Vec<u8>,
) -> Result<Option<(String, TransportMode, usize)>, MessageReadError> {
    loop {
        let Some((n, line_too_long)) =
            read_bounded_line(reader, line_buf, MAX_JSON_RPC_MESSAGE_BYTES)
                .await
                .map_err(|err| MessageReadError {
                    message: err.to_string(),
                    transport_mode: TransportMode::Line,
                    bytes_read: 0,
                    recoverable: false,
                })?
        else {
            return Ok(None);
        };

        if line_too_long {
            return Err(MessageReadError {
                message: format!("JSON-RPC line exceeds {} bytes", MAX_JSON_RPC_MESSAGE_BYTES),
                transport_mode: TransportMode::Line,
                bytes_read: n,
                recoverable: true,
            });
        }

        let line = std::str::from_utf8(line_buf).map_err(|err| MessageReadError {
            message: format!("Invalid UTF-8 in JSON-RPC line: {err}"),
            transport_mode: TransportMode::Line,
            bytes_read: n,
            recoverable: true,
        })?;

        if line.trim().is_empty() {
            continue;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            return Ok(Some((line.to_string(), TransportMode::Line, n)));
        }

        // Not JSON start; try MCP framed headers.
        if !line.contains(':') {
            // Unknown line format, let parser handle it as line mode.
            return Ok(Some((line.to_string(), TransportMode::Line, n)));
        }
        if n > MAX_MCP_HEADER_LINE_BYTES {
            return Err(MessageReadError {
                message: format!(
                    "MCP header line exceeds {} bytes",
                    MAX_MCP_HEADER_LINE_BYTES
                ),
                transport_mode: TransportMode::Framed,
                bytes_read: n,
                recoverable: false,
            });
        }

        let mut content_length = parse_content_length_header(line);
        let mut header_bytes = n;

        loop {
            let Some((h, header_too_long)) =
                read_bounded_line(reader, line_buf, MAX_MCP_HEADER_LINE_BYTES)
                    .await
                    .map_err(|err| MessageReadError {
                        message: err.to_string(),
                        transport_mode: TransportMode::Framed,
                        bytes_read: header_bytes,
                        recoverable: false,
                    })?
            else {
                return Err(MessageReadError {
                    message: "Unexpected EOF while reading MCP framed headers".to_string(),
                    transport_mode: TransportMode::Framed,
                    bytes_read: header_bytes,
                    recoverable: false,
                });
            };
            header_bytes = header_bytes.saturating_add(h);

            if header_too_long || header_bytes > MAX_MCP_HEADER_BYTES {
                return Err(MessageReadError {
                    message: format!("MCP headers exceed {} bytes", MAX_MCP_HEADER_BYTES),
                    transport_mode: TransportMode::Framed,
                    bytes_read: header_bytes,
                    recoverable: false,
                });
            }

            let header_line = std::str::from_utf8(line_buf).map_err(|err| MessageReadError {
                message: format!("Invalid UTF-8 in MCP header: {err}"),
                transport_mode: TransportMode::Framed,
                bytes_read: header_bytes,
                recoverable: false,
            })?;

            if header_line == "\n" || header_line == "\r\n" {
                break;
            }

            if content_length.is_none() {
                content_length = parse_content_length_header(header_line);
            }
        }

        let len = content_length.ok_or_else(|| MessageReadError {
            message: "Missing Content-Length header in MCP frame".to_string(),
            transport_mode: TransportMode::Framed,
            bytes_read: header_bytes,
            recoverable: false,
        })?;

        if len > MAX_JSON_RPC_MESSAGE_BYTES {
            return Err(MessageReadError {
                message: format!("MCP payload exceeds {} bytes", MAX_JSON_RPC_MESSAGE_BYTES),
                transport_mode: TransportMode::Framed,
                bytes_read: header_bytes,
                recoverable: false,
            });
        }

        body_buf.resize(len, 0);
        reader
            .read_exact(body_buf)
            .await
            .map_err(|err| MessageReadError {
                message: format!("Failed to read MCP framed payload: {err}"),
                transport_mode: TransportMode::Framed,
                bytes_read: header_bytes,
                recoverable: false,
            })?;

        let body = std::str::from_utf8(body_buf)
            .map_err(|err| MessageReadError {
                message: format!("Invalid UTF-8 in framed payload: {err}"),
                transport_mode: TransportMode::Framed,
                bytes_read: header_bytes + len,
                recoverable: true,
            })?
            .to_string();

        return Ok(Some((body, TransportMode::Framed, header_bytes + len)));
    }
}

async fn read_bounded_line(
    reader: &mut BufReader<tokio::io::Stdin>,
    buffer: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<Option<(usize, bool)>> {
    buffer.clear();
    let mut total = 0usize;
    let mut too_long = false;

    loop {
        let (consume_len, found_newline) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if total == 0 {
                    Ok(None)
                } else {
                    Ok(Some((total, too_long)))
                };
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let consume_len = newline.map_or(available.len(), |index| index + 1);
            if !too_long && buffer.len().saturating_add(consume_len) <= max_bytes {
                buffer.extend_from_slice(&available[..consume_len]);
            } else {
                too_long = true;
            }
            (consume_len, newline.is_some())
        };

        reader.consume(consume_len);
        total = total.saturating_add(consume_len);
        if found_newline {
            return Ok(Some((total, too_long)));
        }
    }
}

fn write_message(
    stdout: &mut io::Stdout,
    message: &Value,
    mode: TransportMode,
) -> anyhow::Result<usize> {
    let out = serde_json::to_string(message)?;
    match mode {
        TransportMode::Framed => {
            let header = format!("Content-Length: {}\r\n\r\n", out.len());
            write!(stdout, "{}{}", header, out)?;
            stdout.flush()?;
            Ok(header.len() + out.len())
        }
        TransportMode::Line => {
            writeln!(stdout, "{}", out)?;
            stdout.flush()?;
            Ok(out.len() + 1)
        }
    }
}

fn maybe_set_index_roots(params: &serde_json::Value) {
    let roots = extract_index_roots(params);
    for root in &roots {
        workspace_control::register_client_workspace(root.clone(), "client_initialize");
    }
    info!(
        workspace_count = roots.len(),
        "Captured client workspace roots"
    );
}

fn extract_index_roots(params: &serde_json::Value) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(ws_root) = params
        .get("clientInfo")
        .and_then(|v| v.get("workspaceRoot"))
        .and_then(|v| v.as_str())
    {
        push_unique_root(&mut roots, common::path_from_input(ws_root));
    }

    if let Some(root_entries) = params.get("roots").and_then(|v| v.as_array()) {
        for root in root_entries {
            if let Some(uri) = root.get("uri").and_then(|v| v.as_str())
                && let Some(path) = uri_to_path(uri)
            {
                push_unique_root(&mut roots, path);
            }
        }
    }

    if let Some(root_uri) = params.get("rootUri").and_then(|v| v.as_str())
        && let Some(path) = uri_to_path(root_uri)
    {
        push_unique_root(&mut roots, path);
    }

    if let Some(workspace_folders) = params.get("workspaceFolders").and_then(|v| v.as_array()) {
        for folder in workspace_folders {
            if let Some(uri) = folder.get("uri").and_then(|v| v.as_str())
                && let Some(path) = uri_to_path(uri)
            {
                push_unique_root(&mut roots, path);
            }
        }
    }

    normalize_and_collapse_roots(roots)
}

fn extract_roots_list_result(result: &Value) -> Option<Vec<PathBuf>> {
    let root_entries = result.get("roots")?.as_array()?;
    let mut roots = Vec::new();
    for root in root_entries {
        if let Some(uri) = root.get("uri").and_then(Value::as_str)
            && let Some(path) = uri_to_path(uri)
        {
            push_unique_root(&mut roots, path);
        }
    }
    Some(normalize_and_collapse_roots(roots))
}

fn normalize_and_collapse_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut roots = roots
        .into_iter()
        .filter(|path| path.exists() && path.is_dir())
        .map(|path| canonicalize_existing_path(&path))
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| normalize_root_key(left).cmp(&normalize_root_key(right)))
    });

    let mut collapsed: Vec<PathBuf> = Vec::new();
    for root in roots {
        if collapsed
            .iter()
            .any(|parent| common::path_is_within(&root, parent))
        {
            continue;
        }
        collapsed.push(root);
    }
    collapsed
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    if let Some(path) = common::uri_to_path(uri) {
        return Some(path);
    }
    if !uri.contains("://") {
        return Some(PathBuf::from(uri));
    }

    None
}

fn start_indexer_if_needed() {
    workspace_control::start_persisted_and_configured_indexes();

    if let Ok(current_dir) = std::env::current_dir()
        && common::looks_like_workspace_root(&current_dir)
    {
        info!(
            root = %current_dir.display(),
            "Registering current_dir() as an index candidate"
        );
        workspace_control::observe_workspace(current_dir, "process_current_dir");
    }
}

fn observe_tool_workspaces(tool_name: &str, params: &Value) {
    if matches!(
        tool_name,
        "create_file"
            | "create_directory"
            | "edit_file"
            | "edit_files"
            | "delete_file"
            | "convert_file_format"
            | "batch_tool_call"
    ) {
        return;
    }

    let arguments = match params.get("arguments") {
        Some(arguments) => arguments,
        None => return,
    };

    let roots = infer_workspace_roots_from_tool_arguments(arguments);
    for root in roots {
        workspace_control::observe_workspace(root, format!("tool_call:{tool_name}"));
    }
}

fn infer_workspace_roots_from_tool_arguments(arguments: &Value) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    collect_workspace_roots(arguments, None, &mut roots);

    let mut inferred = Vec::new();
    for candidate in roots {
        if let Some(workspace_root) = discover_workspace_root_for_path(&candidate) {
            push_unique_root(&mut inferred, workspace_root);
        }
    }

    inferred
}

fn collect_workspace_roots(value: &Value, parent_key: Option<&str>, roots: &mut Vec<PathBuf>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                collect_workspace_roots(child, Some(key.as_str()), roots);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_workspace_roots(item, parent_key, roots);
            }
        }
        Value::String(raw) => {
            if parent_key.is_some_and(is_workspace_path_key)
                && let Some(candidate) = resolve_tool_path_candidate(raw)
            {
                roots.push(candidate);
            }
        }
        _ => {}
    }
}

fn is_workspace_path_key(key: &str) -> bool {
    matches!(
        key,
        "path"
            | "paths"
            | "file_path"
            | "repo_path"
            | "archive_path"
            | "input_file"
            | "output_file"
            | "file_hint"
            | "left_path"
            | "right_path"
    )
}

fn resolve_tool_path_candidate(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() || common::contains_path_glob(raw) {
        return None;
    }

    existing_anchor_for_path(common::resolve_tool_path(raw))
}

fn existing_anchor_for_path(path: PathBuf) -> Option<PathBuf> {
    let mut current = path.as_path();
    while !current.exists() {
        current = current.parent()?;
    }

    Some(canonicalize_existing_path(current))
}

fn discover_workspace_root_for_path(path: &Path) -> Option<PathBuf> {
    if let Some(indexed_root) = indexer::indexed_workspace_root_for_path(path) {
        return Some(indexed_root);
    }

    common::discover_workspace_root(path)
}

fn canonicalize_existing_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn push_unique_root(roots: &mut Vec<PathBuf>, candidate: PathBuf) {
    let candidate_key = normalize_root_key(&candidate);
    if roots
        .iter()
        .any(|existing| normalize_root_key(existing) == candidate_key)
    {
        return;
    }

    roots.push(candidate);
}

fn normalize_root_key(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        normalized.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

fn register_explicit_workspaces() -> anyhow::Result<()> {
    for (root, source) in explicit_workspace_roots()? {
        let approved = workspace_control::index_mode() != workspace_control::IndexMode::Off;
        if workspace_control::register_configured_workspace(root.clone(), source.clone(), approved)
            .is_none()
        {
            warn!(root = %root.display(), source, "Configured workspace does not exist or is not a directory");
        }
    }
    Ok(())
}

fn explicit_workspace_roots() -> anyhow::Result<Vec<(PathBuf, String)>> {
    let mut roots = Vec::new();
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        let rendered = argument.to_string_lossy();
        if rendered == "--workspace" {
            let path = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--workspace requires a path"))?;
            push_explicit_workspace(&mut roots, PathBuf::from(path), "cli");
        } else if let Some(path) = rendered.strip_prefix("--workspace=") {
            if path.is_empty() {
                return Err(anyhow::anyhow!("--workspace requires a path"));
            }
            push_explicit_workspace(&mut roots, PathBuf::from(path), "cli");
        }
    }

    for variable in [
        "CODELOUPE_MCP_WORKSPACES",
        "CODELOUPE_MCP_WORKSPACE",
        "CODELOUPE_WORKSPACE",
    ] {
        if let Some(value) = std::env::var_os(variable) {
            for path in std::env::split_paths(&value) {
                push_explicit_workspace(&mut roots, path, format!("env:{variable}"));
            }
        }
    }
    Ok(roots)
}

fn push_explicit_workspace(
    roots: &mut Vec<(PathBuf, String)>,
    path: PathBuf,
    source: impl Into<String>,
) {
    let path = common::canonicalize_if_exists(path);
    let key = normalize_root_key(&path);
    if roots
        .iter()
        .any(|(existing, _)| normalize_root_key(existing) == key)
    {
        return;
    }
    roots.push((path, source.into()));
}

fn tool_call_timeout_secs(params: &Value) -> std::result::Result<u64, String> {
    let Some(requested) = params.get("timeout_seconds") else {
        return Ok(DEFAULT_TOOL_TIMEOUT_SECS);
    };
    let Some(requested_seconds) = requested.as_u64() else {
        return Err("tools/call timeout_seconds must be an integer".to_string());
    };
    if !(DEFAULT_TOOL_TIMEOUT_SECS..=MAX_TOOL_TIMEOUT_SECS).contains(&requested_seconds) {
        return Err(format!(
            "tools/call timeout_seconds must be between {DEFAULT_TOOL_TIMEOUT_SECS} and {MAX_TOOL_TIMEOUT_SECS}"
        ));
    }

    Ok(requested_seconds)
}

fn tool_timeout_duration(_tool_name: &str, timeout_secs: u64) -> Duration {
    #[cfg(debug_assertions)]
    if std::env::var("CODELOUPE_MCP_TEST_TOOL_TIMEOUT_NAME")
        .ok()
        .is_some_and(|name| name == _tool_name)
        && let Some(timeout_ms) = std::env::var("CODELOUPE_MCP_TEST_TOOL_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
    {
        return Duration::from_millis(timeout_ms);
    }

    Duration::from_secs(timeout_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_timeout_defaults_to_sixty_seconds() {
        assert_eq!(tool_call_timeout_secs(&json!({})).unwrap(), 60);
    }

    #[test]
    fn tool_timeout_accepts_valid_extension_and_rejects_above_maximum() {
        assert_eq!(
            tool_call_timeout_secs(&json!({"timeout_seconds": 120})).unwrap(),
            120
        );
        assert!(tool_call_timeout_secs(&json!({"timeout_seconds": 900})).is_err());
    }

    #[test]
    fn tool_timeout_rejects_invalid_values() {
        assert!(tool_call_timeout_secs(&json!({"timeout_seconds": 10})).is_err());
        assert!(tool_call_timeout_secs(&json!({"timeout_seconds": "120"})).is_err());
    }
}

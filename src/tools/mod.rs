use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

mod ast_support;
mod atomic_write;
mod diff_support;
mod file_io_error;
mod output_format;
pub(crate) mod path_filters;
mod search_snippet;
mod text_encoding;

pub mod batch_tool_call;
pub mod compare_directories;
pub mod compare_symbols;
pub mod content_index_status;
pub mod convert_file_format;
pub mod count_file_lines;
pub mod create_directory;
pub mod create_file;
pub mod delete_file;
pub mod edit_file;
pub mod edit_files;
pub mod file_hash;
pub mod file_summary;
pub mod find_definition;
pub mod find_references;
pub mod fuzzy_find;
pub mod get_call_graph;
pub mod get_symbols;
pub mod index_gc;
pub mod list_exports;
pub mod list_history;
pub mod list_imports;
pub mod peek_archive;
pub mod project_map;
pub mod read_file;
pub mod read_snippets;
pub mod read_symbol_body;
pub mod resolve_path;
pub mod search_workspace;
pub mod server_health;
pub mod text_search;
pub mod undo_change;
pub mod validate_json;
pub mod warm_content_index;
pub mod workspace_index;
pub mod workspace_stats;

pub fn list_tools() -> Vec<Value> {
    full_tool_schemas()
        .into_iter()
        .map(compact_public_schema)
        .collect()
}

fn full_tool_schemas() -> Vec<Value> {
    let mut tools = vec![
        resolve_path::schema(),
        search_workspace::schema(),
        text_search::schema(),
        read_file::schema(),
        count_file_lines::schema(),
        convert_file_format::schema(),
        create_file::schema(),
        create_directory::schema(),
        delete_file::schema(),
        edit_file::schema(),
        edit_files::schema(),
        file_hash::schema(),
        file_summary::schema(),
        validate_json::schema(),
        read_snippets::schema(),
        read_symbol_body::schema(),
        list_imports::schema(),
        list_exports::schema(),
        compare_directories::schema(),
        compare_symbols::schema(),
        fuzzy_find::schema(),
        project_map::schema(),
        get_symbols::schema(),
        workspace_stats::schema(),
        workspace_index::schema(),
        server_health::schema(),
        index_gc::schema(),
        list_history::schema(),
        content_index_status::schema(),
        warm_content_index::schema(),
        peek_archive::schema(),
        find_definition::schema(),
        find_references::schema(),
        get_call_graph::schema(),
        batch_tool_call::schema(),
        undo_change::schema(),
    ];
    for tool in &mut tools {
        add_write_risk_acknowledgement_schema(tool);
    }
    tools
}

fn add_write_risk_acknowledgement_schema(tool: &mut Value) {
    let Some(name) = tool.get("name").and_then(Value::as_str) else {
        return;
    };
    if !is_write_tool(name) {
        return;
    }
    let Some(properties) = tool
        .pointer_mut("/inputSchema/properties")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    properties.insert(
        "acknowledge_risk".to_string(),
        json!({
            "type": "boolean",
            "description": "Required on a retry before a critical-risk write is allowed. The first unacknowledged call performs no filesystem mutation."
        }),
    );
}

fn compact_public_schema(mut tool: Value) -> Value {
    if let Some(object) = tool.as_object_mut() {
        object.remove("title");
        if let Some(input_schema) = object.get_mut("inputSchema") {
            remove_schema_descriptions(input_schema);
        }
    }
    tool
}

fn remove_schema_descriptions(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("description");
            for child in object.values_mut() {
                remove_schema_descriptions(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                remove_schema_descriptions(item);
            }
        }
        _ => {}
    }
}

pub async fn call_tool(params: Value) -> Result<Value> {
    call_tool_with_cancellation(params, None).await
}

pub async fn call_tool_with_cancellation(
    params: Value,
    cancellation_key: Option<String>,
) -> Result<Value> {
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(call_tool_inner_with_cancellation(
            params,
            cancellation_key.as_deref(),
        ))
    })
    .await
    .map_err(|err| anyhow::anyhow!("tool worker failed to join: {err}"))?
}

#[derive(Debug)]
struct WriteTarget {
    path: PathBuf,
    acknowledged: bool,
}

#[derive(Default)]
struct WritePreflight {
    warnings: Vec<String>,
    critical_paths: Vec<PathBuf>,
    confirmation_error: Option<Value>,
}

fn preflight_write_targets(tool_name: &str, arguments: &Value) -> WritePreflight {
    let mut raw_targets = Vec::new();
    collect_write_targets(tool_name, arguments, &mut raw_targets);
    let mut warnings = BTreeMap::<String, String>::new();
    let mut unacknowledged_critical = Vec::new();
    let mut critical_paths = Vec::new();
    for target in raw_targets {
        let classification = crate::security::path_guard::GUARD.classify_path(&target.path);
        let canonical_display = crate::common::normalize_display_path(&classification.canonical);
        if let Some(warning) = classification.warning.clone() {
            warnings.entry(canonical_display).or_insert(warning);
        }
        if classification.tier == crate::security::path_guard::Tier::CriticalRiskWarn {
            critical_paths.push(classification.canonical.clone());
            if !target.acknowledged {
                unacknowledged_critical.push((target.path, classification.canonical));
            }
        }
    }
    let warnings = warnings.into_values().collect::<Vec<_>>();
    let confirmation_error = unacknowledged_critical.first().map(|(path, canonical)| {
        let display_path = crate::common::normalize_display_path(path);
        let canonical_path = crate::common::normalize_display_path(canonical);
        let mut response = json!({
            "__mcp_is_error": true,
            "error": {
                "code": "risk_confirmation_required",
                "message": "Critical write risk requires confirmation before any filesystem mutation. Review the warning and retry with acknowledge_risk=true."
            },
            "path": display_path,
        });
        if display_path != canonical_path {
            response["canonical_path"] = json!(canonical_path);
        }
        if unacknowledged_critical.len() > 1 {
            response["critical_paths"] = json!(unacknowledged_critical
                .iter()
                .map(|(_, canonical)| crate::common::normalize_display_path(canonical))
                .collect::<Vec<_>>());
        }
        response
    });
    WritePreflight {
        warnings,
        critical_paths,
        confirmation_error,
    }
}

fn collect_write_targets(tool_name: &str, arguments: &Value, targets: &mut Vec<WriteTarget>) {
    let acknowledged = arguments
        .get("acknowledge_risk")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match tool_name {
        "create_directory" => push_write_path(arguments, "path", acknowledged, targets),
        "create_file" | "delete_file" | "edit_file" | "convert_file_format" => {
            push_write_path(arguments, "path", acknowledged, targets)
        }
        "edit_files" => {
            for file in arguments
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                push_write_path(file, "path", acknowledged, targets);
            }
        }
        "undo_change" => {
            if let Some(record) = arguments
                .get("entry_id")
                .and_then(Value::as_str)
                .and_then(crate::history::get_record)
            {
                targets.push(WriteTarget {
                    path: PathBuf::from(record.path),
                    acknowledged,
                });
            }
        }
        "batch_tool_call" => {
            for call in arguments
                .get("calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(nested_tool) = call.get("tool").and_then(Value::as_str) else {
                    continue;
                };
                let nested_args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                collect_write_targets(nested_tool, &nested_args, targets);
            }
        }
        _ => {}
    }
}

fn push_write_path(
    arguments: &Value,
    field: &str,
    acknowledged: bool,
    targets: &mut Vec<WriteTarget>,
) {
    if let Some(path) = arguments.get(field).and_then(Value::as_str) {
        targets.push(WriteTarget {
            path: crate::common::resolve_write_tool_path(path),
            acknowledged,
        });
    }
}

fn is_write_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "edit_file"
            | "create_file"
            | "delete_file"
            | "create_directory"
            | "convert_file_format"
            | "undo_change"
            | "edit_files"
    )
}

fn redact_sensitive_write_fields(value: &mut Value, critical_paths: &[PathBuf]) {
    if critical_paths.is_empty() {
        return;
    }
    remove_field_recursive(value, "actual_hash");
}

fn remove_field_recursive(value: &mut Value, field: &str) {
    match value {
        Value::Object(object) => {
            object.remove(field);
            object
                .values_mut()
                .for_each(|child| remove_field_recursive(child, field));
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|child| remove_field_recursive(child, field)),
        _ => {}
    }
}

fn encode_tool_data(mut data: Value, is_error: bool) -> Value {
    if let Some(object) = data.as_object_mut() {
        object.remove("__mcp_is_error");
    }
    strip_null_fields(&mut data);
    drop_repeated_fields(&mut data);
    let mut response = json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string(&data).unwrap_or_default()
        }]
    });
    if is_error {
        response["isError"] = Value::Bool(true);
    }
    response
}

pub(crate) async fn call_tool_inner_with_cancellation(
    params: Value,
    cancellation_key: Option<&str>,
) -> Result<Value> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let (mut arguments, mut argument_warnings) = normalize_tool_arguments(name, arguments)?;
    if let Some(cancellation_key) = cancellation_key
        && let Some(object) = arguments.as_object_mut()
    {
        object.insert(
            crate::cancellation::ARG_KEY.to_string(),
            Value::String(cancellation_key.to_string()),
        );
    }

    let write_preflight = preflight_write_targets(name, &arguments);
    argument_warnings.extend(write_preflight.warnings.clone());
    if let Some(mut error) = write_preflight.confirmation_error {
        merge_argument_warnings(&mut error, argument_warnings);
        return Ok(encode_tool_data(error, true));
    }

    let advice_arguments = arguments.clone();
    let started_at = Instant::now();
    // MCP tool responses are wrapped in the standard content array shape.
    let result = match name {
        "resolve_path" => resolve_path::execute(&arguments).await,
        "search_workspace" => search_workspace::execute(&arguments).await,
        "text_search" => text_search::execute(&arguments).await,
        "read_file_range" => read_file::execute(&arguments).await,
        "count_file_lines" => count_file_lines::execute(&arguments).await,
        "convert_file_format" => convert_file_format::execute(&arguments).await,
        "create_file" => create_file::execute(&arguments).await,
        "create_directory" => create_directory::execute(&arguments).await,
        "delete_file" => delete_file::execute(&arguments).await,
        "edit_file" => edit_file::execute(&arguments).await,
        "edit_files" => edit_files::execute(&arguments).await,
        "file_hash" => file_hash::execute(&arguments).await,
        "file_summary" => file_summary::execute(&arguments).await,
        "validate_json" => validate_json::execute(&arguments).await,
        "read_snippets" => read_snippets::execute(&arguments).await,
        "read_symbol_body" => read_symbol_body::execute(&arguments).await,
        "list_imports" => list_imports::execute(&arguments).await,
        "list_exports" => list_exports::execute(&arguments).await,
        "compare_directories" => compare_directories::execute(&arguments).await,
        "compare_symbols" => compare_symbols::execute(&arguments).await,
        "fuzzy_find" => fuzzy_find::execute(&arguments).await,
        "project_map" => project_map::execute(&arguments).await,
        "get_symbols" => get_symbols::execute(&arguments).await,
        "workspace_stats" => workspace_stats::execute(&arguments).await,
        "workspace_index" => workspace_index::execute(&arguments).await,
        "server_health" => server_health::execute(&arguments).await,
        "index_gc" => index_gc::execute(&arguments).await,
        "list_history" => list_history::execute(&arguments).await,
        "content_index_status" => content_index_status::execute(&arguments).await,
        "warm_content_index" => warm_content_index::execute(&arguments).await,
        "peek_archive" => peek_archive::execute(&arguments).await,
        "find_definition" => find_definition::execute(&arguments).await,
        "find_references" => find_references::execute(&arguments).await,
        "get_call_graph" => get_call_graph::execute(&arguments).await,
        "batch_tool_call" => batch_tool_call::execute(&arguments).await,
        "undo_change" => undo_change::execute(&arguments).await,
        _ => return Err(anyhow::anyhow!("Tool not found: {}", name)),
    };

    match result {
        Ok(mut data) => {
            redact_sensitive_write_fields(&mut data, &write_preflight.critical_paths);
            crate::workspace_control::maybe_attach_index_advice(
                name,
                &advice_arguments,
                &mut data,
                started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            );
            let is_error = data
                .get("__mcp_is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || data.get("success").and_then(Value::as_bool) == Some(false)
                || data.get("status").and_then(Value::as_str) == Some("error")
                || data.get("error").is_some_and(Value::is_object);
            if let Some(object) = data.as_object_mut() {
                object.remove("__mcp_is_error");
            }
            if is_error {
                attach_structured_error(&mut data);
                compact_write_error_response(name, &mut data);
            }
            if let Some(raw_text) = data.get("__mcp_raw_text").and_then(|v| v.as_str()) {
                let mut content = vec![json!({ "type": "text", "text": raw_text })];
                if !argument_warnings.is_empty() {
                    content.push(json!({
                        "type": "text",
                        "text": serde_json::to_string(&json!({ "warnings": argument_warnings }))
                            .unwrap_or_default()
                    }));
                }
                let mut response = json!({ "content": content });
                if is_error {
                    response["isError"] = Value::Bool(true);
                }
                return Ok(response);
            }
            strip_null_fields(&mut data);
            compact_output_paths(name, &advice_arguments, &mut data);
            drop_repeated_fields(&mut data);
            compact_success_response(name, &mut data);
            merge_argument_warnings(&mut data, argument_warnings);
            let mut response = json!({ "content": [{ "type": "text", "text": serde_json::to_string(&data).unwrap_or_default() }] });
            if is_error {
                response["isError"] = Value::Bool(true);
            }
            Ok(response)
        }
        Err(e) => {
            let payload = structured_tool_error(&e.to_string());
            Ok(json!({
                "isError": true,
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string(&payload).unwrap_or_default()
                }]
            }))
        }
    }
}

pub fn structured_tool_error(message: &str) -> Value {
    if let Ok(mut parsed) = serde_json::from_str::<Value>(message)
        && parsed.is_object()
    {
        sanitize_error_strings(&mut parsed);
        let object = parsed.as_object_mut().expect("parsed object");
        if !object.get("error").is_some_and(Value::is_object) {
            let code = object
                .get("error_code")
                .and_then(Value::as_str)
                .or_else(|| object.get("recommendation").and_then(Value::as_str))
                .or_else(|| object.get("error").and_then(Value::as_str))
                .unwrap_or_else(|| classify_tool_error(message));
            let error_message = object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(message);
            object.insert(
                "error".to_string(),
                json!({
                    "code": code,
                    "message": error_message
                }),
            );
        }
        drop_repeated_fields(&mut parsed);
        return parsed;
    }
    let display_message = crate::common::sanitize_verbatim_path_prefixes(message);
    json!({
        "error": {
            "code": classify_tool_error(message),
            "message": display_message
        }
    })
}

fn attach_structured_error(data: &mut Value) {
    sanitize_error_strings(data);
    if data.get("error").is_some_and(Value::is_object) {
        return;
    }
    let code = data
        .get("error_code")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            classify_tool_error(
                data.get("message")
                    .and_then(Value::as_str)
                    .or_else(|| data.get("reason").and_then(Value::as_str))
                    .unwrap_or("Tool reported an error"),
            )
            .to_string()
        });
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| data.get("reason").and_then(Value::as_str))
        .unwrap_or("Tool reported an error");
    crate::common::insert_object_field(
        data,
        "error",
        json!({
            "code": code,
            "message": message
        }),
    );
}

fn sanitize_error_strings(value: &mut Value) {
    match value {
        Value::String(text) => {
            *text = crate::common::sanitize_verbatim_path_prefixes(text);
        }
        Value::Array(items) => {
            for item in items {
                sanitize_error_strings(item);
            }
        }
        Value::Object(object) => {
            for item in object.values_mut() {
                sanitize_error_strings(item);
            }
        }
        _ => {}
    }
}

fn classify_tool_error(message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    if message.contains("tool not found") {
        "tool_not_found"
    } else if message.contains("permission denied") || message.contains("access denied") {
        "permission_denied"
    } else if message.contains("not found") || message.contains("does not exist") {
        "not_found"
    } else if message.contains("too large") || message.contains("exceeds") {
        "too_large"
    } else if message.contains("timed out") || message.contains("timeout") {
        "timeout"
    } else if message.contains("missing")
        || message.contains("cannot be empty")
        || message.contains("invalid")
        || message.contains("unsupported")
        || message.contains("expected ")
    {
        "invalid_argument"
    } else {
        "tool_error"
    }
}

fn normalize_tool_arguments(name: &str, mut arguments: Value) -> Result<(Value, Vec<String>)> {
    normalize_path_alias(name, &mut arguments)?;
    let tool = full_tool_schemas()
        .into_iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        .ok_or_else(|| anyhow::anyhow!("Tool not found: {name}"))?;
    let schema = tool
        .get("inputSchema")
        .ok_or_else(|| anyhow::anyhow!("Tool '{name}' has no input schema"))?;
    let mut warnings = Vec::new();
    validate_schema_value(&mut arguments, schema, "arguments", name, &mut warnings)?;
    Ok((arguments, warnings))
}

fn normalize_path_alias(name: &str, arguments: &mut Value) -> Result<()> {
    if !matches!(
        name,
        "find_definition"
            | "find_references"
            | "fuzzy_find"
            | "search_workspace"
            | "text_search"
            | "read_symbol_body"
            | "content_index_status"
            | "warm_content_index"
    ) {
        return Ok(());
    }
    let Some(object) = arguments.as_object_mut() else {
        return Ok(());
    };
    let singular = object.remove("path");
    match (singular, object.contains_key("paths")) {
        (Some(_), true) => Err(anyhow::anyhow!(
            "Invalid arguments for tool '{name}': use either 'path' or 'paths', not both"
        )),
        (Some(path), false) => {
            object.insert("paths".to_string(), Value::Array(vec![path]));
            Ok(())
        }
        (None, _) => Ok(()),
    }
}

fn validate_schema_value(
    value: &mut Value,
    schema: &Value,
    path: &str,
    tool_name: &str,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let expected_type = schema.get("type").and_then(Value::as_str);
    if let Some(expected_type) = expected_type {
        coerce_numeric_string(value, expected_type, path, warnings)?;
        if !value_matches_type(value, expected_type) {
            return Err(anyhow::anyhow!(
                "Invalid argument '{path}' for tool '{tool_name}': expected {expected_type}, got {}",
                value_type_name(value)
            ));
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        let choices = allowed
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(anyhow::anyhow!(
            "Invalid argument '{path}' for tool '{tool_name}': expected one of [{choices}]"
        ));
    }

    validate_numeric_bounds(value, schema, path, tool_name)?;

    if let Some(object) = value.as_object_mut() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(field) {
                    let required_path = if path == "arguments" {
                        field.to_string()
                    } else {
                        format!("{path}.{field}")
                    };
                    return Err(anyhow::anyhow!(
                        "Missing required argument '{required_path}' for tool '{tool_name}'"
                    ));
                }
            }
        }

        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            let fields = object.keys().cloned().collect::<Vec<_>>();
            for field in fields {
                let field_path = if path == "arguments" {
                    field.clone()
                } else {
                    format!("{path}.{field}")
                };
                if let Some(field_schema) = properties.get(&field) {
                    if let Some(field_value) = object.get_mut(&field) {
                        validate_schema_value(
                            field_value,
                            field_schema,
                            &field_path,
                            tool_name,
                            warnings,
                        )?;
                    }
                } else {
                    warnings.push(format!(
                        "Unknown argument '{field_path}' for tool '{tool_name}'"
                    ));
                }
            }
        }
    }

    if let Some(items) = value.as_array_mut()
        && let Some(item_schema) = schema.get("items")
    {
        for (index, item) in items.iter_mut().enumerate() {
            validate_schema_value(
                item,
                item_schema,
                &format!("{path}[{index}]"),
                tool_name,
                warnings,
            )?;
        }
    }

    Ok(())
}

fn validate_numeric_bounds(
    value: &Value,
    schema: &Value,
    path: &str,
    tool_name: &str,
) -> Result<()> {
    let Some(actual) = value.as_f64() else {
        return Ok(());
    };

    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && actual < minimum
    {
        return Err(anyhow::anyhow!(
            "Invalid argument '{path}' for tool '{tool_name}': value {actual} is below minimum {minimum}"
        ));
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
        && actual > maximum
    {
        return Err(anyhow::anyhow!(
            "Invalid argument '{path}' for tool '{tool_name}': value {actual} exceeds maximum {maximum}"
        ));
    }

    Ok(())
}

fn coerce_numeric_string(
    value: &mut Value,
    expected_type: &str,
    path: &str,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let Some(raw) = value.as_str() else {
        return Ok(());
    };
    let number = match expected_type {
        "integer" => raw
            .parse::<i64>()
            .map(serde_json::Number::from)
            .or_else(|_| raw.parse::<u64>().map(serde_json::Number::from))
            .ok(),
        "number" => raw
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64),
        _ => None,
    };
    if let Some(number) = number {
        *value = Value::Number(number);
        warnings.push(format!(
            "Coerced argument '{path}' from string to {expected_type}"
        ));
    }
    Ok(())
}

fn value_matches_type(value: &Value, expected_type: &str) -> bool {
    match expected_type {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        _ => true,
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub(crate) fn nest_diagnostics(
    mut response: Value,
    diagnostic_fields: &[&str],
    include_diagnostics: bool,
) -> Value {
    let mut diagnostics = serde_json::Map::new();
    let Some(object) = response.as_object_mut() else {
        return response;
    };
    for field in diagnostic_fields {
        if let Some(value) = object.remove(*field)
            && !value.is_null()
            && !matches!(&value, Value::Array(items) if items.is_empty())
            && !matches!(&value, Value::Object(items) if items.is_empty())
        {
            diagnostics.insert((*field).to_string(), value);
        }
    }
    if include_diagnostics && !diagnostics.is_empty() {
        object.insert("diagnostics".to_string(), Value::Object(diagnostics));
    }
    response
}

fn merge_argument_warnings(data: &mut Value, warnings: Vec<String>) {
    if warnings.is_empty() {
        return;
    }
    let Some(object) = data.as_object_mut() else {
        return;
    };
    let warning_values = warnings.into_iter().map(Value::String);
    match object.get_mut("warnings") {
        Some(Value::Array(existing)) => existing.extend(warning_values),
        _ => {
            object.insert(
                "warnings".to_string(),
                Value::Array(warning_values.collect()),
            );
        }
    }
}

/// Drops object fields whose value is `null` before a result is sent to the
/// client. A missing field carries the same meaning for agents and costs no
/// tokens; array elements are kept so positions stay stable.
fn strip_null_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, field| !field.is_null());
            map.values_mut().for_each(strip_null_fields);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_null_fields),
        _ => {}
    }
}

fn compact_output_paths(tool_name: &str, arguments: &Value, value: &mut Value) {
    let paths = match tool_name {
        "read_snippets" => collect_argument_file_paths(arguments, "requests"),
        "edit_files" => collect_argument_file_paths(arguments, "files"),
        "list_history" | "compare_symbols" => collect_output_file_paths(value),
        _ => Vec::new(),
    };
    let Some(root) = crate::common::common_path_root(&paths) else {
        return;
    };
    relativize_output_paths(value, &root);
    if let Some(object) = value.as_object_mut() {
        object
            .entry("root".to_string())
            .or_insert_with(|| Value::String(crate::common::normalize_display_path(&root)));
    }
}

fn collect_argument_file_paths(arguments: &Value, field: &str) -> Vec<std::path::PathBuf> {
    arguments
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("path").and_then(Value::as_str))
        .map(crate::common::resolve_tool_path)
        .filter_map(|path| path.parent().map(std::path::Path::to_path_buf))
        .collect()
}

fn collect_output_file_paths(value: &Value) -> Vec<std::path::PathBuf> {
    fn collect(value: &Value, paths: &mut Vec<std::path::PathBuf>) {
        match value {
            Value::Array(items) => items.iter().for_each(|item| collect(item, paths)),
            Value::Object(object) => {
                if let Some(path) = object.get("path").and_then(Value::as_str) {
                    let path = std::path::PathBuf::from(path);
                    if path.is_absolute()
                        && let Some(parent) = path.parent()
                    {
                        paths.push(parent.to_path_buf());
                    }
                }
                object.values().for_each(|item| collect(item, paths));
            }
            _ => {}
        }
    }

    let mut paths = Vec::new();
    collect(value, &mut paths);
    paths
}

fn relativize_output_paths(value: &mut Value, root: &std::path::Path) {
    match value {
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| relativize_output_paths(item, root)),
        Value::Object(object) => {
            for (field, item) in object.iter_mut() {
                if matches!(
                    field.as_str(),
                    "path" | "canonical_path" | "input_path" | "file_path"
                ) && let Some(path) = item.as_str()
                {
                    let path = std::path::Path::new(path);
                    if path.is_absolute()
                        && let Some(relative) = crate::common::relative_display_path(path, root)
                    {
                        *item = Value::String(if relative.is_empty() {
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or(".")
                                .to_string()
                        } else {
                            relative
                        });
                    }
                }
                relativize_output_paths(item, root);
            }
        }
        _ => {}
    }
}

fn drop_repeated_fields(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(drop_repeated_fields),
        Value::Object(object) => {
            for (redundant, primary) in [
                ("canonical_path", "path"),
                ("input_path", "path"),
                ("canonical_path", "root"),
                ("repo_root", "workspace_root"),
                ("name", "symbol"),
                ("name", "qualified_name"),
                ("qualified_name", "symbol"),
            ] {
                if object.contains_key(primary) && object.get(redundant) == object.get(primary) {
                    object.remove(redundant);
                }
            }
            if let Some(error) = object.get("error").and_then(Value::as_object) {
                let duplicate_code = object.get("error_code") == error.get("code");
                let duplicate_message = object.get("message") == error.get("message");
                let duplicate_reason = object.get("reason") == error.get("message");
                if duplicate_code {
                    object.remove("error_code");
                }
                if duplicate_message {
                    object.remove("message");
                }
                if duplicate_reason {
                    object.remove("reason");
                }
            }
            object.values_mut().for_each(drop_repeated_fields);
        }
        _ => {}
    }
}

fn compact_success_response(tool_name: &str, value: &mut Value) {
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let Some(source) = value.as_object() else {
        return;
    };
    let compact = match tool_name {
        "edit_file" => compact_edit_file_success(source),
        "create_file" => compact_create_file_success(source),
        "delete_file" => compact_delete_file_success(source),
        "create_directory" => compact_create_directory_success(source),
        "convert_file_format" => compact_convert_file_format_success(source),
        "undo_change" => compact_undo_change_success(source),
        "edit_files" => compact_edit_files_success(source),
        _ => return,
    };
    *value = Value::Object(compact);
}

fn compact_write_error_response(tool_name: &str, value: &mut Value) {
    if !matches!(
        tool_name,
        "edit_file"
            | "create_file"
            | "delete_file"
            | "create_directory"
            | "convert_file_format"
            | "undo_change"
            | "edit_files"
    ) || !value.get("error").is_some_and(Value::is_object)
    {
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for legacy_field in ["success", "error_code", "message", "reason"] {
        object.remove(legacy_field);
    }
}

fn compact_edit_file_success(
    source: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut compact = serde_json::Map::new();
    copy_response_fields(
        source,
        &mut compact,
        &["path", "replacements", "sha256_after", "history_entry_id"],
    );
    for flag in [
        "file_created",
        "encoding_changed",
        "line_endings_normalized",
        "mixed_line_endings",
    ] {
        if source.get(flag).and_then(Value::as_bool) == Some(true) {
            compact.insert(flag.to_string(), Value::Bool(true));
        }
    }
    if source.get("changed").and_then(Value::as_bool) == Some(false) {
        compact.insert("changed".to_string(), Value::Bool(false));
    }
    if source.get("encoding_changed").and_then(Value::as_bool) == Some(true) {
        for field in ["previous_encoding", "target_encoding"] {
            if let Some(field_value) = source.get(field) {
                compact.insert(field.to_string(), field_value.clone());
            }
        }
    }
    let line_endings_abnormal = source
        .get("line_endings_normalized")
        .and_then(Value::as_bool)
        == Some(true)
        || source.get("mixed_line_endings").and_then(Value::as_bool) == Some(true);
    if line_endings_abnormal && let Some(line_ending) = source.get("line_ending") {
        compact.insert("line_ending".to_string(), line_ending.clone());
    }
    copy_history_failure(source, &mut compact);
    compact
}

fn compact_create_file_success(
    source: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut compact = serde_json::Map::new();
    copy_response_fields(
        source,
        &mut compact,
        &["path", "sha256_after", "history_entry_id"],
    );
    copy_true_flag(source, &mut compact, "overwritten");
    if source.get("target_encoding").and_then(Value::as_str) != Some("UTF-8") {
        copy_response_fields(source, &mut compact, &["target_encoding"]);
    }
    if source.get("line_ending").and_then(Value::as_str) != Some("preserve") {
        copy_response_fields(source, &mut compact, &["line_ending"]);
    }
    copy_history_failure(source, &mut compact);
    compact
}

fn compact_delete_file_success(
    source: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut compact = serde_json::Map::new();
    copy_response_fields(
        source,
        &mut compact,
        &["path", "bytes_removed", "history_entry_id"],
    );
    if source.get("deleted").and_then(Value::as_bool) == Some(false) {
        compact.insert("deleted".to_string(), Value::Bool(false));
    }
    copy_history_failure(source, &mut compact);
    compact
}

fn compact_create_directory_success(
    source: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut compact = serde_json::Map::new();
    copy_response_fields(source, &mut compact, &["path", "history_entry_id"]);
    if source.get("created").and_then(Value::as_bool) == Some(false) {
        compact.insert("already_existed".to_string(), Value::Bool(true));
    }
    copy_history_failure(source, &mut compact);
    compact
}

fn compact_convert_file_format_success(
    source: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut compact = serde_json::Map::new();
    copy_response_fields(
        source,
        &mut compact,
        &[
            "path",
            "target_encoding",
            "line_ending",
            "size_bytes",
            "sha256_after",
            "history_entry_id",
        ],
    );
    if source.get("encoding_changed").and_then(Value::as_bool) == Some(true) {
        compact.insert("encoding_changed".to_string(), Value::Bool(true));
        copy_response_fields(source, &mut compact, &["previous_encoding"]);
    }
    copy_history_failure(source, &mut compact);
    compact
}

fn compact_undo_change_success(
    source: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut compact = serde_json::Map::new();
    copy_response_fields(
        source,
        &mut compact,
        &[
            "path",
            "undone_entry_id",
            "restored_state",
            "sha256_after",
            "history_entry_id",
        ],
    );
    copy_history_failure(source, &mut compact);
    compact
}

fn compact_edit_files_success(
    source: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut compact = serde_json::Map::new();
    copy_response_fields(source, &mut compact, &["root", "files_changed"]);
    let files = source
        .get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(Value::as_object)
                .map(|file| {
                    let mut compact_file = serde_json::Map::new();
                    copy_response_fields(
                        file,
                        &mut compact_file,
                        &["path", "replacements", "sha256_after", "history_entry_id"],
                    );
                    if file.get("changed").and_then(Value::as_bool) == Some(false) {
                        compact_file.insert("changed".to_string(), Value::Bool(false));
                    }
                    if file.get("encoding_changed").and_then(Value::as_bool) == Some(true) {
                        compact_file.insert("encoding_changed".to_string(), Value::Bool(true));
                        copy_response_fields(
                            file,
                            &mut compact_file,
                            &["previous_encoding", "target_encoding"],
                        );
                    }
                    if file.get("syntax_validation").and_then(Value::as_str) != Some("skipped") {
                        copy_response_fields(file, &mut compact_file, &["syntax_validation"]);
                    }
                    copy_history_failure(file, &mut compact_file);
                    Value::Object(compact_file)
                })
                .collect()
        })
        .unwrap_or_default();
    compact.insert("files".to_string(), Value::Array(files));
    compact
}

fn copy_response_fields(
    source: &serde_json::Map<String, Value>,
    target: &mut serde_json::Map<String, Value>,
    fields: &[&str],
) {
    for field in fields {
        if let Some(field_value) = source.get(*field) {
            target.insert((*field).to_string(), field_value.clone());
        }
    }
}

fn copy_true_flag(
    source: &serde_json::Map<String, Value>,
    target: &mut serde_json::Map<String, Value>,
    field: &str,
) {
    if source.get(field).and_then(Value::as_bool) == Some(true) {
        target.insert(field.to_string(), Value::Bool(true));
    }
}

fn copy_history_failure(
    source: &serde_json::Map<String, Value>,
    target: &mut serde_json::Map<String, Value>,
) {
    if source.get("history_recorded").and_then(Value::as_bool) == Some(false) {
        target.insert("history_recorded".to_string(), Value::Bool(false));
        copy_response_fields(source, target, &["history_reason"]);
    }
}

#[cfg(test)]
mod tests {
    use super::{drop_repeated_fields, normalize_tool_arguments, strip_null_fields};
    use serde_json::{Value, json};

    #[test]
    fn drop_repeated_fields_compacts_nested_aliases_and_keeps_distinct_values() {
        let mut same = json!({ "path": "/a", "canonical_path": "/a", "input_path": "/a" });
        drop_repeated_fields(&mut same);
        assert_eq!(same, json!({ "path": "/a" }));

        let mut distinct = json!({ "path": "/a", "canonical_path": "/b", "input_path": "a" });
        drop_repeated_fields(&mut distinct);
        assert_eq!(
            distinct,
            json!({ "path": "/a", "canonical_path": "/b", "input_path": "a" })
        );

        let mut resolve = json!({ "input_path": "/a", "canonical_path": "/a" });
        drop_repeated_fields(&mut resolve);
        assert_eq!(
            resolve,
            json!({ "input_path": "/a", "canonical_path": "/a" })
        );

        let mut nested = json!({
            "items": [{
                "path": "/a",
                "canonical_path": "/a",
                "symbol": "run",
                "name": "run",
                "qualified_name": "run"
            }],
            "workspace_root": "/repo",
            "repo_root": "/repo",
            "error_code": "invalid_argument",
            "message": "bad input",
            "reason": "bad input",
            "error": {"code": "invalid_argument", "message": "bad input"}
        });
        drop_repeated_fields(&mut nested);
        assert_eq!(
            nested,
            json!({
                "items": [{"path": "/a", "symbol": "run"}],
                "workspace_root": "/repo",
                "error": {"code": "invalid_argument", "message": "bad input"}
            })
        );

        let mut distinct_error = json!({
            "error_code": "invalid_argument",
            "message": "bad input",
            "reason": "specific cause",
            "error": {"code": "invalid_argument", "message": "bad input"}
        });
        drop_repeated_fields(&mut distinct_error);
        assert_eq!(
            distinct_error,
            json!({
                "reason": "specific cause",
                "error": {"code": "invalid_argument", "message": "bad input"}
            })
        );
    }

    #[test]
    fn strip_null_fields_removes_nested_nulls_but_keeps_array_slots() {
        let mut value = json!({
            "kept": 1,
            "dropped": null,
            "nested": { "inner": null, "other": [null, { "x": null, "y": false }] }
        });
        strip_null_fields(&mut value);
        assert_eq!(
            value,
            json!({ "kept": 1, "nested": { "other": [null, { "y": false }] } })
        );
    }

    #[test]
    fn normalize_tool_arguments_coerces_numeric_strings_and_warns_unknown_fields() {
        let (arguments, warnings) = normalize_tool_arguments(
            "text_search",
            json!({ "query": "needle", "max_results": "1", "unexpected": true }),
        )
        .unwrap();
        assert_eq!(
            arguments.get("max_results").and_then(Value::as_u64),
            Some(1)
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("Coerced argument 'max_results'"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("Unknown argument 'unexpected'"))
        );
    }

    #[test]
    fn normalize_tool_arguments_rejects_wrong_types_and_missing_required_fields() {
        let wrong_type =
            normalize_tool_arguments("text_search", json!({ "query": "needle", "paths": "src" }))
                .unwrap_err();
        assert!(
            wrong_type
                .to_string()
                .contains("expected array, got string")
        );

        let missing = normalize_tool_arguments("read_file_range", json!({})).unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("Missing required argument 'path'")
        );
    }

    #[test]
    fn normalize_tool_arguments_maps_singular_path_alias_and_rejects_ambiguity() {
        let (arguments, warnings) = normalize_tool_arguments(
            "find_definition",
            json!({ "symbol": "helper", "path": "src" }),
        )
        .unwrap();
        assert_eq!(arguments["paths"], json!(["src"]));
        assert!(arguments.get("path").is_none());
        assert!(warnings.is_empty());

        let error = normalize_tool_arguments(
            "fuzzy_find",
            json!({ "pattern": "helper", "path": "src", "paths": ["tests"] }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("either 'path' or 'paths'"));
    }

    #[test]
    fn normalize_tool_arguments_validates_nested_array_items() {
        let (arguments, warnings) = normalize_tool_arguments(
            "read_snippets",
            json!({ "requests": [{ "path": "a.rs", "start_line": "5" }] }),
        )
        .unwrap();
        assert_eq!(
            arguments
                .pointer("/requests/0/start_line")
                .and_then(Value::as_u64),
            Some(5)
        );
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn normalize_tool_arguments_enforces_numeric_schema_bounds() {
        let below_minimum = normalize_tool_arguments(
            "text_search",
            json!({ "query": "needle", "max_line_length": 0 }),
        )
        .unwrap_err();
        assert!(below_minimum.to_string().contains("below minimum 1"));

        let above_maximum = normalize_tool_arguments(
            "text_search",
            json!({ "query": "needle", "max_results": 1001 }),
        )
        .unwrap_err();
        assert!(above_maximum.to_string().contains("exceeds maximum 1000"));

        let invalid_threshold = normalize_tool_arguments(
            "compare_directories",
            json!({
                "left_path": "left",
                "right_path": "right",
                "rename_similarity_threshold": 1.1
            }),
        )
        .unwrap_err();
        assert!(invalid_threshold.to_string().contains("exceeds maximum 1"));

        let invalid_subcall_timeout = normalize_tool_arguments(
            "batch_tool_call",
            json!({
                "calls": [{
                    "tool": "resolve_path",
                    "args": { "path": "." },
                    "timeout_seconds": 301
                }]
            }),
        )
        .unwrap_err();
        assert!(
            invalid_subcall_timeout
                .to_string()
                .contains("exceeds maximum 300")
        );
    }
}

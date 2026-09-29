use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::common::insert_object_field;
use crate::tools::diff_support::build_diff_payload;
use crate::tools::read_symbol_body;

pub fn schema() -> Value {
    json!({
        "name": "compare_symbols",
        "title": "Compare symbols",
        "description": "Resolve one symbol on each side and return metadata plus a unified diff. Use after compare_directories or text_search when you need to review how a function/type changed across files or versions.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "left": {
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Symbol name to resolve on this side of the comparison." },
                        "paths": { "type": "array", "items": { "type": "string" }, "description": "Search roots or files for this side. Defaults to the active workspace." },
                        "file_hint": { "type": "string", "description": "Preferred file to check first for this side." },
                        "language": { "type": "string", "description": "Optional language filter using the same accepted values as read_symbol_body." },
                        "line": { "type": "integer", "minimum": 1, "description": "Optional 1-based declaration line used to disambiguate this side." }
                    },
                    "required": ["symbol"]
                },
                "right": {
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Symbol name to resolve on this side of the comparison." },
                        "paths": { "type": "array", "items": { "type": "string" }, "description": "Search roots or files for this side. Defaults to the active workspace." },
                        "file_hint": { "type": "string", "description": "Preferred file to check first for this side." },
                        "language": { "type": "string", "description": "Optional language filter using the same accepted values as read_symbol_body." },
                        "line": { "type": "integer", "minimum": 1, "description": "Optional 1-based declaration line used to disambiguate this side." }
                    },
                    "required": ["symbol"]
                },
                "include_signature": { "type": "boolean", "description": "Include symbol signatures in compared content. Defaults to true." }
            },
            "required": ["left", "right"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let left = args.get("left").context("Missing left object")?;
    let right = args.get("right").context("Missing right object")?;
    let include_signature = args
        .get("include_signature")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut left_result = read_symbol_for_compare(left, include_signature).await?;
    let mut right_result = read_symbol_for_compare(right, include_signature).await?;

    if is_ambiguous(&left_result) || is_ambiguous(&right_result) {
        return Ok(json!({
            "ambiguous": true,
            "left_ambiguous": is_ambiguous(&left_result),
            "right_ambiguous": is_ambiguous(&right_result),
            "left": left_result,
            "right": right_result
        }));
    }

    let left_content = left_result
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let right_content = right_result
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();

    let left_label = format!(
        "{}:{}",
        left_result
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("left"),
        left_result
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("symbol")
    );
    let right_label = format!(
        "{}:{}",
        right_result
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("right"),
        right_result
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("symbol")
    );

    remove_compared_content(&mut left_result);
    remove_compared_content(&mut right_result);

    let mut response = build_diff_payload(&left_content, &right_content, &left_label, &right_label);
    insert_object_field(&mut response, "left", left_result);
    insert_object_field(&mut response, "right", right_result);

    Ok(response)
}

async fn read_symbol_for_compare(config: &Value, include_signature: bool) -> Result<Value> {
    let symbol = config
        .get("symbol")
        .and_then(|v| v.as_str())
        .context("Missing symbol in compare_symbols side config")?;

    let mut args = json!({
        "symbol": symbol,
        "include_signature": include_signature
    });

    if let Some(paths) = config.get("paths") {
        insert_object_field(&mut args, "paths", paths.clone());
    }
    if let Some(file_hint) = config.get("file_hint") {
        insert_object_field(&mut args, "file_hint", file_hint.clone());
    }
    if let Some(language) = config.get("language") {
        insert_object_field(&mut args, "language", language.clone());
    }
    if let Some(line) = config.get("line") {
        insert_object_field(&mut args, "line", line.clone());
    }

    read_symbol_body::execute(&args).await
}

fn is_ambiguous(result: &Value) -> bool {
    result
        .get("ambiguous")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn remove_compared_content(result: &mut Value) {
    if let Some(object) = result.as_object_mut() {
        object.remove("content");
    }
}

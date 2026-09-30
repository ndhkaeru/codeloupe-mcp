use anyhow::{Context, Result};
use serde_json::{Value, json};
use tree_sitter::Node;

use crate::tools::ast_support::{
    DEFAULT_AST_FILE_SIZE_LIMIT, call_expression_name, find_function_candidates, is_call_node,
    parse_supported_file,
};

pub fn schema() -> Value {
    json!({
        "name": "get_call_graph",
        "title": "Get call graph",
        "description": "List outbound calls made from one function or symbol using Tree-sitter. Use after reading a symbol to understand direct dependencies without scanning the whole file.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "path": { "type": "string", "description": "Alias for file_path; do not pass both." },
                "symbol": { "type": "string" },
                "line": { "type": "integer", "minimum": 1, "description": "Optional 1-based declaration line used to select one candidate when names are duplicated." }
            },
            "required": ["symbol"],
            "oneOf": [{ "required": ["file_path"] }, { "required": ["path"] }]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let path_str = args
        .get("file_path")
        .and_then(|v| v.as_str())
        .context("Missing file_path")?;
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .context("Missing symbol")?;
    let line = args
        .get("line")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    if line == Some(0) {
        return Err(anyhow::anyhow!("line must be >= 1"));
    }

    let path = crate::common::resolve_tool_path(path_str);
    if !path.exists() || !path.is_file() {
        return Err(anyhow::anyhow!("File does not exist: {}", path_str));
    }

    let parsed = parse_supported_file(&path, DEFAULT_AST_FILE_SIZE_LIMIT, None)?
        .ok_or_else(|| anyhow::anyhow!("Unsupported extension for get_call_graph"))?;
    let root = parsed.tree.root_node();
    let mut candidates = find_function_candidates(root, &parsed.source, symbol, line);
    if candidates.len() > 1 {
        return Ok(json!({
            "path": crate::common::normalize_display_path(&path),
            "language": parsed.language_name,
            "symbol": symbol,
            "ambiguous": true,
            "total_candidates": candidates.len(),
            "candidates": candidates.iter().map(|candidate| json!({
                "name": candidate.name,
                "qualified_name": candidate.qualified_name,
                "start_line": candidate.node.start_position().row + 1,
                "end_line": candidate.node.end_position().row + 1
            })).collect::<Vec<_>>()
        }));
    }
    let candidate = candidates
        .pop()
        .ok_or_else(|| anyhow::anyhow!("Could not find function '{}' in the file", symbol))?;
    let function_node = candidate.node;

    let mut outbound = Vec::new();
    find_outbound_calls(function_node, &parsed.source, &mut outbound);
    outbound.sort();
    outbound.dedup();

    Ok(json!({
        "path": crate::common::normalize_display_path(&path),
        "language": parsed.language_name,
        "symbol": symbol,
        "qualified_name": candidate.qualified_name,
        "start_line": function_node.start_position().row + 1,
        "end_line": function_node.end_position().row + 1,
        "outbound_calls": outbound,
        "total_calls": outbound.len()
    }))
}

fn find_outbound_calls(node: Node<'_>, source: &[u8], calls: &mut Vec<String>) {
    if is_call_node(node.kind())
        && let Some(text) = call_expression_name(node, source)
        && !text.is_empty()
    {
        calls.push(text);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        find_outbound_calls(child, source, calls);
    }
}

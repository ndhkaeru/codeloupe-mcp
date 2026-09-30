use anyhow::{Context, Result};
use serde_json::{Value, json};

pub fn schema() -> Value {
    json!({
        "name": "count_file_lines",
        "title": "Count file lines",
        "description": "Count lines in one file with encoding and binary detection. Use before choosing read_file_range/snippet bounds or estimating file size without loading content.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .context("Missing path")?;
    let path = crate::common::resolve_tool_path(path_str);

    if !path.exists() || !path.is_file() {
        return Err(anyhow::anyhow!(
            "File does not exist or is not a file: {}",
            path_str
        ));
    }

    let meta = std::fs::metadata(&path)?;
    let inspection = crate::tools::read_file::inspect_text_file(&path)?;

    if inspection.is_binary {
        return Ok(json!({
            "path": crate::common::normalize_display_path(&path),
            "size_bytes": meta.len(),
            "encoding": Value::Null,
            "bom": inspection.bom,
            "is_binary": true,
            "line_count": 0,
            "warnings": ["This appears to be a binary file."]
        }));
    }

    Ok(json!({
        "path": crate::common::normalize_display_path(&path),
        "size_bytes": meta.len(),
        "encoding": inspection.encoding,
        "bom": inspection.bom,
        "is_binary": false,
        "line_count": inspection.line_count,
        "ends_with_newline": inspection.ends_with_newline
    }))
}

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::task;

use crate::indexer::content_status_for_paths;

pub fn schema() -> Value {
    json!({
        "name": "content_index_status",
        "title": "Check content index status",
        "description": "Report Tantivy content-index zone status for specific files or directories. Use before repeated text_search calls in large repositories to see whether scoped paths are ready, warming, not indexed, or outside the indexed workspace.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Files or directories whose content-index zones should be checked. Prefer scoped directories such as chrome/browser/net over workspace root." }
            },
            "required": ["paths"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("content_index_status background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let paths = parse_paths(&args)?;
    let statuses = content_status_for_paths(&paths);
    Ok(json!({
        "statuses": statuses,
        "total": paths.len()
    }))
}

fn parse_paths(args: &Value) -> Result<Vec<PathBuf>> {
    let paths = args
        .get("paths")
        .and_then(|v| v.as_array())
        .context("Missing paths")?;
    let paths = paths
        .iter()
        .filter_map(|path| path.as_str())
        .map(crate::common::resolve_existing_tool_path)
        .collect::<Result<Vec<_>>>()?;
    if paths.is_empty() {
        return Err(anyhow::anyhow!("paths must contain at least one path"));
    }
    Ok(paths)
}

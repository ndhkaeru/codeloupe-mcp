use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::task;

use crate::indexer::{
    IndexGcOptions, default_index_max_age_secs, default_index_max_total_bytes, run_index_gc,
};

const MAX_RESULTS: usize = 1_000;

pub fn schema() -> Value {
    json!({
        "name": "index_gc",
        "title": "Inspect or clean index storage",
        "description": "Inspect persistent index storage and optionally remove orphaned, expired, explicitly selected, or least-recently-used workspace indexes. Dry-run is the default; active indexes are protected by cross-process locks.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "apply": { "type": "boolean", "description": "Actually delete selected indexes. Defaults to false for a safe dry-run." },
                "remove_orphans": { "type": "boolean", "description": "Select indexes whose workspace no longer exists after the built-in grace period. Defaults to true." },
                "max_age_days": { "type": "integer", "minimum": 0, "description": "Select indexes unused for at least this many days. Defaults to the server retention policy (90 days)." },
                "max_total_bytes": { "type": "integer", "minimum": 0, "description": "Enforce a total storage cap by selecting least-recently-used indexes. Defaults to 2 GiB." },
                "workspace_roots": { "type": "array", "items": { "type": "string" }, "description": "Explicit workspace roots whose indexes should be selected. Paths may be missing because orphan cleanup is supported." },
                "max_results": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum candidate details to return. Counts always cover all candidates." }
            }
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("index_gc background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let apply = args.get("apply").and_then(Value::as_bool).unwrap_or(false);
    let remove_orphans = args
        .get("remove_orphans")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let max_age_secs = args
        .get("max_age_days")
        .map(|value| {
            value
                .as_u64()
                .context("max_age_days must be a non-negative integer")
                .map(|days| days.saturating_mul(24 * 60 * 60))
        })
        .transpose()?
        .or(Some(default_index_max_age_secs()));
    let max_total_bytes = args
        .get("max_total_bytes")
        .map(|value| {
            value
                .as_u64()
                .context("max_total_bytes must be a non-negative integer")
        })
        .transpose()?
        .or(Some(default_index_max_total_bytes()));
    let workspace_roots = parse_workspace_roots(&args)?;
    let max_results = args
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, MAX_RESULTS as u64) as usize;

    serde_json::to_value(run_index_gc(IndexGcOptions {
        apply,
        remove_orphans,
        max_age_secs,
        max_total_bytes,
        workspace_roots,
        max_results,
    }))
    .context("failed to serialize index_gc report")
}

fn parse_workspace_roots(args: &Value) -> Result<Vec<PathBuf>> {
    let Some(values) = args.get("workspace_roots") else {
        return Ok(Vec::new());
    };
    let values = values
        .as_array()
        .context("workspace_roots must be an array of paths")?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .context("workspace_roots entries must be strings")
                .map(crate::common::resolve_tool_path)
        })
        .collect()
}

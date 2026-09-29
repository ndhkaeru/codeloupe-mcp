use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};
use tokio::task;

use crate::indexer::{content_status_for_paths, warm_content_index_paths};

const DEFAULT_POLL_MS: u64 = 250;
const MAX_WAIT_MS: u64 = 30_000;

pub fn schema() -> Value {
    json!({
        "name": "warm_content_index",
        "title": "Warm content index",
        "description": "Request Tantivy content-index warming for specific scoped paths. Use before repeated literal text_search in large repositories; pass subsystem directories, then inspect statuses/warming_zones and retry when ready.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Files or directories whose content zones should be warmed. Avoid workspace root; choose the narrowest subsystem path." },
                "wait_ms": { "type": "integer", "description": "Optional time to wait for warming to complete, capped at 30000 ms. Omit or set 0 to schedule asynchronously." },
                "force": { "type": "boolean", "description": "When true, schedule refresh even if the zone already appears ready." },
                "include_ignored": { "type": "boolean", "description": "Allow warming paths ignored by .gitignore, .git/info/exclude, global gitignore, or .ignore files. Hidden paths remain excluded." }
            },
            "required": ["paths"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("warm_content_index background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let paths = parse_paths(&args)?;
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let include_ignored = args
        .get("include_ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let wait_ms = args
        .get("wait_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(MAX_WAIT_MS);

    let initial_statuses = warm_content_index_paths(&paths, force, include_ignored);
    let mut final_statuses = initial_statuses.clone();
    if wait_ms > 0 && initial_statuses.iter().any(|status| status.warming) {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        while Instant::now() < deadline {
            final_statuses = content_status_for_paths(&paths);
            if final_statuses
                .iter()
                .all(|status| status.ready || !status.warming)
            {
                break;
            }
            thread::sleep(Duration::from_millis(DEFAULT_POLL_MS));
        }
        final_statuses = content_status_for_paths(&paths);
        for (final_status, initial_status) in final_statuses.iter_mut().zip(&initial_statuses) {
            if initial_status.status == "ignored_by_ignore_rules" {
                *final_status = initial_status.clone();
            }
        }
    }

    let requested_zones = zones_from_statuses(&initial_statuses);
    let ready_zones = final_statuses
        .iter()
        .filter(|status| status.ready)
        .filter_map(|status| status.zone.clone())
        .collect::<Vec<_>>();
    let warming_zones = final_statuses
        .iter()
        .filter(|status| status.warming)
        .filter_map(|status| status.zone.clone())
        .collect::<Vec<_>>();
    let failed_statuses = final_statuses
        .iter()
        .filter(|status| !status.ready && !status.warming)
        .collect::<Vec<_>>();
    let outcome = if !warming_zones.is_empty() {
        "scheduled"
    } else if !ready_zones.is_empty() {
        "ready"
    } else {
        "nothing_to_warm"
    };
    let message = match outcome {
        "scheduled" => format!(
            "Scheduled {} content-index zone(s); {} already ready and {} unavailable.",
            warming_zones.len(),
            ready_zones.len(),
            failed_statuses.len()
        ),
        "ready" => format!(
            "{} content-index zone(s) ready; {} unavailable.",
            ready_zones.len(),
            failed_statuses.len()
        ),
        _ => format!(
            "Nothing to warm: {}.",
            status_reasons(&final_statuses).join(", ")
        ),
    };
    let is_error = outcome == "nothing_to_warm";

    if is_error {
        return Ok(json!({
            "__mcp_is_error": true,
            "outcome": outcome,
            "error_code": "invalid_argument",
            "message": message,
            "statuses": final_statuses
        }));
    }

    Ok(json!({
        "__mcp_is_error": is_error,
        "outcome": outcome,
        "message": message,
        "requested_zones": requested_zones,
        "ready_zones": ready_zones,
        "warming_zones": warming_zones,
        "statuses": final_statuses,
        "wait_ms": wait_ms,
        "force": force,
        "include_ignored": include_ignored
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

fn zones_from_statuses(statuses: &[crate::indexer::ContentZoneStatus]) -> Vec<String> {
    let mut zones = statuses
        .iter()
        .filter_map(|status| status.zone.clone())
        .collect::<Vec<_>>();
    zones.sort();
    zones.dedup();
    zones
}

fn status_reasons(statuses: &[crate::indexer::ContentZoneStatus]) -> Vec<String> {
    let mut reasons = statuses
        .iter()
        .map(|status| status.status.clone())
        .collect::<Vec<_>>();
    reasons.sort();
    reasons.dedup();
    if reasons.is_empty() {
        reasons.push("no eligible paths".to_string());
    }
    reasons
}

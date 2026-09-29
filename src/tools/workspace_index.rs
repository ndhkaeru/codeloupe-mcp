use anyhow::{Context, Result};
use serde_json::{Value, json};

pub fn schema() -> Value {
    json!({
        "name": "workspace_index",
        "title": "Manage workspace indexing",
        "description": "Estimate, enable, or disable indexing for one workspace. blocked paths and scan/disk budgets cannot be overridden. approval_required paths may be enabled only when the user explicitly requested work at that exact path. Otherwise use enable only for a user-relevant workspace when repeated repository searches are expected.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["estimate", "enable", "disable"],
                    "description": "estimate is read-only; enable records approval and starts indexing, including approval_required paths only after an explicit user request for that exact path; disable records a 30-day decline and removes an idle index."
                },
                "path": {
                    "type": "string",
                    "description": "Workspace directory. Relative paths resolve against the active workspace context."
                }
            },
            "required": ["action", "path"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .context("Missing 'action'")?;
    let raw_path = args
        .get("path")
        .and_then(Value::as_str)
        .context("Missing 'path'")?;
    let path = crate::common::resolve_existing_tool_path(raw_path)?;

    match action {
        "estimate" => Ok(json!({
            "action": action,
            "estimate": crate::workspace_control::estimate_workspace(path, false)
                .map_err(anyhow::Error::msg)?
        })),
        "enable" => Ok(json!({
            "action": action,
            "outcome": "scheduled",
            "approval_recorded": true,
            "estimate": crate::workspace_control::enable_workspace(path)
                .map_err(anyhow::Error::msg)?
        })),
        "disable" => {
            let root =
                crate::workspace_control::disable_workspace(path).map_err(anyhow::Error::msg)?;
            Ok(json!({
                "action": action,
                "outcome": "disabled",
                "workspace_root": crate::common::normalize_display_path(&root)
            }))
        }
        _ => Err(anyhow::anyhow!("Unsupported action '{action}'")),
    }
}

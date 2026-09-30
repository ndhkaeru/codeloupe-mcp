use anyhow::Result;
use serde_json::{Value, json};

pub fn schema() -> Value {
    json!({
        "name": "resolve_path",
        "title": "Resolve path",
        "description": "Normalize one path and return preflight accessibility metadata. Use when a user-supplied or relative path may be ambiguous before read/write operations.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                }
            },
            "required": ["path"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if path_str.is_empty() {
        return Err(anyhow::anyhow!("Path cannot be empty"));
    }

    let raw_input_path = crate::common::path_from_input(path_str);
    let resolution = crate::common::resolve_tool_path_details(path_str);
    let repo_root = crate::common::discover_workspace_root(&resolution.path);

    let mut response = json!({
        "input_path": path_str,
        "canonical_path": crate::common::normalize_display_path(&resolution.path),
        "input_was_relative": !raw_input_path.is_absolute() && !path_str.starts_with("file://"),
        "resolution_basis": resolution.resolution_basis,
        "workspace_root": resolution.workspace_root.map(|root| crate::common::normalize_display_path(&root)),
        "repo_root": repo_root.map(|root| crate::common::normalize_display_path(&root)),
        "tier": "Allowed",
        "is_accessible": resolution.path.exists() && std::fs::metadata(&resolution.path).is_ok(),
        "exists": resolution.path.exists(),
        "is_file": resolution.path.is_file(),
        "is_dir": resolution.path.is_dir(),
        "reason_if_blocked": serde_json::Value::Null
    });
    if !resolution.ambiguous_resolutions.is_empty() {
        crate::common::insert_object_field(
            &mut response,
            "ambiguous_resolutions",
            json!(
                resolution
                    .ambiguous_resolutions
                    .iter()
                    .map(|path| crate::common::normalize_display_path(path))
                    .collect::<Vec<_>>()
            ),
        );
    }
    Ok(response)
}

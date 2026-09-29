use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor};

pub fn schema() -> Value {
    json!({
        "name": "validate_json",
        "title": "Validate JSON",
        "description": "Validate JSON syntax from one file or inline content without materializing the document. Provide exactly one of path or content; JSON Schema validation is not performed.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "UTF-8 JSON file to validate. Relative paths resolve against the active workspace." },
                "content": { "type": "string", "description": "Inline JSON text to validate instead of a file." }
            }
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let path = args.get("path").and_then(Value::as_str);
    let content = args.get("content").and_then(Value::as_str);
    match (path, content) {
        (Some(_), Some(_)) => Err(anyhow::anyhow!(
            "path and content are mutually exclusive; provide exactly one"
        )),
        (None, None) => Err(anyhow::anyhow!("Missing path or content")),
        (None, Some(content)) => Ok(validation_response(
            validate_reader(Cursor::new(content.as_bytes())),
            "content",
            None,
            content.len() as u64,
        )?),
        (Some(path_str), None) => {
            let path = crate::common::resolve_tool_path(path_str);
            if !path.is_file() {
                return Err(anyhow::anyhow!(
                    "File does not exist or is not a file: {path_str}"
                ));
            }
            let size_bytes = std::fs::metadata(&path)?.len();
            let file = File::open(&path)
                .with_context(|| format!("Failed to open JSON file: {}", path.display()))?;
            let mut reader = BufReader::new(file);
            let prefix = reader.fill_buf()?;
            if prefix.starts_with(&[0xFF, 0xFE]) || prefix.starts_with(&[0xFE, 0xFF]) {
                return Ok(json!({
                    "valid": false,
                    "source": "file",
                    "path": crate::common::normalize_display_path(&path),
                    "input_size_bytes": size_bytes,
                    "message": "validate_json currently supports UTF-8 JSON files only",
                    "category": "encoding"
                }));
            }
            if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
                reader.consume(3);
            }
            Ok(validation_response(
                validate_reader(reader),
                "file",
                Some(crate::common::normalize_display_path(&path)),
                size_bytes,
            )?)
        }
    }
}

fn validate_reader<R: std::io::Read>(reader: R) -> serde_json::Result<()> {
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    IgnoredAny::deserialize(&mut deserializer)?;
    deserializer.end()
}

fn validation_response(
    result: serde_json::Result<()>,
    source: &str,
    path: Option<String>,
    input_size_bytes: u64,
) -> Result<Value> {
    match result {
        Ok(()) => Ok(json!({
            "valid": true,
            "source": source,
            "path": path,
            "input_size_bytes": input_size_bytes,
            "validation": "json_syntax"
        })),
        Err(error) if error.classify() == serde_json::error::Category::Io => Err(error.into()),
        Err(error) => Ok(json!({
            "valid": false,
            "source": source,
            "path": path,
            "input_size_bytes": input_size_bytes,
            "message": error.to_string(),
            "line": error.line(),
            "column": error.column(),
            "category": format!("{:?}", error.classify()).to_ascii_lowercase(),
            "validation": "json_syntax"
        })),
    }
}

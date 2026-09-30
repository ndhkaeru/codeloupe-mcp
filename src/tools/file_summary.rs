use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;
use std::time::UNIX_EPOCH;

use crate::limits::{BINARY_PROBE_BYTES, FILE_SUMMARY_LINE_COUNT_BYTES};

fn detect_language(ext: &str) -> &'static str {
    match ext {
        "rs" => "Rust",
        "py" => "Python",
        "js" => "JavaScript",
        "ts" | "tsx" => "TypeScript",
        "jsx" => "JSX",
        "java" => "Java",
        "c" | "h" => "C",
        "cpp" | "cc" | "cxx" | "hpp" => "C++",
        "cs" => "C#",
        "go" => "Go",
        "rb" => "Ruby",
        "php" => "PHP",
        "swift" => "Swift",
        "m" | "mm" => "Objective-C",
        "kt" | "kts" => "Kotlin",
        "scala" => "Scala",
        "sh" | "bash" => "Shell",
        "ps1" => "PowerShell",
        "sql" => "SQL",
        "html" | "htm" => "HTML",
        "css" | "scss" | "sass" | "less" => "CSS",
        "json" => "JSON",
        "yaml" | "yml" => "YAML",
        "toml" => "TOML",
        "xml" => "XML",
        "md" | "markdown" => "Markdown",
        "txt" => "Text",
        "proto" => "Protobuf",
        "lua" => "Lua",
        "r" => "R",
        "dart" => "Dart",
        "ex" | "exs" => "Elixir",
        "zig" => "Zig",
        _ => "Unknown",
    }
}

pub fn schema() -> Value {
    json!({
        "name": "file_summary",
        "title": "Summarize file",
        "description": "Return quick metadata, binary detection, and a short preview for one file. Use as a cheap preflight before reading large or unfamiliar files.",
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
    let total_size = meta.len();

    // Basic language detection from the file extension.
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let language = detect_language(ext);

    // Last modified timestamp
    let last_modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Read a bounded prefix for encoding, binary, and preview detection.
    let mut file = File::open(&path)?;
    let mut buffer = vec![0; std::cmp::min(BINARY_PROBE_BYTES, total_size as usize)];
    let bytes_read = file.read(&mut buffer)?;
    buffer.truncate(bytes_read);

    // Heuristic: if b'\x00' exists in first 8KB -> Binary file
    let is_binary = crate::tools::read_file::is_probably_binary(&buffer);

    if is_binary {
        return Ok(json!({
            "path": crate::common::normalize_display_path(&path),
            "size_bytes": total_size,
            "language": language,
            "last_modified": last_modified,
            "bom": crate::tools::read_file::has_text_bom(&buffer),
            "is_binary": true,
            "lines": 0,
            "outline_preview": null,
            "warnings": ["This is a binary file."]
        }));
    }

    let (preview_content, encoding, bom) = crate::tools::read_file::decode_for_read(&buffer);
    let preview_lines: Vec<&str> = preview_content.lines().take(10).collect();
    let outline_preview = preview_lines.join("\n");

    let line_count = if total_size > FILE_SUMMARY_LINE_COUNT_BYTES {
        -1
    } else {
        match crate::tools::read_file::inspect_text_file(&path) {
            Ok(inspection) => inspection.line_count as i64,
            Err(_) => -1,
        }
    };

    Ok(json!({
        "path": crate::common::normalize_display_path(&path),
        "size_bytes": total_size,
        "language": language,
        "encoding": encoding,
        "bom": bom,
        "last_modified": last_modified,
        "is_binary": false,
        "lines": line_count,
        "outline_preview": outline_preview,
        "line_count_limit_bytes": FILE_SUMMARY_LINE_COUNT_BYTES,
        "line_count_skipped": line_count < 0,
    }))
}

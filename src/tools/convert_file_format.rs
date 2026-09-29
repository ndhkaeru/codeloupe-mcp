use anyhow::Result;
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;

use crate::history::{attach_history_metadata, file_snapshot, no_history, record_change};
use crate::tools::read_file::decode_fuzzy;
use crate::tools::text_encoding::TextEncoding;

fn line_ending_metadata(content: &str) -> Option<String> {
    if content.contains("\r\n") {
        Some("crlf".to_string())
    } else if content.contains('\n') {
        Some("lf".to_string())
    } else {
        None
    }
}

pub fn schema() -> Value {
    json!({
        "name": "convert_file_format",
        "title": "Convert file format",
        "description": "Rewrite one text file with normalized encoding and line endings. Use for cleanup before edits or tests; do not use for binary files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "target_encoding": {
                    "type": "string",
                    "enum": ["UTF-8", "UTF-16LE", "UTF-16BE", "Windows-1252"],
                    "description": "Output encoding. Defaults to the detected existing encoding. UTF-16 output includes a BOM."
                },
                "target_line_ending": {
                    "type": "string",
                    "enum": ["lf", "crlf"],
                }
            },
            "required": ["path"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if path_str.is_empty() {
        return Err(anyhow::anyhow!("Missing path argument"));
    }
    let path = crate::common::resolve_write_tool_path(path_str);
    if !path.exists() || !path.is_file() {
        return Err(anyhow::anyhow!(
            "File does not exist or is not a file: '{}' (resolved to {})",
            path_str,
            crate::common::normalize_display_path(&path)
        ));
    }

    let target_line_ending = args
        .get("target_line_ending")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());

    let read_result = File::open(&path).and_then(|mut f| {
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    });

    let buffer = match read_result {
        Ok(b) => b,
        Err(e) => {
            if let Some(os_err) = e.raw_os_error()
                && os_err == 32
            {
                return Ok(json!({
                    "isError": true,
                    "content": [{
                        "type": "text",
                        "text": "File is locked by another process (OS error 32). Please check which process is holding the file handle before retrying."
                    }]
                }));
            }
            return Err(e.into());
        }
    };

    let (mut content, detected_encoding) = decode_fuzzy(&buffer);
    let previous_encoding = TextEncoding::parse(detected_encoding)
        .map(TextEncoding::canonical_name)
        .unwrap_or(detected_encoding);
    let target_encoding = match args.get("target_encoding").and_then(Value::as_str) {
        Some(raw) => TextEncoding::parse(raw).map_err(anyhow::Error::msg)?,
        None => TextEncoding::parse(previous_encoding).unwrap_or(TextEncoding::Utf8),
    };
    let encoding_changed = previous_encoding != target_encoding.canonical_name();

    if let Some(le) = target_line_ending {
        if le == "lf" {
            content = content.replace("\r\n", "\n");
        } else if le == "crlf" {
            content = content.replace("\r\n", "\n").replace('\n', "\r\n");
        }
    }

    let final_line_ending = line_ending_metadata(&content);
    let final_bytes = target_encoding
        .encode(&content)
        .map_err(anyhow::Error::msg)?;
    let sha256_after = super::file_hash::sha256_bytes(&final_bytes);
    let history_outcome = if final_bytes == buffer {
        no_history("no filesystem change")
    } else {
        record_change(
            "convert_file_format",
            &path,
            file_snapshot(
                buffer.clone(),
                Some(previous_encoding.to_string()),
                line_ending_metadata(&decode_fuzzy(&buffer).0),
            ),
            file_snapshot(
                final_bytes.clone(),
                Some(target_encoding.canonical_name().to_string()),
                final_line_ending.clone(),
            ),
            "convert file format",
        )
    };

    let write_result = crate::tools::atomic_write::write_bytes(&path, &final_bytes, true);

    match write_result {
        Ok(_) => {
            let mut response = json!({
                "success": true,
                "path": crate::common::normalize_display_path(&path),
                "previous_encoding": previous_encoding,
                "target_encoding": target_encoding.canonical_name(),
                "encoding_changed": encoding_changed,
                "line_ending": final_line_ending,
                "size_bytes": final_bytes.len(),
                "sha256_after": sha256_after,
                "message": format!(
                    "Successfully converted file to {}",
                    target_encoding.canonical_name()
                )
            });
            attach_history_metadata(&mut response, &history_outcome);
            Ok(response)
        }
        Err(e) => {
            if let Some(os_err) = e.raw_os_error()
                && os_err == 32
            {
                return Ok(json!({
                    "isError": true,
                    "content": [{
                        "type": "text",
                        "text": "File is locked by another process (OS error 32) when trying to write. Please check handle."
                    }]
                }));
            }
            Err(e.into())
        }
    }
}

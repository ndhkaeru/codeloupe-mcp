use anyhow::{Context, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

const HASH_BUFFER_BYTES: usize = 64 * 1024;

pub fn schema() -> Value {
    json!({
        "name": "file_hash",
        "title": "Hash file",
        "description": "Compute the SHA-256 digest of one file with bounded-memory streaming I/O. Use the returned sha256 value as expected_hash for guarded edits or deletes.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to hash. Relative paths resolve against the active workspace." }
            },
            "required": ["path"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let path_str = args
        .get("path")
        .and_then(Value::as_str)
        .context("Missing path")?;
    let path = crate::common::resolve_tool_path(path_str);
    if !path.is_file() {
        return Err(anyhow::anyhow!(
            "File does not exist or is not a file: {path_str}"
        ));
    }
    let size_bytes = std::fs::metadata(&path)?.len();
    let sha256 = sha256_file(&path)?;
    Ok(json!({
        "path": crate::common::normalize_display_path(&path),
        "algorithm": "sha256",
        "sha256": sha256,
        "size_bytes": size_bytes
    }))
}

pub(crate) fn sha256_file(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(HASH_BUFFER_BYTES, file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; HASH_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) fn normalize_expected_hash(raw: &str) -> std::result::Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("expected_hash must be a 64-character hexadecimal SHA-256 digest");
    }
    Ok(trimmed.to_ascii_lowercase())
}

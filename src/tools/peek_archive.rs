use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;

use crate::limits::MAX_IN_MEMORY_TEXT_FILE_BYTES;

const MAX_INNER_FILE_BYTES: usize = MAX_IN_MEMORY_TEXT_FILE_BYTES as usize;
const MAX_LISTED_ENTRIES: usize = 1_000;

fn normalize_archive_entry_path(raw: &str) -> String {
    raw.replace('\\', "/").trim_start_matches("./").to_string()
}

pub fn schema() -> Value {
    json!({
        "name": "peek_archive",
        "title": "Peek archive",
        "description": "List up to 1000 archive entries or read one file of at most 10 MiB without extracting it. Use for source bundles or release artifacts; prefer inner_path for targeted reads.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "archive_path": { "type": "string" },
                "inner_path": { "type": "string" }
            },
            "required": ["archive_path"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let archive_path_str = args
        .get("archive_path")
        .and_then(|v| v.as_str())
        .context("Missing archive_path")?;
    let inner_path_opt = args
        .get("inner_path")
        .and_then(|v| v.as_str())
        .map(normalize_archive_entry_path);

    let archive_path = crate::common::resolve_tool_path(archive_path_str);
    if !archive_path.exists() || !archive_path.is_file() {
        return Err(anyhow::anyhow!(
            "Archive file does not exist: {}",
            archive_path_str
        ));
    }

    let ext = archive_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let is_zip = ext == "zip" || ext == "jar" || ext == "apk";
    let is_tar_gz = archive_path_str.ends_with(".tar.gz") || archive_path_str.ends_with(".tgz");
    let is_tar = ext == "tar";

    if is_zip {
        let file = File::open(&archive_path)?;
        let mut archive = zip::ZipArchive::new(file)?;

        if let Some(inner_path) = inner_path_opt {
            let mut file_in_zip = if let Ok(file) = archive.by_name(&inner_path) {
                file
            } else {
                let mut matched_index = None;
                for i in 0..archive.len() {
                    if let Ok(file) = archive.by_index(i)
                        && normalize_archive_entry_path(file.name()) == inner_path
                    {
                        matched_index = Some(i);
                        break;
                    }
                }

                let index = matched_index
                    .context(format!("File {} not found inside archive", inner_path))?;
                archive.by_index(index)?
            };
            let buffer = read_limited_entry(&mut file_in_zip)?;

            let (content, enc) = crate::tools::read_file::decode_fuzzy(&buffer);

            Ok(json!({
                "archive": archive_path_str,
                "inner_file": inner_path,
                "encoding": enc,
                "content": content
            }))
        } else {
            let total_entries = archive.len();
            let mut entries = Vec::new();
            for i in 0..total_entries.min(MAX_LISTED_ENTRIES) {
                if let Ok(file) = archive.by_index(i) {
                    entries.push(json!({
                        "name": normalize_archive_entry_path(file.name()),
                        "size_bytes": file.size(),
                        "is_dir": file.is_dir()
                    }));
                }
            }
            Ok(json!({
                "archive": archive_path_str,
                "entries": entries,
                "entries_returned": entries.len(),
                "entries_complete": total_entries <= MAX_LISTED_ENTRIES,
                "total_entries": total_entries
            }))
        }
    } else if is_tar || is_tar_gz {
        let file = File::open(&archive_path)?;

        // Box the streams so the match arms return a single type.
        let reader: Box<dyn Read> = if is_tar_gz {
            Box::new(GzDecoder::new(file))
        } else {
            Box::new(file)
        };

        let mut archive = tar::Archive::new(reader);

        if let Some(inner_path) = inner_path_opt {
            let mut buf = Vec::new();
            let mut found = false;

            for entry in archive.entries()? {
                let mut entry = entry?;
                let entry_name = normalize_archive_entry_path(&entry.path()?.to_string_lossy());
                if entry_name == inner_path {
                    buf = read_limited_entry(&mut entry)?;
                    found = true;
                    break;
                }
            }

            if !found {
                return Err(anyhow::anyhow!(
                    "File {} not found inside archive",
                    inner_path
                ));
            }

            let (content, enc) = crate::tools::read_file::decode_fuzzy(&buf);

            Ok(json!({
                "archive": archive_path_str,
                "inner_file": inner_path,
                "encoding": enc,
                "content": content
            }))
        } else {
            let mut entries = Vec::new();
            let mut entries_complete = true;
            for entry in archive.entries()? {
                if entries.len() >= MAX_LISTED_ENTRIES {
                    entries_complete = false;
                    break;
                }
                let entry = entry?;
                entries.push(json!({
                    "name": normalize_archive_entry_path(&entry.path()?.to_string_lossy()),
                    "size_bytes": entry.header().size()?,
                    "is_dir": entry.header().entry_type().is_dir()
                }));
            }
            Ok(json!({
                "archive": archive_path_str,
                "entries": entries,
                "entries_returned": entries.len(),
                "entries_complete": entries_complete,
                "total_entries": if entries_complete { json!(entries.len()) } else { Value::Null }
            }))
        }
    } else {
        Err(anyhow::anyhow!(
            "Unsupported archive format. Supported formats: .zip, .tar, .tar.gz"
        ))
    }
}

fn read_limited_entry(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    reader
        .take((MAX_INNER_FILE_BYTES + 1) as u64)
        .read_to_end(&mut buffer)?;
    if buffer.len() > MAX_INNER_FILE_BYTES {
        return Err(anyhow::anyhow!("Inner file is too large (> 10 MiB)"));
    }
    Ok(buffer)
}

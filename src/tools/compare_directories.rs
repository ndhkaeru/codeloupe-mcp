use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::task;

use super::output_format::{OutputFormat, parse_output_format};
use super::path_filters::{
    apply_walk_overrides, default_generated_vendor_globs, parse_pattern_strings,
};

use crate::limits::{DEFAULT_COMPARE_FILE_SIZE_BYTES, MAX_COMPARE_FILE_SIZE_BYTES};

const DEFAULT_MAX_FILE_SIZE: u64 = DEFAULT_COMPARE_FILE_SIZE_BYTES;
const DEFAULT_MAX_DIFF_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_FILES: usize = 20_000;
const MAX_FILE_SIZE_LIMIT: usize = MAX_COMPARE_FILE_SIZE_BYTES;
const MAX_DIFF_BYTES_LIMIT: usize = 4 * 1024 * 1024;
const MAX_FILES_LIMIT: usize = 100_000;
const DEFAULT_TOP_LIMIT: usize = 10;
const TOP_LEVEL_LIST_LIMIT: usize = 100;
const GROUPED_PATH_LIMIT: usize = 50;

#[derive(Clone)]
struct Options {
    max_file_size: u64,
    max_diff_bytes: usize,
    max_files: usize,
    include_content_diff: bool,
    summary_only: bool,
    detect_renames: bool,
    rename_similarity_threshold: f64,
    output_format: OutputFormat,
}

#[derive(Clone)]
struct FileInfo {
    absolute_path: PathBuf,
    size: u64,
    modified_at: u64,
}

struct CollectedFiles {
    files: BTreeMap<String, FileInfo>,
    truncated: bool,
    skipped: Vec<Value>,
}

struct TextChange {
    value: Value,
    inserted_lines: usize,
    deleted_lines: usize,
}

pub fn schema() -> Value {
    json!({
        "name": "compare_directories",
        "title": "Compare source directories",
        "description": "Compare two source directory trees to quickly identify user-visible code changes. Use this before detailed reads when reviewing generated projects, migrations, releases, or copied workspaces; it reports added/deleted/modified/renamed files, bounded diffs, hotspots, and risk hints while skipping common generated/vendor directories by default.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "left_path": { "type": "string", "description": "Base/source directory. Relative paths resolve against the active workspace." },
                "right_path": { "type": "string", "description": "Changed/target directory to compare against left_path. Relative paths resolve against the active workspace." },
                "includes": { "type": "array", "items": { "type": "string" }, "description": "Glob include filters for focused reviews, e.g. [\"src/**\", \"**/*.rs\"]. Omit to scan all supported files." },
                "excludes": { "type": "array", "items": { "type": "string" }, "description": "Extra glob excludes. Shared generated/vendor defaults include .git, node_modules, target, build, dist, obj, coverage, and similar directories. bin is excluded only for recognized .NET/Java workspaces." },
                "max_file_size": { "type": "integer", "description": "Maximum file size to read or diff in bytes. Larger files are summarized as skipped. Defaults to 2 MiB." },
                "max_diff_bytes": { "type": "integer", "description": "Maximum total unified diff bytes returned across all files. Lower this for first-pass reviews. Defaults to 256 KiB." },
                "max_files": { "type": "integer", "description": "Maximum discovered files per side before aborting to protect the agent from huge trees. Defaults to 20000." },
                "include_content_diff": { "type": "boolean", "description": "Include bounded unified diffs for modified text files. Opt in after the default inventory-only pass. Defaults to false." },
                "summary_only": { "type": "boolean", "description": "Return counts, grouped summaries, and changed file lists without per-file diff payloads. Good first pass for large changes. Defaults to false." },
                "detect_renames": { "type": "boolean", "description": "Detect exact and similar-content renames among added/deleted files. Defaults to true." },
                "rename_similarity_threshold": { "type": "number", "minimum": 0.0, "maximum": 1.0, "description": "Line-similarity threshold for fuzzy rename detection, from 0.0 to 1.0. Defaults to 0.85." },
                "output_format": { "type": "string", "enum": ["json", "markdown"], "description": "Return structured JSON for tool chaining or compact Markdown for direct human review. Defaults to json." }
            },
            "required": ["left_path", "right_path"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("compare_directories background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let left_raw = args
        .get("left_path")
        .and_then(|v| v.as_str())
        .context("Missing left_path")?;
    let right_raw = args
        .get("right_path")
        .and_then(|v| v.as_str())
        .context("Missing right_path")?;

    let left_root = crate::common::resolve_tool_path(left_raw);
    let right_root = crate::common::resolve_tool_path(right_raw);
    ensure_dir(&left_root, left_raw)?;
    ensure_dir(&right_root, right_raw)?;

    let mut exclude_globs =
        default_generated_vendor_globs(&[left_root.clone(), right_root.clone()]);
    exclude_globs.extend(parse_pattern_strings(args.get("excludes")));
    let include_globs = parse_pattern_strings(args.get("includes"));
    let options = Options {
        max_file_size: arg_usize(
            &args,
            "max_file_size",
            DEFAULT_MAX_FILE_SIZE as usize,
            0,
            MAX_FILE_SIZE_LIMIT,
        ) as u64,
        max_diff_bytes: arg_usize(
            &args,
            "max_diff_bytes",
            DEFAULT_MAX_DIFF_BYTES,
            0,
            MAX_DIFF_BYTES_LIMIT,
        ),
        max_files: arg_usize(&args, "max_files", DEFAULT_MAX_FILES, 1, MAX_FILES_LIMIT),
        include_content_diff: args
            .get("include_content_diff")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        summary_only: args
            .get("summary_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        detect_renames: args
            .get("detect_renames")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        rename_similarity_threshold: arg_f64(&args, "rename_similarity_threshold", 0.85),
        output_format: parse_output_format(args.get("output_format"), false)?,
    };

    let left_collected = collect_files(
        &left_root,
        &include_globs,
        &exclude_globs,
        options.max_files,
        "left",
    )?;
    let right_collected = collect_files(
        &right_root,
        &include_globs,
        &exclude_globs,
        options.max_files,
        "right",
    )?;
    let partial = left_collected.truncated || right_collected.truncated;
    let left_files = left_collected.files;
    let right_files = right_collected.files;

    let left_keys: BTreeSet<String> = left_files.keys().cloned().collect();
    let right_keys: BTreeSet<String> = right_files.keys().cloned().collect();

    let mut added: Vec<String> = right_keys.difference(&left_keys).cloned().collect();
    let mut deleted: Vec<String> = left_keys.difference(&right_keys).cloned().collect();
    let common: Vec<String> = left_keys.intersection(&right_keys).cloned().collect();
    let mut skipped_files = Vec::new();
    skipped_files.extend(left_collected.skipped);
    skipped_files.extend(right_collected.skipped);
    let renamed_files = if options.detect_renames {
        detect_renames(
            &left_files,
            &right_files,
            &mut deleted,
            &mut added,
            options.max_file_size,
            options.rename_similarity_threshold,
            &mut skipped_files,
        )?
    } else {
        Vec::new()
    };

    let mut modified = Vec::new();
    let mut unchanged_count = 0usize;
    let mut binary_files = Vec::new();
    let mut diff_bytes_used = 0usize;
    let mut inserted_lines_total = 0usize;
    let mut deleted_lines_total = 0usize;

    for relative_path in common {
        let left = left_files
            .get(&relative_path)
            .expect("left common file exists");
        let right = right_files
            .get(&relative_path)
            .expect("right common file exists");

        if left.size > options.max_file_size || right.size > options.max_file_size {
            skipped_files.push(json!({
                "path": relative_path,
                "reason": "file_too_large",
                "left_size_bytes": left.size,
                "right_size_bytes": right.size
            }));
            continue;
        }

        let left_bytes = match read_file_bytes(left) {
            Ok(bytes) => bytes,
            Err(err) => {
                skipped_files.push(json!({
                    "path": relative_path,
                    "reason": "left_read_failed",
                    "message": err.to_string()
                }));
                continue;
            }
        };
        let right_bytes = match read_file_bytes(right) {
            Ok(bytes) => bytes,
            Err(err) => {
                skipped_files.push(json!({
                    "path": relative_path,
                    "reason": "right_read_failed",
                    "message": err.to_string()
                }));
                continue;
            }
        };

        if left_bytes == right_bytes {
            unchanged_count += 1;
            continue;
        }

        if options.summary_only {
            modified.push(metadata_only_change(&relative_path, left, right));
            continue;
        }

        if crate::tools::read_file::is_probably_binary(&left_bytes)
            || crate::tools::read_file::is_probably_binary(&right_bytes)
        {
            binary_files.push(json!({
                "path": relative_path,
                "left_size_bytes": left.size,
                "right_size_bytes": right.size,
                "left_modified_at": left.modified_at,
                "right_modified_at": right.modified_at,
                "status": "modified"
            }));
            continue;
        }

        let change = build_text_change(
            &relative_path,
            left,
            right,
            &left_bytes,
            &right_bytes,
            &options,
            &mut diff_bytes_used,
        );
        inserted_lines_total += change.inserted_lines;
        deleted_lines_total += change.deleted_lines;
        modified.push(change.value);
    }

    let changed_paths =
        collect_changed_paths(&added, &deleted, &renamed_files, &modified, &binary_files);
    let changed_count = changed_paths.len();
    let top_changed_directories = top_counts(changed_paths.iter().map(|path| top_directory(path)));
    let extensions_summary = top_counts(changed_paths.iter().map(|path| extension_key(path)));
    let changed_files_by_directory = changed_by_directory(&changed_paths);
    let risk_hints = risk_hints(&changed_paths);

    let added_files_truncated = added.len() > TOP_LEVEL_LIST_LIMIT;
    let deleted_files_truncated = deleted.len() > TOP_LEVEL_LIST_LIMIT;
    let renamed_files_truncated = renamed_files.len() > TOP_LEVEL_LIST_LIMIT;
    let modified_files_truncated = modified.len() > TOP_LEVEL_LIST_LIMIT;
    let binary_files_truncated = binary_files.len() > TOP_LEVEL_LIST_LIMIT;
    let skipped_files_truncated = skipped_files.len() > TOP_LEVEL_LIST_LIMIT;

    let result = json!({
        "left_path": normalize_path(&left_root),
        "right_path": normalize_path(&right_root),
        "summary": {
            "added_files": added.len(),
            "deleted_files": deleted.len(),
            "renamed_files": renamed_files.len(),
            "modified_text_files": modified.len(),
            "modified_binary_files": binary_files.len(),
            "unchanged_files": unchanged_count,
            "skipped_files": skipped_files.len(),
            "changed_files": changed_count,
            "inserted_lines": inserted_lines_total,
            "deleted_lines": deleted_lines_total,
            "diff_bytes_returned": diff_bytes_used,
            "complete": !partial
        },
        "complete": !partial,
        "limit_reached": partial,
        "limit_reason": if partial { Some("max_files") } else { None },
        "top_changed_directories": top_changed_directories,
        "extensions_summary": extensions_summary,
        "changed_files_by_directory": changed_files_by_directory,
        "risk_hints": risk_hints,
        "details_limit": TOP_LEVEL_LIST_LIMIT,
        "details_complete": {
            "added_files": !added_files_truncated,
            "deleted_files": !deleted_files_truncated,
            "renamed_files": !renamed_files_truncated,
            "modified_files": !modified_files_truncated,
            "binary_files": !binary_files_truncated,
            "skipped_files": !skipped_files_truncated
        },
        "added_files": capped_strings(&added),
        "deleted_files": capped_strings(&deleted),
        "renamed_files": capped_values(&renamed_files),
        "modified_files": capped_values(&modified),
        "binary_files": capped_values(&binary_files),
        "skipped_files": capped_values(&skipped_files),
        "warnings": build_warnings(options.max_diff_bytes, diff_bytes_used)
    });

    if options.output_format == OutputFormat::Markdown {
        return Ok(json!({ "__mcp_raw_text": render_markdown_report(&result) }));
    }

    Ok(result)
}

fn ensure_dir(path: &Path, raw: &str) -> Result<()> {
    if !path.exists() || !path.is_dir() {
        return Err(anyhow::anyhow!("Path is not a valid directory: {}", raw));
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    includes: &[String],
    excludes: &[String],
    max_files: usize,
    side: &str,
) -> Result<CollectedFiles> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut walk = WalkBuilder::new(&canonical_root);
    walk.hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false);
    apply_walk_overrides(&mut walk, &canonical_root, includes, excludes)?;

    let mut files = BTreeMap::new();
    let mut skipped = Vec::new();
    let mut truncated = false;
    for entry in walk.build().flatten() {
        let path = entry.path().to_path_buf();
        let relative = path
            .strip_prefix(&canonical_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if entry.path_is_symlink() {
            skipped.push(json!({
                "path": relative,
                "side": side,
                "reason": "symlink_skipped"
            }));
            continue;
        }

        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        if files.len() >= max_files {
            truncated = true;
            skipped.push(json!({
                "path": relative,
                "side": side,
                "reason": "max_files_reached",
                "max_files": max_files
            }));
            break;
        }

        let relative = path
            .strip_prefix(&canonical_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = entry
            .metadata()
            .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
        files.insert(
            relative,
            FileInfo {
                absolute_path: path,
                size: metadata.len(),
                modified_at: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_secs()),
            },
        );
    }

    Ok(CollectedFiles {
        files,
        truncated,
        skipped,
    })
}

fn detect_renames(
    left_files: &BTreeMap<String, FileInfo>,
    right_files: &BTreeMap<String, FileInfo>,
    deleted: &mut Vec<String>,
    added: &mut Vec<String>,
    max_file_size: u64,
    similarity_threshold: f64,
    skipped_files: &mut Vec<Value>,
) -> Result<Vec<Value>> {
    let mut renamed = Vec::new();
    let mut consumed_deleted = BTreeSet::new();
    let mut consumed_added = BTreeSet::new();
    let mut added_by_extension: HashMap<String, Vec<&String>> = HashMap::new();
    let mut added_content: HashMap<String, Vec<u8>> = HashMap::new();

    for new_path in added.iter() {
        let Some(right) = right_files.get(new_path) else {
            continue;
        };
        if right.size > max_file_size {
            continue;
        }
        added_by_extension
            .entry(extension_key(new_path))
            .or_default()
            .push(new_path);
    }

    for old_path in deleted.iter() {
        let Some(left) = left_files.get(old_path) else {
            continue;
        };
        if left.size > max_file_size {
            continue;
        }
        let left_bytes = match read_file_bytes(left) {
            Ok(bytes) => bytes,
            Err(err) => {
                skipped_files.push(json!({
                    "path": old_path,
                    "reason": "rename_left_read_failed",
                    "message": err.to_string()
                }));
                continue;
            }
        };
        let mut best_match: Option<(String, f64)> = None;
        let old_extension = extension_key(old_path);
        let candidates = added_by_extension
            .get(&old_extension)
            .map(Vec::as_slice)
            .unwrap_or_default();

        for &new_path in candidates {
            if consumed_added.contains(new_path) {
                continue;
            }
            let Some(right) = right_files.get(new_path) else {
                continue;
            };
            if right.size > max_file_size {
                continue;
            }
            if !sizes_can_be_similar(left.size, right.size, similarity_threshold) {
                continue;
            }
            if !added_content.contains_key(new_path) {
                match read_file_bytes(right) {
                    Ok(bytes) => {
                        added_content.insert(new_path.clone(), bytes);
                    }
                    Err(err) => {
                        skipped_files.push(json!({
                            "path": new_path,
                            "reason": "rename_right_read_failed",
                            "message": err.to_string()
                        }));
                        continue;
                    }
                }
            }
            let Some(right_bytes) = added_content.get(new_path) else {
                continue;
            };
            let similarity = if left_bytes == *right_bytes {
                1.0
            } else if crate::tools::read_file::is_probably_binary(&left_bytes)
                || crate::tools::read_file::is_probably_binary(right_bytes)
            {
                0.0
            } else {
                let (left_text, _) = crate::tools::read_file::decode_fuzzy(&left_bytes);
                let (right_text, _) = crate::tools::read_file::decode_fuzzy(right_bytes);
                line_similarity(&left_text, &right_text)
            };

            if similarity >= similarity_threshold
                && best_match
                    .as_ref()
                    .is_none_or(|(_, best_similarity)| similarity > *best_similarity)
            {
                best_match = Some((new_path.clone(), similarity));
            }
        }

        if let Some((new_path, similarity)) = best_match {
            renamed.push(json!({
                "old_path": old_path,
                "new_path": new_path,
                "similarity": similarity,
                "modified_after_rename": similarity < 1.0
            }));
            consumed_deleted.insert(old_path.clone());
            consumed_added.insert(new_path);
        }
    }

    deleted.retain(|path| !consumed_deleted.contains(path));
    added.retain(|path| !consumed_added.contains(path));
    Ok(renamed)
}

fn read_file_bytes(info: &FileInfo) -> Result<Vec<u8>> {
    std::fs::read(&info.absolute_path)
        .with_context(|| format!("Failed to read {}", info.absolute_path.display()))
}

fn sizes_can_be_similar(left_size: u64, right_size: u64, similarity_threshold: f64) -> bool {
    if left_size == right_size || similarity_threshold <= 0.0 {
        return true;
    }
    let larger = left_size.max(right_size) as f64;
    let smaller = left_size.min(right_size) as f64;
    smaller == 0.0 || smaller / larger >= similarity_threshold.min(0.95) * 0.5
}

fn metadata_only_change(relative_path: &str, left: &FileInfo, right: &FileInfo) -> Value {
    json!({
        "path": relative_path,
        "left_size_bytes": left.size,
        "right_size_bytes": right.size,
        "left_modified_at": left.modified_at,
        "right_modified_at": right.modified_at,
        "risk_category": risk_category(relative_path),
        "summary_only": true
    })
}

fn capped_strings(items: &[String]) -> Vec<String> {
    items.iter().take(TOP_LEVEL_LIST_LIMIT).cloned().collect()
}

fn capped_values(items: &[Value]) -> Vec<Value> {
    items.iter().take(TOP_LEVEL_LIST_LIMIT).cloned().collect()
}

fn build_text_change(
    relative_path: &str,
    left: &FileInfo,
    right: &FileInfo,
    left_bytes: &[u8],
    right_bytes: &[u8],
    options: &Options,
    diff_bytes_used: &mut usize,
) -> TextChange {
    let (left_text, left_encoding) = crate::tools::read_file::decode_fuzzy(left_bytes);
    let (right_text, right_encoding) = crate::tools::read_file::decode_fuzzy(right_bytes);
    let diff = TextDiff::from_lines(&left_text, &right_text);
    let (inserted_lines, deleted_lines) = count_changed_lines(&diff);

    let mut item = json!({
        "path": relative_path,
        "left_size_bytes": left.size,
        "right_size_bytes": right.size,
        "left_modified_at": left.modified_at,
        "right_modified_at": right.modified_at,
        "left_encoding": left_encoding,
        "right_encoding": right_encoding,
        "inserted_lines": inserted_lines,
        "deleted_lines": deleted_lines,
        "risk_category": risk_category(relative_path),
        "affected_symbols": affected_symbols(&left_text, &right_text)
    });

    if options.include_content_diff
        && !options.summary_only
        && *diff_bytes_used < options.max_diff_bytes
    {
        let rendered = diff
            .unified_diff()
            .context_radius(3)
            .header(
                &format!("left/{}", relative_path),
                &format!("right/{}", relative_path),
            )
            .to_string();
        let remaining = options.max_diff_bytes - *diff_bytes_used;
        let (bounded, truncated) = truncate_utf8(&rendered, remaining);
        *diff_bytes_used += bounded.len();
        item["unified_diff"] = json!(bounded);
        item["diff_truncated"] = json!(truncated);
    }

    TextChange {
        value: item,
        inserted_lines,
        deleted_lines,
    }
}

fn collect_changed_paths(
    added: &[String],
    deleted: &[String],
    renamed: &[Value],
    modified: &[Value],
    binary_files: &[Value],
) -> Vec<String> {
    let mut paths: BTreeSet<String> = added.iter().chain(deleted.iter()).cloned().collect();
    for item in renamed {
        if let Some(path) = item.get("new_path").and_then(|value| value.as_str()) {
            paths.insert(path.to_string());
        }
    }
    for item in modified.iter().chain(binary_files.iter()) {
        if let Some(path) = item.get("path").and_then(|value| value.as_str()) {
            paths.insert(path.to_string());
        }
    }
    paths.into_iter().collect()
}

fn top_counts(values: impl Iterator<Item = String>) -> Vec<Value> {
    let mut counts = BTreeMap::<String, usize>::new();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    let mut items: Vec<(String, usize)> = counts.into_iter().collect();
    items.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    items
        .into_iter()
        .take(DEFAULT_TOP_LIMIT)
        .map(|(name, count)| json!({ "name": name, "count": count }))
        .collect()
}

fn changed_by_directory(paths: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut grouped = BTreeMap::<String, Vec<String>>::new();
    for path in paths {
        grouped
            .entry(top_directory(path))
            .or_default()
            .push(path.clone());
    }
    for paths in grouped.values_mut() {
        paths.truncate(GROUPED_PATH_LIMIT);
    }
    grouped
}

fn top_directory(path: &str) -> String {
    // Files directly under the compared roots belong to ".", not to a
    // "directory" named after the file itself.
    match path.split_once('/') {
        Some((directory, _)) => directory.to_string(),
        None => ".".to_string(),
    }
}

fn extension_key(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{}", value.to_ascii_lowercase()))
        .unwrap_or_else(|| "[no extension]".to_string())
}

fn risk_hints(paths: &[String]) -> Vec<Value> {
    let mut hints = BTreeMap::<String, Vec<String>>::new();
    for path in paths {
        let category = risk_category(path);
        if category != "general" {
            let paths = hints.entry(category).or_default();
            if paths.len() < GROUPED_PATH_LIMIT {
                paths.push(path.clone());
            }
        }
    }
    hints
        .into_iter()
        .map(|(category, paths)| json!({ "category": category, "paths": paths }))
        .collect()
}

fn risk_category(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    let file_name = lower.rsplit('/').next().unwrap_or(&lower);
    if lower.contains("auth") || lower.contains("login") || lower.contains("permission") {
        "auth/security".to_string()
    } else if lower.contains("migration")
        || lower.contains("schema")
        || lower.contains("database")
        || lower.contains("db/")
    {
        "database".to_string()
    } else if lower.contains("api") || lower.contains("route") || lower.contains("controller") {
        "api".to_string()
    } else if lower.contains(".github")
        || lower.contains("ci")
        || lower.contains("workflow")
        || file_name == "dockerfile"
    {
        "ci/deployment".to_string()
    } else if matches!(
        file_name,
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "requirements.txt"
            | "pyproject.toml"
            | "go.mod"
            | "go.sum"
    ) {
        "dependencies".to_string()
    } else if lower.contains("config")
        || lower.ends_with(".env")
        || lower.ends_with(".toml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
    {
        "config".to_string()
    } else if lower.contains("test") || lower.contains("spec") {
        "tests".to_string()
    } else {
        "general".to_string()
    }
}

fn line_similarity(left: &str, right: &str) -> f64 {
    let diff = TextDiff::from_lines(left, right);
    let mut equal = 0usize;
    let mut changed = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => equal += 1,
            ChangeTag::Insert | ChangeTag::Delete => changed += 1,
        }
    }
    if equal + changed == 0 {
        1.0
    } else {
        equal as f64 / (equal + changed) as f64
    }
}

fn affected_symbols(left: &str, right: &str) -> Vec<String> {
    let mut symbols = BTreeSet::new();
    for text in [left, right] {
        for line in text.lines() {
            if let Some(symbol) = symbol_from_line(line) {
                symbols.insert(symbol);
            }
        }
    }
    symbols.into_iter().take(DEFAULT_TOP_LIMIT).collect()
}

fn symbol_from_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let prefixes = [
        "pub fn ",
        "fn ",
        "async fn ",
        "pub async fn ",
        "class ",
        "struct ",
        "enum ",
        "interface ",
        "def ",
        "function ",
    ];
    for prefix in prefixes {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn count_changed_lines(diff: &TextDiff<'_, '_, str>) -> (usize, usize) {
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => inserted += 1,
            ChangeTag::Delete => deleted += 1,
            ChangeTag::Equal => {}
        }
    }
    (inserted, deleted)
}

fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }

    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn arg_usize(args: &Value, key: &str, default_value: usize, min: usize, max: usize) -> usize {
    args.get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default_value)
        .clamp(min, max)
}

fn arg_f64(args: &Value, key: &str, default_value: f64) -> f64 {
    args.get(key)
        .and_then(|value| value.as_f64())
        .unwrap_or(default_value)
        .clamp(0.0, 1.0)
}

fn build_warnings(max_diff_bytes: usize, diff_bytes_used: usize) -> Vec<String> {
    if diff_bytes_used >= max_diff_bytes {
        vec![format!(
            "Unified diff output reached max_diff_bytes={}; call again with a narrower scope or higher limit for more detail.",
            max_diff_bytes
        )]
    } else {
        Vec::new()
    }
}

fn render_markdown_report(result: &Value) -> String {
    let summary = result.get("summary").unwrap_or(&Value::Null);
    let mut lines = vec![
        "# Directory Compare Report".to_string(),
        String::new(),
        format!(
            "- Left: `{}`",
            result
                .get("left_path")
                .and_then(|value| value.as_str())
                .unwrap_or("")
        ),
        format!(
            "- Right: `{}`",
            result
                .get("right_path")
                .and_then(|value| value.as_str())
                .unwrap_or("")
        ),
        format!("- Changed files: {}", number(summary, "changed_files")),
        format!(
            "- Added / deleted / renamed: {} / {} / {}",
            number(summary, "added_files"),
            number(summary, "deleted_files"),
            number(summary, "renamed_files")
        ),
        format!(
            "- Modified text / binary: {} / {}",
            number(summary, "modified_text_files"),
            number(summary, "modified_binary_files")
        ),
        format!(
            "- Inserted / deleted lines: {} / {}",
            number(summary, "inserted_lines"),
            number(summary, "deleted_lines")
        ),
        String::new(),
    ];

    push_named_counts(
        &mut lines,
        "Top Changed Directories",
        result.get("top_changed_directories"),
    );
    push_named_counts(&mut lines, "Extensions", result.get("extensions_summary"));
    push_string_list(&mut lines, "Added Files", result.get("added_files"));
    push_string_list(&mut lines, "Deleted Files", result.get("deleted_files"));
    push_renames(&mut lines, result.get("renamed_files"));
    push_modified(&mut lines, result.get("modified_files"));
    lines.join("\n")
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(|value| value.as_u64()).unwrap_or(0)
}

fn push_named_counts(lines: &mut Vec<String>, title: &str, value: Option<&Value>) {
    let Some(items) = value.and_then(|value| value.as_array()) else {
        return;
    };
    if items.is_empty() {
        return;
    }
    lines.push(format!("## {}", title));
    for item in items {
        lines.push(format!(
            "- `{}`: {}",
            item.get("name")
                .and_then(|value| value.as_str())
                .unwrap_or(""),
            item.get("count")
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        ));
    }
    lines.push(String::new());
}

fn push_string_list(lines: &mut Vec<String>, title: &str, value: Option<&Value>) {
    let Some(items) = value.and_then(|value| value.as_array()) else {
        return;
    };
    if items.is_empty() {
        return;
    }
    lines.push(format!("## {}", title));
    for item in items.iter().take(DEFAULT_TOP_LIMIT) {
        lines.push(format!("- `{}`", item.as_str().unwrap_or("")));
    }
    lines.push(String::new());
}

fn push_renames(lines: &mut Vec<String>, value: Option<&Value>) {
    let Some(items) = value.and_then(|value| value.as_array()) else {
        return;
    };
    if items.is_empty() {
        return;
    }
    lines.push("## Renamed Files".to_string());
    for item in items.iter().take(DEFAULT_TOP_LIMIT) {
        let old_path = item
            .get("old_path")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let new_path = item
            .get("new_path")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        lines.push(format!("- `{}` → `{}`", old_path, new_path));
    }
    lines.push(String::new());
}

fn push_modified(lines: &mut Vec<String>, value: Option<&Value>) {
    let Some(items) = value.and_then(|value| value.as_array()) else {
        return;
    };
    if items.is_empty() {
        return;
    }
    lines.push("## Modified Files".to_string());
    for item in items.iter().take(DEFAULT_TOP_LIMIT) {
        lines.push(format!(
            "- `{}` (+{}, -{})",
            item.get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(""),
            item.get("inserted_lines")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            item.get("deleted_lines")
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        ));
    }
    lines.push(String::new());
}

fn normalize_path(path: &Path) -> String {
    crate::common::normalize_display_path(path)
}

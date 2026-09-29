use anyhow::{Context, Result};
use ignore::{WalkBuilder, WalkState};
use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task;

use super::path_filters::{
    Pattern, apply_walk_overrides, compile_patterns, configure_walk_filters, filtered_file_counts,
    is_vcs_metadata_dir, matches_patterns, parse_pattern_strings, passes_patterns,
};
use crate::indexer::{is_path_index_available, visit_indexed_entries_under};
use crate::limits::{
    DEFAULT_WORKSPACE_LINE_COUNT_BYTES, MAX_SKIPPED_FILE_DETAILS, MAX_WORKSPACE_LINE_COUNT_BYTES,
};

const DEFAULT_MAX_LINE_COUNT_BYTES: u64 = DEFAULT_WORKSPACE_LINE_COUNT_BYTES;
const MAX_LINE_COUNT_BYTES_LIMIT: u64 = MAX_WORKSPACE_LINE_COUNT_BYTES;
const DEFAULT_MAX_FILES: usize = 100_000;
const MAX_FILES_LIMIT: usize = 1_000_000;

pub fn schema() -> Value {
    json!({
        "name": "workspace_stats",
        "title": "Workspace stats",
        "description": "Summarize file, line, and language counts for a directory. Use to estimate repository size and decide whether text_search needs narrower paths/includes before content search.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to summarize. In large repos, run on candidate subsystems rather than only workspace root." },
                "max_line_count_bytes": { "type": "integer", "description": "Maximum bytes per file to count lines from; lower values keep scans fast." },
                "max_files": { "type": "integer", "description": "Maximum files to inspect before returning a partial result. Defaults to 100000." },
                "include_ignored": { "type": "boolean", "description": "Include files ignored by .gitignore, .git/info/exclude, global gitignore, or .ignore files." },
                "include_hidden": { "type": "boolean", "description": "Include hidden files and directories except VCS metadata directories such as .git, unless scoped directly." },
                "includes": { "type": "array", "items": { "type": "string" }, "description": "Optional glob include filters, e.g. **/*.rs." },
                "excludes": { "type": "array", "items": { "type": "string" }, "description": "Optional glob exclude filters for build/generated/vendor areas." },
                "verbose": { "type": "boolean", "description": "Include scan, filtering, line-count, and index diagnostics. Incomplete or partial line counts include diagnostics automatically." }
            },
            "required": ["path"]
        }
    })
}

fn get_lang_from_ext(ext: &str) -> &'static str {
    match ext {
        "rs" => "Rust",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "ts" | "tsx" => "TypeScript",
        "py" | "pyi" => "Python",
        "go" => "Go",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "c" | "h" => "C",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "inl" | "inc" => "C++",
        "cs" => "C#",
        "swift" => "Swift",
        "dart" => "Dart",
        "rb" => "Ruby",
        "php" => "PHP",
        "html" | "htm" => "HTML",
        "css" | "scss" | "sass" | "less" => "CSS",
        "md" | "rst" => "Markdown",
        "json" => "JSON",
        "yaml" | "yml" => "YAML",
        "toml" => "TOML",
        "xml" => "XML",
        "sh" | "bash" | "zsh" => "Shell",
        "ps1" | "bat" | "cmd" => "PowerShell/Batch",
        "sql" => "SQL",
        "proto" => "Protobuf",
        "gn" | "gni" => "GN",
        "gyp" | "gypi" => "GYP",
        "cmake" | "mk" => "CMake/Make",
        "lua" => "Lua",
        "vue" | "svelte" => "Vue/Svelte",
        "m" | "mm" => "Objective-C",
        _ => "Other",
    }
}

fn count_lines_in_file(path: &Path) -> u64 {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return 0,
    };

    let mut buffer = [0u8; 64 * 1024];
    let mut newline_count = 0u64;
    let mut has_bytes = false;
    let mut ends_with_newline = false;

    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return 0,
        };

        has_bytes = true;
        newline_count += bytecount::count(&buffer[..read], b'\n') as u64;
        ends_with_newline = buffer[read - 1] == b'\n';
    }

    if !has_bytes {
        0
    } else if ends_with_newline {
        newline_count
    } else {
        newline_count + 1
    }
}

pub async fn execute(args: &Value) -> Result<Value> {
    let args_owned = args.clone();
    task::spawn_blocking(move || execute_blocking(args_owned))
        .await
        .context("workspace_stats background task failed to join")?
}

fn execute_blocking(args: Value) -> Result<Value> {
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .context("Missing path")?;
    let path = crate::common::resolve_tool_path(path_str);

    if !path.exists() || !path.is_dir() {
        return Err(anyhow::anyhow!(
            "Path is not a valid directory: {}",
            path_str
        ));
    }

    let canonical_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let cancellation =
        crate::cancellation::token_for_scan(&args, std::slice::from_ref(&canonical_path));
    let max_line_count_bytes = parse_u64_arg(
        &args,
        "max_line_count_bytes",
        DEFAULT_MAX_LINE_COUNT_BYTES,
        0,
        MAX_LINE_COUNT_BYTES_LIMIT,
    );
    let max_files = parse_usize_arg(&args, "max_files", DEFAULT_MAX_FILES, 1, MAX_FILES_LIMIT);
    let include_globs = parse_pattern_strings(args.get("includes"));
    let exclude_globs = parse_pattern_strings(args.get("excludes"));
    let include_ignored = args
        .get("include_ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_hidden = args
        .get("include_hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let verbose = args
        .get("verbose")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let includes = Arc::new(compile_patterns(&include_globs)?);
    let excludes = Arc::new(compile_patterns(&exclude_globs)?);

    if !include_ignored && !include_hidden && is_path_index_available(&canonical_path) {
        let indexed_stats = build_workspace_stats_from_index(
            path_str,
            &canonical_path,
            max_line_count_bytes,
            max_files,
            includes.as_ref(),
            excludes.as_ref(),
            cancellation.as_ref(),
        );
        if indexed_stats
            .get("indexed_entries_available")
            .and_then(|value| value.as_u64())
            .unwrap_or(0)
            > 0
        {
            let indexed_stats = attach_excluded_file_counts(
                indexed_stats,
                &canonical_path,
                include_ignored,
                include_hidden,
            );
            return Ok(finalize_workspace_stats_response(indexed_stats, verbose));
        }
    }

    let walk_threads = crate::common::bounded_walk_threads();
    let mut walker = WalkBuilder::new(&path);
    configure_walk_filters(&mut walker, include_ignored, include_hidden);
    walker.threads(walk_threads);
    apply_walk_overrides(&mut walker, &canonical_path, &include_globs, &exclude_globs)?;
    let filter_root = canonical_path.clone();
    let filter_excludes = Arc::clone(&excludes);
    walker.filter_entry(move |entry| {
        if entry.path() == filter_root {
            return true;
        }
        if is_vcs_metadata_dir(entry.path(), &filter_root) {
            return false;
        }
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir())
        {
            return true;
        }
        let rel = relative_path(entry.path(), &filter_root);
        !matches_patterns(entry.path(), &rel, filter_excludes.as_ref())
    });

    let shard_count = walk_threads.max(1);
    let accumulators = Arc::new(
        (0..shard_count)
            .map(|_| Mutex::new(StatsAccumulator::default()))
            .collect::<Vec<_>>(),
    );
    let next_shard = Arc::new(AtomicUsize::new(0));
    let inspected_files = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(AtomicBool::new(false));
    let root_for_workers = canonical_path.clone();

    walker.build_parallel().run(|| {
        let includes = Arc::clone(&includes);
        let excludes = Arc::clone(&excludes);
        let accumulators = Arc::clone(&accumulators);
        let inspected_files = Arc::clone(&inspected_files);
        let truncated = Arc::clone(&truncated);
        let cancellation = cancellation.clone();
        let shard_index = next_shard.fetch_add(1, Ordering::Relaxed) % shard_count;
        let root = root_for_workers.clone();

        Box::new(move |result| {
            let entry = match result {
                Ok(entry) => entry,
                Err(_) => return WalkState::Continue,
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }

            let seen = inspected_files.fetch_add(1, Ordering::Relaxed);
            let files_seen = seen.saturating_add(1);
            if crate::cancellation::report_scan_progress(cancellation.as_ref(), files_seen) {
                return WalkState::Quit;
            }
            if seen >= max_files {
                truncated.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }

            let rel = relative_path(entry.path(), &root);
            if !passes_patterns(entry.path(), &rel, includes.as_ref(), excludes.as_ref()) {
                let mut accumulator = match accumulators[shard_index].lock() {
                    Ok(guard) => guard,
                    Err(_) => return WalkState::Quit,
                };
                accumulator.record_skipped_file();
                return WalkState::Continue;
            }

            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let ext = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let lang = get_lang_from_ext(ext);
            let should_count = should_count_lines(lang, size, max_line_count_bytes);
            let line_count = if should_count {
                count_lines_in_file(entry.path())
            } else {
                0
            };

            let mut accumulator = match accumulators[shard_index].lock() {
                Ok(guard) => guard,
                Err(_) => return WalkState::Quit,
            };
            accumulator.record_matched_file(
                rel,
                size,
                lang,
                line_count,
                should_count,
                max_line_count_bytes,
            );
            WalkState::Continue
        })
    });
    crate::cancellation::finish_scan_progress(
        cancellation.as_ref(),
        inspected_files.load(Ordering::Relaxed),
    );

    let mut stats = StatsAccumulator::default();
    for accumulator in accumulators.iter() {
        let mut accumulator = accumulator
            .lock()
            .map_err(|_| anyhow::anyhow!("workspace_stats accumulator is unavailable"))?;
        stats.merge(std::mem::take(&mut *accumulator));
    }

    let mut languages_out = Vec::new();
    for (lang, file_count) in &stats.lang_files {
        languages_out.push(json!({
            "language": lang,
            "files": file_count,
            "lines": stats.lang_lines.get(lang).unwrap_or(&0),
            "size_bytes": stats.lang_size.get(lang).unwrap_or(&0)
        }));
    }

    languages_out.sort_by(|a, b| {
        let count_a = a.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
        let count_b = b.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
        count_b.cmp(&count_a)
    });

    stats.largest_files.sort_by_key(|file| Reverse(file.1));
    stats.largest_files.truncate(10);
    let largest_out: Vec<Value> = stats
        .largest_files
        .iter()
        .map(|(p, s)| json!({ "path": p, "size_bytes": s }))
        .collect();

    let line_count_skipped_large_files_omitted = stats
        .line_count_skipped_large_count
        .saturating_sub(stats.line_count_skipped_large_files.len());
    let limit_reached = truncated.load(Ordering::Relaxed);
    let response = json!({
        "path": normalize_path(&canonical_path),
        "input_path": path_str,
        "canonical_path": normalize_path(&canonical_path),
        "total_files": stats.total_files,
        "files_walked": stats.files_walked,
        "files_skipped_by_patterns": stats.files_skipped_by_patterns,
        "total_lines": stats.total_lines,
        "total_size_bytes": stats.total_size,
        "total_size_mb": format!("{:.1}", stats.total_size as f64 / 1_048_576.0),
        "languages_breakdown": languages_out,
        "largest_files": largest_out,
        "line_counted_files": stats.line_counted_files,
        "line_count_skipped_files": stats.line_count_skipped_files,
        "line_count_skipped_large_count": stats.line_count_skipped_large_count,
        "line_count_skipped_large_files": stats.line_count_skipped_large_files,
        "line_count_skipped_large_files_omitted": line_count_skipped_large_files_omitted,
        "max_line_count_bytes": max_line_count_bytes,
        "max_files": max_files,
        "complete": !limit_reached,
        "limit_reached": limit_reached,
        "limit_reason": if limit_reached { Some("max_files") } else { None },
        "line_counts_complete": stats.line_count_skipped_files == 0,
        "search_strategy": "filesystem_walk",
        "metadata_index_used": false,
        "include_ignored": include_ignored,
        "include_hidden": include_hidden,
        "note": if stats.line_count_skipped_files > 0 {
            format!("Line counts skip files over {} bytes or extensions treated as non-text.", max_line_count_bytes)
        } else {
            String::new()
        }
    });
    let response =
        attach_excluded_file_counts(response, &canonical_path, include_ignored, include_hidden);
    Ok(finalize_workspace_stats_response(response, verbose))
}

fn attach_excluded_file_counts(
    mut response: Value,
    canonical_path: &Path,
    include_ignored: bool,
    include_hidden: bool,
) -> Value {
    let counts = filtered_file_counts(
        &[canonical_path.to_path_buf()],
        include_ignored,
        include_hidden,
    );
    if counts.ignored_files > 0 || counts.hidden_files > 0 || !counts.complete {
        crate::common::insert_object_field(
            &mut response,
            "excluded_files",
            json!({
                "ignored": counts.ignored_files,
                "hidden": counts.hidden_files,
                "counts_complete": counts.complete
            }),
        );
    }
    response
}

fn build_workspace_stats_from_index(
    path_str: &str,
    canonical_path: &Path,
    max_line_count_bytes: u64,
    max_files: usize,
    includes: &[Pattern],
    excludes: &[Pattern],
    cancellation: Option<&crate::cancellation::CancellationToken>,
) -> Value {
    let mut stats = StatsAccumulator::default();
    let mut truncated = false;
    let mut scanned_files = 0usize;

    let indexed_entries_available = visit_indexed_entries_under(canonical_path, |entry| {
        if entry.is_dir {
            return true;
        }
        scanned_files = scanned_files.saturating_add(1);
        if crate::cancellation::report_scan_progress(cancellation, scanned_files) {
            return false;
        }

        let relative = relative_path(&entry.path, canonical_path);
        if (!includes.is_empty() || !excludes.is_empty())
            && !passes_patterns(&entry.path, &relative, includes, excludes)
        {
            stats.record_skipped_file();
            return true;
        }

        if stats.total_files >= max_files {
            truncated = true;
            return false;
        }

        let lang = get_lang_from_ext(&entry.extension_lower);
        let should_count = should_count_lines(lang, entry.size, max_line_count_bytes);
        let line_count = if should_count {
            count_lines_in_file(&entry.path)
        } else {
            0
        };
        stats.record_matched_file(
            relative,
            entry.size,
            lang,
            line_count,
            should_count,
            max_line_count_bytes,
        );
        true
    })
    .unwrap_or(0);
    crate::cancellation::finish_scan_progress(cancellation, scanned_files);

    let mut languages_out = Vec::new();
    for (lang, file_count) in &stats.lang_files {
        languages_out.push(json!({
            "language": lang,
            "files": file_count,
            "lines": stats.lang_lines.get(lang).unwrap_or(&0),
            "size_bytes": stats.lang_size.get(lang).unwrap_or(&0)
        }));
    }

    languages_out.sort_by(|a, b| {
        let count_a = a.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
        let count_b = b.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
        count_b.cmp(&count_a)
    });

    stats.largest_files.sort_by_key(|file| Reverse(file.1));
    stats.largest_files.truncate(10);
    let largest_out: Vec<Value> = stats
        .largest_files
        .iter()
        .map(|(p, s)| json!({ "path": p, "size_bytes": s }))
        .collect();

    let line_count_skipped_large_files_omitted = stats
        .line_count_skipped_large_count
        .saturating_sub(stats.line_count_skipped_large_files.len());
    json!({
        "path": normalize_path(canonical_path),
        "input_path": path_str,
        "canonical_path": normalize_path(canonical_path),
        "total_files": stats.total_files,
        "files_walked": stats.files_walked,
        "files_skipped_by_patterns": stats.files_skipped_by_patterns,
        "total_lines": stats.total_lines,
        "total_size_bytes": stats.total_size,
        "total_size_mb": format!("{:.1}", stats.total_size as f64 / 1_048_576.0),
        "languages_breakdown": languages_out,
        "largest_files": largest_out,
        "line_counted_files": stats.line_counted_files,
        "line_count_skipped_files": stats.line_count_skipped_files,
        "line_count_skipped_large_count": stats.line_count_skipped_large_count,
        "line_count_skipped_large_files": stats.line_count_skipped_large_files,
        "line_count_skipped_large_files_omitted": line_count_skipped_large_files_omitted,
        "max_line_count_bytes": max_line_count_bytes,
        "max_files": max_files,
        "complete": !truncated,
        "limit_reached": truncated,
        "limit_reason": if truncated { Some("max_files") } else { None },
        "line_counts_complete": stats.line_count_skipped_files == 0,
        "search_strategy": "lmdb_metadata",
        "metadata_index_used": true,
        "include_ignored": false,
        "include_hidden": false,
        "indexed_entries_available": indexed_entries_available,
        "note": if stats.line_count_skipped_files > 0 {
            format!("File counts, sizes, and language totals came from LMDB. Line counts skip files over {} bytes or extensions treated as non-text.", max_line_count_bytes)
        } else {
            "File counts, sizes, and language totals came from LMDB; line counts were computed from bounded file reads.".to_string()
        }
    })
}

fn finalize_workspace_stats_response(mut response: Value, verbose: bool) -> Value {
    let complete = response
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let line_counts_complete = response
        .get("line_counts_complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let filtered_files = response
        .get("files_skipped_by_patterns")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let include_diagnostics = verbose || !complete || !line_counts_complete || filtered_files > 0;
    let Some(object) = response.as_object_mut() else {
        return response;
    };

    let mut diagnostics = serde_json::Map::new();
    for key in [
        "input_path",
        "canonical_path",
        "files_walked",
        "files_skipped_by_patterns",
        "max_files",
        "metadata_index_used",
        "include_ignored",
        "include_hidden",
        "indexed_entries_available",
        "note",
    ] {
        if let Some(value) = object.remove(key)
            && !value.is_null()
            && !matches!(&value, Value::String(text) if text.is_empty())
        {
            diagnostics.insert(key.to_string(), value);
        }
    }

    let mut line_counting = serde_json::Map::new();
    for (source, target) in [
        ("line_counted_files", "counted_files"),
        ("line_count_skipped_files", "skipped_files"),
        ("line_count_skipped_large_count", "skipped_large_count"),
        ("line_count_skipped_large_files", "skipped_large_files"),
        (
            "line_count_skipped_large_files_omitted",
            "skipped_large_files_omitted",
        ),
        ("max_line_count_bytes", "max_file_bytes"),
    ] {
        if let Some(value) = object.remove(source)
            && !value.is_null()
            && !matches!(&value, Value::Array(items) if items.is_empty())
        {
            line_counting.insert(target.to_string(), value);
        }
    }
    if !line_counting.is_empty() {
        diagnostics.insert("line_counting".to_string(), Value::Object(line_counting));
    }
    if include_diagnostics && !diagnostics.is_empty() {
        object.insert("diagnostics".to_string(), Value::Object(diagnostics));
    }
    response
}

#[derive(Default)]
struct StatsAccumulator {
    total_files: usize,
    files_walked: usize,
    files_skipped_by_patterns: usize,
    line_counted_files: usize,
    line_count_skipped_files: usize,
    line_count_skipped_large_count: usize,
    line_count_skipped_large_files: Vec<Value>,
    total_size: u64,
    total_lines: u64,
    lang_files: HashMap<&'static str, usize>,
    lang_size: HashMap<&'static str, u64>,
    lang_lines: HashMap<&'static str, u64>,
    largest_files: Vec<(String, u64)>,
}

impl StatsAccumulator {
    fn record_skipped_file(&mut self) {
        self.files_walked += 1;
        self.files_skipped_by_patterns += 1;
    }

    fn record_matched_file(
        &mut self,
        path: String,
        size: u64,
        lang: &'static str,
        line_count: u64,
        line_counted: bool,
        max_line_count_bytes: u64,
    ) {
        self.files_walked += 1;
        self.total_files += 1;
        self.total_size += size;
        self.total_lines += line_count;

        if line_counted {
            self.line_counted_files += 1;
        } else {
            self.line_count_skipped_files += 1;
            if lang != "Other" && size > max_line_count_bytes {
                self.line_count_skipped_large_count += 1;
                if self.line_count_skipped_large_files.len() < MAX_SKIPPED_FILE_DETAILS {
                    self.line_count_skipped_large_files.push(json!({
                        "path": path,
                        "size_bytes": size,
                        "limit_bytes": max_line_count_bytes
                    }));
                }
            }
        }

        *self.lang_files.entry(lang).or_insert(0) += 1;
        *self.lang_size.entry(lang).or_insert(0) += size;
        *self.lang_lines.entry(lang).or_insert(0) += line_count;
        self.push_largest_file(path, size);
    }

    fn merge(&mut self, mut other: Self) {
        self.total_files += other.total_files;
        self.files_walked += other.files_walked;
        self.files_skipped_by_patterns += other.files_skipped_by_patterns;
        self.line_counted_files += other.line_counted_files;
        self.line_count_skipped_files += other.line_count_skipped_files;
        self.line_count_skipped_large_count += other.line_count_skipped_large_count;
        self.total_size += other.total_size;
        self.total_lines += other.total_lines;

        for (lang, count) in other.lang_files {
            *self.lang_files.entry(lang).or_insert(0) += count;
        }
        for (lang, size) in other.lang_size {
            *self.lang_size.entry(lang).or_insert(0) += size;
        }
        for (lang, lines) in other.lang_lines {
            *self.lang_lines.entry(lang).or_insert(0) += lines;
        }

        self.line_count_skipped_large_files
            .append(&mut other.line_count_skipped_large_files);
        self.line_count_skipped_large_files
            .truncate(MAX_SKIPPED_FILE_DETAILS);

        self.largest_files.append(&mut other.largest_files);
        self.largest_files.sort_by_key(|file| Reverse(file.1));
        self.largest_files.truncate(10);
    }

    fn push_largest_file(&mut self, path: String, size: u64) {
        if self.largest_files.len() < 10
            || size > self.largest_files.last().map(|file| file.1).unwrap_or(0)
        {
            self.largest_files.push((path, size));
            if self.largest_files.len() > 20 {
                self.largest_files.sort_by_key(|file| Reverse(file.1));
                self.largest_files.truncate(10);
            }
        }
    }
}

fn should_count_lines(language: &str, size: u64, max_line_count_bytes: u64) -> bool {
    max_line_count_bytes > 0 && size <= max_line_count_bytes && language != "Other"
}

fn parse_usize_arg(args: &Value, name: &str, default: usize, min: usize, max: usize) -> usize {
    args.get(name)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn parse_u64_arg(args: &Value, name: &str, default: u64, min: u64, max: u64) -> u64 {
    args.get(name)
        .and_then(|value| value.as_u64())
        .unwrap_or(default)
        .clamp(min, max)
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .map(normalize_path)
        .filter(|relative| !relative.is_empty())
        .unwrap_or_else(|| normalize_path(path))
}

fn normalize_path(path: &Path) -> String {
    crate::common::normalize_display_path(path)
}

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const FILTER_PROBE_MAX_FILES: usize = 50_000;

#[derive(Clone, Debug, Default)]
pub struct FilteredFileCounts {
    pub ignored_files: usize,
    pub hidden_files: usize,
    pub complete: bool,
}

#[derive(Clone, Debug)]
pub struct Pattern {
    source: String,
    matcher: GlobMatcher,
}

impl Pattern {
    pub fn as_str(&self) -> &str {
        &self.source
    }

    pub fn matches(&self, value: &str) -> bool {
        self.matcher.is_match(normalize_pattern(value))
    }
}

pub fn parse_pattern_strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|pattern| !pattern.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn compile_patterns(patterns: &[String]) -> Result<Vec<Pattern>> {
    patterns
        .iter()
        .map(|pattern| compile_pattern(pattern))
        .collect()
}

pub fn compile_pattern(pattern: &str) -> Result<Pattern> {
    validate_pattern(pattern)?;
    let normalized = normalize_pattern(pattern);
    let mut builder = GlobBuilder::new(&normalized);
    builder
        .case_insensitive(cfg!(windows))
        .literal_separator(true)
        .backslash_escape(false);
    let matcher = builder
        .build()
        .with_context(|| format!("Invalid glob pattern '{}'", pattern))?
        .compile_matcher();
    Ok(Pattern {
        source: normalized,
        matcher,
    })
}

pub fn passes_patterns(
    path: &Path,
    relative_path: &str,
    includes: &[Pattern],
    excludes: &[Pattern],
) -> bool {
    if !includes.is_empty() && !matches_patterns_or_ancestors(path, relative_path, includes) {
        return false;
    }
    !matches_patterns_or_ancestors(path, relative_path, excludes)
}

pub fn matches_patterns(path: &Path, relative_path: &str, patterns: &[Pattern]) -> bool {
    if patterns.is_empty() {
        return false;
    }

    let full_path = crate::common::normalize_display_path(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let values = [relative_path, full_path.as_str(), file_name];
    patterns
        .iter()
        .any(|pattern| values.iter().any(|value| pattern.matches(value)))
}

pub fn matches_patterns_or_ancestors(
    path: &Path,
    relative_path: &str,
    patterns: &[Pattern],
) -> bool {
    if matches_patterns(path, relative_path, patterns) {
        return true;
    }

    let mut ancestor_path = path.parent();
    let mut ancestor_relative = Path::new(relative_path).parent().map(Path::to_path_buf);
    while let (Some(path), Some(relative)) = (ancestor_path, ancestor_relative.as_deref()) {
        if relative.as_os_str().is_empty() {
            break;
        }
        let relative = crate::common::normalize_display_path(relative);
        if matches_patterns(path, &relative, patterns) {
            return true;
        }
        ancestor_path = path.parent();
        ancestor_relative = Path::new(&relative).parent().map(Path::to_path_buf);
    }
    false
}

pub fn apply_walk_overrides(
    walk: &mut WalkBuilder,
    root: &Path,
    includes: &[String],
    excludes: &[String],
) -> Result<()> {
    if includes.is_empty() && excludes.is_empty() {
        return Ok(());
    }

    let mut builder = OverrideBuilder::new(root);
    builder
        .case_insensitive(cfg!(windows))
        .context("Failed to configure walk override case sensitivity")?;
    for pattern in includes {
        add_override(&mut builder, pattern, false)?;
    }
    for pattern in excludes {
        add_override(&mut builder, pattern, true)?;
    }

    let overrides = builder
        .build()
        .context("Invalid include/exclude walk override globs")?;
    walk.overrides(overrides);
    Ok(())
}

pub fn configure_walk_filters(walk: &mut WalkBuilder, include_ignored: bool, include_hidden: bool) {
    walk.hidden(!include_hidden)
        .ignore(!include_ignored)
        .git_ignore(!include_ignored)
        .git_global(!include_ignored)
        .git_exclude(!include_ignored)
        .require_git(false);
}

pub fn is_vcs_metadata_dir(path: &Path, root: &Path) -> bool {
    path != root
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                matches!(name.to_ascii_lowercase().as_str(), ".git" | ".hg" | ".svn")
            })
}

pub fn filtered_file_counts(
    paths: &[PathBuf],
    include_ignored: bool,
    include_hidden: bool,
) -> FilteredFileCounts {
    if include_ignored && include_hidden {
        return FilteredFileCounts {
            complete: true,
            ..FilteredFileCounts::default()
        };
    }

    let (current, current_complete) = collect_probe_files(paths, include_ignored, include_hidden);
    let (all_files, all_complete) = collect_probe_files(paths, true, true);
    let mut counts = FilteredFileCounts {
        complete: current_complete && all_complete,
        ..FilteredFileCounts::default()
    };

    match (include_ignored, include_hidden) {
        (false, false) => {
            let (with_ignored, ignored_complete) = collect_probe_files(paths, true, false);
            counts.ignored_files = with_ignored.difference(&current).count();
            counts.hidden_files = all_files.difference(&with_ignored).count();
            counts.complete &= ignored_complete;
        }
        (false, true) => {
            counts.ignored_files = all_files.difference(&current).count();
        }
        (true, false) => {
            counts.hidden_files = all_files.difference(&current).count();
        }
        (true, true) => unreachable!("inclusive scopes return before probing"),
    }
    counts
}

pub fn filtered_scope_warnings(
    paths: &[PathBuf],
    include_ignored: bool,
    include_hidden: bool,
) -> Vec<String> {
    if include_ignored && include_hidden {
        return Vec::new();
    }
    let counts = filtered_file_counts(paths, include_ignored, include_hidden);
    let qualifier = if counts.complete { "" } else { "at least " };
    let mut warnings = Vec::new();
    if !include_ignored && counts.ignored_files > 0 {
        warnings.push(format!(
            "Scope excludes {qualifier}{} ignored file(s); retry with include_ignored=true to search them.",
            counts.ignored_files
        ));
    }
    if !include_hidden && counts.hidden_files > 0 {
        warnings.push(format!(
            "Scope excludes {qualifier}{} hidden file(s); retry with include_hidden=true to search them. Hidden VCS metadata directories remain excluded unless scoped directly.",
            counts.hidden_files
        ));
    }
    warnings
}

fn collect_probe_files(
    paths: &[PathBuf],
    include_ignored: bool,
    include_hidden: bool,
) -> (HashSet<PathBuf>, bool) {
    let mut files = HashSet::new();
    for input_path in paths {
        let path = input_path
            .canonicalize()
            .unwrap_or_else(|_| input_path.to_path_buf());
        if path.is_file() {
            files.insert(path);
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        let root = path.clone();
        let mut walk = WalkBuilder::new(&root);
        configure_walk_filters(&mut walk, include_ignored, include_hidden);
        walk.filter_entry(move |entry| {
            !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
                || !is_vcs_metadata_dir(entry.path(), &root)
        });
        for entry in walk.build().flatten() {
            if entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                files.insert(
                    entry
                        .path()
                        .canonicalize()
                        .unwrap_or_else(|_| entry.path().to_path_buf()),
                );
                if files.len() >= FILTER_PROBE_MAX_FILES {
                    return (files, false);
                }
            }
        }
    }
    (files, true)
}

pub fn default_generated_vendor_globs(input_paths: &[PathBuf]) -> Vec<String> {
    let include_managed_bin = !input_paths.is_empty()
        && input_paths
            .iter()
            .all(|path| crate::common::workspace_uses_managed_build_outputs(path));
    let mut names = crate::common::DEFAULT_EXCLUDED_DIRECTORY_NAMES.to_vec();
    if include_managed_bin {
        names.push("bin");
    }

    let mut patterns = Vec::with_capacity(names.len() * 2);
    for name in names {
        patterns.push(format!("{name}/**"));
        patterns.push(format!("**/{name}/**"));
    }
    patterns
}

pub fn is_direct_vendor_or_generated_scope(path: &Path) -> bool {
    let include_managed_bin = crate::common::workspace_uses_managed_build_outputs(path);
    crate::common::normalize_display_path(path)
        .split('/')
        .any(|part| crate::common::is_default_excluded_directory_name(part, include_managed_bin))
}

fn add_override(builder: &mut OverrideBuilder, pattern: &str, exclude: bool) -> Result<()> {
    validate_pattern(pattern)?;
    let normalized = normalize_pattern(pattern);
    let override_pattern = if exclude {
        format!("!{normalized}")
    } else {
        normalized
    };

    builder
        .add(&override_pattern)
        .with_context(|| format!("Invalid walk override glob '{}'", pattern))?;
    Ok(())
}

fn validate_pattern(pattern: &str) -> Result<()> {
    if pattern.starts_with('!') {
        return Err(anyhow::anyhow!(
            "Invalid glob pattern '{}': patterns must not start with '!'; pass it through excludes instead",
            pattern
        ));
    }
    Ok(())
}

fn normalize_pattern(pattern: &str) -> String {
    pattern.replace('\\', "/")
}

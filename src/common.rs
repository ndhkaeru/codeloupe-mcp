use serde_json::{Map, Value};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const WALK_THREADS_ENV_VARS: &[&str] = &[
    "CODELOUPE_MCP_WALK_THREADS",
    "codeloupe_mcp_WALK_THREADS",
    "CODEBASE_MCP_WALK_THREADS",
];
pub const INDEX_DIR_ENV_VARS: &[&str] = &[
    "CODELOUPE_MCP_INDEX_DIR",
    "codeloupe_mcp_INDEX_DIR",
    "CODEBASE_MCP_INDEX_DIR",
];
pub const TANTIVY_ENABLED_ENV_VARS: &[&str] = &[
    "CODELOUPE_MCP_TANTIVY_ENABLED",
    "codeloupe_mcp_TANTIVY_ENABLED",
    "CODEBASE_MCP_TANTIVY_ENABLED",
];
pub const WRITE_ROOTS_ENV_VARS: &[&str] = &[
    "CODELOUPE_MCP_WRITE_ROOTS",
    "codeloupe_mcp_WRITE_ROOTS",
    "CODEBASE_MCP_WRITE_ROOTS",
];

const VCS_WORKSPACE_MARKERS: &[&str] = &[".git", ".hg", ".svn"];
const PROJECT_WORKSPACE_MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pnpm-workspace.yaml",
    "yarn.lock",
    "package-lock.json",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "global.json",
    "Directory.Build.props",
    "CMakeLists.txt",
    "composer.json",
    "Gemfile",
    "mix.exs",
    "build.sbt",
];
const PROJECT_WORKSPACE_EXTENSIONS: &[&str] = &["sln", "slnx"];
pub const DEFAULT_EXCLUDED_DIRECTORY_NAMES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".cache",
    ".next",
    ".venv",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "gen",
    "generated",
    "node_modules",
    "obj",
    "out",
    "target",
    "third_party",
    "vendor",
];
const MANAGED_BUILD_MARKERS: &[&str] = &[
    "global.json",
    "Directory.Build.props",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
];
const MANAGED_BUILD_EXTENSIONS: &[&str] = &["sln", "slnx", "csproj", "fsproj", "vbproj"];

pub fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }

    match value {
        Value::Object(object) => object,
        _ => unreachable!("value was normalized into an object"),
    }
}

pub fn insert_object_field(target: &mut Value, key: impl Into<String>, value: Value) {
    ensure_object(target).insert(key.into(), value);
}

pub fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn env_var_os(names: &[&str]) -> Option<OsString> {
    env_var_os_with(names, |name| std::env::var_os(name))
}

fn env_var_os_with(
    names: &[&str],
    mut lookup: impl FnMut(&str) -> Option<OsString>,
) -> Option<OsString> {
    names.iter().find_map(|name| lookup(name))
}

pub fn path_from_input(raw: &str) -> PathBuf {
    uri_to_path(raw).unwrap_or_else(|| PathBuf::from(raw))
}

pub fn uri_to_path(raw: &str) -> Option<PathBuf> {
    if let Some(path_str) = raw.strip_prefix("file:///") {
        return Some(PathBuf::from(percent_decode_uri_path(path_str)));
    }

    if let Some(path_str) = raw.strip_prefix("file://") {
        let decoded = percent_decode_uri_path(path_str);
        if decoded.starts_with('/') || decoded.starts_with('\\') {
            return Some(PathBuf::from(decoded));
        }

        return Some(PathBuf::from(format!("//{}", decoded)));
    }

    None
}

fn percent_decode_uri_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
            continue;
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

pub fn canonicalize_if_exists(path: PathBuf) -> PathBuf {
    if path.exists() {
        path.canonicalize().unwrap_or(path)
    } else {
        lexical_normalize(&path)
    }
}

pub fn common_path_root(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut bases = paths.iter().map(|path| {
        let canonical = canonicalize_if_exists(path.clone());
        if canonical.is_file() {
            canonical
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(canonical)
        } else {
            canonical
        }
    });
    let mut common = bases.next()?;
    let remaining = bases.collect::<Vec<_>>();

    while !remaining.iter().all(|path| path_is_within(path, &common)) {
        if !common.pop() {
            return None;
        }
    }

    Some(common)
}

pub fn looks_like_workspace_root(path: &Path) -> bool {
    if !path.exists() || !path.is_dir() {
        return false;
    }

    has_vcs_workspace_marker(path) || has_project_workspace_marker(path)
}

pub fn discover_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    let mut project_root = None;

    loop {
        if has_vcs_workspace_marker(&current) {
            return Some(canonicalize_if_exists(current));
        }
        if project_root.is_none() && has_project_workspace_marker(&current) {
            project_root = Some(canonicalize_if_exists(current.clone()));
        }

        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }

    project_root
}

fn has_vcs_workspace_marker(path: &Path) -> bool {
    VCS_WORKSPACE_MARKERS
        .iter()
        .any(|marker| path.join(marker).exists())
}

fn has_project_workspace_marker(path: &Path) -> bool {
    if PROJECT_WORKSPACE_MARKERS
        .iter()
        .any(|marker| path.join(marker).exists())
    {
        return true;
    }

    fs_entries_have_project_extension(path)
}

fn fs_entries_have_project_extension(path: &Path) -> bool {
    std::fs::read_dir(path).ok().is_some_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .file_type()
                .ok()
                .is_some_and(|file_type| file_type.is_file())
                && entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        PROJECT_WORKSPACE_EXTENSIONS
                            .iter()
                            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
                    })
        })
    })
}

pub fn is_default_excluded_directory_name(name: &str, include_managed_bin: bool) -> bool {
    DEFAULT_EXCLUDED_DIRECTORY_NAMES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
        || (include_managed_bin && name.eq_ignore_ascii_case("bin"))
}

pub fn workspace_uses_managed_build_outputs(path: &Path) -> bool {
    let start = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let root =
        discover_workspace_root(start).unwrap_or_else(|| canonicalize_if_exists(start.into()));
    if MANAGED_BUILD_MARKERS
        .iter()
        .any(|marker| root.join(marker).exists())
    {
        return true;
    }

    std::fs::read_dir(root).ok().is_some_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .file_type()
                .ok()
                .is_some_and(|file_type| file_type.is_file())
                && entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        MANAGED_BUILD_EXTENSIONS
                            .iter()
                            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
                    })
        })
    })
}

pub fn preferred_workspace_root() -> Option<(PathBuf, &'static str)> {
    if let Some((root, _)) = crate::workspace_control::active_workspace() {
        let root = canonicalize_if_exists(root);
        if root.exists() && root.is_dir() {
            return Some((root, "active_workspace"));
        }
    }

    if let Some(active_runtime) = crate::indexer::get_active_runtime_snapshot() {
        let root = canonicalize_if_exists(PathBuf::from(active_runtime.workspace_root));
        if root.exists() && root.is_dir() {
            return Some((root, "active_index_workspace"));
        }
    }

    if let Ok(current_dir) = std::env::current_dir()
        && let Some(root) = discover_workspace_root(&current_dir)
    {
        return Some((root, "workspace_root_discovered"));
    }

    std::env::current_dir()
        .ok()
        .map(|cwd| (canonicalize_if_exists(cwd), "current_dir"))
}

pub fn default_tool_root() -> PathBuf {
    preferred_workspace_root()
        .map(|(root, _)| root)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn resolve_tool_path(raw: &str) -> PathBuf {
    resolve_tool_path_details(raw).path
}

#[derive(Debug, Clone)]
pub struct ToolPathResolution {
    pub path: PathBuf,
    pub resolution_basis: String,
    pub workspace_root: Option<PathBuf>,
    pub ambiguous_resolutions: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct PathResolutionCandidate {
    path: PathBuf,
    resolution_basis: String,
    workspace_root: Option<PathBuf>,
}

pub fn resolve_tool_path_details(raw: &str) -> ToolPathResolution {
    let path = path_from_input(raw);
    if path.is_absolute() {
        let resolved = canonicalize_if_exists(path);
        let workspace_root = crate::workspace_control::workspace_root_for_path(&resolved)
            .or_else(|| crate::indexer::indexed_workspace_root_for_path(&resolved))
            .or_else(|| discover_workspace_root(&resolved));
        return ToolPathResolution {
            path: resolved,
            resolution_basis: if raw.starts_with("file://") {
                "file_uri".to_string()
            } else {
                "absolute_input".to_string()
            },
            workspace_root,
            ambiguous_resolutions: Vec::new(),
        };
    }

    resolve_relative_tool_path(&path)
}

pub fn resolve_write_tool_path(raw: &str) -> PathBuf {
    let path = path_from_input(raw);
    if path.is_absolute() {
        return lexical_normalize(&path);
    }

    preferred_write_root()
        .map(|(root, _)| lexical_normalize(&root.join(&path)))
        .unwrap_or_else(|| lexical_normalize(&path))
}

pub fn resolve_existing_tool_path(raw: &str) -> anyhow::Result<PathBuf> {
    if contains_path_glob(raw) {
        return Err(anyhow::anyhow!(
            "Path contains glob syntax: '{}' (use includes/excludes for glob filters)",
            raw
        ));
    }

    let resolved = resolve_tool_path(raw);
    if !resolved.exists() {
        return Err(anyhow::anyhow!(
            "Path does not exist: '{}' (resolved to {})",
            raw,
            normalize_display_path(&resolved)
        ));
    }

    Ok(resolved)
}

pub fn contains_path_glob(raw: &str) -> bool {
    raw.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

fn resolve_relative_tool_path(path: &Path) -> ToolPathResolution {
    let mut candidates = Vec::new();

    if let Some((active_root, _)) = crate::workspace_control::active_workspace() {
        push_resolution_root(&mut candidates, active_root, "active_workspace", path);
    }

    if let Some(active_runtime) = crate::indexer::get_active_runtime_snapshot() {
        push_resolution_root(
            &mut candidates,
            PathBuf::from(active_runtime.workspace_root),
            "active_index_workspace",
            path,
        );
    }

    if let Ok(current_dir) = std::env::current_dir() {
        if let Some(root) = discover_workspace_root(&current_dir) {
            push_resolution_root(&mut candidates, root, "workspace_root_discovered", path);
        }
        push_resolution_root(&mut candidates, current_dir, "current_dir", path);
    }

    for root in crate::workspace_control::configured_workspace_roots() {
        push_resolution_root(&mut candidates, root, "configured_workspace", path);
    }

    for runtime in crate::indexer::get_runtime_snapshots() {
        push_resolution_root(
            &mut candidates,
            PathBuf::from(runtime.workspace_root),
            "registered_index_workspace",
            path,
        );
    }

    let selected_index = candidates
        .iter()
        .position(|candidate| candidate.path.exists())
        .unwrap_or(0);
    let selected =
        candidates
            .get(selected_index)
            .cloned()
            .unwrap_or_else(|| PathResolutionCandidate {
                path: PathBuf::from(path),
                resolution_basis: "relative_input".to_string(),
                workspace_root: None,
            });
    let selected_path = canonicalize_if_exists(selected.path);
    let selected_key = normalize_path_key(&selected_path);
    let mut ambiguous_resolutions = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if index == selected_index || !candidate.path.exists() {
            continue;
        }
        let candidate_path = canonicalize_if_exists(candidate.path.clone());
        if normalize_path_key(&candidate_path) != selected_key {
            push_unique_path(&mut ambiguous_resolutions, candidate_path);
        }
    }

    ToolPathResolution {
        path: selected_path,
        resolution_basis: selected.resolution_basis,
        workspace_root: selected.workspace_root,
        ambiguous_resolutions,
    }
}

fn push_resolution_root(
    candidates: &mut Vec<PathResolutionCandidate>,
    root: PathBuf,
    basis: &str,
    relative_path: &Path,
) {
    let root = canonicalize_if_exists(root);
    push_resolution_candidate(
        candidates,
        PathResolutionCandidate {
            path: root.join(relative_path),
            resolution_basis: basis.to_string(),
            workspace_root: Some(root.clone()),
        },
    );

    let first_component = relative_path
        .components()
        .find_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        });
    if first_component == root.file_name()
        && let Some(parent) = root.parent()
    {
        push_resolution_candidate(
            candidates,
            PathResolutionCandidate {
                path: parent.join(relative_path),
                resolution_basis: format!("{basis}_parent"),
                workspace_root: Some(root),
            },
        );
    }
}

fn push_resolution_candidate(
    candidates: &mut Vec<PathResolutionCandidate>,
    candidate: PathResolutionCandidate,
) {
    let candidate_key = normalize_path_key(&candidate.path);
    if candidates
        .iter()
        .any(|existing| normalize_path_key(&existing.path) == candidate_key)
    {
        return;
    }
    candidates.push(candidate);
}

fn push_unique_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    let candidate_key = normalize_path_key(&candidate);
    if paths
        .iter()
        .any(|existing| normalize_path_key(existing) == candidate_key)
    {
        return;
    }

    paths.push(candidate);
}

pub fn preferred_write_root() -> Option<(PathBuf, &'static str)> {
    let writable_roots = crate::workspace_control::writable_workspace_roots();
    let has_declared_workspaces = crate::workspace_control::has_declared_workspace_roots();
    let active_root = crate::workspace_control::active_workspace()
        .map(|(root, _)| canonicalize_if_exists(root))
        .filter(|root| {
            writable_roots
                .iter()
                .any(|allowed| normalize_path_key(allowed) == normalize_path_key(root))
        });

    if let Some(active_root) = active_root.as_ref()
        && writable_roots
            .iter()
            .any(|root| normalize_path_key(root) == normalize_path_key(active_root))
    {
        return Some((active_root.clone(), "configured_active_workspace"));
    }
    if let Ok(current_dir) = std::env::current_dir()
        && let Some(root) = writable_roots
            .iter()
            .find(|root| path_is_within(&current_dir, root))
    {
        return Some((root.clone(), "configured_current_workspace"));
    }
    if writable_roots.len() == 1
        && let Some(root) = writable_roots.first().cloned()
    {
        return Some((root, "configured_workspace"));
    }

    if writable_roots.is_empty()
        && !has_declared_workspaces
        && let Ok(current_dir) = std::env::current_dir()
        && let Some(root) = discover_workspace_root(&current_dir)
        && implicit_write_root_allowed(&root)
    {
        return Some((root, "workspace_root_discovered"));
    }

    None
}

pub fn known_write_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let writable_roots = crate::workspace_control::writable_workspace_roots();
    let has_declared_workspaces = crate::workspace_control::has_declared_workspace_roots();
    if writable_roots.is_empty() && !has_declared_workspaces {
        if let Ok(current_dir) = std::env::current_dir()
            && let Some(root) = discover_workspace_root(&current_dir)
            && implicit_write_root_allowed(&root)
        {
            push_unique_path(&mut roots, root);
        }
    } else {
        for root in writable_roots {
            push_unique_path(&mut roots, root);
        }
    }
    if let Some(value) = env_var_os(WRITE_ROOTS_ENV_VARS) {
        for root in std::env::split_paths(&value) {
            push_unique_path(&mut roots, canonicalize_with_existing_ancestor(&root));
        }
    }
    roots
}

pub fn user_home_directories() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    for variable in ["USERPROFILE", "HOME"] {
        if let Some(home) = std::env::var_os(variable).map(PathBuf::from) {
            push_unique_path(&mut homes, lexical_normalize(&home));
            push_unique_path(&mut homes, canonicalize_with_existing_ancestor(&home));
        }
    }
    homes
}

pub fn implicit_write_root_allowed(path: &Path) -> bool {
    let path = canonicalize_with_existing_ancestor(path);
    if path.parent().is_none() {
        return false;
    }

    !user_home_directories()
        .iter()
        .any(|home| paths_equivalent(&path, home))
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    let left = normalize_display_path(&canonicalize_with_existing_ancestor(left));
    let right = normalize_display_path(&canonicalize_with_existing_ancestor(right));
    #[cfg(windows)]
    {
        left.eq_ignore_ascii_case(&right)
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

pub fn canonicalize_with_existing_ancestor(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }

    let mut missing_components = Vec::new();
    let mut current = path;
    while !current.exists() {
        let Some(name) = current.file_name() else {
            return lexical_normalize(path);
        };
        missing_components.push(name.to_os_string());
        let Some(parent) = current.parent() else {
            return lexical_normalize(path);
        };
        current = parent;
    }

    let mut resolved = current
        .canonicalize()
        .unwrap_or_else(|_| lexical_normalize(current));
    for component in missing_components.into_iter().rev() {
        resolved.push(component);
    }
    lexical_normalize(&resolved)
}

pub fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = canonicalize_with_existing_ancestor(path);
    let root = canonicalize_with_existing_ancestor(root);
    if path.starts_with(&root) {
        return true;
    }

    #[cfg(windows)]
    {
        let path = normalize_display_path(&path).to_ascii_lowercase();
        let root = normalize_display_path(&root)
            .trim_end_matches('/')
            .to_ascii_lowercase();
        path == root || path.starts_with(&format!("{root}/"))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn lexical_path_is_within(path: &Path, root: &Path) -> bool {
    let path = lexical_normalize(path);
    let root = lexical_normalize(root);
    if path.starts_with(&root) {
        return true;
    }

    #[cfg(windows)]
    {
        let path = normalize_display_path(&path).to_ascii_lowercase();
        let root = normalize_display_path(&root)
            .trim_end_matches('/')
            .to_ascii_lowercase();
        path == root || path.starts_with(&format!("{root}/"))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn relative_display_path(path: &Path, root: &Path) -> Option<String> {
    let normalized_path = normalize_display_path(path);
    let normalized_root = normalize_display_path(root)
        .trim_end_matches('/')
        .to_string();
    #[cfg(windows)]
    let (path_key, root_key) = (
        normalized_path.to_ascii_lowercase(),
        normalized_root.to_ascii_lowercase(),
    );
    #[cfg(not(windows))]
    let (path_key, root_key) = (normalized_path.clone(), normalized_root.clone());
    if path_key == root_key {
        return Some(String::new());
    }
    let prefix = format!("{root_key}/");
    path_key
        .starts_with(&prefix)
        .then(|| normalized_path[normalized_root.len() + 1..].to_string())
}

pub fn display_path_relative_to(path: &Path, root: Option<&Path>) -> String {
    let Some(root) = root else {
        return normalize_display_path(path);
    };
    relative_display_path(path, root)
        .filter(|relative| !relative.is_empty())
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| normalize_display_path(path))
}

fn normalize_path_key(path: &Path) -> String {
    let normalized = normalize_display_path(path);
    #[cfg(windows)]
    {
        normalized.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

pub fn normalize_display_path(path: &Path) -> String {
    normalize_display_path_str(&path.to_string_lossy())
}

pub fn normalize_display_path_str(raw: &str) -> String {
    let normalized = raw.replace('\\', "/");
    sanitize_verbatim_path_prefixes(&normalized)
}

pub fn sanitize_verbatim_path_prefixes(raw: &str) -> String {
    raw.replace("//?/UNC/", "//")
        .replace("//?/unc/", "//")
        .replace("//?/", "")
        .replace(r"\\?\UNC\", r"\\")
        .replace(r"\\?\unc\", r"\\")
        .replace(r"\\?\", "")
}

pub fn bounded_walk_threads() -> usize {
    if let Some(value) = env_var_os(WALK_THREADS_ENV_VARS)
        && let Ok(parsed) = value.to_string_lossy().parse::<usize>()
    {
        return parsed.clamp(1, 6);
    }

    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get().clamp(1, 6))
        .unwrap_or(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[test]
    fn configuration_environment_aliases_prioritize_standard_names() {
        let values = HashMap::from([
            ("CODELOUPE_MCP_INDEX_DIR", OsString::from("standard")),
            ("codeloupe_mcp_INDEX_DIR", OsString::from("legacy")),
            ("CODEBASE_MCP_INDEX_DIR", OsString::from("old-name")),
        ]);
        let selected = env_var_os_with(INDEX_DIR_ENV_VARS, |name| values.get(name).cloned());
        assert_eq!(selected, Some(OsString::from("standard")));

        let legacy = HashMap::from([("codeloupe_mcp_TANTIVY_ENABLED", OsString::from("false"))]);
        let selected = env_var_os_with(TANTIVY_ENABLED_ENV_VARS, |name| legacy.get(name).cloned());
        assert_eq!(selected, Some(OsString::from("false")));

        let old_name = HashMap::from([("CODEBASE_MCP_WALK_THREADS", OsString::from("3"))]);
        let selected = env_var_os_with(WALK_THREADS_ENV_VARS, |name| old_name.get(name).cloned());
        assert_eq!(selected, Some(OsString::from("3")));

        let write_roots = HashMap::from([
            ("CODELOUPE_MCP_WRITE_ROOTS", OsString::from("standard")),
            ("codeloupe_mcp_WRITE_ROOTS", OsString::from("legacy")),
        ]);
        let selected = env_var_os_with(WRITE_ROOTS_ENV_VARS, |name| write_roots.get(name).cloned());
        assert_eq!(selected, Some(OsString::from("standard")));
    }

    #[test]
    fn workspace_discovery_prefers_vcs_root_over_nested_manifest() {
        let root = tempdir().unwrap();
        let nested = root.path().join("nested");
        let source = nested.join("src/lib.rs");
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(
            nested.join("pyproject.toml"),
            "[project]\nname = 'nested'\n",
        )
        .unwrap();
        std::fs::write(&source, "fn sample() {}\n").unwrap();

        assert_eq!(
            discover_workspace_root(&source),
            Some(root.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn filesystem_root_is_not_an_implicit_write_root() {
        let current_dir = std::env::current_dir().unwrap();
        let filesystem_root = current_dir.ancestors().last().unwrap();
        assert!(!implicit_write_root_allowed(filesystem_root));
    }
}

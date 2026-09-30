use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Tier {
    Allowed,
    LowRiskWarn,
    MediumRiskWarn,
    HighRiskWarn,
    CriticalRiskWarn,
}

#[derive(Debug, Clone)]
pub struct PathClassification {
    pub canonical: PathBuf,
    pub tier: Tier,
    pub warning: Option<String>,
    pub outside_declared: bool,
}

pub struct PathGuard {
    pub risk_patterns: Vec<glob::Pattern>,
}

impl PathGuard {
    pub fn new(patterns: Vec<String>) -> Self {
        let patterns = patterns
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect();
        Self {
            risk_patterns: patterns,
        }
    }

    pub fn check_path(&self, raw_path: impl AsRef<Path>) -> PathBuf {
        self.classify_path(raw_path).canonical
    }

    pub fn classify_path(&self, raw_path: impl AsRef<Path>) -> PathClassification {
        let path = raw_path.as_ref();
        let canonical = crate::common::canonicalize_with_existing_ancestor(path);
        let canonical_str = crate::common::normalize_display_path(&canonical);
        let display_str =
            crate::common::normalize_display_path(&crate::common::lexical_normalize(path));
        let allowed_roots = crate::common::known_write_roots();
        let inside_declared = allowed_roots
            .iter()
            .any(|root| crate::common::path_is_within(&canonical, root));

        if contains_git_component(path) || contains_git_component(&canonical) {
            return warned(
                canonical,
                Tier::CriticalRiskWarn,
                format!("critical write risk: .git path: {canonical_str}"),
                !inside_declared,
            );
        }

        if sensitive_write_reason(path).is_some() || sensitive_write_reason(&canonical).is_some() {
            return warned(
                canonical,
                Tier::CriticalRiskWarn,
                format!("critical write risk: sensitive home path: {canonical_str}"),
                !inside_declared,
            );
        }

        if system_write_reason(path).is_some() || system_write_reason(&canonical).is_some() {
            return warned(
                canonical,
                Tier::CriticalRiskWarn,
                format!("critical write risk: operating-system path: {canonical_str}"),
                !inside_declared,
            );
        }

        for pattern in &self.risk_patterns {
            let raw_str = path.to_string_lossy().replace('\\', "/");
            if pattern.matches(&raw_str) || pattern.matches(&canonical_str) {
                return warned(
                    canonical,
                    Tier::HighRiskWarn,
                    format!(
                        "high write risk: matched configured risk pattern {}: {canonical_str}",
                        pattern
                    ),
                    !inside_declared,
                );
            }
        }

        if inside_declared {
            return allowed(canonical);
        }

        if allowed_roots
            .iter()
            .any(|root| crate::common::lexical_path_is_within(path, root))
        {
            return warned(
                canonical,
                Tier::CriticalRiskWarn,
                format!(
                    "critical write risk: symlink or junction escapes write root: {canonical_str}"
                ),
                true,
            );
        }

        if let Some(lexical_scope) =
            crate::workspace_control::write_workspace_scope_for_lexical_path(path)
            && !crate::common::path_is_within(&canonical, &lexical_scope.workspace_root)
        {
            return warned(
                canonical,
                Tier::CriticalRiskWarn,
                format!(
                    "critical write risk: symlink or junction escapes repository: {canonical_str}"
                ),
                true,
            );
        }

        if path_resolves_through_link(path) && !same_display_path(path, &canonical) {
            return warned(
                canonical,
                Tier::HighRiskWarn,
                format!(
                    "high write risk: symlink or junction resolves outside the requested path: requested {display_str}; canonical target {canonical_str}"
                ),
                true,
            );
        }

        if crate::workspace_control::write_workspace_scope_for_path(&canonical).is_some() {
            return warned(
                canonical,
                Tier::MediumRiskWarn,
                format!("medium write risk: outside declared roots: {canonical_str}"),
                true,
            );
        }

        let warning = format!("low write risk: outside write roots: {canonical_str}");
        warned(canonical, Tier::LowRiskWarn, warning, true)
    }
}

fn allowed(canonical: PathBuf) -> PathClassification {
    PathClassification {
        canonical,
        tier: Tier::Allowed,
        warning: None,
        outside_declared: false,
    }
}

fn warned(
    canonical: PathBuf,
    tier: Tier,
    warning: impl Into<String>,
    outside_declared: bool,
) -> PathClassification {
    PathClassification {
        canonical,
        tier,
        warning: Some(warning.into()),
        outside_declared,
    }
}

fn system_write_reason(path: &Path) -> Option<&'static str> {
    system_write_roots()
        .iter()
        .any(|root| crate::common::path_is_within(path, root))
        .then_some("system_directory")
}

fn system_write_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(windows)]
    for variable in [
        "SystemRoot",
        "WINDIR",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
    ] {
        if let Some(root) = std::env::var_os(variable).map(PathBuf::from) {
            roots.push(crate::common::canonicalize_with_existing_ancestor(&root));
        }
    }
    #[cfg(not(windows))]
    roots.extend(
        [
            "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/boot", "/proc", "/sys",
            "/dev", "/run", "/System",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    roots
}

fn sensitive_write_reason(path: &Path) -> Option<&'static str> {
    for home in crate::common::user_home_directories() {
        let Some(relative) = crate::common::relative_display_path(path, &home) else {
            continue;
        };
        let components = relative
            .split('/')
            .filter(|component| !component.is_empty())
            .collect::<Vec<_>>();
        let Some(first) = components.first() else {
            continue;
        };

        if [".ssh", ".gnupg", ".aws"]
            .iter()
            .any(|candidate| first.eq_ignore_ascii_case(candidate))
        {
            return Some("credential_directory");
        }
        if first.eq_ignore_ascii_case(".config")
            && components
                .last()
                .is_some_and(|name| name.eq_ignore_ascii_case("credentials"))
        {
            return Some("credential_file");
        }
        if components.len() == 1 && is_shell_profile_name(first) {
            return Some("shell_profile");
        }
        if is_nested_shell_profile(&components) {
            return Some("shell_profile");
        }
    }

    if windows_startup_directories()
        .iter()
        .any(|root| crate::common::path_is_within(path, root))
    {
        return Some("startup_directory");
    }

    None
}

fn is_shell_profile_name(name: &str) -> bool {
    [
        ".profile",
        ".bashrc",
        ".bash_profile",
        ".bash_login",
        ".zshrc",
        ".zprofile",
        ".zlogin",
        ".zlogout",
        ".cshrc",
        ".tcshrc",
        ".kshrc",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn is_nested_shell_profile(components: &[&str]) -> bool {
    if components.len() == 3
        && components[0].eq_ignore_ascii_case(".config")
        && components[1].eq_ignore_ascii_case("fish")
        && components[2].eq_ignore_ascii_case("config.fish")
    {
        return true;
    }

    let powershell_profile = components.last().is_some_and(|name| {
        name.eq_ignore_ascii_case("profile.ps1")
            || name.to_ascii_lowercase().ends_with("_profile.ps1")
    });
    powershell_profile
        && ((components.len() == 3
            && components[0].eq_ignore_ascii_case("documents")
            && ["powershell", "windowspowershell"]
                .iter()
                .any(|directory| components[1].eq_ignore_ascii_case(directory)))
            || (components.len() == 3
                && components[0].eq_ignore_ascii_case(".config")
                && components[1].eq_ignore_ascii_case("powershell")))
}

fn windows_startup_directories() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for variable in ["APPDATA", "ProgramData"] {
        if let Some(base) = std::env::var_os(variable).map(PathBuf::from) {
            roots.push(crate::common::canonicalize_with_existing_ancestor(
                &base.join("Microsoft/Windows/Start Menu/Programs/Startup"),
            ));
        }
    }
    roots
}

fn contains_git_component(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(".git"))
    })
}

fn same_display_path(left: &Path, right: &Path) -> bool {
    let left = crate::common::normalize_display_path(&crate::common::lexical_normalize(left));
    let right = crate::common::normalize_display_path(right);
    #[cfg(windows)]
    {
        left.eq_ignore_ascii_case(&right)
    }
    #[cfg(target_os = "macos")]
    {
        normalize_macos_system_alias(&left) == normalize_macos_system_alias(&right)
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        left == right
    }
}

#[cfg(target_os = "macos")]
fn normalize_macos_system_alias(path: &str) -> String {
    for (canonical, alias) in [
        ("/private/var", "/var"),
        ("/private/tmp", "/tmp"),
        ("/private/etc", "/etc"),
    ] {
        if path == canonical {
            return alias.to_string();
        }
        if let Some(suffix) = path.strip_prefix(canonical)
            && suffix.starts_with('/')
        {
            return format!("{alias}{suffix}");
        }
    }
    path.to_string()
}

fn path_resolves_through_link(path: &Path) -> bool {
    let absolute = crate::common::lexical_normalize(path);
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return true;
            }
        }
    }
    false
}

lazy_static::lazy_static! {
    pub static ref GUARD: PathGuard = PathGuard::new(vec![
        "**/node_modules/**".to_string(),
    ]);
}

#[cfg(test)]
mod tests {
    use super::same_display_path;
    use std::path::Path;

    #[test]
    #[cfg(windows)]
    fn display_path_comparison_is_case_insensitive_on_windows() {
        assert!(same_display_path(
            Path::new(r"C:\Temp\Link"),
            Path::new(r"c:\temp\link")
        ));
    }

    #[test]
    #[cfg(not(windows))]
    fn display_path_comparison_is_case_sensitive_off_windows() {
        assert!(!same_display_path(
            Path::new("/tmp/Link"),
            Path::new("/tmp/link")
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn display_path_comparison_accepts_standard_macos_aliases() {
        assert!(same_display_path(
            Path::new("/var/folders/example/file.txt"),
            Path::new("/private/var/folders/example/file.txt")
        ));
        assert!(same_display_path(
            Path::new("/tmp/example/file.txt"),
            Path::new("/private/tmp/example/file.txt")
        ));
        assert!(same_display_path(
            Path::new("/etc/example.conf"),
            Path::new("/private/etc/example.conf")
        ));
    }
}

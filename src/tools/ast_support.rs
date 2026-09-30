use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tree_sitter::{Language, Node, Parser, Tree};

use crate::indexer::{is_path_index_available, visit_indexed_entries_under};
pub use crate::limits::DEFAULT_AST_FILE_SIZE_BYTES as DEFAULT_AST_FILE_SIZE_LIMIT;

const CODE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx", "inc", "inl", "asm", "s", "S", "rs", "js",
    "jsx", "ts", "tsx", "mjs", "cjs", "vue", "svelte", "py", "pyi", "rb", "php", "java", "kt",
    "kts", "scala", "go", "swift", "dart", "cs", "fs", "fsx", "sh", "bash", "zsh", "ps1", "bat",
    "cmd", "json", "yaml", "yml", "toml", "xml", "html", "htm", "css", "scss", "less", "sql",
    "proto", "graphql", "gql", "gn", "gni", "gyp", "gypi", "cmake", "mk", "mak", "md", "txt",
    "rst", "cfg", "ini", "conf", "lua", "r", "m", "mm", "d", "zig", "nim", "v", "ex", "exs", "elm",
    "clj",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanguageKind {
    Rust,
    JavaScript,
    Python,
    C,
    Cpp,
    Go,
    Java,
    CSharp,
    Php,
    Ruby,
    Swift,
    ObjectiveC,
}

#[derive(Clone, Debug, Default)]
pub struct CandidateVisitStats {
    pub direct_files_considered: usize,
    pub indexed_roots_used: usize,
    pub filesystem_roots_walked: usize,
}

impl CandidateVisitStats {
    pub fn search_strategy(&self) -> &'static str {
        let strategies_used = usize::from(self.direct_files_considered > 0)
            + usize::from(self.indexed_roots_used > 0)
            + usize::from(self.filesystem_roots_walked > 0);
        if strategies_used > 1 {
            "mixed"
        } else if self.indexed_roots_used > 0 {
            "index"
        } else if self.filesystem_roots_walked > 0 {
            "filesystem_walk"
        } else if self.direct_files_considered > 0 {
            "direct_files"
        } else {
            "none"
        }
    }
}

pub struct ParsedAstFile {
    pub language_kind: LanguageKind,
    pub language_name: &'static str,
    pub source: Vec<u8>,
    pub tree: Tree,
}

#[derive(Clone)]
pub struct SymbolCandidate<'a> {
    pub node: Node<'a>,
    pub name: String,
    pub qualified_name: String,
    pub parent: Option<String>,
}

pub fn parse_language_filter(raw: Option<&str>) -> Result<Option<LanguageKind>> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    match raw.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Ok(Some(LanguageKind::Rust)),
        "python" | "py" => Ok(Some(LanguageKind::Python)),
        "javascript" | "js" | "jsx" | "ts" | "tsx" | "typescript" => {
            Ok(Some(LanguageKind::JavaScript))
        }
        "c" => Ok(Some(LanguageKind::C)),
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Ok(Some(LanguageKind::Cpp)),
        "go" | "golang" => Ok(Some(LanguageKind::Go)),
        "java" => Ok(Some(LanguageKind::Java)),
        "csharp" | "c#" | "cs" => Ok(Some(LanguageKind::CSharp)),
        "php" => Ok(Some(LanguageKind::Php)),
        "ruby" | "rb" => Ok(Some(LanguageKind::Ruby)),
        "swift" => Ok(Some(LanguageKind::Swift)),
        "objc" | "objective-c" | "objectivec" | "m" | "mm" => Ok(Some(LanguageKind::ObjectiveC)),
        other => Err(anyhow::anyhow!("Unsupported language '{}'", other)),
    }
}

pub fn detect_language(path: &Path) -> Option<(LanguageKind, &'static str, Language)> {
    let ext = path
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Some((
            LanguageKind::Rust,
            "Rust",
            tree_sitter_rust::LANGUAGE.into(),
        )),
        "c" => Some((LanguageKind::C, "C", tree_sitter_c::LANGUAGE.into())),
        "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx" | "inc" | "inl" => {
            Some((LanguageKind::Cpp, "C++", tree_sitter_cpp::LANGUAGE.into()))
        }
        "go" => Some((LanguageKind::Go, "Go", tree_sitter_go::LANGUAGE.into())),
        "java" => Some((
            LanguageKind::Java,
            "Java",
            tree_sitter_java::LANGUAGE.into(),
        )),
        "cs" => Some((
            LanguageKind::CSharp,
            "C#",
            tree_sitter_c_sharp::LANGUAGE.into(),
        )),
        "php" => Some((
            LanguageKind::Php,
            "PHP",
            tree_sitter_php::LANGUAGE_PHP.into(),
        )),
        "rb" => Some((
            LanguageKind::Ruby,
            "Ruby",
            tree_sitter_ruby::LANGUAGE.into(),
        )),
        "js" | "jsx" | "mjs" | "cjs" => Some((
            LanguageKind::JavaScript,
            "JavaScript",
            tree_sitter_javascript::LANGUAGE.into(),
        )),
        "ts" => Some((
            LanguageKind::JavaScript,
            "TypeScript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        )),
        "tsx" => Some((
            LanguageKind::JavaScript,
            "TypeScript",
            tree_sitter_typescript::LANGUAGE_TSX.into(),
        )),
        "py" => Some((
            LanguageKind::Python,
            "Python",
            tree_sitter_python::LANGUAGE.into(),
        )),
        "swift" => Some((
            LanguageKind::Swift,
            "Swift",
            tree_sitter_swift::LANGUAGE.into(),
        )),
        "m" | "mm" => Some((
            LanguageKind::ObjectiveC,
            "Objective-C",
            tree_sitter_objc::LANGUAGE.into(),
        )),
        _ => None,
    }
}

pub fn parse_supported_file(
    path: &Path,
    max_bytes: u64,
    language_filter: Option<LanguageKind>,
) -> Result<Option<ParsedAstFile>> {
    if !path.exists() || !path.is_file() {
        return Ok(None);
    }

    let meta = std::fs::metadata(path)?;
    if meta.len() > max_bytes {
        return Ok(None);
    }

    let (language_kind, language_name, language) = match detect_language(path) {
        Some(language) => language,
        None => return Ok(None),
    };

    if let Some(filter) = language_filter
        && filter != language_kind
    {
        return Ok(None);
    }

    let mut parser = Parser::new();
    parser.set_language(&language)?;

    let mut file = File::open(path)?;
    let mut source = Vec::new();
    file.read_to_end(&mut source)?;

    let tree = parser
        .parse(source.as_slice(), None)
        .context("Tree-sitter parse failed")?;

    Ok(Some(ParsedAstFile {
        language_kind,
        language_name,
        source,
        tree,
    }))
}

pub(crate) fn source_has_parse_error(path: &Path, source: &[u8]) -> Result<Option<bool>> {
    let Some((_, _, language)) = detect_language(path) else {
        return Ok(None);
    };
    let mut parser = Parser::new();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .context("Tree-sitter parse failed")?;
    Ok(Some(tree.root_node().has_error()))
}

pub fn visit_candidate_code_files<F>(
    search_paths: &[PathBuf],
    file_hint: Option<&Path>,
    language_filter: Option<LanguageKind>,
    visitor: F,
) -> Result<()>
where
    F: FnMut(&Path) -> Result<bool>,
{
    visit_candidate_code_files_with_stats(search_paths, file_hint, language_filter, visitor)
        .map(|_| ())
}

pub fn visit_candidate_code_files_with_options<F>(
    search_paths: &[PathBuf],
    file_hint: Option<&Path>,
    language_filter: Option<LanguageKind>,
    include_ignored: bool,
    include_hidden: bool,
    visitor: F,
) -> Result<()>
where
    F: FnMut(&Path) -> Result<bool>,
{
    visit_candidate_code_files_with_stats_and_options(
        search_paths,
        file_hint,
        language_filter,
        include_ignored,
        include_hidden,
        visitor,
    )
    .map(|_| ())
}

pub fn visit_candidate_code_files_with_stats<F>(
    search_paths: &[PathBuf],
    file_hint: Option<&Path>,
    language_filter: Option<LanguageKind>,
    visitor: F,
) -> Result<CandidateVisitStats>
where
    F: FnMut(&Path) -> Result<bool>,
{
    visit_candidate_code_files_with_stats_and_options(
        search_paths,
        file_hint,
        language_filter,
        false,
        false,
        visitor,
    )
}

pub fn visit_candidate_code_files_with_stats_and_options<F>(
    search_paths: &[PathBuf],
    file_hint: Option<&Path>,
    language_filter: Option<LanguageKind>,
    include_ignored: bool,
    include_hidden: bool,
    mut visitor: F,
) -> Result<CandidateVisitStats>
where
    F: FnMut(&Path) -> Result<bool>,
{
    let mut seen = HashSet::new();
    let mut stats = CandidateVisitStats::default();

    if let Some(file_hint) = file_hint {
        stats.direct_files_considered += 1;
        if !visit_candidate(file_hint, language_filter, &mut seen, &mut visitor)? {
            return Ok(stats);
        }
    }

    for search_path in search_paths {
        if !search_path.exists() {
            continue;
        }

        let canonical_search_path = search_path
            .canonicalize()
            .unwrap_or_else(|_| search_path.to_path_buf());

        if canonical_search_path.is_file() {
            stats.direct_files_considered += 1;
            if !visit_candidate(
                &canonical_search_path,
                language_filter,
                &mut seen,
                &mut visitor,
            )? {
                return Ok(stats);
            }
            continue;
        }

        if !canonical_search_path.is_dir() {
            continue;
        }

        if !include_ignored && !include_hidden && is_path_index_available(&canonical_search_path) {
            stats.indexed_roots_used += 1;
            let mut should_continue = true;
            let mut visit_error: Option<anyhow::Error> = None;
            let _ = visit_indexed_entries_under(&canonical_search_path, |entry| {
                if !should_continue || entry.is_dir {
                    return should_continue;
                }
                match visit_candidate(&entry.path, language_filter, &mut seen, &mut visitor) {
                    Ok(true) => true,
                    Ok(false) => {
                        should_continue = false;
                        false
                    }
                    Err(err) => {
                        visit_error = Some(err);
                        should_continue = false;
                        false
                    }
                }
            });
            if let Some(err) = visit_error {
                return Err(err);
            }
            if !should_continue {
                return Ok(stats);
            }
            if crate::indexer::indexed_workspace_file_count(&canonical_search_path)
                .is_some_and(|count| count > crate::indexer::LARGE_WORKSPACE_FILE_THRESHOLD)
            {
                continue;
            }
        }

        stats.filesystem_roots_walked += 1;
        let candidates = collect_code_candidates_parallel(
            &canonical_search_path,
            language_filter,
            include_ignored,
            include_hidden,
        )?;
        for path in candidates {
            if !visit_candidate(&path, language_filter, &mut seen, &mut visitor)? {
                return Ok(stats);
            }
        }
    }

    Ok(stats)
}

fn collect_code_candidates_parallel(
    root: &Path,
    language_filter: Option<LanguageKind>,
    include_ignored: bool,
    include_hidden: bool,
) -> Result<Vec<PathBuf>> {
    let candidates = Arc::new(Mutex::new(Vec::new()));
    let mut walk = WalkBuilder::new(root);
    super::path_filters::configure_walk_filters(&mut walk, include_ignored, include_hidden);
    walk.threads(crate::common::bounded_walk_threads());
    let filter_root = root.to_path_buf();
    walk.filter_entry(move |entry| {
        !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir())
            || !super::path_filters::is_vcs_metadata_dir(entry.path(), &filter_root)
    });
    walk.build_parallel().run(|| {
        let candidates = Arc::clone(&candidates);
        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return ignore::WalkState::Continue,
            };
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();
            if !is_code_file(path) {
                return ignore::WalkState::Continue;
            }
            if let Some(filter) = language_filter {
                match detect_language(path) {
                    Some((kind, _, _)) if kind == filter => {}
                    _ => return ignore::WalkState::Continue,
                }
            }
            if let Ok(mut guard) = candidates.lock() {
                guard.push(path.to_path_buf());
            }
            ignore::WalkState::Continue
        })
    });

    let mut candidates = Arc::try_unwrap(candidates)
        .map_err(|_| anyhow::anyhow!("AST candidate collector still shared"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("AST candidate collector unavailable"))?;
    candidates.sort();
    candidates.dedup();
    Ok(candidates)
}

fn visit_candidate<F>(
    path: &Path,
    language_filter: Option<LanguageKind>,
    seen: &mut HashSet<String>,
    visitor: &mut F,
) -> Result<bool>
where
    F: FnMut(&Path) -> Result<bool>,
{
    if !is_code_file(path) {
        return Ok(true);
    }

    if let Some(filter) = language_filter {
        match detect_language(path) {
            Some((kind, _, _)) if kind == filter => {}
            _ => return Ok(true),
        }
    }

    let normalized = normalize_path(path);
    if seen.insert(normalized) {
        return visitor(path);
    }

    Ok(true)
}

pub fn is_code_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| CODE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn normalize_path(path: &Path) -> String {
    crate::common::normalize_display_path(path)
}

pub fn node_text<'a>(node: Node<'a>, source: &'a [u8]) -> Option<&'a str> {
    std::str::from_utf8(&source[node.byte_range()]).ok()
}

pub fn child_field_text<'a>(node: &Node<'a>, field: &str, source: &'a [u8]) -> Option<&'a str> {
    let field_node = node.child_by_field_name(field)?;
    node_text(field_node, source)
}

pub fn declaration_name<'a>(node: &Node<'a>, source: &'a [u8]) -> Option<&'a str> {
    declaration_name_node(node).and_then(|name| node_text(name, source))
}

fn declaration_name_node<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("function"))
        .or_else(|| node.child_by_field_name("method"))
        .or_else(|| {
            node.child_by_field_name("declarator")
                .and_then(declarator_name_node)
        })
        .or_else(|| first_identifier_child_node(node))
}

fn first_identifier_child_node<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| is_identifier_like_node(child.kind()))
}

fn declarator_name_node(node: Node<'_>) -> Option<Node<'_>> {
    if is_identifier_like_node(node.kind()) {
        return Some(node);
    }

    for field in ["declarator", "name", "function", "method"] {
        if let Some(field_node) = node.child_by_field_name(field)
            && let Some(name) = declarator_name_node(field_node)
        {
            return Some(name);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(name) = declarator_name_node(child) {
            return Some(name);
        }
    }

    None
}

fn is_identifier_like_node(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "namespace_identifier"
            | "qualified_identifier"
            | "qualified_name"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "generic_name"
            | "constant"
            | "name"
            | "variable_name"
            | "property_identifier"
            | "destructor_name"
            | "operator_name"
            | "operator"
            | "simple_identifier"
    )
}

pub fn symbol_basename(name: &str) -> &str {
    name.rsplit([':', '.', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(name)
        .trim_start_matches('~')
}

pub fn symbol_segments(name: &str) -> Vec<String> {
    name.split([':', '.', '\\'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.trim_start_matches('~').to_string())
        .collect()
}

pub fn call_expression_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if let Some(function) = child_field_text(&node, "function", source) {
        return Some(normalize_call_target(function));
    }

    if let Some(method) = child_field_text(&node, "method", source) {
        if let Some(receiver) = child_field_text(&node, "receiver", source) {
            return Some(format!(
                "{}.{}",
                normalize_call_target(receiver),
                normalize_call_target(method)
            ));
        }
        return Some(normalize_call_target(method));
    }

    if let Some(name) = child_field_text(&node, "name", source) {
        if let Some(object) = child_field_text(&node, "object", source) {
            return Some(format!(
                "{}.{}",
                normalize_call_target(object),
                normalize_call_target(name)
            ));
        }
        return Some(normalize_call_target(name));
    }

    node_text(node, source).map(|text| {
        let first_line = text.lines().next().unwrap_or("");
        normalize_call_target(first_line)
    })
}

pub fn is_call_node(kind: &str) -> bool {
    matches!(
        kind,
        "call_expression"
            | "call"
            | "invocation"
            | "invocation_expression"
            | "method_invocation"
            | "function_call_expression"
            | "member_call_expression"
            | "nullsafe_member_call_expression"
            | "scoped_call_expression"
            | "object_creation_expression"
            | "explicit_constructor_invocation"
            | "message_expression"
    )
}

fn normalize_call_target(raw: &str) -> String {
    raw.trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('(')
        .to_string()
}

pub fn first_line_preview(node: Node<'_>, source: &[u8], max_bytes: usize) -> String {
    let end_byte = std::cmp::min(node.start_byte() + max_bytes, source.len());
    std::str::from_utf8(&source[node.start_byte()..end_byte])
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

pub fn is_symbol_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "impl_item"
            | "mod_item"
            | "const_item"
            | "static_item"
            | "type_item"
            | "type_alias"
            | "function_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "destructor_declaration"
            | "class_definition"
            | "function_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "enum_declaration"
            | "namespace_declaration"
            | "namespace_definition"
            | "class_specifier"
            | "struct_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "type_spec"
            | "delegate_declaration"
            | "trait_declaration"
            | "property_declaration"
            | "method_definition"
            | "method"
            | "singleton_method"
            | "class"
            | "module"
            | "arrow_function"
            | "init_declaration"
            | "protocol_declaration"
            | "protocol_function_declaration"
            | "enum_entry"
            | "class_interface"
            | "class_implementation"
            | "implementation_definition"
            | "struct_declaration"
            | "category_interface"
            | "category_implementation"
    )
}

pub fn is_function_like_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "destructor_declaration"
            | "method"
            | "singleton_method"
            | "init_declaration"
            | "protocol_function_declaration"
    )
}

pub fn find_symbol_candidates<'a>(
    root: Node<'a>,
    source: &[u8],
    symbol: &str,
    line: Option<usize>,
) -> Vec<SymbolCandidate<'a>> {
    let candidates = collect_symbol_candidates(root, source)
        .into_iter()
        .filter(|candidate| {
            symbol_query_matches(symbol, &candidate.qualified_name)
                && line.is_none_or(|line| candidate.node.start_position().row + 1 == line)
        })
        .collect::<Vec<_>>();
    if line.is_some() {
        candidates
    } else {
        prefer_definitions_over_declarations(candidates)
    }
}

pub fn find_function_candidates<'a>(
    root: Node<'a>,
    source: &[u8],
    symbol: &str,
    line: Option<usize>,
) -> Vec<SymbolCandidate<'a>> {
    find_symbol_candidates(root, source, symbol, line)
        .into_iter()
        .filter(|candidate| is_function_like_node(candidate.node.kind()))
        .collect()
}

pub fn classify_reference_match(root: Node<'_>, byte_offset: usize) -> &'static str {
    if byte_offset >= root.end_byte() {
        return "code";
    }
    let end_byte = (byte_offset + 1).min(root.end_byte());
    let Some(mut node) = root.descendant_for_byte_range(byte_offset, end_byte) else {
        return "code";
    };
    loop {
        let kind = node.kind();
        if kind.contains("comment") {
            return "comment";
        }
        if kind.contains("interpolation") {
            return "code";
        }
        if kind.contains("string")
            || kind.contains("character_literal")
            || matches!(kind, "char_literal" | "template_literal")
        {
            return "string";
        }
        let Some(parent) = node.parent() else {
            break;
        };
        node = parent;
    }
    "code"
}

pub fn is_symbol_definition_match(
    root: Node<'_>,
    source: &[u8],
    symbol: &str,
    byte_offset: usize,
) -> bool {
    if byte_offset >= root.end_byte() {
        return false;
    }
    let end_byte = (byte_offset + 1).min(root.end_byte());
    let Some(mut node) = root.descendant_for_byte_range(byte_offset, end_byte) else {
        return false;
    };
    loop {
        if is_symbol_node(node.kind())
            && let Some(name_node) = declaration_name_node(&node)
            && byte_offset >= name_node.start_byte()
            && byte_offset < name_node.end_byte()
            && node_text(name_node, source).is_some_and(|name| symbol_query_matches(symbol, name))
        {
            return true;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

pub fn collect_symbols(root: Node<'_>, source: &[u8]) -> Vec<Value> {
    collect_symbol_candidates(root, source)
        .into_iter()
        .map(|candidate| {
            let start_pos = candidate.node.start_position();
            let end_pos = candidate.node.end_position();
            json!({
                "name": candidate.name,
                "qualified_name": candidate.qualified_name,
                "kind": candidate.node.kind(),
                "start_line": start_pos.row + 1,
                "end_line": end_pos.row + 1,
                "signature": first_line_preview(candidate.node, source, 160),
                "parent": candidate.parent
            })
        })
        .collect()
}

pub fn collect_symbol_candidates<'a>(root: Node<'a>, source: &[u8]) -> Vec<SymbolCandidate<'a>> {
    let mut candidates = Vec::new();
    collect_symbol_candidates_recursive(root, source, &mut candidates, &[]);
    candidates
}

fn collect_symbol_candidates_recursive<'a>(
    node: Node<'a>,
    source: &[u8],
    candidates: &mut Vec<SymbolCandidate<'a>>,
    parent_segments: &[String],
) {
    let mut child_parent_segments = parent_segments.to_vec();

    if is_symbol_node(node.kind())
        && let Some(name) = declaration_name(&node, source)
    {
        let name_segments = symbol_segments(name);
        let qualified_segments = append_qualified_segments(parent_segments, &name_segments);
        let qualified_name = qualified_segments.join(".");
        candidates.push(SymbolCandidate {
            node,
            name: name.to_string(),
            qualified_name,
            parent: parent_segments.last().cloned(),
        });
        child_parent_segments = qualified_segments;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_symbol_candidates_recursive(child, source, candidates, &child_parent_segments);
    }
}

fn symbol_query_matches(symbol: &str, qualified_name: &str) -> bool {
    let query_segments = symbol_segments(symbol);
    let candidate_segments = symbol_segments(qualified_name);
    !query_segments.is_empty() && candidate_segments.ends_with(&query_segments)
}

fn append_qualified_segments(parent: &[String], name: &[String]) -> Vec<String> {
    let overlap = (0..=parent.len().min(name.len()))
        .rev()
        .find(|&size| parent[parent.len() - size..] == name[..size])
        .unwrap_or(0);
    let mut qualified = parent.to_vec();
    qualified.extend_from_slice(&name[overlap..]);
    qualified
}

fn prefer_definitions_over_declarations<'a>(
    candidates: Vec<SymbolCandidate<'a>>,
) -> Vec<SymbolCandidate<'a>> {
    let mut filtered = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        let same_name_has_body = candidates.iter().any(|other| {
            other.qualified_name == candidate.qualified_name
                && other.node.child_by_field_name("body").is_some()
        });
        if !same_name_has_body || candidate.node.child_by_field_name("body").is_some() {
            filtered.push(candidate.clone());
        }
    }
    filtered
}

pub fn normalized_string_literal(raw: &str) -> String {
    raw.trim()
        .trim_start_matches(['"', '\'', '`'])
        .trim_end_matches(['"', '\'', '`'])
        .to_string()
}

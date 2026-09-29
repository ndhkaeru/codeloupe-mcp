use anyhow::{Context, Result};
use serde_json::{Value, json};
use tree_sitter::Node;

use crate::tools::ast_support::{
    DEFAULT_AST_FILE_SIZE_LIMIT, LanguageKind, child_field_text, node_text,
    normalized_string_literal, parse_supported_file,
};

pub fn schema() -> Value {
    json!({
        "name": "list_imports",
        "title": "List imports",
        "description": "List imports from one Rust, JavaScript/TypeScript, Swift, or Objective-C file. Use to understand dependencies before deeper reads or edits.",
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

    let parsed = parse_supported_file(&path, DEFAULT_AST_FILE_SIZE_LIMIT, None)?
        .context("Unsupported extension for list_imports")?;

    if !matches!(
        parsed.language_kind,
        LanguageKind::Rust
            | LanguageKind::JavaScript
            | LanguageKind::Swift
            | LanguageKind::ObjectiveC
    ) {
        return Err(anyhow::anyhow!(
            "list_imports currently supports Rust, JavaScript/TypeScript, Swift, and Objective-C files"
        ));
    }

    let mut imports = Vec::new();
    collect_imports_recursive(
        parsed.tree.root_node(),
        parsed.language_kind,
        &parsed.source,
        &mut imports,
    );

    Ok(json!({
        "path": crate::common::normalize_display_path(&path),
        "language": parsed.language_name,
        "imports": imports,
        "total_imports": imports.len()
    }))
}

fn collect_imports_recursive(
    node: Node<'_>,
    language_kind: LanguageKind,
    source: &[u8],
    imports: &mut Vec<Value>,
) {
    match language_kind {
        LanguageKind::Rust if node.kind() == "use_declaration" => {
            if let Some(statement) = node_text(node, source) {
                let trimmed = statement.trim();
                let is_public = trimmed.starts_with("pub use ");
                let clause = trimmed
                    .trim_start_matches("pub ")
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .trim()
                    .to_string();

                // `clause` and `statement` would only repeat `source` here.
                imports.push(json!({
                    "line": node.start_position().row + 1,
                    "kind": if is_public { "pub_use" } else { "use" },
                    "source": clause
                }));
            }
        }
        LanguageKind::JavaScript if node.kind() == "import_statement" => {
            if let Some(statement) = node_text(node, source) {
                let trimmed = statement.trim();
                let source_value = child_field_text(&node, "source", source)
                    .map(normalized_string_literal)
                    .unwrap_or_default();
                let clause = parse_js_import_clause(trimmed);
                let kind = if trimmed.starts_with("import type ") {
                    "type_import"
                } else if clause.is_empty() {
                    "side_effect"
                } else {
                    "import"
                };

                imports.push(json!({
                    "line": node.start_position().row + 1,
                    "kind": kind,
                    "source": source_value,
                    "clause": clause,
                    "statement": trimmed
                }));
            }
        }
        LanguageKind::Swift if node.kind() == "import_declaration" => {
            if let Some(statement) = node_text(node, source) {
                let trimmed = statement.trim();
                let clause = trimmed.trim_start_matches("import ").trim().to_string();
                imports.push(json!({
                    "line": node.start_position().row + 1,
                    "kind": "import",
                    "source": clause
                }));
            }
        }
        LanguageKind::ObjectiveC if node.kind() == "preproc_include" => {
            if let Some(statement) = node_text(node, source) {
                let trimmed = statement.trim();
                let is_import = trimmed.starts_with("#import");
                let directive = if is_import { "#import" } else { "#include" };
                let path_value = trimmed
                    .trim_start_matches(directive)
                    .trim()
                    .trim_matches(|c| c == '"' || c == '<' || c == '>')
                    .to_string();
                // `statement` stays: it tells `<system>` from `"local"` includes.
                imports.push(json!({
                    "line": node.start_position().row + 1,
                    "kind": if is_import { "import" } else { "include" },
                    "source": path_value,
                    "statement": trimmed
                }));
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_imports_recursive(child, language_kind, source, imports);
    }
}

fn parse_js_import_clause(statement: &str) -> String {
    let trimmed = statement.trim_end_matches(';').trim();
    if !trimmed.starts_with("import ") {
        return String::new();
    }

    if let Some(index) = trimmed.rfind(" from ") {
        trimmed["import ".len()..index].trim().to_string()
    } else {
        String::new()
    }
}

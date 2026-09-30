use codeloupe_mcp::tools::{
    compare_symbols, create_directory, find_definition, find_references, get_call_graph,
    get_symbols, list_exports, list_imports, read_symbol_body,
};
use codeloupe_mcp::workspace_control;
use serde_json::json;
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn test_variable_functions_are_symbols_definitions_and_call_graph_roots() {
    let dir = tempdir().unwrap();
    for extension in ["js", "ts", "tsx"] {
        let path = dir.path().join(format!("functions.{extension}"));
        fs::write(
            &path,
            "const arrow = () => helper();\nconst expression = function() { return helper(); };\nexport const asyncArrow = async () => helper();\nconst literal = 123;\n",
        )
        .unwrap();
        let symbols = get_symbols::execute(&json!({"path": path})).await.unwrap();
        for (name, prefix) in [
            ("arrow", "const arrow ="),
            ("expression", "const expression ="),
            ("asyncArrow", "export const asyncArrow ="),
        ] {
            let entry = symbols["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == name)
                .unwrap();
            assert!(
                entry["signature"].as_str().unwrap().starts_with(prefix),
                "{extension}: {entry}"
            );
            let body = read_symbol_body::execute(&json!({
                "symbol": name, "paths": [path], "file_hint": path,
            }))
            .await
            .unwrap();
            assert!(
                body["content"].as_str().unwrap().starts_with(prefix),
                "{extension}: {body}"
            );
            assert_eq!(body["start_line"], entry["start_line"]);
        }
        for name in ["arrow", "expression", "asyncArrow"] {
            assert!(
                symbols["symbols"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["name"] == name),
                "{extension}: missing {name}: {symbols}"
            );
            let definition = find_definition::execute(&json!({
                "symbol": name, "paths": [path]
            }))
            .await
            .unwrap();
            assert_eq!(
                definition["total_returned"], 1,
                "{extension}: {name}: {definition}"
            );
            let graph = get_call_graph::execute(&json!({
                "symbol": name, "file_path": path
            }))
            .await
            .unwrap();
            assert!(
                graph["outbound_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|call| call == "helper"),
                "{extension}: {name}: {graph}"
            );
        }
        assert!(
            !symbols["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == "literal")
        );
    }
}

#[tokio::test]
async fn test_create_directory_supports_nested_creation_and_existing_behavior() {
    let dir = tempdir().unwrap();
    workspace_control::register_configured_workspace(
        dir.path().to_path_buf(),
        "write_tool_test",
        true,
    );
    let target = dir.path().join("nested").join("leaf");

    let create_res = create_directory::execute(&json!({
        "path": target.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        create_res.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        create_res.get("created").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(target.exists());

    let existing_res = create_directory::execute(&json!({
        "path": target.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        existing_res.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        existing_res.get("created").and_then(|v| v.as_bool()),
        Some(false)
    );

    let fail_existing_res = create_directory::execute(&json!({
        "path": target.to_str().unwrap(),
        "allow_existing": false
    }))
    .await
    .unwrap();
    assert_eq!(
        fail_existing_res.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        fail_existing_res.get("error_code").and_then(|v| v.as_str()),
        Some("already_exists")
    );
}

#[tokio::test]
async fn test_list_imports_and_exports_cover_typescript_and_rust() {
    let dir = tempdir().unwrap();
    let tsx_path = dir.path().join("widget.tsx");
    fs::write(
        &tsx_path,
        "import React from \"react\";\nimport type { Foo } from \"./types\";\nexport { Foo } from \"./types\";\nexport const answer = 42;\nexport default function App() { return <div />; }\n",
    )
    .unwrap();

    let rust_path = dir.path().join("mod.rs");
    fs::write(
        &rust_path,
        "use crate::inner::Thing;\npub use crate::inner::PublicThing;\npub struct Model;\npub fn run() {}\n",
    )
    .unwrap();

    let ts_imports = list_imports::execute(&json!({ "path": tsx_path.to_str().unwrap() }))
        .await
        .unwrap();
    assert_eq!(
        ts_imports.get("path").and_then(|value| value.as_str()),
        Some(canonical_display_path(&tsx_path).as_str())
    );
    assert!(ts_imports.get("file").is_none());
    assert_eq!(
        ts_imports.get("total_imports").and_then(|v| v.as_u64()),
        Some(2)
    );

    let first_ts_import = ts_imports
        .get("imports")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .cloned()
        .unwrap();
    assert_eq!(
        first_ts_import.get("source").and_then(|v| v.as_str()),
        Some("react")
    );

    let ts_exports = list_exports::execute(&json!({ "path": tsx_path.to_str().unwrap() }))
        .await
        .unwrap();
    assert_eq!(
        ts_exports.get("path").and_then(|value| value.as_str()),
        Some(canonical_display_path(&tsx_path).as_str())
    );
    assert!(ts_exports.get("file").is_none());
    assert_eq!(
        ts_exports.get("total_exports").and_then(|v| v.as_u64()),
        Some(3)
    );
    assert!(
        ts_exports
            .get("exports")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .any(|item| item.get("kind").and_then(|v| v.as_str()) == Some("reexport"))
    );

    let rust_imports = list_imports::execute(&json!({ "path": rust_path.to_str().unwrap() }))
        .await
        .unwrap();
    assert_eq!(
        rust_imports.get("total_imports").and_then(|v| v.as_u64()),
        Some(2)
    );

    let rust_exports = list_exports::execute(&json!({ "path": rust_path.to_str().unwrap() }))
        .await
        .unwrap();
    assert_eq!(
        rust_exports
            .pointer("/export_defaults/kind")
            .and_then(serde_json::Value::as_str),
        Some("pub_item")
    );
    assert!(
        rust_exports
            .get("exports")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .any(|item| {
                item.get("name").and_then(|v| v.as_str()) == Some("run")
                    && item.get("kind").is_none()
            })
    );
}

#[tokio::test]
async fn test_list_exports_treats_swift_public_set_properties_as_public_api() {
    let dir = tempdir().unwrap();
    let swift_path = dir.path().join("Counter.swift");
    fs::write(
        &swift_path,
        "public struct Counter {\n    public(set) var count: Int\n}\n",
    )
    .unwrap();

    let swift_exports = list_exports::execute(&json!({ "path": swift_path.to_str().unwrap() }))
        .await
        .unwrap();

    assert!(
        swift_exports
            .get("exports")
            .and_then(|value| value.as_array())
            .unwrap()
            .iter()
            .any(|item| item.get("name").and_then(|value| value.as_str()) == Some("count"))
    );
}

#[tokio::test]
async fn test_compare_symbols_returns_unified_diff() {
    let dir = tempdir().unwrap();
    let left_path = dir.path().join("left.rs");
    let right_path = dir.path().join("right.rs");
    fs::write(
        &left_path,
        "fn provider() {\n    step_one();\n    step_two();\n}\n",
    )
    .unwrap();
    fs::write(
        &right_path,
        "fn provider() {\n    step_one();\n    step_three();\n}\n",
    )
    .unwrap();

    let compare_res = compare_symbols::execute(&json!({
        "left": {
            "symbol": "provider",
            "paths": [left_path.to_str().unwrap()]
        },
        "right": {
            "symbol": "provider",
            "paths": [right_path.to_str().unwrap()]
        }
    }))
    .await
    .unwrap();

    assert_eq!(
        compare_res.get("same_content").and_then(|v| v.as_bool()),
        Some(false)
    );
    let compare_diff = compare_res
        .get("unified_diff")
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(compare_diff.contains("step_two"));
    assert!(compare_diff.contains("step_three"));
    assert!(compare_res.pointer("/left/content").is_none());
    assert!(compare_res.pointer("/right/content").is_none());
    assert_eq!(
        compare_res
            .pointer("/left/path")
            .and_then(|value| value.as_str()),
        Some(canonical_display_path(&left_path).as_str())
    );
    assert_eq!(
        compare_res
            .pointer("/right/path")
            .and_then(|value| value.as_str()),
        Some(canonical_display_path(&right_path).as_str())
    );
    assert!(compare_res.pointer("/left/file").is_none());
    assert!(compare_res.pointer("/right/file").is_none());
}

#[tokio::test]
async fn test_relative_file_hint_resolves_against_search_paths() {
    let dir = tempdir().unwrap();
    let source_dir = dir.path().join("src");
    fs::create_dir(&source_dir).unwrap();
    let source_path = source_dir.join("sample.rs");
    fs::write(
        &source_path,
        "fn alpha() {\n    first();\n}\n\nfn beta() {\n    second();\n}\n",
    )
    .unwrap();

    let body = read_symbol_body::execute(&json!({
        "symbol": "alpha",
        "paths": [source_dir.to_str().unwrap()],
        "file_hint": "sample.rs"
    }))
    .await
    .unwrap();
    assert_eq!(
        body.get("path").and_then(|value| value.as_str()),
        Some(canonical_display_path(&source_path).as_str())
    );

    let comparison = compare_symbols::execute(&json!({
        "left": {
            "symbol": "alpha",
            "paths": [source_dir.to_str().unwrap()],
            "file_hint": "sample.rs"
        },
        "right": {
            "symbol": "beta",
            "paths": [source_dir.to_str().unwrap()],
            "file_hint": "sample.rs"
        }
    }))
    .await
    .unwrap();
    assert_eq!(
        comparison
            .get("same_content")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    assert!(
        comparison
            .get("unified_diff")
            .and_then(|value| value.as_str())
            .is_some_and(|diff| diff.contains("first") && diff.contains("second"))
    );
}

#[tokio::test]
async fn test_symbol_resolver_supports_qualified_names_and_reports_ambiguity() {
    let dir = tempdir().unwrap();
    let python_path = dir.path().join("workers.py");
    fs::write(
        &python_path,
        "class Alpha:\n    def process(self):\n        alpha_call()\n\nclass Beta:\n    def process(self):\n        beta_call()\n\nBeta.process()\n",
    )
    .unwrap();

    let symbols = get_symbols::execute(&json!({ "path": python_path.to_str().unwrap() }))
        .await
        .unwrap();
    assert_eq!(
        symbols.get("path").and_then(|value| value.as_str()),
        Some(canonical_display_path(&python_path).as_str())
    );
    assert!(symbols.get("file").is_none());
    assert_symbol_qualified_name(&symbols, "Alpha.process", 2);
    assert_symbol_qualified_name(&symbols, "Beta.process", 6);

    let alpha_body = read_symbol_body::execute(&json!({
        "symbol": "Alpha.process",
        "file_hint": python_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        alpha_body
            .get("qualified_name")
            .and_then(|value| value.as_str()),
        Some("Alpha.process")
    );
    assert_eq!(
        alpha_body.get("path").and_then(|value| value.as_str()),
        Some(canonical_display_path(&python_path).as_str())
    );
    assert!(alpha_body.get("file").is_none());
    assert!(
        alpha_body
            .get("content")
            .and_then(|value| value.as_str())
            .is_some_and(|content| content.contains("alpha_call"))
    );

    let beta_body = read_symbol_body::execute(&json!({
        "symbol": "Beta::process",
        "file_hint": python_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        beta_body
            .get("qualified_name")
            .and_then(|value| value.as_str()),
        Some("Beta.process")
    );
    assert!(
        beta_body
            .get("content")
            .and_then(|value| value.as_str())
            .is_some_and(|content| content.contains("beta_call"))
    );

    let ambiguous_body = read_symbol_body::execute(&json!({
        "symbol": "process",
        "file_hint": python_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_ambiguous_candidates(&ambiguous_body, &["Alpha.process", "Beta.process"]);
    assert!(ambiguous_body.get("content").is_none());
    assert!(
        ambiguous_body
            .get("candidates")
            .and_then(|value| value.as_array())
            .unwrap()
            .iter()
            .all(|candidate| {
                candidate.get("path").and_then(|value| value.as_str())
                    == Some(canonical_display_path(&python_path).as_str())
                    && candidate.get("file").is_none()
            })
    );

    let line_selected = read_symbol_body::execute(&json!({
        "symbol": "process",
        "file_hint": python_path.to_str().unwrap(),
        "line": 6
    }))
    .await
    .unwrap();
    assert_eq!(
        line_selected
            .get("qualified_name")
            .and_then(|value| value.as_str()),
        Some("Beta.process")
    );

    let beta_graph = get_call_graph::execute(&json!({
        "file_path": python_path.to_str().unwrap(),
        "symbol": "Beta.process"
    }))
    .await
    .unwrap();
    assert_eq!(
        beta_graph.get("path").and_then(|value| value.as_str()),
        Some(canonical_display_path(&python_path).as_str())
    );
    assert!(beta_graph.get("file").is_none());
    assert_string_array_contains(&beta_graph, "outbound_calls", "beta_call");

    let ambiguous_graph = get_call_graph::execute(&json!({
        "file_path": python_path.to_str().unwrap(),
        "symbol": "process"
    }))
    .await
    .unwrap();
    assert_ambiguous_candidates(&ambiguous_graph, &["Alpha.process", "Beta.process"]);
    assert!(ambiguous_graph.get("outbound_calls").is_none());
    assert_eq!(
        ambiguous_graph.get("path").and_then(|value| value.as_str()),
        Some(canonical_display_path(&python_path).as_str())
    );
    assert!(
        ambiguous_graph
            .get("candidates")
            .and_then(|value| value.as_array())
            .unwrap()
            .iter()
            .all(|candidate| candidate.get("path").is_none() && candidate.get("file").is_none())
    );

    let definitions = find_definition::execute(&json!({
        "symbol": "Beta::process",
        "paths": [python_path.to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        definitions
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        definitions
            .get("complete")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        definitions
            .pointer("/definitions/0/qualified_name")
            .and_then(|value| value.as_str()),
        Some("Beta.process")
    );
    assert_eq!(
        definitions.get("root").and_then(|value| value.as_str()),
        Some(canonical_display_path(dir.path()).as_str())
    );
    assert_eq!(
        definitions
            .pointer("/definitions/0/path")
            .and_then(|value| value.as_str()),
        Some("workers.py")
    );
    assert!(definitions.pointer("/definitions/0/file").is_none());

    let references = find_references::execute(&json!({
        "symbol": "Beta::process",
        "paths": [python_path.to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        references
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        references.get("complete").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        references.get("root").and_then(|value| value.as_str()),
        Some(canonical_display_path(dir.path()).as_str())
    );
    assert_eq!(
        references
            .pointer("/references/0/path")
            .and_then(|value| value.as_str()),
        Some("workers.py")
    );
    assert!(references.pointer("/references/0/file").is_none());

    let rust_path = dir.path().join("workers.rs");
    fs::write(
        &rust_path,
        "struct Alpha;\nimpl Alpha {\n    fn process() { alpha_call(); }\n}\nstruct Beta;\nimpl Beta {\n    fn process() { beta_call(); }\n}\n",
    )
    .unwrap();
    assert_qualified_and_ambiguous(&rust_path, "Alpha.process", "Beta::process").await;

    let csharp_path = dir.path().join("Workers.cs");
    fs::write(
        &csharp_path,
        "namespace Demo {\nclass Alpha {\n    void Process() { AlphaCall(); }\n}\nclass Beta {\n    void Process() { BetaCall(); }\n}\n}\n",
    )
    .unwrap();
    assert_qualified_and_ambiguous(&csharp_path, "Alpha.Process", "Beta::Process").await;
}

#[tokio::test]
async fn test_find_definition_uses_case_sensitive_ast_matches_for_supported_languages() {
    let dir = tempdir().unwrap();
    let rust_path = dir.path().join("semantic.rs");
    fs::write(
        &rust_path,
        "// fn Widget() {}\nconst TEXT: &str = \"fn Widget() {}\";\nfn Widget() {}\nfn widget() {}\n",
    )
    .unwrap();

    let rust_definitions = find_definition::execute(&json!({
        "symbol": "Widget",
        "paths": [rust_path.to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        rust_definitions
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        rust_definitions
            .pointer("/definitions/0/line")
            .and_then(|value| value.as_u64()),
        Some(3)
    );
    assert_eq!(
        rust_definitions
            .pointer("/definitions/0/resolution")
            .and_then(|value| value.as_str()),
        Some("ast")
    );

    let csharp_path = dir.path().join("Service.cs");
    fs::write(
        &csharp_path,
        "using System.Threading.Tasks;\nclass Service {\n    public async Task<int> Foo() { return await Task.FromResult(1); }\n}\n",
    )
    .unwrap();
    let csharp_definitions = find_definition::execute(&json!({
        "symbol": "Foo",
        "paths": [csharp_path.to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        csharp_definitions
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        csharp_definitions
            .pointer("/definitions/0/resolution")
            .and_then(|value| value.as_str()),
        Some("ast")
    );
}

#[tokio::test]
async fn test_find_references_classifies_code_comments_and_strings_with_ast() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("references.rs");
    fs::write(
        &path,
        "fn target() {}\nfn use_it() {\n    target();\n    let text = \"target\";\n    // target\n}\n",
    )
    .unwrap();

    let result = find_references::execute(&json!({
        "symbol": "target",
        "paths": [path.to_str().unwrap()]
    }))
    .await
    .unwrap();
    let references = result
        .get("references")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(references.len(), 4);
    let kinds = references
        .iter()
        .map(|reference| {
            reference
                .get("match_kind")
                .and_then(|value| value.as_str())
                .unwrap_or("code")
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["code", "code", "string", "comment"]);
    assert_eq!(
        references[0]
            .get("is_definition")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(
        references[1..]
            .iter()
            .all(|reference| reference.get("is_definition").is_none())
    );
    assert!(
        references
            .iter()
            .all(|reference| reference.get("classification").is_none())
    );
    assert_eq!(
        result
            .pointer("/reference_defaults/classification")
            .and_then(serde_json::Value::as_str),
        Some("ast")
    );
}

#[tokio::test]
async fn test_compare_symbols_does_not_compare_ambiguous_first_matches() {
    let dir = tempdir().unwrap();
    let left_path = dir.path().join("left.py");
    let right_path = dir.path().join("right.py");
    fs::write(
        &left_path,
        "class Alpha:\n    def process(self):\n        same()\n\nclass Beta:\n    def process(self):\n        old_call()\n",
    )
    .unwrap();
    fs::write(
        &right_path,
        "class Alpha:\n    def process(self):\n        same()\n\nclass Beta:\n    def process(self):\n        new_call()\n",
    )
    .unwrap();

    let ambiguous = compare_symbols::execute(&json!({
        "left": { "symbol": "process", "paths": [left_path.to_str().unwrap()] },
        "right": { "symbol": "process", "paths": [right_path.to_str().unwrap()] }
    }))
    .await
    .unwrap();
    assert_eq!(
        ambiguous.get("ambiguous").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(ambiguous.get("same_content").is_none());
    assert!(ambiguous.get("unified_diff").is_none());

    let qualified = compare_symbols::execute(&json!({
        "left": { "symbol": "Beta.process", "paths": [left_path.to_str().unwrap()] },
        "right": { "symbol": "Beta::process", "paths": [right_path.to_str().unwrap()] }
    }))
    .await
    .unwrap();
    assert_eq!(
        qualified
            .get("same_content")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    assert!(
        qualified
            .get("unified_diff")
            .and_then(|value| value.as_str())
            .is_some_and(|diff| diff.contains("old_call") && diff.contains("new_call"))
    );
}

#[tokio::test]
async fn test_ast_tools_support_cpp_symbols_body_and_call_graph() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("widget.cc");
    fs::write(
        &path,
        r#"
namespace demo {
class Widget {
 public:
  void Render();
};

void Widget::Render() {
  HelperCall();
  base::DoThing();
}

int FreeFunction() {
  return ComputeValue();
}
}
"#,
    )
    .unwrap();

    let symbols = get_symbols::execute(&json!({ "path": path.to_str().unwrap() }))
        .await
        .unwrap();
    assert_eq!(
        symbols.get("language").and_then(|v| v.as_str()),
        Some("C++")
    );
    assert_symbol_name(&symbols, "Widget");
    assert_symbol_name(&symbols, "Widget::Render");
    assert_symbol_name(&symbols, "FreeFunction");

    let body = read_symbol_body::execute(&json!({
        "symbol": "Render",
        "file_hint": path.to_str().unwrap(),
        "include_signature": true
    }))
    .await
    .unwrap();
    assert_eq!(
        body.get("match_source").and_then(|v| v.as_str()),
        Some("ast")
    );
    assert!(
        body.get("content")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("HelperCall")
    );

    let call_graph = get_call_graph::execute(&json!({
        "file_path": path.to_str().unwrap(),
        "symbol": "Render"
    }))
    .await
    .unwrap();
    assert_string_array_contains(&call_graph, "outbound_calls", "HelperCall");
    assert_string_array_contains(&call_graph, "outbound_calls", "base::DoThing");
}

#[tokio::test]
async fn test_ast_tools_support_popular_language_symbols_and_calls() {
    let dir = tempdir().unwrap();

    let go_path = dir.path().join("service.go");
    fs::write(
        &go_path,
        r#"
package main

type Server struct{}

func Start() {
	helperCall()
	fmt.Println("ready")
}

func helperCall() {}
"#,
    )
    .unwrap();
    assert_language_symbols(&go_path, "Go", &["Server", "Start"]).await;
    let go_calls = get_call_graph::execute(&json!({
        "file_path": go_path.to_str().unwrap(),
        "symbol": "Start"
    }))
    .await
    .unwrap();
    assert_string_array_contains(&go_calls, "outbound_calls", "helperCall");

    let java_path = dir.path().join("App.java");
    fs::write(
        &java_path,
        r#"
class App {
  void run() {
    helperCall();
    System.out.println("ready");
  }

  void helperCall() {}
}
"#,
    )
    .unwrap();
    assert_language_symbols(&java_path, "Java", &["App", "run"]).await;
    let java_calls = get_call_graph::execute(&json!({
        "file_path": java_path.to_str().unwrap(),
        "symbol": "run"
    }))
    .await
    .unwrap();
    assert_string_array_contains(&java_calls, "outbound_calls", "helperCall");
    assert_string_array_contains(&java_calls, "outbound_calls", "System.out.println");

    let cs_path = dir.path().join("Worker.cs");
    fs::write(
        &cs_path,
        r#"
namespace Demo {
  class Worker {
    void Run() {
      Helper();
      Console.WriteLine("ready");
    }

    void Helper() {}
  }
}
"#,
    )
    .unwrap();
    assert_language_symbols(&cs_path, "C#", &["Demo", "Worker", "Run"]).await;
    let cs_calls = get_call_graph::execute(&json!({
        "file_path": cs_path.to_str().unwrap(),
        "symbol": "Run"
    }))
    .await
    .unwrap();
    assert_string_array_contains(&cs_calls, "outbound_calls", "Helper");
    assert_string_array_contains(&cs_calls, "outbound_calls", "Console.WriteLine");

    let php_path = dir.path().join("Service.php");
    fs::write(
        &php_path,
        r#"
<?php
class Service {
  function run() {
    helper_call();
    $this->emit();
  }

  function emit() {}
}

function helper_call() {}
"#,
    )
    .unwrap();
    assert_language_symbols(&php_path, "PHP", &["Service", "run", "helper_call"]).await;
    let php_calls = get_call_graph::execute(&json!({
        "file_path": php_path.to_str().unwrap(),
        "symbol": "run"
    }))
    .await
    .unwrap();
    assert_string_array_contains(&php_calls, "outbound_calls", "helper_call");

    let ruby_path = dir.path().join("worker.rb");
    fs::write(
        &ruby_path,
        r#"
module Demo
  class Worker
    def run
      helper_call()
      logger.info("ready")
    end

    def helper_call
    end
  end
end
"#,
    )
    .unwrap();
    assert_language_symbols(
        &ruby_path,
        "Ruby",
        &["Demo", "Worker", "run", "helper_call"],
    )
    .await;
    let ruby_calls = get_call_graph::execute(&json!({
        "file_path": ruby_path.to_str().unwrap(),
        "symbol": "run"
    }))
    .await
    .unwrap();
    assert_string_array_contains(&ruby_calls, "outbound_calls", "helper_call");
}

async fn assert_language_symbols(path: &std::path::Path, language: &str, expected_names: &[&str]) {
    let symbols = get_symbols::execute(&json!({ "path": path.to_str().unwrap() }))
        .await
        .unwrap();
    assert_eq!(
        symbols.get("language").and_then(|v| v.as_str()),
        Some(language)
    );
    for expected_name in expected_names {
        assert_symbol_name(&symbols, expected_name);
    }
}

fn assert_symbol_name(symbols: &serde_json::Value, expected_name: &str) {
    let names = symbols
        .get("symbols")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .filter_map(|symbol| symbol.get("name").and_then(|v| v.as_str()))
        .collect::<Vec<_>>();
    assert!(
        names.contains(&expected_name),
        "expected symbol {expected_name}, got {names:?}"
    );
}

fn assert_symbol_qualified_name(
    symbols: &serde_json::Value,
    expected_qualified_name: &str,
    expected_start_line: u64,
) {
    let symbol = symbols
        .get("symbols")
        .and_then(|value| value.as_array())
        .and_then(|items| {
            items.iter().find(|item| {
                item.get("qualified_name").and_then(|value| value.as_str())
                    == Some(expected_qualified_name)
            })
        })
        .unwrap_or_else(|| panic!("missing qualified symbol {expected_qualified_name}: {symbols}"));
    assert_eq!(
        symbol.get("start_line").and_then(|value| value.as_u64()),
        Some(expected_start_line)
    );
}

fn assert_ambiguous_candidates(value: &serde_json::Value, expected: &[&str]) {
    assert_eq!(
        value.get("ambiguous").and_then(|item| item.as_bool()),
        Some(true)
    );
    let qualified_names = value
        .get("candidates")
        .and_then(|item| item.as_array())
        .unwrap()
        .iter()
        .filter_map(|candidate| {
            candidate
                .get("qualified_name")
                .and_then(|item| item.as_str())
        })
        .collect::<Vec<_>>();
    for expected_name in expected {
        assert!(
            qualified_names.contains(expected_name),
            "expected candidate {expected_name}, got {qualified_names:?}"
        );
    }
}

async fn assert_qualified_and_ambiguous(
    path: &std::path::Path,
    first_qualified: &str,
    second_qualified: &str,
) {
    let first = read_symbol_body::execute(&json!({
        "symbol": first_qualified,
        "file_hint": path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert!(
        first
            .get("content")
            .and_then(|value| value.as_str())
            .is_some()
    );

    let second = read_symbol_body::execute(&json!({
        "symbol": second_qualified,
        "file_hint": path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert!(
        second
            .get("content")
            .and_then(|value| value.as_str())
            .is_some()
    );

    let unqualified = second_qualified
        .rsplit(['.', ':'])
        .find(|part| !part.is_empty())
        .unwrap();
    let ambiguous = read_symbol_body::execute(&json!({
        "symbol": unqualified,
        "file_hint": path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        ambiguous.get("ambiguous").and_then(|value| value.as_bool()),
        Some(true)
    );
}

fn assert_string_array_contains(value: &serde_json::Value, field: &str, expected: &str) {
    let values = value
        .get(field)
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .filter_map(|item| item.as_str())
        .collect::<Vec<_>>();
    assert!(
        values.contains(&expected),
        "expected {field} to contain {expected}, got {values:?}"
    );
}

fn canonical_display_path(path: &std::path::Path) -> String {
    codeloupe_mcp::common::normalize_display_path(&codeloupe_mcp::common::canonicalize_if_exists(
        path.to_path_buf(),
    ))
}

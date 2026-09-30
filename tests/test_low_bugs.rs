use codeloupe_mcp::tools::{
    self, batch_tool_call, count_file_lines, file_summary, fuzzy_find, get_symbols, list_exports,
    list_imports, read_file, read_symbol_body, resolve_path, text_search,
};
use serde_json::json;
use std::fs;
use tempfile::tempdir;

fn tool_payload(response: &serde_json::Value) -> serde_json::Value {
    serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn wrong_path_types_and_oversized_batch_have_argument_errors() {
    let directory = tempdir().unwrap();
    let file = directory.path().join("file.txt");
    fs::write(&file, "hello").unwrap();
    for (name, arguments) in [
        ("project_map", json!({"path": file})),
        (
            "compare_directories",
            json!({"left_path": file, "right_path": directory.path()}),
        ),
        (
            "batch_tool_call",
            json!({"calls": vec![json!({"tool": "resolve_path", "args": {"path": file}}); 21]}),
        ),
    ] {
        let response = tools::call_tool(json!({"name": name, "arguments": arguments}))
            .await
            .unwrap();
        assert_eq!(response["isError"], true, "{name}: {response}");
        let payload = tool_payload(&response);
        assert_eq!(
            payload["error"]["code"], "invalid_argument",
            "{name}: {payload}"
        );
    }
}

#[tokio::test]
async fn dispatch_aliases_change_behavior_instead_of_becoming_unknown_arguments() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("sample.js");
    fs::write(&source, "function main() { helper(); }\n// alpha123\n").unwrap();

    let searched = tools::call_tool(json!({
        "name": "search_workspace", "arguments": {
            "query": "alpha[0-9]+", "paths": [source], "mode": "regex"
        }
    }))
    .await
    .unwrap();
    assert!(searched.get("isError").is_none(), "{searched}");
    let search_payload = tool_payload(&searched);
    assert_eq!(
        search_payload["groups"]["text"]["results"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "{search_payload}"
    );

    let graph = tools::call_tool(json!({
        "name": "get_call_graph", "arguments": {"path": source, "symbol": "main"}
    }))
    .await
    .unwrap();
    assert!(graph.get("isError").is_none(), "{graph}");
    assert!(
        tool_payload(&graph)["outbound_calls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|call| call == "helper")
    );

    let zip_path = directory.path().join("empty.zip");
    let writer = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
    writer.finish().unwrap();
    let archive = tools::call_tool(json!({
        "name": "peek_archive", "arguments": {"path": zip_path}
    }))
    .await
    .unwrap();
    assert!(archive.get("isError").is_none(), "{archive}");
    assert!(tool_payload(&archive).get("entries").is_some(), "{archive}");
}

#[tokio::test]
async fn extension_filter_never_returns_directories() {
    let directory = tempdir().unwrap();
    fs::create_dir(directory.path().join("dir with space")).unwrap();
    fs::create_dir(directory.path().join("example.rs")).unwrap();
    fs::write(
        directory.path().join("dir with space").join("test.rs"),
        "fn test() {}",
    )
    .unwrap();
    for target_type in ["any", "dir", "file"] {
        let result = fuzzy_find::execute(&json!({
            "pattern": "dir with space", "paths": [directory.path()],
            "target_type": target_type, "extensions": ["rs"]
        }))
        .await
        .unwrap();
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["type"] == "file"),
            "{target_type}: {result}"
        );
        let extension_named_directory = fuzzy_find::execute(&json!({
            "pattern": "example.rs", "paths": [directory.path()],
            "target_type": target_type, "extensions": ["rs"]
        }))
        .await
        .unwrap();
        assert!(
            extension_named_directory["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["type"] != "dir"),
            "{extension_named_directory}"
        );
    }
}

#[tokio::test]
async fn batch_subcalls_do_not_consume_rpc_rate_limit() {
    let calls = (0..20)
        .map(|_| json!({"tool": "validate_json", "args": {"content": "{}"}}))
        .collect::<Vec<_>>();
    for _ in 0..3 {
        let result = batch_tool_call::execute(&json!({"calls": calls}))
            .await
            .unwrap();
        assert_eq!(result["completed"], 20, "{result}");
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["status"] == "ok"),
            "{result}"
        );
    }
}

#[tokio::test]
async fn basename_file_hint_resolves_and_ambiguity_lists_candidates() {
    let directory = tempdir().unwrap();
    for folder in ["first", "second"] {
        let folder_path = directory.path().join(folder);
        fs::create_dir(&folder_path).unwrap();
        fs::write(
            folder_path.join("util.py"),
            "def calculate():\n    return 3\n",
        )
        .unwrap();
    }
    let chosen = read_symbol_body::execute(&json!({
        "symbol": "calculate", "paths": [directory.path().join("first")], "file_hint": "util.py"
    }))
    .await
    .unwrap();
    assert_eq!(chosen["name"], "calculate");

    let ambiguous = read_symbol_body::execute(&json!({
        "symbol": "calculate", "paths": [directory.path()]
    }))
    .await
    .unwrap();
    assert_eq!(ambiguous["ambiguous"], true);
    assert!(
        ambiguous["suggested_next_query"]
            .as_str()
            .unwrap()
            .contains("file_hint")
    );

    let hint_error = read_symbol_body::execute(&json!({
        "symbol": "calculate", "paths": [directory.path()], "file_hint": "util.py"
    }))
    .await
    .unwrap_err()
    .to_string();
    assert!(
        hint_error.contains("first")
            && hint_error.contains("second")
            && hint_error.contains("relative subpath"),
        "{hint_error}"
    );
}

#[tokio::test]
async fn invalid_search_and_range_arguments_are_explicit() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("input.txt");
    fs::write(&path, "hello\n").unwrap();
    let conflict = text_search::execute(&json!({
        "query": "hello", "paths": [directory.path()], "case_sensitive": true,
        "case_mode": "insensitive"
    }))
    .await
    .unwrap_err()
    .to_string();
    assert!(conflict.contains("case_sensitive") && conflict.contains("case_mode"));
    let regex_error = text_search::execute(&json!({
        "query": "[", "mode": "regex", "paths": [directory.path()]
    }))
    .await
    .unwrap_err()
    .to_string();
    assert!(
        regex_error.contains("Invalid search query")
            && regex_error.len() > "Invalid search query".len() + 10,
        "{regex_error}"
    );
    let range_error = read_file::execute(&json!({
        "path": path, "start_line": 3, "end_line": 2
    }))
    .await
    .unwrap_err()
    .to_string();
    assert!(range_error.contains("end_line must be >= start_line"));
}

#[tokio::test]
async fn missing_path_is_not_accessible_and_binary_warnings_are_plural() {
    let directory = tempdir().unwrap();
    let missing = resolve_path::execute(&json!({"path": directory.path().join("missing.txt")}))
        .await
        .unwrap();
    assert_eq!(missing["exists"], false);
    assert_eq!(missing["is_accessible"], false);

    let binary_path = directory.path().join("binary.dat");
    fs::write(&binary_path, [0, 1, 2, 3]).unwrap();
    for response in [
        read_file::execute(&json!({"path": binary_path}))
            .await
            .unwrap(),
        count_file_lines::execute(&json!({"path": binary_path}))
            .await
            .unwrap(),
        file_summary::execute(&json!({"path": binary_path}))
            .await
            .unwrap(),
    ] {
        assert!(response.get("warning").is_none(), "{response}");
        assert!(
            response["warnings"]
                .as_array()
                .is_some_and(|warnings| !warnings.is_empty()),
            "{response}"
        );
    }
}

#[tokio::test]
async fn imports_and_public_exports_cover_python_go_java_csharp() {
    let directory = tempdir().unwrap();
    for (filename, source, imported, exported, private) in [
        (
            "sample.py",
            "import os\nfrom pathlib import Path\ndef decorator(fn):\n    return fn\n@decorator\ndef visible():\n    pass\ndef _private():\n    pass\n",
            "pathlib",
            "visible",
            "_private",
        ),
        (
            "sample.go",
            "package example\nimport (\"fmt\"; alias \"os\")\ntype Widget struct {}\nfunc (w *Widget) Render() { fmt.Println(alias.Args) }\nfunc hidden() {}\n",
            "fmt",
            "Render",
            "hidden",
        ),
        (
            "Sample.java",
            "import java.util.List;\n@Deprecated\npublic class Sample { public void show() {} private void hidden() {} }\n",
            "java.util.List",
            "Sample",
            "hidden",
        ),
        (
            "Sample.cs",
            "using System.IO;\n[Obsolete]\npublic class Sample { public void Show() {} private void Hidden() {} }\n",
            "System.IO",
            "Sample",
            "Hidden",
        ),
    ] {
        let path = directory.path().join(filename);
        fs::write(&path, source).unwrap();
        let imports = list_imports::execute(&json!({"path": path})).await.unwrap();
        assert!(
            imports["imports"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["source"].as_str().unwrap_or("").contains(imported)),
            "{filename}: {imports}"
        );
        let exports = list_exports::execute(&json!({"path": path})).await.unwrap();
        assert!(
            exports["exports"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == exported),
            "{filename}: {exports}"
        );
        assert!(
            !exports["exports"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == private),
            "{filename}: {exports}"
        );
        if filename == "sample.go" {
            let symbols = get_symbols::execute(&json!({"path": path})).await.unwrap();
            assert!(
                symbols["symbols"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["qualified_name"] == "Widget.Render"),
                "{symbols}"
            );
        }
    }
}

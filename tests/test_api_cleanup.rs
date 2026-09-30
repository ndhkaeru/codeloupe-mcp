use codeloupe_mcp::indexer;
use codeloupe_mcp::tools::{
    self, compare_directories, create_file, edit_file, find_definition, find_references,
    fuzzy_find, project_map, read_file, read_snippets, read_symbol_body, search_workspace,
    text_search, warm_content_index, workspace_stats,
};
use codeloupe_mcp::workspace_control;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tempfile::tempdir;

fn decode_tool_content(result: &serde_json::Value) -> serde_json::Value {
    serde_json::from_str(
        result
            .pointer("/content/0/text")
            .and_then(serde_json::Value::as_str)
            .unwrap(),
    )
    .unwrap()
}

fn contains_object_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(key) || object.values().any(|value| contains_object_key(value, key))
        }
        Value::Array(items) => items.iter().any(|value| contains_object_key(value, key)),
        _ => false,
    }
}

#[tokio::test]
async fn test_search_workspace_distinguishes_default_exclusions_from_git_ignored_files() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::create_dir(dir.path().join("build")).unwrap();
    fs::create_dir(dir.path().join("dist")).unwrap();
    fs::write(dir.path().join(".gitignore"), "build/\n*.log\n").unwrap();
    fs::write(dir.path().join("build/gen.py"), "SECRETTOKEN\n").unwrap();
    fs::write(dir.path().join("dist/bundle.py"), "SECRETTOKEN\n").unwrap();
    fs::write(dir.path().join("run.log"), "SECRETTOKEN\n").unwrap();
    let result = search_workspace::execute(&json!({
        "query": "SECRETTOKEN", "paths": [dir.path()], "max_results": 10
    }))
    .await
    .unwrap();
    let warnings = result["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("1 ignored file(s)")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| {
            let text = warning.as_str().unwrap();
            text.contains("2 file(s) in default-excluded directories")
                && text.contains("build")
                && text.contains("dist")
                && text.contains("paths")
        }),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn test_text_search_decodes_windows_1252_for_matching_and_rendering() {
    let dir = tempdir().unwrap();
    let legacy = dir.path().join("legacy.txt");
    fs::write(&legacy, b"first\ncaf\xe9 au lait\n").unwrap();
    let utf8 = dir.path().join("unicode.txt");
    fs::write(&utf8, "café au lait\n").unwrap();
    let late = dir.path().join("late-legacy.txt");
    let mut late_content = vec![b'a'; 9_000];
    late_content.extend_from_slice(b"\ncaf\xe9 au lait\n");
    fs::write(&late, late_content).unwrap();
    for path in [legacy, utf8, late] {
        let result = text_search::execute(&json!({
            "query": "café", "paths": [path], "context_lines": 1
        }))
        .await
        .unwrap();
        assert_eq!(result["total_returned"], 1, "{result}");
        assert_eq!(result["matches"][0]["line_text"], "café au lait");
        assert_eq!(result["matches"][0]["match_column"], 1);
    }
}

#[test]
fn test_structured_errors_hide_windows_verbatim_path_prefixes() {
    let plain = tools::structured_tool_error("failed to read //?/E:/repo/file.rs");
    let plain_text = plain.to_string();
    assert!(!plain_text.contains("//?/"));
    assert!(plain_text.contains("E:/repo/file.rs"));

    let structured = tools::structured_tool_error(
        r#"{"error_code":"io_error","message":"failed at \\\\?\\E:\\repo\\file.rs"}"#,
    );
    let structured_text = structured.to_string();
    assert!(!structured_text.contains(r"\\?\"));
    assert!(structured_text.contains("E:"));
    assert!(structured.get("error_code").is_none());
    assert!(structured.get("message").is_none());
}

#[tokio::test]
async fn test_dispatcher_compacts_nested_paths_aliases_and_batch_children() {
    let dir = tempdir().unwrap();
    let workspace_root = PathBuf::from(codeloupe_mcp::common::normalize_display_path(
        &codeloupe_mcp::common::canonicalize_if_exists(dir.path().to_path_buf()),
    ));
    indexer::ensure_workspace_index(workspace_root.clone(), "compact_output_test".to_string());
    workspace_control::register_configured_workspace(
        workspace_root.clone(),
        "compact_output_test",
        true,
    );
    let first = workspace_root.join("first.rs");
    let second = workspace_root.join("second.rs");
    fs::write(&first, "fn provider() { step_one(); }\n").unwrap();
    fs::write(&second, "fn provider() { step_two(); }\n").unwrap();
    let root = codeloupe_mcp::common::normalize_display_path(&workspace_root);

    let snippets = decode_tool_content(
        &tools::call_tool(json!({
            "name": "read_snippets",
            "arguments": {"requests": [{"path": first.to_str().unwrap(), "max_lines": 1}]}
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        snippets.get("root").and_then(Value::as_str),
        Some(root.as_str())
    );
    assert_eq!(
        snippets.pointer("/results/0/path").and_then(Value::as_str),
        Some("first.rs")
    );
    assert!(!contains_object_key(&snippets, "canonical_path"));

    let edited = decode_tool_content(
        &tools::call_tool(json!({
            "name": "edit_files",
            "arguments": {"files": [{
                "path": first.to_str().unwrap(),
                "edits": [{"find": "step_one", "replace": "step_three", "expected_replacements": 1}]
            }]}
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        edited.get("root").and_then(Value::as_str),
        Some(root.as_str())
    );
    assert_eq!(
        edited.pointer("/files/0/path").and_then(Value::as_str),
        Some("first.rs")
    );
    assert!(!contains_object_key(&edited, "canonical_path"));
    for omitted in [
        "success",
        "message",
        "atomicity",
        "files_total",
        "edits_total",
        "rollback_performed",
    ] {
        assert!(edited.get(omitted).is_none());
    }
    for omitted in [
        "changed",
        "encoding_changed",
        "history_recorded",
        "previous_encoding",
        "target_encoding",
        "sha256_before",
    ] {
        assert!(edited.pointer(&format!("/files/0/{omitted}")).is_none());
    }
    assert!(
        edited
            .pointer("/files/0/sha256_after")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(
        edited
            .pointer("/files/0/history_entry_id")
            .and_then(Value::as_str)
            .is_some()
    );

    let single_edit = decode_tool_content(
        &tools::call_tool(json!({
            "name": "edit_file",
            "arguments": {
                "path": first.to_str().unwrap(),
                "mode": "find_replace",
                "find": "step_three",
                "replace": "step_four",
                "expected_replacements": 1
            }
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        single_edit.get("path").and_then(Value::as_str),
        Some(codeloupe_mcp::common::normalize_display_path(&first).as_str())
    );
    assert_eq!(
        single_edit.get("replacements").and_then(Value::as_u64),
        Some(1)
    );
    assert!(
        single_edit
            .get("sha256_after")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(
        single_edit
            .get("history_entry_id")
            .and_then(Value::as_str)
            .is_some()
    );
    let history_entry_id = single_edit
        .get("history_entry_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    assert!(single_edit.get("success").is_none());
    assert!(single_edit.get("message").is_none());
    assert!(single_edit.get("encoding_changed").is_none());
    assert!(
        single_edit
            .as_object()
            .is_some_and(|object| object.len() == 4)
    );

    let history = decode_tool_content(
        &tools::call_tool(json!({
            "name": "list_history",
            "arguments": {"limit": 100}
        }))
        .await
        .unwrap(),
    );
    let history_root = PathBuf::from(history.get("root").and_then(Value::as_str).unwrap());
    let history_entry = history
        .get("entries")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .find(|entry| {
            entry.get("entry_id").and_then(Value::as_str) == Some(history_entry_id.as_str())
        })
        .unwrap();
    let compact_path = PathBuf::from(history_entry.get("path").and_then(Value::as_str).unwrap());
    assert!(compact_path.is_relative());
    assert_eq!(
        codeloupe_mcp::common::canonicalize_if_exists(history_root.join(compact_path)),
        codeloupe_mcp::common::canonicalize_if_exists(first.clone())
    );
    assert!(!contains_object_key(history_entry, "canonical_path"));

    let compared = decode_tool_content(
        &tools::call_tool(json!({
            "name": "compare_symbols",
            "arguments": {
                "left": {"symbol": "provider", "paths": [first.to_str().unwrap()]},
                "right": {"symbol": "provider", "paths": [second.to_str().unwrap()]}
            }
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        compared.get("root").and_then(Value::as_str),
        Some(root.as_str())
    );
    assert_eq!(
        compared.pointer("/left/path").and_then(Value::as_str),
        Some("first.rs")
    );
    assert_eq!(
        compared.pointer("/right/path").and_then(Value::as_str),
        Some("second.rs")
    );
    assert!(compared.pointer("/left/name").is_none());
    assert!(compared.pointer("/left/qualified_name").is_none());

    let batch = decode_tool_content(
        &tools::call_tool(json!({
            "name": "batch_tool_call",
            "arguments": {"calls": [{
                "tool": "read_snippets",
                "args": {"requests": [{"path": second.to_str().unwrap(), "max_lines": 1}]}
            }]}
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        batch
            .pointer("/results/0/result/root")
            .and_then(Value::as_str),
        Some(root.as_str())
    );
    assert_eq!(
        batch
            .pointer("/results/0/result/results/0/path")
            .and_then(Value::as_str),
        Some("second.rs")
    );
}

#[tokio::test]
async fn test_dispatcher_compacts_all_write_success_payloads() {
    let dir = tempdir().unwrap();
    indexer::ensure_workspace_index(dir.path().to_path_buf(), "compact_write_test".to_string());
    workspace_control::register_configured_workspace(
        dir.path().to_path_buf(),
        "compact_write_test",
        true,
    );

    let created_path = dir.path().join("created.txt");
    let created = decode_tool_content(
        &tools::call_tool(json!({
            "name": "create_file",
            "arguments": {
                "path": created_path.to_str().unwrap(),
                "content": "alpha\nbeta\n"
            }
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        created.get("path").and_then(Value::as_str),
        Some(codeloupe_mcp::common::normalize_display_path(&created_path).as_str())
    );
    assert!(
        created
            .get("sha256_after")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(
        created
            .get("history_entry_id")
            .and_then(Value::as_str)
            .is_some()
    );
    for omitted in [
        "success",
        "message",
        "created",
        "history_recorded",
        "target_encoding",
        "line_ending",
    ] {
        assert!(created.get(omitted).is_none());
    }

    let overwritten = decode_tool_content(
        &tools::call_tool(json!({
            "name": "create_file",
            "arguments": {
                "path": created_path.to_str().unwrap(),
                "content": "gamma\n",
                "overwrite": true
            }
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        overwritten.get("overwritten").and_then(Value::as_bool),
        Some(true)
    );

    let converted = decode_tool_content(
        &tools::call_tool(json!({
            "name": "convert_file_format",
            "arguments": {
                "path": created_path.to_str().unwrap(),
                "target_line_ending": "crlf"
            }
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        converted.get("path").and_then(Value::as_str),
        Some(codeloupe_mcp::common::normalize_display_path(&created_path).as_str())
    );
    assert_eq!(
        converted.get("target_encoding").and_then(Value::as_str),
        Some("UTF-8")
    );
    assert_eq!(
        converted.get("line_ending").and_then(Value::as_str),
        Some("crlf")
    );
    assert!(
        converted
            .get("size_bytes")
            .and_then(Value::as_u64)
            .is_some()
    );
    assert!(
        converted
            .get("sha256_after")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(converted.get("file_size").is_none());
    assert!(converted.get("encoding_changed").is_none());
    assert!(converted.get("success").is_none());
    assert!(converted.get("message").is_none());

    let directory_path = dir.path().join("nested").join("leaf");
    let directory = decode_tool_content(
        &tools::call_tool(json!({
            "name": "create_directory",
            "arguments": {"path": directory_path.to_str().unwrap()}
        }))
        .await
        .unwrap(),
    );
    assert!(directory.get("path").and_then(Value::as_str).is_some());
    assert!(
        directory
            .get("history_entry_id")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(directory.get("created").is_none());
    assert!(directory.get("success").is_none());

    let existing_directory = decode_tool_content(
        &tools::call_tool(json!({
            "name": "create_directory",
            "arguments": {"path": directory_path.to_str().unwrap()}
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        existing_directory
            .get("already_existed")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        existing_directory
            .get("history_recorded")
            .and_then(Value::as_bool),
        Some(false)
    );

    let delete_path = dir.path().join("delete.txt");
    fs::write(&delete_path, "delete me").unwrap();
    let deleted = decode_tool_content(
        &tools::call_tool(json!({
            "name": "delete_file",
            "arguments": {"path": delete_path.to_str().unwrap()}
        }))
        .await
        .unwrap(),
    );
    assert!(deleted.get("path").and_then(Value::as_str).is_some());
    assert_eq!(
        deleted.get("bytes_removed").and_then(Value::as_u64),
        Some(9)
    );
    assert!(
        deleted
            .get("history_entry_id")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(deleted.get("deleted").is_none());
    assert!(deleted.get("success").is_none());
    assert!(deleted.get("message").is_none());

    let undo_path = dir.path().join("undo.txt");
    fs::write(&undo_path, "before").unwrap();
    let edited = decode_tool_content(
        &tools::call_tool(json!({
            "name": "edit_file",
            "arguments": {
                "path": undo_path.to_str().unwrap(),
                "mode": "replace",
                "content": "after"
            }
        }))
        .await
        .unwrap(),
    );
    let edited_entry = edited
        .get("history_entry_id")
        .and_then(Value::as_str)
        .unwrap();
    let undone = decode_tool_content(
        &tools::call_tool(json!({
            "name": "undo_change",
            "arguments": {"entry_id": edited_entry}
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        undone.get("undone_entry_id").and_then(Value::as_str),
        Some(edited_entry)
    );
    assert_eq!(
        undone.get("restored_state").and_then(Value::as_str),
        Some("file")
    );
    assert!(undone.get("sha256_after").and_then(Value::as_str).is_some());
    assert!(
        undone
            .get("history_entry_id")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(undone.get("success").is_none());
    assert!(undone.get("message").is_none());
    assert_eq!(fs::read_to_string(&undo_path).unwrap(), "before");
}

async fn wait_for_path_index(root: &std::path::Path) {
    indexer::ensure_workspace_index(root.to_path_buf(), "regression_test".to_string());
    for _ in 0..200 {
        if indexer::is_path_index_ready(root) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("path index did not become ready for {}", root.display());
}

async fn wait_for_content_index(path: &std::path::Path) -> serde_json::Value {
    let mut last_result = serde_json::Value::Null;
    for attempt in 0..3 {
        last_result = warm_content_index::execute(&json!({
            "paths": [path.to_str().unwrap()],
            "wait_ms": 30_000,
            "force": attempt > 0
        }))
        .await
        .unwrap();
        if last_result
            .get("ready_zones")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|zones| !zones.is_empty())
        {
            assert_eq!(
                last_result.get("outcome").and_then(|value| value.as_str()),
                Some("ready")
            );
            return last_result;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "content index did not become ready for {}: {last_result}",
        path.display()
    );
}

#[test]
fn test_tools_list_schema_is_compact_and_actionable() {
    let listed_tools = tools::list_tools();
    let serialized_bytes = serde_json::to_vec(&listed_tools).unwrap().len();
    assert!(
        serialized_bytes <= 24_000,
        "tools/list schema should stay below 24 KB, got {serialized_bytes} bytes"
    );

    for tool in listed_tools {
        assert!(tool.get("name").and_then(Value::as_str).is_some());
        assert!(
            tool.get("description")
                .and_then(Value::as_str)
                .is_some_and(|description| !description.is_empty())
        );
        assert!(tool.get("title").is_none());
        let input_schema = tool.get("inputSchema").unwrap();
        assert_eq!(
            input_schema.get("type").and_then(Value::as_str),
            Some("object")
        );
        assert!(!contains_object_key(input_schema, "description"));
    }
}

#[tokio::test]
async fn test_git_related_tools_are_not_exposed_or_dispatchable() {
    let removed_tools = [
        "git_status",
        "git_diff",
        "git_log",
        "git_blame",
        "get_semantic_diff",
        "markdown_outline",
        "read_markdown_section",
        "replace_markdown_section",
        "find_json_paths",
        "extract_json_schema",
        "sqlite_inspect",
        "diff_two_snippets",
        "history_status",
        "undo_last_change",
        "redo_last_change",
    ];
    let listed_tools = tools::list_tools();

    for removed_tool in removed_tools {
        assert!(
            !listed_tools
                .iter()
                .any(|tool| tool.get("name").and_then(|v| v.as_str()) == Some(removed_tool)),
            "{removed_tool} should not be exposed"
        );

        let result = tools::call_tool(json!({
            "name": removed_tool,
            "arguments": {}
        }))
        .await;
        assert!(result.is_err(), "{removed_tool} should not dispatch");
    }
}

#[tokio::test]
async fn test_tool_dispatch_validates_and_normalizes_arguments_from_schema() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("lines.txt");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();

    let normalized = tools::call_tool(json!({
        "name": "read_file_range",
        "arguments": {
            "path": path.to_str().unwrap(),
            "start_line": "2",
            "end_line": "2",
            "unexpected": true
        }
    }))
    .await
    .unwrap();
    assert_ne!(
        normalized.get("isError").and_then(|value| value.as_bool()),
        Some(true)
    );
    let payload = decode_tool_content(&normalized);
    assert_eq!(
        payload.get("start_line").and_then(|value| value.as_u64()),
        Some(2)
    );
    assert_eq!(
        payload.get("content").and_then(|value| value.as_str()),
        Some("two")
    );
    let warnings = payload
        .get("warnings")
        .and_then(|value| value.as_array())
        .unwrap();
    assert!(warnings.iter().any(|warning| {
        warning
            .as_str()
            .is_some_and(|text| text.contains("Coerced argument 'start_line'"))
    }));
    assert!(warnings.iter().any(|warning| {
        warning
            .as_str()
            .is_some_and(|text| text.contains("Unknown argument 'unexpected'"))
    }));

    let rejected = tools::call_tool(json!({
        "name": "text_search",
        "arguments": {"query": "needle", "paths": "src"}
    }))
    .await
    .unwrap_err();
    assert!(rejected.to_string().contains("expected array, got string"));
}

#[tokio::test]
async fn test_hash_and_json_validation_tools_are_exposed_and_dispatchable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("payload.json");
    fs::write(&path, "{\"value\":1}").unwrap();
    let listed_tools = tools::list_tools();
    for name in ["file_hash", "validate_json"] {
        assert!(
            listed_tools
                .iter()
                .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name)),
            "{name} should be exposed"
        );
    }

    let hashed = tools::call_tool(json!({
        "name": "file_hash",
        "arguments": { "path": path.to_str().unwrap() }
    }))
    .await
    .unwrap();
    let hash_payload = decode_tool_content(&hashed);
    assert_eq!(
        hash_payload.get("algorithm").and_then(Value::as_str),
        Some("sha256")
    );

    let validated = tools::call_tool(json!({
        "name": "validate_json",
        "arguments": { "path": path.to_str().unwrap() }
    }))
    .await
    .unwrap();
    let validation_payload = decode_tool_content(&validated);
    assert_eq!(
        validation_payload.get("valid").and_then(Value::as_bool),
        Some(true)
    );
}

#[tokio::test]
async fn test_search_workspace_runs_path_symbol_and_text_engines() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("PolicyValueNet.rs");
    fs::write(
        &source,
        "pub struct PolicyValueNet;\nimpl PolicyValueNet { pub fn evaluate() {} }\n",
    )
    .unwrap();

    let result = search_workspace::execute(&json!({
        "query": "PolicyValueNet",
        "paths": [dir.path().to_str().unwrap()],
        "max_results": 10,
        "verbose": true
    }))
    .await
    .unwrap();

    assert!(result.get("root").and_then(Value::as_str).is_some());

    assert!(
        tools::list_tools()
            .iter()
            .any(|tool| { tool.get("name").and_then(Value::as_str) == Some("search_workspace") })
    );
    let dispatched = tools::call_tool(json!({
        "name": "search_workspace",
        "arguments": {
            "query": "PolicyValueNet",
            "paths": [dir.path().to_str().unwrap()],
            "max_results": 1
        }
    }))
    .await
    .unwrap();
    assert!(dispatched.get("content").is_some());

    for (group, source, result_path) in [
        ("path", "fuzzy_find", "/groups/path/results"),
        ("symbol", "find_definition", "/groups/symbol/results"),
        ("text", "text_search", "/groups/text/results"),
    ] {
        assert_eq!(
            result
                .pointer(&format!("/groups/{group}/source"))
                .and_then(Value::as_str),
            Some(source)
        );
        assert_eq!(
            result
                .pointer(&format!("/groups/{group}/status"))
                .and_then(Value::as_str),
            Some("ok")
        );
        assert!(
            result
                .pointer(result_path)
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty()),
            "{group} results should not be empty: {result}"
        );
        assert!(result.pointer(&format!("/groups/{group}/root")).is_none());
        assert!(
            result
                .pointer(&format!("/groups/{group}/strategy/search_strategy"))
                .and_then(Value::as_str)
                .is_some()
        );
        assert!(
            result
                .pointer(&format!("/groups/{group}/strategy/index_used"))
                .is_some()
        );
        assert!(
            result
                .pointer(&format!("/groups/{group}/complete"))
                .and_then(Value::as_bool)
                .is_some()
        );
    }
}

#[tokio::test]
async fn test_search_workspace_keeps_other_groups_when_one_engine_fails() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("paren.rs"),
        "pub fn paren() { println!(\"(\"); }\n",
    )
    .unwrap();

    let result = search_workspace::execute(&json!({
        "query": "(",
        "paths": [dir.path().to_str().unwrap()],
        "text_mode": "regex"
    }))
    .await
    .unwrap();

    assert_eq!(
        result
            .pointer("/groups/path/status")
            .and_then(Value::as_str),
        Some("ok")
    );
    assert_eq!(
        result
            .pointer("/groups/symbol/status")
            .and_then(Value::as_str),
        Some("ok")
    );
    assert_eq!(
        result
            .pointer("/groups/text/status")
            .and_then(Value::as_str),
        Some("error")
    );
    assert!(
        result
            .pointer("/groups/text/error/code")
            .and_then(Value::as_str)
            .is_some()
    );
    assert_eq!(result.get("complete").and_then(Value::as_bool), Some(false));
}

#[tokio::test]
async fn test_search_workspace_bounds_total_output_without_dropping_groups() {
    let dir = tempdir().unwrap();
    for index in 0..40 {
        fs::write(
            dir.path().join(format!("needle_{index}.rs")),
            format!(
                "pub fn needle_{index}() {{ println!(\"needle {}\"); }}\n",
                "x".repeat(800)
            ),
        )
        .unwrap();
    }

    let max_output_bytes = 4_096usize;
    let result = search_workspace::execute(&json!({
        "query": "needle",
        "paths": [dir.path().to_str().unwrap()],
        "max_results": 100,
        "max_line_length": 1000,
        "max_output_bytes": max_output_bytes,
        "verbose": true
    }))
    .await
    .unwrap();

    assert!(serde_json::to_vec(&result).unwrap().len() <= max_output_bytes);
    assert_eq!(
        result.get("output_truncated").and_then(Value::as_bool),
        Some(true)
    );
    for group in ["path", "symbol", "text"] {
        assert!(result.pointer(&format!("/groups/{group}")).is_some());
    }
}

#[tokio::test]
async fn test_symbol_search_decodes_non_utf8_matches_without_losing_later_results() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy.rs");
    let mut bytes = b"fn helper() {}\n// fn helper ".to_vec();
    bytes.push(0xE9);
    bytes.extend_from_slice(b"\nfn helper() {}\n");
    fs::write(&path, bytes).unwrap();

    let references = find_references::execute(&json!({
        "symbol": "helper",
        "paths": [path.to_str().unwrap()],
        "verbose": true
    }))
    .await
    .unwrap();
    assert_eq!(
        references
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(3)
    );
    assert_eq!(
        references
            .pointer("/diagnostics/files_skipped_non_code")
            .and_then(|value| value.as_u64()),
        Some(0)
    );
    assert_eq!(
        references
            .pointer("/diagnostics/files_with_read_errors")
            .and_then(|value| value.as_u64()),
        Some(0)
    );

    let definitions = find_definition::execute(&json!({
        "symbol": "helper",
        "paths": [path.to_str().unwrap()],
        "verbose": true
    }))
    .await
    .unwrap();
    assert_eq!(
        definitions
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(2)
    );
    assert_eq!(
        definitions
            .get("definitions")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("line").and_then(|value| value.as_u64()))
                    .collect::<Vec<_>>()
            }),
        Some(vec![1, 3])
    );
    assert_eq!(
        definitions
            .pointer("/diagnostics/files_with_read_errors")
            .and_then(|value| value.as_u64()),
        Some(0)
    );
}

#[tokio::test]
async fn test_symbol_search_bounds_snippets_around_the_match() {
    let dir = tempdir().unwrap();
    let references_path = dir.path().join("minified.js");
    fs::write(
        &references_path,
        format!("{} helper();\n", "x".repeat(300_000)),
    )
    .unwrap();
    let references = find_references::execute(&json!({
        "symbol": "helper",
        "paths": [references_path.to_str().unwrap()],
        "max_line_length": 40
    }))
    .await
    .unwrap();
    let reference = references
        .get("references")
        .and_then(|value| value.as_array())
        .and_then(|items| items.first())
        .unwrap();
    let snippet = reference
        .get("snippet")
        .and_then(|value| value.as_str())
        .unwrap();
    assert!(snippet.contains("helper"));
    assert!(snippet.chars().count() <= 46);
    assert_eq!(
        reference
            .get("line_truncated")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(
        reference
            .get("match_column")
            .and_then(|value| value.as_u64())
            .is_some_and(|column| column > 300_000)
    );

    let definitions_path = dir.path().join("minified-definition.js");
    fs::write(
        &definitions_path,
        format!("helper();{} function helper() {{}}\n", "x".repeat(300_000)),
    )
    .unwrap();
    let definitions = find_definition::execute(&json!({
        "symbol": "helper",
        "paths": [definitions_path.to_str().unwrap()],
        "max_line_length": 40
    }))
    .await
    .unwrap();
    let definition = definitions
        .get("definitions")
        .and_then(|value| value.as_array())
        .and_then(|items| items.first())
        .unwrap();
    assert!(
        definition
            .get("snippet")
            .and_then(|value| value.as_str())
            .is_some_and(|snippet| snippet.contains("helper"))
    );
    assert_eq!(
        definition
            .get("line_truncated")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(
        definition
            .get("match_column")
            .and_then(|value| value.as_u64())
            .is_some_and(|column| column > 300_000)
    );
}

#[tokio::test]
async fn test_text_search_truncates_around_match_and_reports_column() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("long.rs");
    fs::write(
        &path,
        format!("const LONG: &str = \"{}Needle\";\n", "x".repeat(300)),
    )
    .unwrap();

    let result = text_search::execute(&json!({
        "query": "Needle",
        "paths": [path.to_str().unwrap()],
        "max_line_length": 20
    }))
    .await
    .unwrap();
    let first = result
        .get("matches")
        .and_then(|value| value.as_array())
        .and_then(|items| items.first())
        .unwrap();
    assert!(
        first
            .get("line_text")
            .and_then(|value| value.as_str())
            .is_some_and(|line| line.contains("Needle"))
    );
    assert_eq!(
        first
            .get("line_truncated")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(
        first
            .get("match_column")
            .and_then(|value| value.as_u64())
            .is_some_and(|column| column > 300)
    );
}

#[tokio::test]
async fn test_text_search_scans_files_larger_than_five_mebibytes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("large.rs");
    fs::write(
        &path,
        format!("{}large_file_needle\n", "x".repeat(6 * 1024 * 1024)),
    )
    .unwrap();

    let result = text_search::execute(&json!({
        "query": "large_file_needle",
        "paths": [path.to_str().unwrap()],
        "max_line_length": 40,
        "verbose": true
    }))
    .await
    .unwrap();

    assert_eq!(
        result
            .get("matches")
            .and_then(|value| value.as_array())
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        result
            .pointer("/diagnostics/files_skipped_large")
            .and_then(|value| value.as_u64()),
        Some(0)
    );
}

#[tokio::test]
async fn test_text_search_output_formats_group_relative_paths_and_report_completeness() {
    let dir = tempdir().unwrap();
    let root = PathBuf::from(codeloupe_mcp::common::normalize_display_path(
        &codeloupe_mcp::common::canonicalize_if_exists(dir.path().to_path_buf()),
    ));
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("alpha.rs"), "Needle one\nNeedle two\n").unwrap();
    fs::write(src.join("beta.rs"), "Needle three\n").unwrap();

    let json_result = text_search::execute(&json!({
        "query": "Needle",
        "paths": [root.to_str().unwrap()]
    }))
    .await
    .unwrap();
    let expected_root = codeloupe_mcp::common::normalize_display_path(&root);
    assert_eq!(
        json_result.get("root").and_then(Value::as_str),
        Some(expected_root.as_str())
    );
    assert_eq!(
        json_result.get("complete").and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        json_result
            .get("matches")
            .and_then(Value::as_array)
            .is_some_and(|matches| matches.iter().all(|item| {
                item.get("file")
                    .and_then(Value::as_str)
                    .is_some_and(|path| path.starts_with("src/"))
            }))
    );

    let compact = text_search::execute(&json!({
        "query": "Needle",
        "paths": [dir.path().to_str().unwrap()],
        "output_format": "compact"
    }))
    .await
    .unwrap();
    let compact_text = compact
        .get("__mcp_raw_text")
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(compact_text.matches("file: src/alpha.rs").count(), 1);
    assert_eq!(compact_text.matches("file: src/beta.rs").count(), 1);
    assert!(compact_text.contains("complete: true"));
    assert!(!compact_text.contains("diagnostics:"));

    let limited = text_search::execute(&json!({
        "query": "Needle",
        "paths": [dir.path().to_str().unwrap()],
        "max_results": 1,
        "output_format": "compact"
    }))
    .await
    .unwrap();
    let limited_text = limited
        .get("__mcp_raw_text")
        .and_then(Value::as_str)
        .unwrap();
    assert!(limited_text.contains("complete: false"));
    assert!(limited_text.contains("diagnostics:"));

    let markdown = text_search::execute(&json!({
        "query": "Needle",
        "paths": [dir.path().to_str().unwrap()],
        "output_format": "markdown",
        "verbose": true
    }))
    .await
    .unwrap();
    let markdown_text = markdown
        .get("__mcp_raw_text")
        .and_then(Value::as_str)
        .unwrap();
    assert!(markdown_text.contains("# Text Search"));
    assert!(markdown_text.contains("## `src/alpha.rs`"));
    assert!(markdown_text.contains("## Diagnostics"));
}

#[tokio::test]
async fn test_json_diagnostics_are_nested_only_when_abnormal_or_verbose() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join("alpha.rs"), "fn needle() {}\n").unwrap();
    fs::write(dir.path().join("beta.rs"), "fn needle() {}\n").unwrap();

    let search = text_search::execute(&json!({
        "query": "needle",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(search.get("complete").and_then(Value::as_bool), Some(true));
    assert!(search.get("files_considered").is_none());
    assert!(search.get("diagnostics").is_none());

    let verbose_search = text_search::execute(&json!({
        "query": "needle",
        "paths": [dir.path().to_str().unwrap()],
        "verbose": true
    }))
    .await
    .unwrap();
    assert!(
        verbose_search
            .pointer("/diagnostics/files_considered")
            .and_then(Value::as_u64)
            .is_some()
    );

    let limited_search = text_search::execute(&json!({
        "query": "needle",
        "paths": [dir.path().to_str().unwrap()],
        "max_results": 1
    }))
    .await
    .unwrap();
    assert_eq!(
        limited_search.get("complete").and_then(Value::as_bool),
        Some(false)
    );
    assert!(limited_search.get("diagnostics").is_some());

    let map = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(map.get("complete").and_then(Value::as_bool), Some(true));
    assert!(map.get("entries_seen").is_none());
    assert!(map.get("diagnostics").is_none());

    let verbose_map = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "verbose": true
    }))
    .await
    .unwrap();
    assert!(
        verbose_map
            .pointer("/diagnostics/entries_seen")
            .and_then(Value::as_u64)
            .is_some()
    );
}

#[tokio::test]
async fn test_symbol_search_diagnostics_are_nested_only_when_needed() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() {}\nfn main() { helper(); }\n",
    )
    .unwrap();

    let definitions = find_definition::execute(&json!({
        "symbol": "helper",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        definitions.get("complete").and_then(Value::as_bool),
        Some(true)
    );
    assert!(definitions.get("files_searched").is_none());
    assert!(definitions.get("diagnostics").is_none());

    let verbose_definitions = find_definition::execute(&json!({
        "symbol": "helper",
        "paths": [dir.path().to_str().unwrap()],
        "verbose": true
    }))
    .await
    .unwrap();
    assert!(
        verbose_definitions
            .pointer("/diagnostics/files_searched")
            .and_then(Value::as_u64)
            .is_some()
    );

    let references = find_references::execute(&json!({
        "symbol": "helper",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        references.get("complete").and_then(Value::as_bool),
        Some(true)
    );
    assert!(references.get("files_searched").is_none());
    assert!(references.get("diagnostics").is_none());

    let verbose_references = find_references::execute(&json!({
        "symbol": "helper",
        "paths": [dir.path().to_str().unwrap()],
        "verbose": true
    }))
    .await
    .unwrap();
    assert!(
        verbose_references
            .pointer("/diagnostics/files_searched")
            .and_then(Value::as_u64)
            .is_some()
    );
}

#[tokio::test]
async fn test_zero_result_searches_report_files_searched() {
    let dir = tempdir().unwrap();
    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    fs::write(dir.path().join("main.rs"), "fn present() {}\n").unwrap();

    let text_with_candidate = text_search::execute(&json!({
        "query": "absent",
        "paths": [dir.path().join("main.rs").to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        text_with_candidate
            .get("files_searched")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert!(text_with_candidate.get("diagnostics").is_none());

    let text_without_candidates = text_search::execute(&json!({
        "query": "absent",
        "paths": [empty.to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        text_without_candidates
            .get("files_searched")
            .and_then(Value::as_u64),
        Some(0)
    );

    let compact = text_search::execute(&json!({
        "query": "absent",
        "paths": [dir.path().join("main.rs").to_str().unwrap()],
        "output_format": "compact"
    }))
    .await
    .unwrap();
    assert!(
        compact
            .get("__mcp_raw_text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("files_searched: 1"))
    );

    let definitions = find_definition::execute(&json!({
        "symbol": "absent",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        definitions.get("files_searched").and_then(Value::as_u64),
        Some(1)
    );

    let references = find_references::execute(&json!({
        "symbol": "absent",
        "paths": [empty.to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        references.get("files_searched").and_then(Value::as_u64),
        Some(0)
    );
}

#[tokio::test]
async fn test_path_tools_report_consistent_index_age_fields() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::create_dir_all(dir.path().join("src/deep/nested")).unwrap();
    fs::write(dir.path().join("src/deep/nested/main.rs"), "fn main() {}\n").unwrap();
    wait_for_path_index(dir.path()).await;

    let map = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "max_depth": 5,
        "verbose": true
    }))
    .await
    .unwrap();
    assert_eq!(map.get("index_used").and_then(Value::as_bool), Some(true));
    assert_eq!(
        map.get("index_complete").and_then(Value::as_bool),
        Some(true)
    );
    assert!(map.get("index_age_secs").and_then(Value::as_u64).is_some());
    assert!(
        map.pointer("/diagnostics/indexed_at")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
    );

    let fuzzy = fuzzy_find::execute(&json!({
        "pattern": "main",
        "paths": [dir.path().to_str().unwrap()],
        "verbose": true
    }))
    .await
    .unwrap();
    assert_eq!(fuzzy.get("index_used").and_then(Value::as_bool), Some(true));
    assert_eq!(
        fuzzy.get("index_complete").and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        fuzzy
            .get("index_age_secs")
            .and_then(Value::as_u64)
            .is_some()
    );
    assert!(
        fuzzy
            .pointer("/diagnostics/indexed_at")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
    );
}

#[tokio::test]
async fn test_dispatcher_wraps_tool_failures_in_structured_error() {
    let result = tools::call_tool(json!({
        "name": "text_search",
        "arguments": { "query": "" }
    }))
    .await
    .unwrap();
    assert_eq!(result.get("isError").and_then(Value::as_bool), Some(true));
    let payload = decode_tool_content(&result);
    assert_eq!(
        payload.pointer("/error/code").and_then(Value::as_str),
        Some("invalid_argument")
    );
    assert!(
        payload
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("Query cannot be empty"))
    );
}

#[tokio::test]
async fn test_dispatcher_classifies_negated_glob_as_invalid_argument() {
    let dir = tempdir().unwrap();
    let result = tools::call_tool(json!({
        "name": "text_search",
        "arguments": {
            "query": "needle",
            "paths": [dir.path().to_str().unwrap()],
            "includes": ["!*.rs"]
        }
    }))
    .await
    .unwrap();

    assert_eq!(result.get("isError").and_then(Value::as_bool), Some(true));
    let payload = decode_tool_content(&result);
    assert_eq!(
        payload.pointer("/error/code").and_then(Value::as_str),
        Some("invalid_argument")
    );
}

#[tokio::test]
async fn test_dispatcher_marks_structured_write_failures_as_errors() {
    let result = tools::call_tool(json!({
        "name": "create_file",
        "arguments": {
            "path": "",
            "content": "replacement"
        }
    }))
    .await
    .unwrap();

    assert_eq!(result.get("isError").and_then(Value::as_bool), Some(true));
    let payload = decode_tool_content(&result);
    assert_eq!(
        payload.pointer("/error/code").and_then(Value::as_str),
        Some("invalid_path")
    );
    assert!(payload.get("error_code").is_none());
    assert!(payload.get("message").is_none());
    assert!(payload.get("reason").is_none());
    assert_eq!(
        payload.pointer("/error/message").and_then(Value::as_str),
        Some("path is required")
    );
}

#[tokio::test]
async fn test_content_index_tools_are_exposed_and_report_path_status() {
    let listed_tools = tools::list_tools();
    assert!(
        listed_tools
            .iter()
            .any(|tool| tool.get("name").and_then(|v| v.as_str()) == Some("content_index_status"))
    );
    assert!(
        listed_tools
            .iter()
            .any(|tool| tool.get("name").and_then(|v| v.as_str()) == Some("warm_content_index"))
    );
    assert!(
        listed_tools
            .iter()
            .any(|tool| tool.get("name").and_then(|v| v.as_str()) == Some("index_gc"))
    );

    let dir = tempdir().unwrap();
    let status_result = tools::call_tool(json!({
        "name": "content_index_status",
        "arguments": { "paths": [dir.path().to_str().unwrap()] }
    }))
    .await
    .unwrap();
    let status_text = status_result
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|v| v.as_str())
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_str(status_text).unwrap();
    assert_eq!(status_json.get("total").and_then(|v| v.as_u64()), Some(1));

    let warm_result = tools::call_tool(json!({
        "name": "warm_content_index",
        "arguments": { "paths": [dir.path().to_str().unwrap()], "wait_ms": 0 }
    }))
    .await
    .unwrap();
    assert_eq!(
        warm_result.get("isError").and_then(|value| value.as_bool()),
        Some(true)
    );
    let warm_payload = decode_tool_content(&warm_result);
    assert_eq!(
        warm_payload.get("outcome").and_then(|value| value.as_str()),
        Some("nothing_to_warm")
    );
    assert!(
        warm_payload
            .pointer("/error/message")
            .and_then(|value| value.as_str())
            .is_some_and(|message| message.contains("Nothing to warm"))
    );
    assert_eq!(
        warm_payload.pointer("/error/code").and_then(Value::as_str),
        Some("invalid_argument")
    );
    for omitted in [
        "requested_zones",
        "ready_zones",
        "warming_zones",
        "wait_ms",
        "force",
        "include_ignored",
    ] {
        assert!(warm_payload.get(omitted).is_none());
    }
}

#[tokio::test]
async fn test_literal_search_verifies_warmed_index_and_reports_scope_diagnostics() {
    let dir = tempdir().unwrap();
    workspace_control::register_configured_workspace(
        dir.path().to_path_buf(),
        "content_index_write_test",
        true,
    );
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("Foo.cs"),
        "class Foo { string v = \"ZqxToken42\"; }\n",
    )
    .unwrap();
    fs::write(src.join("config.json"), "{\"value\":\"ZqxToken42\"}\n").unwrap();
    fs::write(src.join("trace.jsonl"), "{\"event\":\"ZqxToken42\"}\n").unwrap();
    fs::write(src.join("notes.xyz"), "ZqxToken42\n").unwrap();

    wait_for_path_index(dir.path()).await;
    let warm = wait_for_content_index(&src).await;
    assert!(
        warm.get("ready_zones")
            .and_then(|value| value.as_array())
            .is_some_and(|zones| !zones.is_empty()),
        "warm result: {warm}"
    );
    assert!(
        warm.get("statuses")
            .and_then(|value| value.as_array())
            .is_some_and(|statuses| statuses.iter().any(|status| {
                status
                    .get("indexed_at")
                    .and_then(|value| value.as_u64())
                    .is_some()
            }))
    );

    let literal = text_search::execute(&json!({
        "query": "Token42",
        "paths": [src.to_str().unwrap()],
        "max_results": 20,
        "verbose": true
    }))
    .await
    .unwrap();
    let regex = text_search::execute(&json!({
        "query": "Token42",
        "mode": "regex",
        "paths": [src.to_str().unwrap()],
        "max_results": 20
    }))
    .await
    .unwrap();

    assert_eq!(
        literal
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(4),
        "literal result: {literal}"
    );
    assert_eq!(
        literal
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        regex.get("total_returned").and_then(|value| value.as_u64())
    );
    assert_eq!(
        literal
            .get("search_strategy")
            .and_then(|value| value.as_str()),
        Some("mixed")
    );
    assert!(
        literal
            .pointer("/diagnostics/fallback_reason")
            .and_then(|value| value.as_array())
            .is_some_and(|reasons| reasons
                .iter()
                .any(|reason| { reason.as_str() == Some("literal_verification_requires_grep") }))
    );
    assert!(
        literal
            .pointer("/diagnostics/zone_indexed_at")
            .and_then(|value| value.as_array())
            .is_some_and(|zones| !zones.is_empty())
    );
    assert!(
        literal
            .get("index_age_secs")
            .and_then(|value| value.as_u64())
            .is_some()
    );
    assert!(
        literal
            .pointer("/diagnostics/unindexed_files_in_scope")
            .and_then(|value| value.as_array())
            .is_some_and(|paths| paths.iter().any(|path| {
                path.as_str()
                    .is_some_and(|path| path.ends_with("notes.xyz"))
            }))
    );
    assert_eq!(
        literal
            .pointer("/diagnostics/unindexed_files_in_scope_count")
            .and_then(|value| value.as_u64()),
        Some(1),
        "unindexed diagnostics: {literal}"
    );
    assert_eq!(
        literal
            .pointer("/diagnostics/unindexed_files_complete")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(literal.get("unindexed_files_truncated").is_none());
    assert_eq!(
        literal
            .pointer("/diagnostics/candidates_complete")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(literal.get("candidates_truncated").is_none());
    assert_eq!(
        literal
            .pointer("/diagnostics/grep_fallback_performed")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(literal.pointer("/diagnostics/no_fallback_reason").is_none());
    assert_eq!(
        literal
            .pointer("/diagnostics/unindexed_files_scope_complete")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    let edited = edit_file::execute(&json!({
        "path": src.join("config.json").to_str().unwrap(),
        "mode": "find_replace",
        "find": "ZqxToken42",
        "replace": "ZqxToken42 QwvNew77",
        "expected_replacements": 1
    }))
    .await
    .unwrap();
    assert_eq!(
        edited.get("success").and_then(|value| value.as_bool()),
        Some(true)
    );
    let created = create_file::execute(&json!({
        "path": src.join("new.json").to_str().unwrap(),
        "content": "{\"value\":\"QwvNew77\"}\n"
    }))
    .await
    .unwrap();
    assert_eq!(
        created.get("success").and_then(|value| value.as_bool()),
        Some(true)
    );

    let stale_safe = text_search::execute(&json!({
        "query": "QwvNew77",
        "paths": [src.to_str().unwrap()],
        "max_results": 20
    }))
    .await
    .unwrap();
    assert_eq!(
        stale_safe
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(2),
        "stale-safe result: {stale_safe}"
    );
}

fn write_glob_fixture(root: &std::path::Path) {
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("g/sub/deep")).unwrap();
    for relative in [
        "g/a.rs",
        "g/b.py",
        "g/sub/c.rs",
        "g/sub/deep/d.py",
        "g/sub/e.RS",
    ] {
        fs::write(root.join(relative), format!("GlobNeedle {relative}\n")).unwrap();
    }
}

fn project_map_file_count(result: &serde_json::Value) -> u64 {
    result
        .get("tree_representation")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flat_map(|tree| tree.values())
        .filter_map(|children| children.get("files"))
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .count() as u64
}

#[tokio::test]
async fn test_glob_filters_are_consistent_across_tools() {
    let dir = tempdir().unwrap();
    write_glob_fixture(dir.path());
    let root = dir.path().join("g");
    wait_for_path_index(dir.path()).await;
    let warm = wait_for_content_index(&root).await;
    assert!(
        warm.get("ready_zones")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|zones| !zones.is_empty()),
        "warm result: {warm}"
    );
    let expected = if cfg!(windows) { 5 } else { 4 };
    let brace_pattern = "*.{rs,py}";

    let search = text_search::execute(&json!({
        "query": "GlobNeedle",
        "paths": [root.to_str().unwrap()],
        "includes": [brace_pattern],
        "max_results": 20,
        "verbose": true
    }))
    .await
    .unwrap();
    assert_eq!(
        search
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(expected),
        "text_search result: {search}"
    );
    assert_eq!(
        search
            .pointer("/diagnostics/content_index_used")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "text_search did not exercise warmed-index filtering: {search}"
    );

    let excluded = text_search::execute(&json!({
        "query": "GlobNeedle",
        "paths": [root.to_str().unwrap()],
        "excludes": ["**/*.{rs,py}"],
        "max_results": 20
    }))
    .await
    .unwrap();
    assert_eq!(
        excluded
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(if cfg!(windows) { 0 } else { 1 }),
        "warmed exclude result: {excluded}"
    );

    let map = project_map::execute(&json!({
        "path": root.to_str().unwrap(),
        "max_depth": 5,
        "includes": [brace_pattern]
    }))
    .await
    .unwrap();
    assert_eq!(
        project_map_file_count(&map),
        expected,
        "project_map result: {map}"
    );

    let stats = workspace_stats::execute(&json!({
        "path": root.to_str().unwrap(),
        "includes": [brace_pattern]
    }))
    .await
    .unwrap();
    assert_eq!(
        stats.get("total_files").and_then(serde_json::Value::as_u64),
        Some(expected),
        "workspace_stats result: {stats}"
    );

    let fuzzy = fuzzy_find::execute(&json!({
        "pattern": brace_pattern,
        "paths": [root.to_str().unwrap()],
        "target_type": "file",
        "max_results": 20
    }))
    .await
    .unwrap();
    assert_eq!(
        fuzzy
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(expected),
        "fuzzy_find result: {fuzzy}"
    );

    let empty = tempdir().unwrap();
    let compared = compare_directories::execute(&json!({
        "left_path": root.to_str().unwrap(),
        "right_path": empty.path().to_str().unwrap(),
        "includes": [brace_pattern],
        "summary_only": true
    }))
    .await
    .unwrap();
    assert_eq!(
        compared
            .pointer("/summary/changed_files")
            .and_then(serde_json::Value::as_u64),
        Some(expected),
        "compare_directories result: {compared}"
    );

    let backslash = text_search::execute(&json!({
        "query": "GlobNeedle",
        "paths": [root.to_str().unwrap()],
        "includes": ["sub\\*.{rs,py}"],
        "max_results": 20
    }))
    .await
    .unwrap();
    assert_eq!(
        backslash
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(if cfg!(windows) { 2 } else { 1 }),
        "backslash glob result: {backslash}"
    );

    let invalid = text_search::execute(&json!({
        "query": "GlobNeedle",
        "paths": [root.to_str().unwrap()],
        "includes": ["!*.rs"]
    }))
    .await;
    assert!(invalid.is_err(), "leading ! must be rejected");
}

#[tokio::test]
async fn test_directory_excludes_match_in_walk_and_index_modes() {
    let walk_dir = tempdir().unwrap();
    let indexed_dir = tempdir().unwrap();
    write_glob_fixture(walk_dir.path());
    write_glob_fixture(indexed_dir.path());
    wait_for_path_index(indexed_dir.path()).await;
    let walk_root = walk_dir.path().join("g");
    let indexed_root = indexed_dir.path().join("g");

    for exclude in ["sub", "sub/**", "**/sub"] {
        let walk_map = project_map::execute(&json!({
            "path": walk_root.to_str().unwrap(),
            "max_depth": 5,
            "excludes": [exclude]
        }))
        .await
        .unwrap();
        let indexed_map = project_map::execute(&json!({
            "path": indexed_root.to_str().unwrap(),
            "max_depth": 5,
            "excludes": [exclude]
        }))
        .await
        .unwrap();
        assert_eq!(
            project_map_file_count(&walk_map),
            2,
            "walk map for {exclude}: {walk_map}"
        );
        assert_eq!(
            project_map_file_count(&indexed_map),
            2,
            "index map for {exclude}: {indexed_map}"
        );

        let walk_stats = workspace_stats::execute(&json!({
            "path": walk_root.to_str().unwrap(),
            "excludes": [exclude]
        }))
        .await
        .unwrap();
        let indexed_stats = workspace_stats::execute(&json!({
            "path": indexed_root.to_str().unwrap(),
            "excludes": [exclude]
        }))
        .await
        .unwrap();
        assert_eq!(
            walk_stats
                .get("total_files")
                .and_then(serde_json::Value::as_u64),
            Some(2),
            "walk stats for {exclude}: {walk_stats}"
        );
        assert_eq!(
            indexed_stats
                .get("total_files")
                .and_then(serde_json::Value::as_u64),
            Some(2),
            "index stats for {exclude}: {indexed_stats}"
        );
    }
}

#[tokio::test]
async fn test_text_search_supports_explicit_modes_and_preserves_raw_line_text() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.rs");
    fs::write(&path, "    TODO\nFIXME\n").unwrap();

    let literal_res = text_search::execute(&json!({
        "query": "TODO|FIXME",
        "paths": [path.to_str().unwrap()],
        "explain_no_results": true
    }))
    .await
    .unwrap();

    // The effective mode is reported with the no-result diagnostics, where it
    // explains why "TODO|FIXME" matched nothing as a literal.
    assert_eq!(
        literal_res
            .pointer("/diagnostics/no_results/mode")
            .and_then(|v| v.as_str()),
        Some("literal")
    );
    assert_eq!(
        literal_res.get("total_returned").and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        literal_res
            .pointer("/diagnostics/no_results")
            .and_then(|v| v.get("reason"))
            .and_then(|v| v.as_str()),
        Some("no_match_found")
    );

    let regex_res = text_search::execute(&json!({
        "query": "TODO|FIXME",
        "paths": [path.to_str().unwrap()],
        "mode": "regex"
    }))
    .await
    .unwrap();

    assert_eq!(
        regex_res.get("total_returned").and_then(|v| v.as_u64()),
        Some(2)
    );

    let raw_line_res = text_search::execute(&json!({
        "query": "TODO",
        "paths": [path.to_str().unwrap()]
    }))
    .await
    .unwrap();

    let first_match = raw_line_res
        .get("matches")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .cloned()
        .unwrap();
    assert_eq!(
        first_match.get("line_text").and_then(|v| v.as_str()),
        Some("    TODO")
    );
    // `snippet` duplicated `line_text` minus indentation and was dropped.
    assert!(first_match.get("snippet").is_none());
    assert!(first_match.get("context_before").is_none());
}

#[tokio::test]
async fn test_text_search_returns_context_lines() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("context.rs");
    fs::write(
        &path,
        "one\ntwo\nNEEDLE_A\nthree\nfour\nfive\nNEEDLE_B\nsix\n",
    )
    .unwrap();

    let res = text_search::execute(&json!({
        "query": "NEEDLE",
        "paths": [path.to_str().unwrap()],
        "context_lines": 2
    }))
    .await
    .unwrap();

    let matches = res.get("matches").and_then(|v| v.as_array()).unwrap();
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].get("line").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(
        matches[0].get("context_before"),
        Some(&json!(["one", "two"]))
    );
    assert_eq!(
        matches[0].get("context_after"),
        Some(&json!(["three", "four"]))
    );
    assert_eq!(
        matches[1].get("context_before"),
        Some(&json!(["four", "five"]))
    );
    assert_eq!(matches[1].get("context_after"), Some(&json!(["six"])));

    let first_line = text_search::execute(&json!({
        "query": "one",
        "paths": [path.to_str().unwrap()],
        "context_lines": 1
    }))
    .await
    .unwrap();
    let only = first_line.pointer("/matches/0").cloned().unwrap();
    assert!(only.get("context_before").is_none());
    assert_eq!(only.get("context_after"), Some(&json!(["two"])));
}

#[tokio::test]
async fn test_text_search_keeps_matches_in_non_utf8_files() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("latin1.txt");
    // "caf\xe9 TODO" is valid Windows-1252 but not valid UTF-8.
    fs::write(&path, b"caf\xe9 TODO\n").unwrap();

    let res = text_search::execute(&json!({
        "query": "TODO",
        "paths": [path.to_str().unwrap()],
        "verbose": true
    }))
    .await
    .unwrap();

    assert_eq!(res.get("total_returned").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(
        res.pointer("/diagnostics/search_errors")
            .and_then(|v| v.as_u64()),
        Some(0)
    );
}

#[tokio::test]
async fn test_text_search_applies_excludes_to_explicit_file_paths() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("target.rs");
    fs::write(&path, "needle\n").unwrap();

    let res = text_search::execute(&json!({
        "query": "needle",
        "paths": [path.to_str().unwrap()],
        "excludes": ["target.rs"],
        "explain_no_results": true
    }))
    .await
    .unwrap();

    assert_eq!(res.get("total_returned").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(
        res.pointer("/diagnostics/no_results")
            .and_then(|v| v.get("reason"))
            .and_then(|v| v.as_str()),
        Some("no_candidate_files")
    );
}

#[tokio::test]
async fn test_text_search_reports_fallback_diagnostics_for_regex() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.rs");
    fs::write(&path, "TODO\n").unwrap();

    let res = text_search::execute(&json!({
        "query": "TODO|FIXME",
        "paths": [path.to_str().unwrap()],
        "mode": "regex",
        "explain_no_results": true,
        "verbose": true
    }))
    .await
    .unwrap();

    assert_eq!(
        res.get("search_strategy").and_then(|v| v.as_str()),
        Some("grep_fallback")
    );
    assert!(
        res.pointer("/diagnostics/fallback_reason")
            .and_then(|v| v.as_array())
            .is_some_and(|items| items.iter().any(|item| item == "regex_mode_requires_grep"))
    );
    // Echoed inputs and engine labels derivable from search_strategy are gone.
    for removed in [
        "engine",
        "candidate_engine",
        "verification_engine",
        "mode",
        "max_line_length",
        "allow_expensive_fallback",
        "default_excludes",
        "candidate_count",
    ] {
        assert!(res.get(removed).is_none(), "unexpected field {removed}");
    }
}

#[tokio::test]
async fn test_text_search_allows_root_grep_for_small_indexed_scopes() {
    for with_marker in [true, false] {
        let dir = tempdir().unwrap();
        if with_marker {
            fs::create_dir_all(dir.path().join(".git")).unwrap();
        }
        fs::write(dir.path().join("small.rs"), "SMALL_ROOT_TOKEN\n").unwrap();
        wait_for_path_index(dir.path()).await;

        let result = text_search::execute(&json!({
            "query": "SMALL_ROOT_TOKEN|MISSING",
            "mode": "regex",
            "paths": [dir.path().to_str().unwrap()]
        }))
        .await
        .unwrap();

        assert_eq!(
            result
                .get("search_strategy")
                .and_then(|value| value.as_str()),
            Some("grep_fallback")
        );
        assert_eq!(
            result
                .get("total_returned")
                .and_then(|value| value.as_u64()),
            Some(1)
        );
        assert!(result.get("suggested_next_query").is_none());
    }
}

#[tokio::test]
async fn test_text_search_applies_default_fallback_excludes_but_allows_direct_scope() {
    let dir = tempdir().unwrap();
    let vendor_dir = dir.path().join("third_party");
    fs::create_dir(&vendor_dir).unwrap();
    let vendor_file = vendor_dir.join("lib.rs");
    fs::write(&vendor_file, "needle\n").unwrap();

    let root_res = text_search::execute(&json!({
        "query": "needle",
        "paths": [dir.path().to_str().unwrap()],
        "explain_no_results": true
    }))
    .await
    .unwrap();

    assert_eq!(
        root_res
            .pointer("/diagnostics/default_excludes_applied")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        root_res.get("total_returned").and_then(|v| v.as_u64()),
        Some(0)
    );

    let direct_res = text_search::execute(&json!({
        "query": "needle",
        "paths": [vendor_dir.to_str().unwrap()],
        "verbose": true
    }))
    .await
    .unwrap();

    assert_eq!(
        direct_res
            .pointer("/diagnostics/default_excludes_applied")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        direct_res.get("total_returned").and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[tokio::test]
async fn test_managed_build_outputs_are_excluded_without_hiding_source_bin() {
    let managed = tempdir().unwrap();
    fs::write(managed.path().join("sample.sln"), "").unwrap();
    fs::create_dir_all(managed.path().join("obj/Debug")).unwrap();
    fs::create_dir_all(managed.path().join("bin/Debug")).unwrap();
    fs::write(
        managed.path().join("obj/Debug/generated.cs"),
        "managed_build_needle\n",
    )
    .unwrap();
    fs::write(
        managed.path().join("bin/Debug/generated.cs"),
        "managed_build_needle\n",
    )
    .unwrap();

    let managed_root = text_search::execute(&json!({
        "query": "managed_build_needle",
        "paths": [managed.path().to_str().unwrap()],
        "explain_no_results": true
    }))
    .await
    .unwrap();
    assert_eq!(
        managed_root
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(0)
    );
    let defaults = managed_root
        .pointer("/diagnostics/no_results/default_excludes")
        .and_then(Value::as_array)
        .unwrap();
    assert!(defaults.iter().any(|value| value == "obj"));
    assert!(defaults.iter().any(|value| value == "bin"));
    assert!(defaults.len() < 30);
    let unique = defaults.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(
        unique.len(),
        defaults.len(),
        "default exclusions repeat: {defaults:?}"
    );

    let direct_bin = text_search::execute(&json!({
        "query": "managed_build_needle",
        "paths": [managed.path().join("bin").to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        direct_bin
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(1)
    );

    let source = tempdir().unwrap();
    fs::write(source.path().join("package.json"), "{}").unwrap();
    fs::create_dir(source.path().join("bin")).unwrap();
    fs::write(source.path().join("bin/cli.js"), "source_bin_needle\n").unwrap();
    let source_root = text_search::execute(&json!({
        "query": "source_bin_needle",
        "paths": [source.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        source_root
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(1)
    );
}

#[tokio::test]
async fn test_compare_directories_ignores_managed_build_outputs() {
    let dir = tempdir().unwrap();
    let left = dir.path().join("left-managed");
    let right = dir.path().join("right-managed");
    for root in [&left, &right] {
        fs::create_dir_all(root.join("obj/Debug")).unwrap();
        fs::create_dir_all(root.join("bin/Debug")).unwrap();
        fs::write(root.join("sample.sln"), "").unwrap();
    }
    fs::write(left.join("obj/Debug/generated.cs"), "old obj\n").unwrap();
    fs::write(right.join("obj/Debug/generated.cs"), "new obj\n").unwrap();
    fs::write(left.join("bin/Debug/generated.cs"), "old bin\n").unwrap();
    fs::write(right.join("bin/Debug/generated.cs"), "new bin\n").unwrap();

    let result = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap()
    }))
    .await
    .unwrap();

    assert_eq!(
        result
            .pointer("/summary/changed_files")
            .and_then(Value::as_u64),
        Some(0)
    );
}

#[tokio::test]
async fn test_text_search_include_directory_matches_descendants() {
    let dir = tempdir().unwrap();
    let src_dir = dir.path().join("src");
    fs::create_dir(&src_dir).unwrap();
    fs::write(src_dir.join("lib.rs"), "include_descendant_needle\n").unwrap();

    let res = text_search::execute(&json!({
        "query": "include_descendant_needle",
        "paths": [dir.path().to_str().unwrap()],
        "includes": ["src"],
        "allow_expensive_fallback": true
    }))
    .await
    .unwrap();

    assert_eq!(res.get("total_returned").and_then(|v| v.as_u64()), Some(1));
}

#[tokio::test]
async fn test_read_file_range_and_snippets_report_truncation_metadata() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("lines.txt");
    fs::write(&path, "alpha\nbeta\ngamma\ndelta\nepsilon").unwrap();

    let read_res = read_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "start_line": 1,
        "end_line": 5,
        "max_lines": 2,
        "include_line_numbers": true
    }))
    .await
    .unwrap();

    assert_eq!(
        read_res.get("content").and_then(|v| v.as_str()),
        Some("1: alpha\n2: beta")
    );
    assert_eq!(
        read_res.get("truncated").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        read_res.get("returned_lines").and_then(|v| v.as_u64()),
        Some(2)
    );
    assert_eq!(
        read_res.get("omitted_lines").and_then(|v| v.as_u64()),
        Some(3)
    );
    assert_eq!(
        read_res.get("next_start_line").and_then(|v| v.as_u64()),
        Some(3)
    );
    assert_eq!(read_res.get("end_line").and_then(|v| v.as_u64()), Some(2));

    let snippets_res = read_snippets::execute(&json!({
        "requests": [{
            "path": path.to_str().unwrap(),
            "start_line": 1,
            "end_line": 5,
            "max_lines": 2
        }]
    }))
    .await
    .unwrap();

    let first_result = snippets_res
        .get("results")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .cloned()
        .unwrap();
    assert_eq!(
        first_result.get("truncated").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        first_result.get("next_start_line").and_then(|v| v.as_u64()),
        Some(3)
    );
}

#[tokio::test]
async fn test_read_snippets_reports_batch_continuations_and_skipped_requests() {
    let dir = tempdir().unwrap();
    let first = dir.path().join("first.txt");
    let second = dir.path().join("second.txt");
    fs::write(&first, "alpha\nbeta\ngamma\ndelta\n").unwrap();
    fs::write(&second, "epsilon\nzeta\neta\ntheta\n").unwrap();

    let result = read_snippets::execute(&json!({
        "requests": [
            {
                "path": first.to_str().unwrap(),
                "start_line": 1,
                "end_line": 4
            },
            {
                "path": second.to_str().unwrap(),
                "start_line": 1,
                "end_line": 4
            }
        ],
        "max_total_bytes": 10
    }))
    .await
    .unwrap();

    assert_eq!(
        result.get("complete").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert!(result.get("has_more").is_none());
    assert_eq!(
        result
            .get("batch_limits")
            .and_then(|v| v.get("max_total_bytes"))
            .and_then(|v| v.as_u64()),
        Some(10)
    );

    let results = result.get("results").and_then(|v| v.as_array()).unwrap();
    assert_eq!(results.len(), 2);

    assert!(results[0].get("status").is_none());
    assert!(results[0].get("encoding").is_none());
    assert!(results[0].get("is_binary").is_none());
    assert_eq!(
        result
            .pointer("/result_defaults/status")
            .and_then(Value::as_str),
        Some("success")
    );
    assert_eq!(
        results[0].get("truncated").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        results[0]
            .get("continuation")
            .and_then(|v| v.get("next_start_line"))
            .and_then(|v| v.as_u64()),
        Some(3)
    );

    assert_eq!(
        results[1].get("status").and_then(|v| v.as_str()),
        Some("skipped")
    );
    assert_eq!(
        results[1].get("reason").and_then(|v| v.as_str()),
        Some("batch_total_byte_limit_reached")
    );

    let continuations = result
        .get("continuations")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(continuations.len(), 2);
}

#[tokio::test]
async fn test_read_snippets_returns_structured_item_errors() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("missing.txt");
    let result = read_snippets::execute(&json!({
        "requests": [{ "path": missing.to_str().unwrap() }]
    }))
    .await
    .unwrap();

    assert_eq!(result.get("complete").and_then(Value::as_bool), Some(false));
    assert_eq!(
        result
            .pointer("/results/0/error/code")
            .and_then(Value::as_str),
        Some("not_found")
    );
    assert!(
        result
            .pointer("/results/0/error/message")
            .and_then(Value::as_str)
            .is_some()
    );
}

#[tokio::test]
async fn test_read_symbol_body_prefers_ast_and_supports_body_only_mode() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    fs::write(
        &path,
        "fn sample() {\n    println!(\"hi\");\n    call();\n}\n\nfn other() {}\n",
    )
    .unwrap();

    let full_res = read_symbol_body::execute(&json!({
        "symbol": "sample",
        "paths": [path.to_str().unwrap()]
    }))
    .await
    .unwrap();

    assert_eq!(
        full_res.get("match_source").and_then(|v| v.as_str()),
        Some("ast")
    );
    assert_eq!(
        full_res.get("confidence").and_then(|v| v.as_str()),
        Some("high")
    );
    assert!(
        full_res
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("fn sample")
    );

    let body_only_res = read_symbol_body::execute(&json!({
        "symbol": "sample",
        "paths": [path.to_str().unwrap()],
        "include_signature": false
    }))
    .await
    .unwrap();

    let body_only_content = body_only_res
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(body_only_content.contains("println!"));
    assert!(!body_only_content.contains("fn sample"));
}

#[tokio::test]
async fn test_project_map_and_fuzzy_find_return_polished_fields() {
    let dir = tempdir().unwrap();
    let nested_dir = dir.path().join("src");
    fs::create_dir_all(&nested_dir).unwrap();
    fs::write(nested_dir.join("main.rs"), "fn main() {}\n").unwrap();

    let project_map_res = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "show_sizes": false
    }))
    .await
    .unwrap();
    let root_dirs = project_map_res
        .get("tree_representation")
        .and_then(|tree| tree.get("."))
        .and_then(|children| children.get("dirs"))
        .and_then(Value::as_array)
        .unwrap();
    let src_entry = root_dirs
        .iter()
        .find(|entry| entry.get("name").and_then(|v| v.as_str()) == Some("src"))
        .unwrap();
    assert!(src_entry.get("type").is_none());
    assert!(src_entry.get("size_bytes").is_none());

    let src_files = project_map_res
        .get("tree_representation")
        .and_then(|tree| tree.get("src"))
        .and_then(|children| children.get("files"))
        .and_then(Value::as_array)
        .unwrap();
    assert!(src_files.iter().any(|entry| {
        entry.get("name").and_then(Value::as_str) == Some("main.rs")
            && entry.get("type").is_none()
            && entry.get("size_bytes").is_none()
    }));

    let sized_project_map = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "show_sizes": true
    }))
    .await
    .unwrap();
    assert!(
        sized_project_map
            .pointer("/tree_representation/src/files/0/size_bytes")
            .and_then(Value::as_u64)
            .is_some_and(|size| size > 0)
    );

    let fuzzy_find_res = fuzzy_find::execute(&json!({
        "pattern": "main",
        "paths": [dir.path().to_str().unwrap()],
        "extensions": ["rs"]
    }))
    .await
    .unwrap();

    let first_match = fuzzy_find_res
        .get("results")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .cloned()
        .unwrap();
    assert_eq!(
        first_match.get("path").and_then(|v| v.as_str()),
        Some("src/main.rs")
    );
    assert!(first_match.get("score").and_then(|v| v.as_i64()).is_some());
    assert!(fuzzy_find_res.get("entries_scanned").is_none());
    assert!(
        fuzzy_find_res
            .get("indexed_candidates_considered")
            .is_none()
    );
    assert!(fuzzy_find_res.get("indexed_at").is_none());
    assert!(fuzzy_find_res.get("warnings").is_none());
    assert!(fuzzy_find_res.get("diagnostics").is_none());

    let verbose_fuzzy = fuzzy_find::execute(&json!({
        "pattern": "main",
        "paths": [dir.path().to_str().unwrap()],
        "verbose": true
    }))
    .await
    .unwrap();
    assert!(
        verbose_fuzzy
            .pointer("/diagnostics/entries_scanned")
            .and_then(|value| value.as_u64())
            .is_some_and(|count| count > 0)
    );
    assert!(
        verbose_fuzzy
            .pointer("/diagnostics/filesystem_roots_walked")
            .and_then(|value| value.as_u64())
            .is_some_and(|count| count > 0)
    );

    fs::write(nested_dir.join("main_test.rs"), "fn main_test() {}\n").unwrap();
    let limited_fuzzy = fuzzy_find::execute(&json!({
        "pattern": "main",
        "paths": [dir.path().to_str().unwrap()],
        "max_results": 1
    }))
    .await
    .unwrap();
    assert_eq!(
        limited_fuzzy
            .get("complete")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    assert!(limited_fuzzy.get("diagnostics").is_some());
}

#[tokio::test]
async fn test_fuzzy_find_refreshes_indexed_metadata_before_returning_results() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("stale-metadata.txt");
    fs::write(&path, "x".repeat(20)).unwrap();
    wait_for_path_index(dir.path()).await;

    fs::write(&path, "y".repeat(41)).unwrap();
    let refreshed = fuzzy_find::execute(&json!({
        "pattern": "stale-metadata",
        "paths": [dir.path().to_str().unwrap()],
        "target_type": "file"
    }))
    .await
    .unwrap();
    assert_eq!(
        refreshed
            .pointer("/results/0/size_bytes")
            .and_then(Value::as_u64),
        Some(41),
        "fuzzy_find result: {refreshed}"
    );

    fs::remove_file(&path).unwrap();
    let deleted = fuzzy_find::execute(&json!({
        "pattern": "stale-metadata",
        "paths": [dir.path().to_str().unwrap()],
        "target_type": "file"
    }))
    .await
    .unwrap();
    assert_eq!(
        deleted.get("total_returned").and_then(Value::as_u64),
        Some(0)
    );
}

#[tokio::test]
async fn test_workspace_stats_keeps_indexed_output_compact_and_relative() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/largest.rs"),
        "fn one() {}\nfn two() {}\n",
    )
    .unwrap();
    wait_for_path_index(dir.path()).await;
    let indexed_records = indexer::visit_indexed_entries_under(dir.path(), |_| true).unwrap_or(0);
    assert!(
        indexed_records > 0,
        "fixture path index should contain records"
    );

    let compact = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        compact.get("search_strategy").and_then(Value::as_str),
        Some("lmdb_metadata")
    );
    assert_eq!(
        compact
            .pointer("/largest_files/0/path")
            .and_then(Value::as_str),
        Some("src/largest.rs")
    );
    assert!(compact.get("line_counted_files").is_none());
    assert!(compact.get("diagnostics").is_none());

    let verbose = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "verbose": true
    }))
    .await
    .unwrap();
    assert_eq!(
        verbose
            .pointer("/diagnostics/line_counting/counted_files")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert!(verbose.get("line_counted_files").is_none());
}

#[tokio::test]
async fn test_include_ignored_is_consistent_across_discovery_tools() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::create_dir_all(dir.path().join("ignored/nested")).unwrap();
    fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
    fs::write(dir.path().join("visible.rs"), "fn visible_symbol() {}\n").unwrap();
    fs::write(
        dir.path().join("ignored/nested/secret.rs"),
        "fn ignored_symbol() {}\nfn call_ignored() { ignored_symbol(); }\n",
    )
    .unwrap();
    wait_for_path_index(dir.path()).await;

    let default_map = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "max_depth": 4
    }))
    .await
    .unwrap();
    assert!(!default_map.to_string().contains("secret.rs"));

    let inclusive_map = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "max_depth": 4,
        "include_ignored": true
    }))
    .await
    .unwrap();
    assert!(inclusive_map.to_string().contains("secret.rs"));
    assert_eq!(
        inclusive_map
            .get("search_strategy")
            .and_then(serde_json::Value::as_str),
        Some("filesystem_walk")
    );

    let direct_map = project_map::execute(&json!({
        "path": dir.path().join("ignored").to_str().unwrap(),
        "max_depth": 4
    }))
    .await
    .unwrap();
    assert!(direct_map.to_string().contains("secret.rs"));

    let direct_fuzzy = fuzzy_find::execute(&json!({
        "pattern": "secret",
        "paths": [dir.path().join("ignored").to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        direct_fuzzy
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );

    let direct_search = text_search::execute(&json!({
        "query": "ignored_symbol",
        "paths": [dir.path().join("ignored").to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert!(
        direct_search
            .get("total_returned")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|count| count > 0)
    );

    let direct_stats = workspace_stats::execute(&json!({
        "path": dir.path().join("ignored").to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        direct_stats
            .get("total_files")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );

    let default_fuzzy = fuzzy_find::execute(&json!({
        "pattern": "secret",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        default_fuzzy
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(0)
    );
    assert!(default_fuzzy.to_string().contains("include_ignored=true"));
    let inclusive_fuzzy = fuzzy_find::execute(&json!({
        "pattern": "secret",
        "paths": [dir.path().to_str().unwrap()],
        "include_ignored": true
    }))
    .await
    .unwrap();
    assert_eq!(
        inclusive_fuzzy
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );

    let default_search = text_search::execute(&json!({
        "query": "ignored_symbol",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        default_search
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(0)
    );
    assert!(default_search.to_string().contains("include_ignored=true"));
    let inclusive_search = text_search::execute(&json!({
        "query": "ignored_symbol",
        "paths": [dir.path().to_str().unwrap()],
        "include_ignored": true
    }))
    .await
    .unwrap();
    assert!(
        inclusive_search
            .get("total_returned")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|count| count > 0)
    );

    let default_definition = find_definition::execute(&json!({
        "symbol": "ignored_symbol",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        default_definition
            .get("total_returned")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert!(
        default_definition
            .to_string()
            .contains("include_ignored=true")
    );
    let inclusive_definition = find_definition::execute(&json!({
        "symbol": "ignored_symbol",
        "paths": [dir.path().to_str().unwrap()],
        "include_ignored": true
    }))
    .await
    .unwrap();
    assert!(
        inclusive_definition
            .get("total_returned")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
    );

    let default_references = find_references::execute(&json!({
        "symbol": "ignored_symbol",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        default_references
            .get("total_returned")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert!(
        default_references
            .to_string()
            .contains("include_ignored=true")
    );
    let inclusive_references = find_references::execute(&json!({
        "symbol": "ignored_symbol",
        "paths": [dir.path().to_str().unwrap()],
        "include_ignored": true
    }))
    .await
    .unwrap();
    assert!(
        inclusive_references
            .get("total_returned")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
    );

    let default_facade = search_workspace::execute(&json!({
        "query": "ignored_symbol",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert!(
        default_facade
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("include_ignored=true"))))
    );
    let inclusive_facade = search_workspace::execute(&json!({
        "query": "ignored_symbol",
        "paths": [dir.path().to_str().unwrap()],
        "include_ignored": true
    }))
    .await
    .unwrap();
    assert!(
        inclusive_facade
            .pointer("/groups/symbol/total_returned")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
    );
    assert!(
        inclusive_facade
            .pointer("/groups/text/total_returned")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
    );

    let default_stats = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        default_stats
            .get("total_files")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert_eq!(
        default_stats
            .pointer("/excluded_files/ignored")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        default_stats
            .pointer("/excluded_files/hidden")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        default_stats
            .pointer("/excluded_files/counts_complete")
            .and_then(Value::as_bool),
        Some(true)
    );
    let inclusive_stats = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "include_ignored": true
    }))
    .await
    .unwrap();
    assert_eq!(
        inclusive_stats
            .get("total_files")
            .and_then(serde_json::Value::as_u64),
        Some(2)
    );
}

#[tokio::test]
async fn test_include_hidden_discovers_dot_paths_without_traversing_vcs_metadata() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
    fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    fs::create_dir_all(dir.path().join(".github/scripts")).unwrap();
    fs::create_dir_all(dir.path().join(".vscode")).unwrap();
    fs::write(dir.path().join("visible.rs"), "fn visible_symbol() {}\n").unwrap();
    fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
    fs::write(dir.path().join(".env.example"), "APP_MODE=test\n").unwrap();
    fs::write(
        dir.path().join(".github/workflows/ci.yml"),
        "name: hidden_ci_token\n",
    )
    .unwrap();
    fs::write(
        dir.path().join(".github/scripts/helper.py"),
        "def hidden_helper():\n    return 1\n\nhidden_helper()\n",
    )
    .unwrap();
    fs::write(
        dir.path().join(".vscode/settings.json"),
        "{\"editor.formatOnSave\": true}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join(".git/objects/vcs_only.rs"),
        "fn vcs_only_token() {}\n",
    )
    .unwrap();
    wait_for_path_index(dir.path()).await;

    let default_map = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "max_depth": 4
    }))
    .await
    .unwrap();
    assert!(!default_map.to_string().contains(".github"));
    assert!(!default_map.to_string().contains(".env.example"));

    let inclusive_map = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "max_depth": 4,
        "include_hidden": true
    }))
    .await
    .unwrap();
    let inclusive_map_text = inclusive_map.to_string();
    assert!(inclusive_map_text.contains(".github"));
    assert!(inclusive_map_text.contains(".env.example"));
    assert!(inclusive_map_text.contains(".vscode"));
    assert!(!inclusive_map_text.contains("vcs_only.rs"));

    let default_fuzzy = fuzzy_find::execute(&json!({
        "pattern": ".env",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        default_fuzzy.get("total_returned").and_then(Value::as_u64),
        Some(0)
    );
    assert!(default_fuzzy.to_string().contains("include_hidden=true"));
    let inclusive_fuzzy = fuzzy_find::execute(&json!({
        "pattern": ".env",
        "paths": [dir.path().to_str().unwrap()],
        "include_hidden": true
    }))
    .await
    .unwrap();
    assert_eq!(
        inclusive_fuzzy
            .get("total_returned")
            .and_then(Value::as_u64),
        Some(1)
    );

    let default_search = text_search::execute(&json!({
        "query": "hidden_ci_token",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        default_search.get("total_returned").and_then(Value::as_u64),
        Some(0)
    );
    assert!(default_search.to_string().contains("include_hidden=true"));
    let inclusive_search = text_search::execute(&json!({
        "query": "hidden_ci_token",
        "paths": [dir.path().to_str().unwrap()],
        "include_hidden": true
    }))
    .await
    .unwrap();
    assert_eq!(
        inclusive_search
            .get("total_returned")
            .and_then(Value::as_u64),
        Some(1)
    );

    let default_definition = find_definition::execute(&json!({
        "symbol": "hidden_helper",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        default_definition
            .get("total_returned")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert!(
        default_definition
            .to_string()
            .contains("include_hidden=true")
    );
    let inclusive_definition = find_definition::execute(&json!({
        "symbol": "hidden_helper",
        "paths": [dir.path().to_str().unwrap()],
        "include_hidden": true
    }))
    .await
    .unwrap();
    assert!(
        inclusive_definition
            .get("total_returned")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
    );

    let default_references = find_references::execute(&json!({
        "symbol": "hidden_helper",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        default_references
            .get("total_returned")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert!(
        default_references
            .to_string()
            .contains("include_hidden=true")
    );
    let inclusive_references = find_references::execute(&json!({
        "symbol": "hidden_helper",
        "paths": [dir.path().to_str().unwrap()],
        "include_hidden": true
    }))
    .await
    .unwrap();
    assert!(
        inclusive_references
            .get("total_returned")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
    );

    let default_stats = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        default_stats.get("total_files").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        default_stats
            .pointer("/excluded_files/hidden")
            .and_then(Value::as_u64),
        Some(5)
    );
    assert_eq!(
        default_stats
            .pointer("/excluded_files/counts_complete")
            .and_then(Value::as_bool),
        Some(true)
    );
    let inclusive_stats = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "include_hidden": true
    }))
    .await
    .unwrap();
    assert_eq!(
        inclusive_stats.get("total_files").and_then(Value::as_u64),
        Some(6)
    );
    assert!(!inclusive_stats.to_string().contains("vcs_only.rs"));

    let vcs_from_root = text_search::execute(&json!({
        "query": "vcs_only_token",
        "paths": [dir.path().to_str().unwrap()],
        "include_hidden": true
    }))
    .await
    .unwrap();
    assert_eq!(
        vcs_from_root.get("total_returned").and_then(Value::as_u64),
        Some(0)
    );
    let vcs_direct = text_search::execute(&json!({
        "query": "vcs_only_token",
        "paths": [dir.path().join(".git").to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        vcs_direct.get("total_returned").and_then(Value::as_u64),
        Some(1)
    );

    let default_facade = search_workspace::execute(&json!({
        "query": "hidden_helper",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert!(
        default_facade
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("include_hidden=true"))))
    );
    let inclusive_facade = search_workspace::execute(&json!({
        "query": "hidden_helper",
        "paths": [dir.path().to_str().unwrap()],
        "include_hidden": true
    }))
    .await
    .unwrap();
    assert!(
        inclusive_facade
            .pointer("/groups/symbol/total_returned")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
    );
    assert!(
        inclusive_facade
            .pointer("/groups/text/total_returned")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
    );
}

#[tokio::test]
async fn test_warm_content_index_requires_include_ignored() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::create_dir_all(dir.path().join("scope/ignored/nested")).unwrap();
    fs::write(dir.path().join(".gitignore"), "scope/ignored/\n").unwrap();
    fs::write(dir.path().join("scope/visible.rs"), "fn visible() {}\n").unwrap();
    fs::write(
        dir.path().join("scope/ignored/nested/secret.rs"),
        "fn ignored_symbol() {}\n",
    )
    .unwrap();
    wait_for_path_index(dir.path()).await;

    let rejected = warm_content_index::execute(&json!({
        "paths": [dir.path().join("scope/ignored").to_str().unwrap()],
        "wait_ms": 1_000
    }))
    .await
    .unwrap();
    assert_eq!(
        rejected.get("outcome").and_then(serde_json::Value::as_str),
        Some("nothing_to_warm")
    );
    assert_eq!(
        rejected
            .pointer("/statuses/0/status")
            .and_then(serde_json::Value::as_str),
        Some("ignored_by_ignore_rules")
    );

    let included = warm_content_index::execute(&json!({
        "paths": [dir.path().join("scope/ignored").to_str().unwrap()],
        "include_ignored": true,
        "wait_ms": 5_000
    }))
    .await
    .unwrap();
    assert_eq!(
        included.get("outcome").and_then(serde_json::Value::as_str),
        Some("ready")
    );
    assert_eq!(
        included
            .pointer("/statuses/0/ready")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );

    let parent = warm_content_index::execute(&json!({
        "paths": [dir.path().join("scope").to_str().unwrap()],
        "wait_ms": 5_000
    }))
    .await
    .unwrap();
    assert_eq!(
        parent.get("outcome").and_then(serde_json::Value::as_str),
        Some("ready")
    );

    let default_search = text_search::execute(&json!({
        "query": "ignored_symbol",
        "paths": [dir.path().join("scope").to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        default_search
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(0)
    );

    let inclusive_search = text_search::execute(&json!({
        "query": "ignored_symbol",
        "paths": [dir.path().join("scope").to_str().unwrap()],
        "include_ignored": true
    }))
    .await
    .unwrap();
    assert_eq!(
        inclusive_search
            .get("total_returned")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
}

#[tokio::test]
async fn test_index_backed_tools_see_files_created_after_scan() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/config.rs"),
        "fn existing_config() {}\n",
    )
    .unwrap();

    wait_for_path_index(dir.path()).await;

    let tools_dir = dir.path().join("tools");
    fs::create_dir_all(&tools_dir).unwrap();
    let late_file = tools_dir.join("config_v2.py");
    fs::write(&late_file, "def late_helper():\n    return 1\n").unwrap();

    let map = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        map.get("search_strategy").and_then(|value| value.as_str()),
        Some("filesystem_walk")
    );
    assert!(
        map.get("tree_representation")
            .and_then(|tree| tree.get("."))
            .and_then(|children| children.get("dirs"))
            .and_then(Value::as_array)
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry.get("name").and_then(|value| value.as_str()) == Some("tools")
            }))
    );

    let fuzzy = fuzzy_find::execute(&json!({
        "pattern": "config",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        fuzzy
            .get("search_strategy")
            .and_then(|value| value.as_str()),
        Some("mixed")
    );
    assert!(
        fuzzy
            .get("results")
            .and_then(|value| value.as_array())
            .is_some_and(|results| results.iter().any(|entry| {
                entry.get("path").and_then(|value| value.as_str()) == Some("tools/config_v2.py")
            }))
    );

    let references = find_references::execute(&json!({
        "symbol": "late_helper",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        references
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(1)
    );

    let definitions = find_definition::execute(&json!({
        "symbol": "late_helper",
        "paths": [dir.path().to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        definitions
            .get("total_returned")
            .and_then(|value| value.as_u64()),
        Some(1)
    );

    let body = read_symbol_body::execute(&json!({
        "symbol": "late_helper",
        "paths": [dir.path().to_str().unwrap()],
        "language": "python"
    }))
    .await
    .unwrap();
    assert!(
        body.get("content")
            .and_then(|value| value.as_str())
            .is_some_and(|content| content.contains("return 1"))
    );
}

#[tokio::test]
async fn test_project_map_reports_output_child_truncation() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    fs::write(dir.path().join("b.txt"), "b\n").unwrap();

    let result = project_map::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "max_children_per_dir": 1
    }))
    .await
    .unwrap();

    assert_eq!(
        result.get("limit_reached").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        result.get("limit_reason").and_then(|v| v.as_str()),
        Some("max_children_per_dir")
    );
    assert_eq!(
        result
            .pointer("/diagnostics/truncated_directory_count")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[tokio::test]
async fn test_compare_directories_reports_added_deleted_modified_and_ignored() {
    let dir = tempdir().unwrap();
    let left = dir.path().join("left");
    let right = dir.path().join("right");
    fs::create_dir_all(left.join("src")).unwrap();
    fs::create_dir_all(right.join("src")).unwrap();
    fs::create_dir_all(right.join("target")).unwrap();

    fs::write(
        left.join("src/common.rs"),
        "fn shared() {\n    println!(\"old\");\n}\n",
    )
    .unwrap();
    fs::write(
        right.join("src/common.rs"),
        "fn shared() {\n    println!(\"new\");\n}\n",
    )
    .unwrap();
    fs::write(left.join("src/removed.rs"), "fn removed() {}\n").unwrap();
    fs::write(right.join("src/added.rs"), "fn added() {}\n").unwrap();
    fs::write(right.join("target/generated.rs"), "ignored\n").unwrap();

    let res = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap()
    }))
    .await
    .unwrap();

    assert_eq!(res.get("complete").and_then(|v| v.as_bool()), Some(true));
    assert!(res.get("partial").is_none());
    assert_eq!(
        res.pointer("/summary/complete").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(res.pointer("/summary/partial").is_none());
    assert!(
        res.get("details_complete")
            .and_then(|v| v.as_object())
            .is_some_and(|details| details.values().all(|value| value.as_bool() == Some(true)))
    );
    assert!(res.get("details_truncated").is_none());

    assert_eq!(
        res.pointer("/summary/added_files").and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        res.pointer("/summary/deleted_files")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        res.pointer("/summary/modified_text_files")
            .and_then(|v| v.as_u64()),
        Some(1)
    );

    let added = res.get("added_files").and_then(|v| v.as_array()).unwrap();
    assert!(
        added
            .iter()
            .any(|value| value.as_str() == Some("src/added.rs"))
    );
    assert!(
        !added
            .iter()
            .any(|value| value.as_str() == Some("target/generated.rs"))
    );

    let modified = res
        .get("modified_files")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .unwrap();
    assert_eq!(
        modified.get("path").and_then(|v| v.as_str()),
        Some("src/common.rs")
    );
    assert!(modified.get("left_size_bytes").is_some());
    assert!(modified.get("right_size_bytes").is_some());
    assert!(modified.get("left_size").is_none());
    assert!(modified.get("right_size").is_none());
    assert!(modified.get("unified_diff").is_none());

    let with_diff = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap(),
        "include_content_diff": true
    }))
    .await
    .unwrap();
    assert!(
        with_diff
            .pointer("/modified_files/0/unified_diff")
            .and_then(|v| v.as_str())
            .is_some_and(|diff| diff.contains("println!(\"new\")"))
    );
}

#[tokio::test]
async fn test_compare_directories_is_exposed_and_dispatchable() {
    let listed_tools = tools::list_tools();
    assert!(listed_tools.iter().any(|tool| {
        tool.get("name").and_then(|value| value.as_str()) == Some("compare_directories")
    }));

    let dir = tempdir().unwrap();
    let left = dir.path().join("left");
    let right = dir.path().join("right");
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();

    let result = tools::call_tool(json!({
        "name": "compare_directories",
        "arguments": {
            "left_path": left.to_str().unwrap(),
            "right_path": right.to_str().unwrap()
        }
    }))
    .await
    .unwrap();
    assert!(result.get("content").is_some());
}

#[tokio::test]
async fn test_compare_directories_summary_truncation_binary_large_and_rename() {
    let dir = tempdir().unwrap();
    let left = dir.path().join("left-rich");
    let right = dir.path().join("right-rich");
    fs::create_dir_all(left.join("src/auth")).unwrap();
    fs::create_dir_all(right.join("src/auth")).unwrap();
    fs::create_dir_all(left.join("docs")).unwrap();
    fs::create_dir_all(right.join("docs")).unwrap();

    fs::write(left.join("docs/old.md"), "same rename content\n").unwrap();
    fs::write(right.join("docs/new.md"), "same rename content\n").unwrap();
    fs::write(
        left.join("src/auth/login.rs"),
        "fn login() {\n    old();\n}\n",
    )
    .unwrap();
    fs::write(
        right.join("src/auth/login.rs"),
        "fn login() {\n    new_call();\n}\n",
    )
    .unwrap();
    fs::write(left.join("src/blob.bin"), [0x01u8, 0x00, 0x02]).unwrap();
    fs::write(right.join("src/blob.bin"), [0x01u8, 0x00, 0x03]).unwrap();
    fs::write(left.join("src/big.txt"), "a".repeat(64)).unwrap();
    fs::write(right.join("src/big.txt"), "b".repeat(64)).unwrap();

    let res = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap(),
        "max_file_size": 32,
        "max_diff_bytes": 12,
        "summary_only": true
    }))
    .await
    .unwrap();

    assert_eq!(
        res.pointer("/summary/renamed_files")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        res.pointer("/summary/modified_binary_files")
            .and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        res.pointer("/summary/skipped_files")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        res.get("skipped_files")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("path"))
            .and_then(|v| v.as_str()),
        Some("src/big.txt")
    );
    assert_eq!(
        res.pointer("/summary/diff_bytes_returned")
            .and_then(|v| v.as_u64()),
        Some(0)
    );

    let modified = res
        .get("modified_files")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .unwrap();
    assert!(modified.get("unified_diff").is_none());
    assert_eq!(
        modified.get("risk_category").and_then(|v| v.as_str()),
        Some("auth/security")
    );
    assert!(
        res.get("top_changed_directories")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .any(|item| item.get("name").and_then(|v| v.as_str()) == Some("src"))
    );
    assert!(
        res.get("extensions_summary")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .any(|item| item.get("name").and_then(|v| v.as_str()) == Some(".rs"))
    );
    assert!(
        res.get("risk_hints")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .any(|item| item.get("category").and_then(|v| v.as_str()) == Some("auth/security"))
    );
}

#[tokio::test]
async fn test_compare_directories_markdown_output() {
    let dir = tempdir().unwrap();
    let left = dir.path().join("left-md");
    let right = dir.path().join("right-md");
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();
    fs::write(right.join("added.rs"), "fn added() {}\n").unwrap();

    let result = tools::call_tool(json!({
        "name": "compare_directories",
        "arguments": {
            "left_path": left.to_str().unwrap(),
            "right_path": right.to_str().unwrap(),
            "output_format": "markdown"
        }
    }))
    .await
    .unwrap();

    let text = result
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(text.contains("# Directory Compare Report"));
    assert!(text.contains("`added.rs`"));
}

#[tokio::test]
async fn test_compare_directories_groups_root_level_files_under_dot() {
    let dir = tempdir().unwrap();
    let left = dir.path().join("left-root");
    let right = dir.path().join("right-root");
    fs::create_dir_all(left.join("sub")).unwrap();
    fs::create_dir_all(right.join("sub")).unwrap();
    fs::write(left.join("root.rs"), "fn a() {}\n").unwrap();
    fs::write(right.join("root.rs"), "fn b() {}\n").unwrap();
    fs::write(left.join("sub/nested.rs"), "fn c() {}\n").unwrap();
    fs::write(right.join("sub/nested.rs"), "fn d() {}\n").unwrap();

    let res = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap()
    }))
    .await
    .unwrap();

    let names = res
        .get("top_changed_directories")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .filter_map(|item| item.get("name").and_then(|v| v.as_str()))
        .collect::<Vec<_>>();
    assert!(names.contains(&"."), "names: {names:?}");
    assert!(names.contains(&"sub"), "names: {names:?}");
    assert!(!names.contains(&"root.rs"), "names: {names:?}");
    assert_eq!(
        res.pointer("/changed_files_by_directory/.")
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
        Some(1)
    );
}

#[tokio::test]
async fn test_compare_directories_summary_only_detects_same_metadata_content_change() {
    let dir = tempdir().unwrap();
    let left = dir.path().join("left-summary-metadata");
    let right = dir.path().join("right-summary-metadata");
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();

    let left_file = left.join("same.txt");
    let right_file = right.join("same.txt");
    fs::write(&left_file, "AAAA").unwrap();
    fs::write(&right_file, "BBBB").unwrap();

    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_380_800);
    let times = fs::FileTimes::new().set_modified(modified);
    fs::File::options()
        .write(true)
        .open(&left_file)
        .unwrap()
        .set_times(times)
        .unwrap();
    fs::File::options()
        .write(true)
        .open(&right_file)
        .unwrap()
        .set_times(times)
        .unwrap();

    let res = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap(),
        "summary_only": true,
        "detect_renames": false
    }))
    .await
    .unwrap();

    assert_eq!(
        res.pointer("/summary/changed_files")
            .and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        res.pointer("/summary/unchanged_files")
            .and_then(|value| value.as_u64()),
        Some(0)
    );
    assert_eq!(
        res.get("modified_files")
            .and_then(|value| value.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("path"))
            .and_then(|value| value.as_str()),
        Some("same.txt")
    );
}

#[tokio::test]
async fn test_compare_directories_edge_options() {
    let dir = tempdir().unwrap();
    let left = dir.path().join("left-options");
    let right = dir.path().join("right-options");
    fs::create_dir_all(left.join("src")).unwrap();
    fs::create_dir_all(right.join("src")).unwrap();
    fs::create_dir_all(left.join("docs")).unwrap();
    fs::create_dir_all(right.join("docs")).unwrap();

    fs::write(left.join("docs/a.md"), "same\n").unwrap();
    fs::write(right.join("docs/b.md"), "same\n").unwrap();
    fs::write(left.join("src/main.rs"), "fn main() {\n    old();\n}\n").unwrap();
    fs::write(right.join("src/main.rs"), "fn main() {\n    new();\n}\n").unwrap();
    fs::write(right.join("src/ignored.rs"), "fn ignored() {}\n").unwrap();

    let no_rename = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap(),
        "detect_renames": false,
        "include_content_diff": false,
        "excludes": ["src/ignored.rs"]
    }))
    .await
    .unwrap();
    assert_eq!(
        no_rename
            .pointer("/summary/renamed_files")
            .and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        no_rename
            .pointer("/summary/added_files")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        no_rename
            .pointer("/summary/deleted_files")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    let modified = no_rename
        .get("modified_files")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .unwrap();
    assert!(modified.get("unified_diff").is_none());
    assert_eq!(
        modified
            .get("affected_symbols")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|v| v.as_str()),
        Some("main")
    );

    let include_only_docs = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap(),
        "includes": ["docs/**"]
    }))
    .await
    .unwrap();
    assert_eq!(
        include_only_docs
            .pointer("/summary/renamed_files")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        include_only_docs
            .pointer("/summary/modified_text_files")
            .and_then(|v| v.as_u64()),
        Some(0)
    );

    let invalid = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap(),
        "output_format": "html"
    }))
    .await;
    assert!(
        invalid
            .unwrap_err()
            .to_string()
            .contains("Unsupported output_format")
    );
}

#[tokio::test]
async fn test_compare_directories_truncates_diff_and_detects_fuzzy_rename() {
    let dir = tempdir().unwrap();
    let left = dir.path().join("left-fuzzy");
    let right = dir.path().join("right-fuzzy");
    fs::create_dir_all(left.join("src")).unwrap();
    fs::create_dir_all(right.join("src")).unwrap();

    fs::write(
        left.join("src/old_name.rs"),
        "fn same() {\n    alpha();\n    beta();\n}\n",
    )
    .unwrap();
    fs::write(
        right.join("src/new_name.rs"),
        "fn same() {\n    alpha();\n    gamma();\n}\n",
    )
    .unwrap();
    fs::write(
        left.join("src/changed.rs"),
        "fn changed() {\n    old_line_one();\n    old_line_two();\n}\n",
    )
    .unwrap();
    fs::write(
        right.join("src/changed.rs"),
        "fn changed() {\n    new_line_one();\n    new_line_two();\n}\n",
    )
    .unwrap();

    let res = compare_directories::execute(&json!({
        "left_path": left.to_str().unwrap(),
        "right_path": right.to_str().unwrap(),
        "include_content_diff": true,
        "rename_similarity_threshold": 0.5,
        "max_diff_bytes": 24
    }))
    .await
    .unwrap();

    assert_eq!(
        res.pointer("/summary/renamed_files")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    let rename = res
        .get("renamed_files")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .unwrap();
    assert_eq!(
        rename
            .get("modified_after_rename")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    let modified = res
        .get("modified_files")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .unwrap();
    assert_eq!(
        modified.get("diff_truncated").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        modified
            .get("affected_symbols")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("changed"))
    );
}

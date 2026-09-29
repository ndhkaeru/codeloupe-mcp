use codeloupe_mcp::tools::{
    convert_file_format, create_file, delete_file, edit_file, edit_files, file_hash, list_history,
    read_file, undo_change,
};
use codeloupe_mcp::{indexer, workspace_control};
use serde_json::{Value, json};
use std::path::Path;
use tempfile::tempdir;

fn register_write_workspace(path: &Path) {
    indexer::ensure_workspace_index(path.to_path_buf(), "write_tool_test".to_string());
    workspace_control::register_configured_workspace(path.to_path_buf(), "write_tool_test", true);
}

fn encode_utf16_with_bom(content: &str, encoding: &str) -> Vec<u8> {
    let mut bytes = match encoding {
        "UTF-16LE" => vec![0xFF, 0xFE],
        "UTF-16BE" => vec![0xFE, 0xFF],
        _ => panic!("unsupported test encoding: {encoding}"),
    };
    for unit in content.encode_utf16() {
        let encoded = if encoding == "UTF-16LE" {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        bytes.extend_from_slice(&encoded);
    }
    bytes
}

fn assert_utf16_file(path: &Path, encoding: &str, expected_content: &str) {
    let bytes = std::fs::read(path).unwrap();
    let expected_bom = if encoding == "UTF-16LE" {
        [0xFF, 0xFE]
    } else {
        [0xFE, 0xFF]
    };
    assert!(bytes.starts_with(&expected_bom));
    let (content, detected_encoding) = read_file::decode_fuzzy(&bytes);
    assert_eq!(detected_encoding, encoding);
    assert_eq!(content, expected_content);
}

#[tokio::test]
async fn test_create_file_create_and_overwrite_flow() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let path = dir.path().join("nested").join("note.txt");

    let create_res = create_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "content": "hello"
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
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");

    let exists_res = create_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "content": "second"
    }))
    .await
    .unwrap();

    assert_eq!(
        exists_res.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        exists_res.get("error_code").and_then(|v| v.as_str()),
        Some("already_exists")
    );
    assert!(exists_res.get("reason").and_then(|v| v.as_str()).is_some());

    let overwrite_res = create_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "content": "second",
        "overwrite": true
    }))
    .await
    .unwrap();

    assert_eq!(
        overwrite_res.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        overwrite_res.get("overwritten").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
}

#[tokio::test]
async fn test_edit_file_find_replace_and_missing_file() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let path = dir.path().join("edit.txt");
    std::fs::write(&path, "alpha beta beta").unwrap();

    let replace_res = edit_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "mode": "find_replace",
        "find": "beta",
        "replace": "B",
        "replace_all": true,
        "expected_replacements": 2
    }))
    .await
    .unwrap();

    assert_eq!(
        replace_res.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        replace_res.get("replacements").and_then(|v| v.as_u64()),
        Some(2)
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha B B");

    let no_match_res = edit_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "mode": "find_replace",
        "find": "gamma",
        "replace": "X"
    }))
    .await
    .unwrap();

    assert_eq!(
        no_match_res.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        no_match_res.get("error_code").and_then(|v| v.as_str()),
        Some("no_match")
    );
    assert!(
        no_match_res
            .get("reason")
            .and_then(|v| v.as_str())
            .is_some()
    );

    let missing_path = dir.path().join("missing.txt");
    let missing_res = edit_file::execute(&json!({
        "path": missing_path.to_str().unwrap(),
        "mode": "replace",
        "content": "new"
    }))
    .await
    .unwrap();

    assert_eq!(
        missing_res.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        missing_res.get("error_code").and_then(|v| v.as_str()),
        Some("file_not_found")
    );
    assert!(missing_res.get("reason").and_then(|v| v.as_str()).is_some());

    let create_missing_res = edit_file::execute(&json!({
        "path": missing_path.to_str().unwrap(),
        "mode": "replace",
        "content": "new",
        "create_if_missing": true
    }))
    .await
    .unwrap();

    assert_eq!(
        create_missing_res.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        create_missing_res
            .get("file_created")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(std::fs::read_to_string(&missing_path).unwrap(), "new");
}

#[tokio::test]
async fn test_edit_file_normalizes_fragments_to_existing_line_endings() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let replace_path = dir.path().join("replace-crlf.txt");
    std::fs::write(&replace_path, b"alpha\r\nbeta\r\ngamma\r\n").unwrap();

    let replace_result = edit_file::execute(&json!({
        "path": replace_path.to_str().unwrap(),
        "mode": "find_replace",
        "find": "alpha\nbeta",
        "replace": "AB\nCD"
    }))
    .await
    .unwrap();

    assert_eq!(
        replace_result
            .get("line_endings_normalized")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        std::fs::read(&replace_path).unwrap(),
        b"AB\r\nCD\r\ngamma\r\n"
    );

    let append_path = dir.path().join("append-crlf.txt");
    std::fs::write(&append_path, b"alpha\r\nbeta\r\n").unwrap();
    let append_result = edit_file::execute(&json!({
        "path": append_path.to_str().unwrap(),
        "mode": "append",
        "content": "gamma\ndelta\n"
    }))
    .await
    .unwrap();
    assert_eq!(
        append_result
            .get("line_endings_normalized")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        std::fs::read(&append_path).unwrap(),
        b"alpha\r\nbeta\r\ngamma\r\ndelta\r\n"
    );

    let prepend_result = edit_file::execute(&json!({
        "path": append_path.to_str().unwrap(),
        "mode": "prepend",
        "content": "zero\none\n"
    }))
    .await
    .unwrap();
    assert_eq!(
        prepend_result
            .get("line_endings_normalized")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        std::fs::read(&append_path).unwrap(),
        b"zero\r\none\r\nalpha\r\nbeta\r\ngamma\r\ndelta\r\n"
    );

    let mixed_path = dir.path().join("mixed.txt");
    std::fs::write(&mixed_path, b"alpha\r\nbeta\ngamma").unwrap();
    let mixed_result = edit_file::execute(&json!({
        "path": mixed_path.to_str().unwrap(),
        "mode": "append",
        "content": "\ndelta\n"
    }))
    .await
    .unwrap();
    assert_eq!(
        mixed_result
            .get("mixed_line_endings")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        mixed_result
            .get("line_endings_normalized")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    assert_eq!(
        std::fs::read(&mixed_path).unwrap(),
        b"alpha\r\nbeta\ngamma\ndelta\n"
    );
}

#[tokio::test]
async fn test_edit_file_preserves_utf16_encoding_in_every_mode() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let cases = [
        ("replace", json!({"content": "replacement"}), "replacement"),
        ("append", json!({"content": "-tail"}), "base-tail"),
        ("prepend", json!({"content": "head-"}), "head-base"),
        (
            "find_replace",
            json!({"find": "base", "replace": "next"}),
            "next",
        ),
    ];

    for encoding in ["UTF-16LE", "UTF-16BE"] {
        for (mode, extra, expected_content) in &cases {
            let path = dir.path().join(format!("{encoding}-{mode}.txt"));
            std::fs::write(&path, encode_utf16_with_bom("base", encoding)).unwrap();
            let mut arguments = json!({
                "path": path.to_str().unwrap(),
                "mode": mode
            });
            arguments
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());

            let result = edit_file::execute(&arguments).await.unwrap();
            assert_eq!(result.get("success").and_then(Value::as_bool), Some(true));
            assert_eq!(
                result.get("previous_encoding").and_then(Value::as_str),
                Some(encoding)
            );
            assert_eq!(
                result.get("target_encoding").and_then(Value::as_str),
                Some(encoding)
            );
            assert_eq!(
                result.get("encoding_changed").and_then(Value::as_bool),
                Some(false)
            );
            assert_utf16_file(&path, encoding, expected_content);
        }
    }
}

#[tokio::test]
async fn test_convert_file_format_preserves_utf16_when_only_line_endings_change() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());

    for encoding in ["UTF-16LE", "UTF-16BE"] {
        let path = dir.path().join(format!("convert-{encoding}.txt"));
        std::fs::write(&path, encode_utf16_with_bom("alpha\nbeta\n", encoding)).unwrap();

        let result = convert_file_format::execute(&json!({
            "path": path.to_str().unwrap(),
            "target_line_ending": "crlf"
        }))
        .await
        .unwrap();

        assert_eq!(result.get("success").and_then(Value::as_bool), Some(true));
        assert_eq!(
            result.get("previous_encoding").and_then(Value::as_str),
            Some(encoding)
        );
        assert_eq!(
            result.get("target_encoding").and_then(Value::as_str),
            Some(encoding)
        );
        assert_eq!(
            result.get("encoding_changed").and_then(Value::as_bool),
            Some(false)
        );
        assert_utf16_file(&path, encoding, "alpha\r\nbeta\r\n");
    }
}

#[tokio::test]
async fn test_write_tools_report_explicit_encoding_changes() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let edit_path = dir.path().join("edit-encoding.txt");
    std::fs::write(&edit_path, encode_utf16_with_bom("before", "UTF-16LE")).unwrap();

    let edit_result = edit_file::execute(&json!({
        "path": edit_path.to_str().unwrap(),
        "mode": "replace",
        "content": "after",
        "target_encoding": "UTF-8"
    }))
    .await
    .unwrap();
    assert_eq!(
        edit_result.get("encoding_changed").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(std::fs::read(&edit_path).unwrap(), b"after");

    let convert_path = dir.path().join("convert-encoding.txt");
    std::fs::write(&convert_path, b"plain").unwrap();
    let convert_result = convert_file_format::execute(&json!({
        "path": convert_path.to_str().unwrap(),
        "target_encoding": "UTF-16BE"
    }))
    .await
    .unwrap();
    assert_eq!(
        convert_result
            .get("encoding_changed")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_utf16_file(&convert_path, "UTF-16BE", "plain");
}

#[tokio::test]
async fn test_edit_files_preflights_and_applies_multiple_files() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let first = dir.path().join("first.rs");
    let second = dir.path().join("second.rs");
    std::fs::write(&first, "fn alpha() { let value = 1; }\n").unwrap();
    std::fs::write(&second, "fn beta() { let value = 2; }\n").unwrap();

    let result = edit_files::execute(&json!({
        "files": [
            {
                "path": first.to_str().unwrap(),
                "edits": [
                    {"find": "alpha", "replace": "gamma", "expected_replacements": 1},
                    {"find": "value = 1", "replace": "value = 10", "expected_replacements": 1}
                ]
            },
            {
                "path": second.to_str().unwrap(),
                "edits": [
                    {"find": "beta", "replace": "delta", "expected_replacements": 1}
                ]
            }
        ]
    }))
    .await
    .unwrap();

    assert_eq!(result.get("success").and_then(Value::as_bool), Some(true));
    assert_eq!(result.get("files_changed").and_then(Value::as_u64), Some(2));
    assert_eq!(
        std::fs::read_to_string(&first).unwrap(),
        "fn gamma() { let value = 10; }\n"
    );
    assert_eq!(
        std::fs::read_to_string(&second).unwrap(),
        "fn delta() { let value = 2; }\n"
    );
}

#[tokio::test]
async fn test_edit_files_preflight_failure_writes_nothing() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let first = dir.path().join("first.txt");
    let second = dir.path().join("second.txt");
    std::fs::write(&first, "alpha\n").unwrap();
    std::fs::write(&second, "beta\n").unwrap();

    let result = edit_files::execute(&json!({
        "files": [
            {
                "path": first.to_str().unwrap(),
                "edits": [
                    {"find": "alpha", "replace": "changed", "expected_replacements": 1}
                ]
            },
            {
                "path": second.to_str().unwrap(),
                "edits": [
                    {"find": "missing", "replace": "changed", "expected_replacements": 1}
                ]
            }
        ]
    }))
    .await
    .unwrap();

    assert_eq!(result.get("success").and_then(Value::as_bool), Some(false));
    assert_eq!(
        result.get("error_code").and_then(Value::as_str),
        Some("replacement_mismatch")
    );
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "alpha\n");
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "beta\n");
}

#[tokio::test]
async fn test_edit_files_rejects_new_syntax_errors_before_writing() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let path = dir.path().join("valid.rs");
    let original = "fn main() { println!(\"ok\"); }\n";
    std::fs::write(&path, original).unwrap();

    let result = edit_files::execute(&json!({
        "files": [{
            "path": path.to_str().unwrap(),
            "edits": [{
                "find": "println!(\"ok\"); }",
                "replace": "println!(\"broken\");",
                "expected_replacements": 1
            }]
        }]
    }))
    .await
    .unwrap();

    assert_eq!(result.get("success").and_then(Value::as_bool), Some(false));
    assert_eq!(
        result.get("error_code").and_then(Value::as_str),
        Some("syntax_validation_failed")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

#[tokio::test]
async fn test_history_tools_list_undo_and_detect_conflicts() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let created_path = dir.path().join("history-created.txt");
    let create_result = create_file::execute(&json!({
        "path": created_path.to_str().unwrap(),
        "content": "secret-history-payload"
    }))
    .await
    .unwrap();
    let create_entry = create_result
        .get("history_entry_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let history = list_history::execute(&json!({"limit": 100})).await.unwrap();
    assert!(
        history
            .get("entries")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .any(|entry| entry.get("entry_id").and_then(Value::as_str) == Some(&create_entry))
    );
    assert!(!history.to_string().contains("secret-history-payload"));

    let undo_result = undo_change::execute(&json!({"entry_id": create_entry}))
        .await
        .unwrap();
    assert_eq!(
        undo_result.get("success").and_then(Value::as_bool),
        Some(true)
    );
    assert!(!created_path.exists());

    let conflict_path = dir.path().join("conflict.txt");
    std::fs::write(&conflict_path, "before").unwrap();
    let edit_result = edit_file::execute(&json!({
        "path": conflict_path.to_str().unwrap(),
        "mode": "replace",
        "content": "after"
    }))
    .await
    .unwrap();
    let edit_entry = edit_result
        .get("history_entry_id")
        .and_then(Value::as_str)
        .unwrap();
    std::fs::write(&conflict_path, "external").unwrap();

    let conflict = undo_change::execute(&json!({"entry_id": edit_entry}))
        .await
        .unwrap();
    assert_eq!(
        conflict.get("success").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        conflict.get("error_code").and_then(Value::as_str),
        Some("history_conflict")
    );
    assert_eq!(std::fs::read_to_string(&conflict_path).unwrap(), "external");
}

#[tokio::test]
async fn test_edit_files_rejects_binary_and_treats_mixed_line_ending_noop_as_unchanged() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let binary_path = dir.path().join("sample.bin");
    std::fs::write(&binary_path, [0x01u8, 0x00, 0x02]).unwrap();

    let binary = edit_files::execute(&json!({
        "files": [{
            "path": binary_path.to_str().unwrap(),
            "edits": [{"find": "missing", "replace": "value", "expected_replacements": 0}]
        }]
    }))
    .await
    .unwrap();
    assert_eq!(binary.get("success").and_then(Value::as_bool), Some(false));
    assert_eq!(
        binary.get("error_code").and_then(Value::as_str),
        Some("binary_file")
    );

    let mixed_path = dir.path().join("mixed.txt");
    let mixed_content = b"alpha\r\nbeta\ngamma\r\n";
    std::fs::write(&mixed_path, mixed_content).unwrap();
    let no_op = edit_files::execute(&json!({
        "files": [{
            "path": mixed_path.to_str().unwrap(),
            "edits": [{"find": "missing", "replace": "value", "expected_replacements": 0}]
        }]
    }))
    .await
    .unwrap();
    assert_eq!(no_op.get("success").and_then(Value::as_bool), Some(true));
    assert_eq!(no_op.get("files_changed").and_then(Value::as_u64), Some(0));
    assert_eq!(std::fs::read(&mixed_path).unwrap(), mixed_content);
}

#[tokio::test]
async fn test_undo_change_matches_file_contents_when_snapshot_metadata_differs() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let path = dir.path().join("mixed-undo.txt");
    let original = "alpha\r\nbeta\ngamma\r\n";
    std::fs::write(&path, original).unwrap();

    let edit = edit_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "mode": "find_replace",
        "find": "alpha",
        "replace": "delta",
        "expected_replacements": 1
    }))
    .await
    .unwrap();
    let entry_id = edit
        .get("history_entry_id")
        .and_then(Value::as_str)
        .unwrap();
    let undo = undo_change::execute(&json!({"entry_id": entry_id}))
        .await
        .unwrap();

    assert_eq!(undo.get("success").and_then(Value::as_bool), Some(true));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

#[tokio::test]
async fn test_delete_file_success_missing_and_directory_case() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let file_path = dir.path().join("to-delete.txt");
    std::fs::write(&file_path, "delete me").unwrap();

    let delete_ok = delete_file::execute(&json!({
        "path": file_path.to_str().unwrap()
    }))
    .await
    .unwrap();

    assert_eq!(
        delete_ok.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        delete_ok.get("deleted").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(!file_path.exists());

    let delete_missing = delete_file::execute(&json!({
        "path": file_path.to_str().unwrap()
    }))
    .await
    .unwrap();

    assert_eq!(
        delete_missing.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        delete_missing.get("error_code").and_then(|v| v.as_str()),
        Some("file_not_found")
    );
    assert!(
        delete_missing
            .get("reason")
            .and_then(|v| v.as_str())
            .is_some()
    );

    let delete_missing_ok = delete_file::execute(&json!({
        "path": file_path.to_str().unwrap(),
        "missing_ok": true
    }))
    .await
    .unwrap();

    assert_eq!(
        delete_missing_ok.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        delete_missing_ok.get("deleted").and_then(|v| v.as_bool()),
        Some(false)
    );

    let dir_path = dir.path().join("folder");
    std::fs::create_dir_all(&dir_path).unwrap();
    let delete_dir = delete_file::execute(&json!({
        "path": dir_path.to_str().unwrap()
    }))
    .await
    .unwrap();

    assert_eq!(
        delete_dir.get("success").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        delete_dir.get("error_code").and_then(|v| v.as_str()),
        Some("path_is_directory")
    );
    assert!(delete_dir.get("reason").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn test_expected_hash_rejects_stale_single_file_edits_and_deletes() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let path = dir.path().join("precondition.txt");
    std::fs::write(&path, "alpha").unwrap();

    let invalid_hash = edit_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "mode": "replace",
        "content": "must not be written",
        "expected_hash": "not-a-sha256"
    }))
    .await
    .unwrap();
    assert_eq!(
        invalid_hash.get("error_code").and_then(Value::as_str),
        Some("invalid_hash")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha");

    let original_hash = file_hash::execute(&json!({ "path": path.to_str().unwrap() }))
        .await
        .unwrap()["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(&path, "changed elsewhere").unwrap();

    let stale_edit = edit_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "mode": "replace",
        "content": "agent edit",
        "expected_hash": original_hash
    }))
    .await
    .unwrap();
    assert_eq!(
        stale_edit.get("success").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        stale_edit.get("error_code").and_then(Value::as_str),
        Some("hash_mismatch")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed elsewhere");

    let current_hash = file_hash::execute(&json!({ "path": path.to_str().unwrap() }))
        .await
        .unwrap()["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let edited = edit_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "mode": "replace",
        "content": "agent edit",
        "expected_hash": current_hash
    }))
    .await
    .unwrap();
    assert_eq!(edited.get("success").and_then(Value::as_bool), Some(true));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "agent edit");

    let stale_delete = delete_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "expected_hash": original_hash
    }))
    .await
    .unwrap();
    assert_eq!(
        stale_delete.get("success").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        stale_delete.get("error_code").and_then(Value::as_str),
        Some("hash_mismatch")
    );
    assert!(path.exists());

    let delete_hash = file_hash::execute(&json!({ "path": path.to_str().unwrap() }))
        .await
        .unwrap()["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let deleted = delete_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "expected_hash": delete_hash
    }))
    .await
    .unwrap();
    assert_eq!(deleted.get("success").and_then(Value::as_bool), Some(true));
    assert!(!path.exists());
}

#[tokio::test]
async fn test_edit_files_expected_hash_preflights_every_file() {
    let dir = tempdir().unwrap();
    register_write_workspace(dir.path());
    let first = dir.path().join("first.txt");
    let second = dir.path().join("second.txt");
    std::fs::write(&first, "alpha").unwrap();
    std::fs::write(&second, "beta").unwrap();

    let first_hash = file_hash::execute(&json!({ "path": first.to_str().unwrap() }))
        .await
        .unwrap()["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let stale_second_hash = file_hash::execute(&json!({ "path": second.to_str().unwrap() }))
        .await
        .unwrap()["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(&second, "beta changed elsewhere").unwrap();

    let result = edit_files::execute(&json!({
        "files": [
            {
                "path": first.to_str().unwrap(),
                "expected_hash": first_hash,
                "edits": [{
                    "find": "alpha",
                    "replace": "ALPHA",
                    "expected_replacements": 1
                }]
            },
            {
                "path": second.to_str().unwrap(),
                "expected_hash": stale_second_hash,
                "edits": [{
                    "find": "beta",
                    "replace": "BETA",
                    "expected_replacements": 1
                }]
            }
        ]
    }))
    .await
    .unwrap();

    assert_eq!(result.get("success").and_then(Value::as_bool), Some(false));
    assert_eq!(
        result.get("error_code").and_then(Value::as_str),
        Some("hash_mismatch")
    );
    assert_eq!(result.get("file_index").and_then(Value::as_u64), Some(1));
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "alpha");
    assert_eq!(
        std::fs::read_to_string(&second).unwrap(),
        "beta changed elsewhere"
    );
}

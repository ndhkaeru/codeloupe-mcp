use codeloupe_mcp::tools::{
    self, copy_file, create_file, diff_files, edit_files, file_hash, move_file, undo_change,
};
use serde_json::{Value, json};
use std::{fs, path::Path};
use tempfile::tempdir;

fn register(path: &Path) {
    codeloupe_mcp::indexer::ensure_workspace_index(path.to_path_buf(), "file_workflow_test".into());
    codeloupe_mcp::workspace_control::register_configured_workspace(
        path.to_path_buf(),
        "file_workflow_test",
        true,
    );
}

async fn hash(path: &Path) -> String {
    file_hash::execute(&json!({"path":path})).await.unwrap()["sha256"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn call(name: &str, arguments: Value) -> (Value, bool) {
    let response = tools::call_tool(json!({"name":name,"arguments":arguments}))
        .await
        .unwrap();
    (
        serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap(),
        response["isError"] == true,
    )
}

#[tokio::test]
async fn preview_group_undo_and_conflict_are_exercised_through_dispatcher() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, "alpha\n").unwrap();
    fs::write(&b, "beta\n").unwrap();
    let args = json!({"files":[
        {"path":a,"edits":[{"find":"alpha","replace":"ALPHA","expected_replacements":1}]},
        {"path":b,"edits":[{"find":"beta","replace":"BETA","expected_replacements":1}]}
    ]});
    let mut preview_args = args.clone();
    preview_args["dry_run"] = json!(true);
    let (preview, error) = call("edit_files", preview_args).await;
    assert!(!error, "{preview}");
    assert_eq!(preview["dry_run"], true);
    assert!(
        preview["files"][0]["diff"]["unified_diff"]
            .as_str()
            .unwrap()
            .contains("+ALPHA")
    );
    assert!(preview.get("change_set_id").is_none());
    assert_eq!(fs::read_to_string(&a).unwrap(), "alpha\n");
    let (edited, error) = call("edit_files", args).await;
    assert!(!error, "{edited}");
    let group = edited["change_set_id"].as_str().unwrap();
    fs::write(&b, "newer\n").unwrap();
    let (conflict, error) = call("undo_change", json!({"change_set_id":group})).await;
    assert!(error);
    assert_eq!(conflict["error"]["code"], "history_conflict");
    assert_eq!(fs::read_to_string(&a).unwrap(), "ALPHA\n");
    fs::write(&b, "BETA\n").unwrap();
    let (undo, error) = call("undo_change", json!({"change_set_id":group})).await;
    assert!(!error, "{undo}");
    assert_eq!(undo["undone_change_set_id"], group);
    assert_eq!(fs::read_to_string(&a).unwrap(), "alpha\n");
    assert_eq!(fs::read_to_string(&b).unwrap(), "beta\n");
    let (redo, error) = call(
        "undo_change",
        json!({"change_set_id":undo["change_set_id"]}),
    )
    .await;
    assert!(!error, "{redo}");
    assert_eq!(fs::read_to_string(&a).unwrap(), "ALPHA\n");
}

#[tokio::test]
async fn create_overwrite_hash_is_checked_without_creating_parents_on_failure() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let path = dir.path().join("a");
    fs::write(&path, "original").unwrap();
    let stale = "0".repeat(64);
    let result = create_file::execute(
        &json!({"path":path,"overwrite":true,"content":"new","expected_hash":stale}),
    )
    .await
    .unwrap();
    assert_eq!(result["error_code"], "hash_mismatch");
    assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    let expected = hash(&path).await;
    let result = create_file::execute(
        &json!({"path":path,"overwrite":true,"content":"new","expected_hash":expected}),
    )
    .await
    .unwrap();
    assert_eq!(result["success"], true);
    let nested = dir.path().join("absent/a");
    let result = create_file::execute(
        &json!({"path":nested,"overwrite":true,"content":"new","expected_hash":stale}),
    )
    .await
    .unwrap();
    assert_eq!(result["error_code"], "hash_mismatch");
    assert!(!nested.parent().unwrap().exists());
}

#[tokio::test]
async fn diff_sources_history_limits_and_utf8_truncation() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let path = dir.path().join("a.txt");
    let created = create_file::execute(&json!({"path":path,"content":"old\n"}))
        .await
        .unwrap();
    let id = created["history_entry_id"].as_str().unwrap();
    let result=diff_files::execute(&json!({"left":{"entry_id":id,"snapshot":"before"},"right":{"entry_id":id,"snapshot":"after"}})).await.unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["diff_summary"]["inserted_lines"], 1);
    let (result, error) = call(
        "diff_files",
        json!({"left":{"path":path},"right":{"content":"new\n"}}),
    )
    .await;
    assert!(!error, "{result}");
    assert_eq!(result["diff_summary"]["deleted_lines"], 1);
    let ambiguous =
        diff_files::execute(&json!({"left":{"content":"a","path":path},"right":{"content":"b"}}))
            .await
            .unwrap();
    assert_eq!(ambiguous["error_code"], "invalid_source");
    let missing = diff_files::execute(&json!({"left":{"entry_id":id},"right":{"content":"b"}}))
        .await
        .unwrap();
    assert_eq!(missing["error_code"], "invalid_snapshot");
    let huge = diff_files::execute(
        &json!({"left":{"content":"x".repeat(1024*1024+1)},"right":{"content":""}}),
    )
    .await
    .unwrap();
    assert_eq!(huge["error_code"], "input_too_large");
    let truncated=diff_files::execute(&json!({"left":{"content":""},"right":{"content":"xin chào\n".repeat(100)},"max_output_bytes":256})).await.unwrap();
    assert_eq!(truncated["output_truncated"], true);
    assert!(truncated["unified_diff"].as_str().unwrap().len() <= 256);
    assert!(truncated["omitted_bytes"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn large_binary_copy_uses_hash_history_and_guarded_undo() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("large.bin");
    let destination = dir.path().join("copy.bin");
    let bytes = vec![0u8; 10 * 1024 * 1024 + 1];
    fs::write(&source, &bytes).unwrap();
    let preview =
        copy_file::execute(&json!({"source":source,"destination":destination,"dry_run":true}))
            .await
            .unwrap();
    assert_eq!(preview["dry_run"], true);
    assert!(!destination.exists());
    let (result, error) = call(
        "copy_file",
        json!({"source":source,"destination":destination}),
    )
    .await;
    assert!(!error, "{result}");
    assert_eq!(fs::read(&destination).unwrap(), bytes);
    let id = result["history_entry_id"].as_str().unwrap();
    let unavailable = diff_files::execute(
        &json!({"left":{"entry_id":id,"snapshot":"after"},"right":{"content":""}}),
    )
    .await
    .unwrap();
    assert_eq!(unavailable["error_code"], "snapshot_content_unavailable");
    let refused = copy_file::execute(&json!({"source":source,"destination":destination}))
        .await
        .unwrap();
    assert_eq!(refused["error_code"], "already_exists");
    fs::write(&destination, "newer").unwrap();
    let refused = undo_change::execute(&json!({"entry_id":id})).await.unwrap();
    assert_eq!(refused["error_code"], "history_conflict");
    fs::write(&destination, &bytes).unwrap();
    let undone = undo_change::execute(&json!({"entry_id":id})).await.unwrap();
    assert_eq!(undone["success"], true);
    assert_eq!(undone["history_recorded"], false);
    assert!(!destination.exists());
    assert_eq!(fs::read(&source).unwrap(), bytes);
}

#[tokio::test]
async fn move_inverse_conflicts_and_redo() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("from.bin");
    let destination = dir.path().join("to.bin");
    fs::write(&source, b"x\0y").unwrap();
    let result = move_file::execute(&json!({"source":source,"destination":destination}))
        .await
        .unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert!(!source.exists());
    fs::write(&source, "occupied").unwrap();
    let conflict = undo_change::execute(&json!({"change_set_id":result["change_set_id"]}))
        .await
        .unwrap();
    assert_eq!(conflict["error_code"], "history_conflict");
    assert_eq!(fs::read(&destination).unwrap(), b"x\0y");
    fs::remove_file(&source).unwrap();
    let undo = undo_change::execute(&json!({"change_set_id":result["change_set_id"]}))
        .await
        .unwrap();
    assert_eq!(undo["success"], true, "{undo}");
    assert_eq!(fs::read(&source).unwrap(), b"x\0y");
    assert!(!destination.exists());
    let redo = undo_change::execute(&json!({"change_set_id":undo["change_set_id"]}))
        .await
        .unwrap();
    assert_eq!(redo["success"], true, "{redo}");
    assert!(!source.exists());
    assert_eq!(fs::read(&destination).unwrap(), b"x\0y");
}

#[tokio::test]
async fn case_only_rename_respects_actual_filesystem_case_sensitivity() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("Foo.txt");
    let destination = dir.path().join("foo.txt");
    fs::write(&source, "payload").unwrap();
    let result = move_file::execute(&json!({"source":source,"destination":destination}))
        .await
        .unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert!(
        fs::read_dir(dir.path())
            .unwrap()
            .any(|e| e.unwrap().file_name() == "foo.txt")
    );
    let undo = undo_change::execute(&json!({"entry_id":result["history_entry_id"]}))
        .await
        .unwrap();
    assert_eq!(undo["success"], true, "{undo}");
    assert!(
        fs::read_dir(dir.path())
            .unwrap()
            .any(|e| e.unwrap().file_name() == "Foo.txt")
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn distinct_case_paths_are_never_overwritten() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("Foo");
    let destination = dir.path().join("foo");
    fs::write(&source, "first").unwrap();
    // On a case-insensitive filesystem this represents the source itself.
    if destination.exists() {
        return;
    }
    fs::write(&destination, "second").unwrap();
    let result = move_file::execute(&json!({"source":source,"destination":destination}))
        .await
        .unwrap();
    assert_eq!(result["error_code"], "already_exists");
    assert_eq!(fs::read_to_string(&source).unwrap(), "first");
    assert_eq!(fs::read_to_string(&destination).unwrap(), "second");
}

#[tokio::test]
async fn ordered_operations_move_then_edit_create_copy_delete_and_group_undo() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    let c = dir.path().join("c.txt");
    let d = dir.path().join("d.txt");
    fs::write(&a, "alpha\n").unwrap();
    let args = json!({"operations":[
        {"op":"move","source":a,"destination":b},
        {"op":"edit","path":b,"edits":[{"find":"alpha","replace":"beta","expected_replacements":1}]},
        {"op":"create","path":c,"content":"temporary\n"},
        {"op":"copy","source":b,"destination":d},
        {"op":"delete","path":c}
    ]});
    let mut preview = args.clone();
    preview["dry_run"] = json!(true);
    let (result, error) = call("edit_files", preview).await;
    assert!(!error, "{result}");
    assert_eq!(result["dry_run"], true);
    assert!(a.exists());
    assert!(!b.exists());
    let (result, error) = call("edit_files", args).await;
    assert!(!error, "{result}");
    assert_eq!(result["files_changed"], 3);
    assert!(!a.exists());
    assert!(!c.exists());
    assert_eq!(fs::read_to_string(&b).unwrap(), "beta\n");
    assert_eq!(fs::read_to_string(&d).unwrap(), "beta\n");
    let undo = undo_change::execute(&json!({"change_set_id":result["change_set_id"]}))
        .await
        .unwrap();
    assert_eq!(undo["success"], true, "{undo}");
    assert_eq!(fs::read_to_string(&a).unwrap(), "alpha\n");
    assert!(!b.exists());
    assert!(!c.exists());
    assert!(!d.exists());
}

#[tokio::test]
async fn failed_later_virtual_step_writes_nothing() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    fs::write(&a, "original").unwrap();
    let result = edit_files::execute(&json!({"operations":[
        {"op":"move","source":a,"destination":b},
        {"op":"edit","path":b,"edits":[{"find":"missing","replace":"x","expected_replacements":1}]}
    ]}))
    .await
    .unwrap();
    assert_eq!(result["error_code"], "replacement_mismatch");
    assert_eq!(result["operation_index"], 1);
    assert_eq!(fs::read_to_string(&a).unwrap(), "original");
    assert!(!b.exists());
}

#[tokio::test]
async fn line_transfer_requires_both_hashes_preserves_encoding_and_undo() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, b"one\ntwo\nthree\n").unwrap();
    fs::write(&b, b"\xef\xbb\xbfhead\r\nfoot\r\n").unwrap();
    let mut args = json!({"operations":[{"op":"transfer","action":"move","source":a,"destination":b,"start_line":2,"end_line":2,"insert_before_line":2}]});
    let missing = edit_files::execute(&args).await.unwrap();
    assert_eq!(missing["error_code"], "missing_expected_hash");
    args["operations"][0]["expected_hash"] = json!(hash(&a).await);
    let missing = edit_files::execute(&args).await.unwrap();
    assert_eq!(missing["error_code"], "missing_expected_hash");
    args["operations"][0]["destination_expected_hash"] = json!(hash(&b).await);
    let (result, error) = call("edit_files", args).await;
    assert!(!error, "{result}");
    assert_eq!(fs::read_to_string(&a).unwrap(), "one\nthree\n");
    assert_eq!(
        fs::read(&b).unwrap(),
        b"\xef\xbb\xbfhead\r\ntwo\r\nfoot\r\n"
    );
    let undo = undo_change::execute(&json!({"change_set_id":result["change_set_id"]}))
        .await
        .unwrap();
    assert_eq!(undo["success"], true, "{undo}");
    assert_eq!(fs::read(&a).unwrap(), b"one\ntwo\nthree\n");
    assert_eq!(fs::read(&b).unwrap(), b"\xef\xbb\xbfhead\r\nfoot\r\n");
}

#[tokio::test]
async fn same_file_transfer_and_stale_range_hash() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let path = dir.path().join("a.txt");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let expected = hash(&path).await;
    let mut args = json!({"operations":[{"op":"transfer","action":"move","source":path,"destination":path,"start_line":1,"end_line":1,"insert_before_line":4,"expected_hash":expected}]});
    let result = edit_files::execute(&args).await.unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "two\nthree\none\n");
    let stale = edit_files::execute(&args).await.unwrap();
    assert_eq!(stale["error_code"], "hash_mismatch");
    args["operations"][0]["expected_hash"] = json!(hash(&path).await);
    args["operations"][0]["start_line"] = json!(1);
    args["operations"][0]["end_line"] = json!(2);
    args["operations"][0]["insert_before_line"] = json!(2);
    let overlap = edit_files::execute(&args).await.unwrap();
    assert_eq!(overlap["error_code"], "overlapping_range");
}

#[tokio::test]
async fn critical_destinations_and_group_undo_are_preflighted_as_a_whole() {
    let dir = tempdir().unwrap();
    register(dir.path());
    fs::create_dir(dir.path().join(".git")).unwrap();
    let source = dir.path().join("a.txt");
    let critical = dir.path().join(".git/config");
    fs::write(&source, "source\n").unwrap();
    fs::write(&critical, "old\n").unwrap();
    let args = json!({"operations":[
        {"op":"edit","path":source,"edits":[{"find":"source","replace":"new","expected_replacements":1}]},
        {"op":"edit","path":critical,"edits":[{"find":"old","replace":"new","expected_replacements":1}]}
    ]});
    let (refused, error) = call("edit_files", args.clone()).await;
    assert!(error);
    assert_eq!(refused["error"]["code"], "risk_confirmation_required");
    assert_eq!(fs::read_to_string(&source).unwrap(), "source\n");
    let mut preview = args.clone();
    preview["dry_run"] = json!(true);
    let (preview, error) = call("edit_files", preview).await;
    assert!(!error, "{preview}");
    assert_eq!(preview["dry_run"], true);
    let mut acknowledged = args;
    acknowledged["acknowledge_risk"] = json!(true);
    let (edited, error) = call("edit_files", acknowledged).await;
    assert!(!error, "{edited}");
    let (refused, error) = call(
        "undo_change",
        json!({"change_set_id":edited["change_set_id"]}),
    )
    .await;
    assert!(error);
    assert_eq!(refused["error"]["code"], "risk_confirmation_required");
    assert_eq!(fs::read_to_string(&source).unwrap(), "new\n");
    let (undone, error) = call(
        "undo_change",
        json!({"change_set_id":edited["change_set_id"],"acknowledge_risk":true}),
    )
    .await;
    assert!(!error, "{undone}");
    assert_eq!(fs::read_to_string(&source).unwrap(), "source\n");
    let (refused, error) = call(
        "copy_file",
        json!({"source":source,"destination":dir.path().join(".git/copied")}),
    )
    .await;
    assert!(error);
    assert_eq!(refused["error"]["code"], "risk_confirmation_required");
    assert!(!dir.path().join(".git/copied").exists());
}

#[cfg(windows)]
#[tokio::test]
async fn windows_locked_file_is_not_moved_and_no_destination_is_created() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("locked");
    let destination = dir.path().join("target");
    fs::write(&source, "keep").unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&source)
        .unwrap();
    let result = move_file::execute(&json!({"source":source,"destination":destination}))
        .await
        .unwrap();
    assert_eq!(result["success"], false);
    assert_eq!(result["error_code"], "file_locked");
    assert!(!destination.exists());
    drop(lock);
    assert_eq!(fs::read_to_string(&source).unwrap(), "keep");
}

#[cfg(windows)]
#[tokio::test]
async fn cross_volume_move_verifies_bytes_before_removing_source() {
    let source_dir = tempdir().unwrap();
    // Each directory is owned by tempfile and is removed by the test harness.
    let target_dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    register(source_dir.path());
    register(target_dir.path());
    let source = source_dir.path().join("cross.bin");
    let destination = target_dir.path().join("cross.bin");
    fs::write(&source, vec![42u8; 1024 * 1024]).unwrap();
    let expected = hash(&source).await;
    let moved = move_file::execute(
        &json!({"source":source,"destination":destination,"expected_hash":expected}),
    )
    .await
    .unwrap();
    assert_eq!(moved["success"], true, "{moved}");
    assert!(!source.exists());
    assert_eq!(hash(&destination).await, expected);
    if source.components().next() != destination.components().next() {
        assert_eq!(moved["method"], "copy_verify_delete");
    }
    let undo = undo_change::execute(&json!({"change_set_id":moved["change_set_id"]}))
        .await
        .unwrap();
    assert_eq!(undo["success"], true, "{undo}");
    assert_eq!(hash(&source).await, expected);
    assert!(!destination.exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires writable /dev/shm on a device different from the temporary directory"]
async fn cross_device_move_when_shared_memory_is_available() {
    use std::os::unix::fs::MetadataExt;
    let source_dir = tempdir().unwrap();
    let target_dir =
        tempfile::tempdir_in("/dev/shm").expect("cross-device test requires writable /dev/shm");
    assert_ne!(
        fs::metadata(source_dir.path()).unwrap().dev(),
        fs::metadata(target_dir.path()).unwrap().dev(),
        "cross-device test requires /dev/shm and the temporary directory on different devices"
    );
    register(source_dir.path());
    register(target_dir.path());
    let source = source_dir.path().join("cross.bin");
    let destination = target_dir.path().join("cross.bin");
    fs::write(&source, b"cross-device\0payload").unwrap();
    let expected = hash(&source).await;
    let moved = move_file::execute(&json!({"source":source,"destination":destination}))
        .await
        .unwrap();
    assert_eq!(moved["success"], true, "{moved}");
    assert_eq!(moved["method"], "copy_verify_delete");
    assert_eq!(hash(&destination).await, expected);
    assert!(!source.exists());
    let undo = undo_change::execute(&json!({"change_set_id":moved["change_set_id"]}))
        .await
        .unwrap();
    assert_eq!(undo["success"], true, "{undo}");
    assert_eq!(hash(&source).await, expected);
    assert!(!destination.exists());
}

#[tokio::test]
async fn transfer_preserves_utf16_and_rejects_invalid_ranges_and_boundaries() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("a.txt");
    let destination = dir.path().join("b.txt");
    let bytes: Vec<u8> = [0xff, 0xfe]
        .into_iter()
        .chain("đầu\ncuối\n".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    fs::write(&source, &bytes).unwrap();
    fs::write(&destination, "target\n").unwrap();
    let mut args = json!({"operations":[{"op":"transfer","action":"copy","source":source,"destination":destination,
        "start_line":1,"end_line":1,"insert_before_line":2,"expected_hash":hash(&source).await,"destination_expected_hash":hash(&destination).await}]});
    let result = edit_files::execute(&args).await.unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(fs::read(&source).unwrap(), bytes);
    assert_eq!(fs::read_to_string(&destination).unwrap(), "target\nđầu\n");
    args["operations"][0]["destination_expected_hash"] = json!(hash(&destination).await);
    args["operations"][0]["end_line"] = json!(999);
    let invalid = edit_files::execute(&args).await.unwrap();
    assert_eq!(invalid["error_code"], "invalid_range");
    fs::write(&destination, "unterminated").unwrap();
    args["operations"][0]["end_line"] = json!(1);
    args["operations"][0]["destination_expected_hash"] = json!(hash(&destination).await);
    let invalid = edit_files::execute(&args).await.unwrap();
    assert_eq!(invalid["error_code"], "line_boundary");
    assert_eq!(fs::read_to_string(&destination).unwrap(), "unterminated");
}

#[tokio::test]
async fn operation_preflight_rejects_new_json_syntax_errors_and_ambiguous_input() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let path = dir.path().join("a.json");
    let result = edit_files::execute(
        &json!({"operations":[{"op":"create","path":path,"content":"{broken"}]}),
    )
    .await
    .unwrap();
    assert_eq!(result["error_code"], "syntax_validation_failed");
    assert!(!path.exists());
    let ambiguous = edit_files::execute(&json!({"files":[],"operations":[]}))
        .await
        .unwrap();
    assert_eq!(ambiguous["error_code"], "ambiguous_operations");
}

#[tokio::test]
async fn batch_preview_of_critical_destination_remains_read_only() {
    let dir = tempdir().unwrap();
    register(dir.path());
    fs::create_dir(dir.path().join(".git")).unwrap();
    let path = dir.path().join(".git/config");
    fs::write(&path, "old\n").unwrap();
    let (result,error)=call("batch_tool_call",json!({"calls":[{"tool":"edit_files","args":{
        "dry_run":true,"files":[{"path":path,"edits":[{"find":"old","replace":"new","expected_replacements":1}]}]
    }}]})).await;
    assert!(!error, "{result}");
    assert!(!result.to_string().contains("risk_confirmation_required"));
    assert!(result.to_string().contains("dry_run"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
}

#[tokio::test]
async fn only_final_virtual_syntax_is_validated() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let path = dir.path().join("a.json");
    fs::write(&path, "{\"value\":1}\n").unwrap();
    let result=edit_files::execute(&json!({"operations":[
        {"op":"edit","path":path,"edits":[{"find":"1","replace":"BROKEN","expected_replacements":1}]},
        {"op":"edit","path":path,"edits":[{"find":"BROKEN","replace":"2","expected_replacements":1}]}
    ]})).await.unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"value\":2}\n");
}

#[tokio::test]
async fn new_tool_schemas_and_preview_history_contract() {
    let schemas = tools::list_tools();
    for name in ["copy_file", "move_file", "diff_files"] {
        assert_eq!(schemas.iter().filter(|s| s["name"] == name).count(), 1);
    }
    let dir = tempdir().unwrap();
    register(dir.path());
    let path = dir.path().join("preview.txt");
    fs::write(&path, "original").unwrap();
    let result = edit_files::execute(&json!({"dry_run":true,"files":[
        {"path":path,"edits":[{"find":"original","replace":"new","expected_replacements":1}]}
    ]}))
    .await
    .unwrap();
    assert_eq!(result["dry_run"], true);
    assert!(
        !codeloupe_mcp::history::list_records(200)
            .0
            .iter()
            .any(|r| Path::new(&r.path) == path)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "original");
}

#[tokio::test]
async fn irrelevant_operation_arguments_are_rejected_before_any_write() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let path = dir.path().join("a");
    fs::write(&path, "keep").unwrap();
    let result = edit_files::execute(&json!({"operations":[
        {"op":"delete","path":path,"content":"unexpected"}
    ]}))
    .await
    .unwrap();
    assert_eq!(result["error_code"], "invalid_operation_argument");
    assert_eq!(fs::read_to_string(&path).unwrap(), "keep");
}

#[tokio::test]
async fn whitespace_history_ids_cannot_bypass_critical_preflight() {
    let dir = tempdir().unwrap();
    register(dir.path());
    fs::create_dir(dir.path().join(".git")).unwrap();
    let path = dir.path().join(".git/config");
    fs::write(&path, "old\n").unwrap();
    let (edited, error) = call(
        "edit_files",
        json!({"acknowledge_risk":true,"files":[
            {"path":path,"edits":[{"find":"old","replace":"new","expected_replacements":1}]}
        ]}),
    )
    .await;
    assert!(!error, "{edited}");
    for (field, id) in [
        (
            "entry_id",
            edited["files"][0]["history_entry_id"].as_str().unwrap(),
        ),
        ("change_set_id", edited["change_set_id"].as_str().unwrap()),
    ] {
        let mut args = json!({});
        args[field] = json!(format!("  {id}  "));
        let (refused, error) = call("undo_change", args).await;
        assert!(error, "{refused}");
        assert_eq!(refused["error"]["code"], "risk_confirmation_required");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
    }
}

#[tokio::test]
async fn copy_and_move_preserve_preexisting_syntax_baselines() {
    let dir = tempdir().unwrap();
    register(dir.path());
    for (extension, original) in [
        ("json", "// JSONC comment\n{\"compilerOptions\": {}}\n"),
        ("rs", "fn broken( {\n"),
    ] {
        for operation in ["copy", "move"] {
            let source = dir.path().join(format!("source-{operation}.{extension}"));
            let target = dir.path().join(format!("target-{operation}.{extension}"));
            fs::write(&source, original).unwrap();
            let args = json!({"operations": [
                {"op": operation, "source": source, "destination": target}
            ]});
            let mut preview = args.clone();
            preview["dry_run"] = json!(true);
            let (result, error) = call("edit_files", preview).await;
            assert!(!error, "{result}");
            assert!(!target.exists());
            assert_eq!(fs::read_to_string(&source).unwrap(), original);
            let (result, error) = call("edit_files", args).await;
            assert!(!error, "{result}");
            assert_eq!(fs::read_to_string(&target).unwrap(), original);
            let undo = undo_change::execute(&json!({"change_set_id": result["change_set_id"]}))
                .await
                .unwrap();
            assert_eq!(undo["success"], true, "{undo}");
            assert_eq!(fs::read_to_string(&source).unwrap(), original);
            assert!(!target.exists());
        }
    }
}

#[tokio::test]
async fn syntax_baselines_follow_chains_and_do_not_mask_new_errors() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let a = dir.path().join("a.rs");
    let b = dir.path().join("b.rs");
    let c = dir.path().join("c.rs");
    fs::write(&a, "fn broken( {\n").unwrap();
    let result = edit_files::execute(&json!({"operations": [
        {"op": "move", "source": a, "destination": b},
        {"op": "copy", "source": b, "destination": c},
        {"op": "edit", "path": c, "edits": [{"find": "broken", "replace": "renamed", "expected_replacements": 1}]}
    ]})).await.unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(fs::read_to_string(&c).unwrap(), "fn renamed( {\n");
    undo_change::execute(&json!({"change_set_id": result["change_set_id"]}))
        .await
        .unwrap();
    fs::write(&a, "fn good() {}\n").unwrap();
    fs::write(&b, "fn already_broken( {\n").unwrap();
    let result = edit_files::execute(&json!({"operations": [
        {"op": "delete", "path": b},
        {"op": "copy", "source": a, "destination": b},
        {"op": "edit", "path": b, "edits": [{"find": "{}", "replace": "{", "expected_replacements": 1}]}
    ]})).await.unwrap();
    assert_eq!(result["error_code"], "syntax_validation_failed");
    assert_eq!(fs::read_to_string(&b).unwrap(), "fn already_broken( {\n");
    let result = edit_files::execute(&json!({"operations": [
        {"op": "create", "path": c, "content": "fn generated_broken( {"},
        {"op": "move", "source": c, "destination": dir.path().join("d.rs")}
    ]}))
    .await
    .unwrap();
    assert_eq!(result["error_code"], "syntax_validation_failed");
    assert!(!c.exists());
}

#[tokio::test]
async fn unicode_case_rename_and_undo_use_file_identity() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("Ảnh.txt");
    let target = dir.path().join("ảnh.txt");
    fs::write(&source, "image bytes").unwrap();
    let result = move_file::execute(&json!({"source": source, "destination": target}))
        .await
        .unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert!(
        fs::read_dir(dir.path())
            .unwrap()
            .any(|entry| entry.unwrap().file_name() == "ảnh.txt")
    );
    let undo = undo_change::execute(&json!({"entry_id": result["history_entry_id"]}))
        .await
        .unwrap();
    assert_eq!(undo["success"], true, "{undo}");
    assert!(
        fs::read_dir(dir.path())
            .unwrap()
            .any(|entry| entry.unwrap().file_name() == "Ảnh.txt")
    );
}

#[tokio::test]
async fn grouped_move_requires_explicit_partial_undo_and_preserves_group_metadata() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("source.txt");
    let target = dir.path().join("target.txt");
    fs::write(&source, "original").unwrap();
    let (moved, error) = call(
        "edit_files",
        json!({"operations": [
            {"op": "move", "source": source, "destination": target}
        ]}),
    )
    .await;
    assert!(!error, "{moved}");
    for file in moved["files"].as_array().unwrap() {
        let (refused, error) =
            call("undo_change", json!({"entry_id": file["history_entry_id"]})).await;
        assert!(error, "{refused}");
        assert_eq!(refused["error"]["code"], "partial_undo_requires_opt_in");
        assert_eq!(refused["change_set_id"], moved["change_set_id"]);
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
    }
    let source_entry = moved["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"].as_str().unwrap().ends_with("source.txt"))
        .unwrap();
    let (partial, error) = call(
        "undo_change",
        json!({
            "entry_id": source_entry["history_entry_id"], "allow_partial": true
        }),
    )
    .await;
    assert!(!error, "{partial}");
    assert_eq!(partial["partial_undo"], true);
    assert_eq!(partial["partial_change_set_id"], moved["change_set_id"]);
    assert_eq!(partial["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(fs::read_to_string(&source).unwrap(), "original");
    assert_eq!(fs::read_to_string(&target).unwrap(), "original");
    // Undo the intentional partial undo, then restore the complete original set.
    let (reverted, error) = call(
        "undo_change",
        json!({"entry_id": partial["history_entry_id"]}),
    )
    .await;
    assert!(!error, "{reverted}");
    let (restored, error) = call(
        "undo_change",
        json!({"change_set_id": moved["change_set_id"]}),
    )
    .await;
    assert!(!error, "{restored}");
    assert_eq!(fs::read_to_string(&source).unwrap(), "original");
    assert!(!target.exists());
}

#[cfg(unix)]
fn assert_no_group_or_other_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o077,
        0,
        "broader private-file permissions at {}",
        path.display()
    );
}

#[cfg(unix)]
async fn check_existing_file_writers_with_permission_status(expected: Option<bool>) {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let dir = tempdir().unwrap();
    register(dir.path());
    for tool in [
        "edit_file",
        "create_file",
        "edit_files",
        "operations",
        "convert_file_format",
    ] {
        let path = dir.path().join(format!("{tool}.txt"));
        let mut original = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        original.write_all(b"old\r\n").unwrap();
        drop(original);
        let arguments = match tool {
            "edit_file" => json!({"path": path, "mode": "replace", "content": "new\r\n"}),
            "create_file" => json!({"path": path, "overwrite": true, "content": "new\r\n"}),
            "edit_files" => json!({"files": [{
                "path": path, "edits": [{
                    "find": "old", "replace": "new", "expected_replacements": 1
                }]
            }]}),
            "operations" => json!({"operations": [{
                "op": "edit", "path": path, "edits": [{
                    "find": "old", "replace": "new", "expected_replacements": 1
                }]
            }]}),
            _ => json!({"path": path, "target_line_ending": "lf"}),
        };
        let name = if tool == "operations" {
            "edit_files"
        } else {
            tool
        };
        let (edited, error) = call(name, arguments).await;
        assert!(!error, "{tool}: {edited}");
        let status = edited.get("files").map_or(&edited, |files| &files[0]);
        assert_eq!(
            status.get("permissions_preserved").and_then(Value::as_bool),
            expected,
            "{tool}: {edited}"
        );
        assert!(status.get("publication_atomic").is_none(), "{edited}");
        let expected_bytes: &[u8] = if tool == "convert_file_format" {
            b"old\n"
        } else {
            b"new\r\n"
        };
        assert_eq!(fs::read(&path).unwrap(), expected_bytes, "{tool}");
        assert_no_group_or_other_permissions(&path);
        if expected.is_none() {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let selector = if let Some(group) = edited.get("change_set_id") {
            json!({"change_set_id": group})
        } else {
            json!({"entry_id": edited["history_entry_id"]})
        };
        let (undone, error) = call("undo_change", selector).await;
        assert!(!error, "{tool}: {undone}");
        let status = undone.get("files").map_or(&undone, |files| &files[0]);
        assert_eq!(
            status.get("permissions_preserved").and_then(Value::as_bool),
            expected,
            "{tool}: {undone}"
        );
        assert!(status.get("publication_atomic").is_none(), "{undone}");
        assert_eq!(fs::read(&path).unwrap(), b"old\r\n", "{tool}");
        assert_no_group_or_other_permissions(&path);
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "temporary file left by {tool}"
        );
        fs::remove_file(&path).unwrap();
    }

    let source = dir.path().join("private-source.bin");
    let copied_path = dir.path().join("private-copy.bin");
    let moved_path = dir.path().join("private-move.bin");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&source)
        .unwrap()
        .write_all(b"private bytes")
        .unwrap();
    let (copied, error) = call(
        "copy_file",
        json!({"source": source, "destination": copied_path}),
    )
    .await;
    assert!(!error, "{copied}");
    assert_eq!(
        copied.get("permissions_preserved").and_then(Value::as_bool),
        expected,
        "{copied}"
    );
    assert_no_group_or_other_permissions(&copied_path);
    assert_eq!(fs::read(&copied_path).unwrap(), b"private bytes");
    let (moved, error) = call(
        "move_file",
        json!({"source": copied_path, "destination": moved_path}),
    )
    .await;
    assert!(!error, "{moved}");
    assert_no_group_or_other_permissions(&moved_path);
    assert_eq!(fs::read(&moved_path).unwrap(), b"private bytes");
    let copied_during_move = moved["method"].as_str().unwrap().starts_with("copy_");
    assert_eq!(
        moved.get("permissions_preserved").and_then(Value::as_bool),
        if copied_during_move { expected } else { None },
        "{moved}"
    );
    let (undone, error) = call(
        "undo_change",
        json!({"entry_id": moved["history_entry_id"]}),
    )
    .await;
    assert!(!error, "{undone}");
    assert_no_group_or_other_permissions(&copied_path);
    assert_eq!(fs::read(&copied_path).unwrap(), b"private bytes");
    assert_eq!(
        undone.get("permissions_preserved").and_then(Value::as_bool),
        if copied_during_move { expected } else { None },
        "{undone}"
    );
    assert!(!moved_path.exists());
    assert_no_group_or_other_permissions(&source);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn existing_file_writers_preserve_permission_modes_when_supported() {
    check_existing_file_writers_with_permission_status(None).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires CODELOUPE_TEST_DENY_CHMOD and seccomp denying chmod/fchmod/fchmodat"]
async fn existing_file_writers_succeed_when_chmod_is_denied() {
    assert!(
        std::env::var_os("CODELOUPE_TEST_DENY_CHMOD").is_some(),
        "run explicitly with CODELOUPE_TEST_DENY_CHMOD and the chmod seccomp profile"
    );
    check_existing_file_writers_with_permission_status(Some(false)).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires CODELOUPE_TEST_EXCLUSIVE_CREATE and seccomp denying renameat2/link/linkat"]
async fn unix_exclusive_create_fallback_is_reported_by_all_new_file_writers() {
    // Run with --ignored in a container denying renameat2/link/linkat with EPERM.
    assert!(
        std::env::var_os("CODELOUPE_TEST_EXCLUSIVE_CREATE").is_some(),
        "run explicitly with CODELOUPE_TEST_EXCLUSIVE_CREATE and the fallback seccomp profile"
    );
    let chmod_denied = std::env::var_os("CODELOUPE_TEST_DENY_CHMOD").is_some();
    let dir = tempdir().unwrap();
    register(dir.path());
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    let c = dir.path().join("c.txt");
    let d = dir.path().join("d.txt");
    let (created, error) = call("create_file", json!({"path": a, "content": "alpha\n"})).await;
    assert!(!error, "{created}");
    assert_eq!(created["publication_atomic"], false);
    assert!(created.get("permissions_preserved").is_none());
    let (edited, error) = call(
        "edit_file",
        json!({
            "path": b, "mode": "replace", "create_if_missing": true, "content": "beta\n"
        }),
    )
    .await;
    assert!(!error, "{edited}");
    assert_eq!(edited["publication_atomic"], false);
    assert!(edited.get("permissions_preserved").is_none());
    let (copied, error) = call("copy_file", json!({"source": a, "destination": c})).await;
    assert!(!error, "{copied}");
    assert_eq!(copied["publication_atomic"], false);
    assert_eq!(
        copied.get("permissions_preserved").and_then(Value::as_bool),
        chmod_denied.then_some(false),
        "{copied}"
    );
    let (moved, error) = call("move_file", json!({"source": c, "destination": d})).await;
    assert!(!error, "{moved}");
    assert_eq!(moved["publication_atomic"], false);
    assert_eq!(
        moved.get("permissions_preserved").and_then(Value::as_bool),
        chmod_denied.then_some(false),
        "{moved}"
    );
    let (undone, error) = call(
        "undo_change",
        json!({"entry_id": moved["history_entry_id"]}),
    )
    .await;
    assert!(!error, "{undone}");
    assert_eq!(undone["publication_atomic"], false);
    assert_eq!(
        undone.get("permissions_preserved").and_then(Value::as_bool),
        chmod_denied.then_some(false),
        "{undone}"
    );
    let (deleted, error) = call("delete_file", json!({"path": a})).await;
    assert!(!error, "{deleted}");
    let (restored, error) = call(
        "undo_change",
        json!({"entry_id": deleted["history_entry_id"]}),
    )
    .await;
    assert!(!error, "{restored}");
    assert_eq!(restored["publication_atomic"], false);
    assert!(restored.get("permissions_preserved").is_none());
    let e = dir.path().join("e.txt");
    let (operations, error) = call(
        "edit_files",
        json!({"operations": [
            {"op": "create", "path": e, "content": "epsilon\n"},
            {"op": "delete", "path": b}
        ]}),
    )
    .await;
    assert!(!error, "{operations}");
    let created_entry = operations["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"].as_str().unwrap().ends_with("e.txt"))
        .unwrap();
    assert_eq!(created_entry["publication_atomic"], false);
    let (undo_group, error) = call(
        "undo_change",
        json!({"change_set_id": operations["change_set_id"]}),
    )
    .await;
    assert!(!error, "{undo_group}");
    let restored_entry = undo_group["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"].as_str().unwrap().ends_with("b.txt"))
        .unwrap();
    assert_eq!(restored_entry["publication_atomic"], false);
    assert_eq!(fs::read_to_string(&a).unwrap(), "alpha\n");
    assert_eq!(fs::read_to_string(&b).unwrap(), "beta\n");
    assert_eq!(fs::read_to_string(&c).unwrap(), "alpha\n");
    assert!(!d.exists());
    assert!(!e.exists());
}

#[tokio::test]
async fn mkdir_preview_move_and_group_undo_remove_nested_parents() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("old.txt");
    let parent = dir.path().join("module");
    let nested = parent.join("inner");
    let target = nested.join("new.txt");
    fs::write(&source, "original\n").unwrap();
    let operations = json!({"operations": [
        {"op": "mkdir", "path": parent},
        {"op": "mkdir", "path": nested},
        {"op": "move", "source": source, "destination": target}
    ]});
    let mut preview_args = operations.clone();
    preview_args["dry_run"] = json!(true);
    let (preview, error) = call("edit_files", preview_args).await;
    assert!(!error, "{preview}");
    assert_eq!(preview["files_changed"], 4);
    assert!(!parent.exists());
    assert_eq!(fs::read_to_string(&source).unwrap(), "original\n");
    assert!(
        preview["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|file| file["after_state"] == "directory")
            .all(|file| file["diff"]["diff_skipped"] == "directory state change")
    );
    let (created, error) = call("edit_files", operations).await;
    assert!(!error, "{created}");
    assert_eq!(created["files_changed"], 4);
    assert!(!source.exists());
    assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
    let (undone, error) = call(
        "undo_change",
        json!({"change_set_id": created["change_set_id"]}),
    )
    .await;
    assert!(!error, "{undone}");
    assert_eq!(fs::read_to_string(&source).unwrap(), "original\n");
    assert!(!parent.exists());
    let (redone, error) = call(
        "undo_change",
        json!({"change_set_id": undone["change_set_id"]}),
    )
    .await;
    assert!(!error, "{redone}");
    assert!(!source.exists());
    assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
}

#[tokio::test]
async fn mkdir_preflight_requires_declared_parents_and_keeps_existing_directories() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let parent = dir.path().join("parent");
    let child = parent.join("child");
    let file = child.join("a.txt");
    let (failed, error) = call(
        "edit_files",
        json!({"operations": [
            {"op": "create", "path": file, "content": "data"},
            {"op": "mkdir", "path": parent}
        ]}),
    )
    .await;
    assert!(error, "{failed}");
    assert_eq!(failed["error"]["code"], "parent_missing");
    assert!(!parent.exists());

    fs::create_dir(&parent).unwrap();
    let (failed, error) = call(
        "edit_files",
        json!({"operations": [
            {"op": "mkdir", "path": child},
            {"op": "mkdir", "path": parent, "allow_existing": false}
        ]}),
    )
    .await;
    assert!(error, "{failed}");
    assert_eq!(failed["error"]["code"], "already_exists");
    assert!(!child.exists());
    let (created, error) = call(
        "edit_files",
        json!({"operations": [
            {"op": "mkdir", "path": parent},
            {"op": "mkdir", "path": child},
            {"op": "create", "path": file, "content": "data"}
        ]}),
    )
    .await;
    assert!(!error, "{created}");
    assert_eq!(created["files_changed"], 2);
    let (undone, error) = call(
        "undo_change",
        json!({"change_set_id": created["change_set_id"]}),
    )
    .await;
    assert!(!error, "{undone}");
    assert!(parent.is_dir());
    assert!(!child.exists());

    let (failed, error) = call(
        "edit_files",
        json!({"operations": [
            {"op": "mkdir", "path": parent},
            {"op": "delete", "path": parent}
        ]}),
    )
    .await;
    assert!(error, "{failed}");
    assert_eq!(failed["error"]["code"], "not_a_regular_file");
    assert!(parent.is_dir());
}

#[tokio::test]
async fn mkdir_group_undo_preflights_untracked_children_before_any_restore() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let parent = dir.path().join("module");
    let file = parent.join("tracked.txt");
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, "old").unwrap();
    let (created, error) = call("edit_files", json!({"operations": [
        {"op": "mkdir", "path": parent},
        {"op": "create", "path": file, "content": "tracked"},
        {"op": "edit", "path": outside, "edits": [{"find": "old", "replace": "new", "expected_replacements": 1}]}
    ]})).await;
    assert!(!error, "{created}");
    let unexpected = parent.join("user.txt");
    fs::write(&unexpected, "independent").unwrap();
    let (failed, error) = call(
        "undo_change",
        json!({"change_set_id": created["change_set_id"]}),
    )
    .await;
    assert!(error, "{failed}");
    assert_eq!(failed["error"]["code"], "history_conflict");
    assert_eq!(failed["rollback_performed"], false);
    assert_eq!(fs::read_to_string(&file).unwrap(), "tracked");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "new");
    assert_eq!(fs::read_to_string(&unexpected).unwrap(), "independent");
    fs::remove_file(&unexpected).unwrap();
    let (undone, error) = call(
        "undo_change",
        json!({"change_set_id": created["change_set_id"]}),
    )
    .await;
    assert!(!error, "{undone}");
    assert!(!parent.exists());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "old");
}

#[tokio::test]
async fn mkdir_critical_path_is_preflighted_before_the_first_directory() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let normal = dir.path().join("normal");
    let critical = dir.path().join(".git");
    let arguments = json!({"operations": [
        {"op": "mkdir", "path": normal},
        {"op": "mkdir", "path": critical}
    ]});
    let (failed, error) = call("edit_files", arguments.clone()).await;
    assert!(error, "{failed}");
    assert_eq!(failed["error"]["code"], "risk_confirmation_required");
    assert!(!normal.exists());
    assert!(!critical.exists());
    let mut preview = arguments;
    preview["dry_run"] = json!(true);
    let (preview, error) = call("edit_files", preview).await;
    assert!(!error, "{preview}");
    assert_eq!(preview["dry_run"], true);
    assert!(!normal.exists());
    assert!(!critical.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn mkdir_rejects_a_final_symlink_even_when_existing_is_allowed() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let target = dir.path().join("real");
    let alias = dir.path().join("alias");
    fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let (failed, error) = call(
        "edit_files",
        json!({"operations": [{"op": "mkdir", "path": alias}]}),
    )
    .await;
    assert!(error, "{failed}");
    assert_eq!(failed["error"]["code"], "not_a_regular_file");
    assert!(target.is_dir());
}

#[tokio::test]
async fn move_reference_hints_are_scoped_bounded_and_read_only_in_preview() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("legacy.rs");
    let destination = dir.path().join("renamed.rs");
    let scope = dir.path().join("consumers");
    fs::create_dir(&scope).unwrap();
    fs::write(&source, "// legacy.rs self-mention\npub fn legacy() {}\n").unwrap();
    let consumer = scope.join("consumer.rs");
    fs::write(&consumer, "mod legacy;\n// see legacy.rs\n").unwrap();
    fs::write(scope.join("ignored.txt"), "legacy").unwrap();
    fs::write(scope.join("excluded.rs"), "mod legacy;").unwrap();
    fs::write(dir.path().join("outside.rs"), "mod legacy;").unwrap();
    let arguments = json!({
        "source": source, "destination": destination, "dry_run": true,
        "reference_hints": {"paths": [scope], "includes": ["**/*.rs"], "excludes": ["excluded.rs"], "max_results": 10}
    });
    let (preview, error) = call("move_file", arguments.clone()).await;
    assert!(!error, "{preview}");
    assert_eq!(preview["reference_hints"]["kind"], "lexical");
    assert_eq!(preview["reference_hints"]["timing"], "before_move");
    assert_eq!(preview["reference_hints"]["files"], json!(["consumer.rs"]));
    assert_eq!(
        preview["reference_hints"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(preview["reference_hints"]["complete"], true);
    assert!(!destination.exists());
    assert!(source.exists());
    assert!(preview.get("history_entry_id").is_none());
    let mut apply = arguments;
    apply["dry_run"] = json!(false);
    let (moved, error) = call("move_file", apply).await;
    assert!(!error, "{moved}");
    assert_eq!(moved["reference_hints"]["files"], json!(["consumer.rs"]));
    assert!(!source.exists());
    assert_eq!(
        fs::read_to_string(&consumer).unwrap(),
        "mod legacy;\n// see legacy.rs\n"
    );

    let second = dir.path().join("fresh.rs");
    let (moved, error) = call(
        "move_file",
        json!({"source": destination, "destination": second}),
    )
    .await;
    assert!(!error, "{moved}");
    assert!(moved.get("reference_hints").is_none());
}

#[tokio::test]
async fn move_reference_hints_report_limits_and_validate_options_before_moving() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let source = dir.path().join("old.rs");
    let destination = dir.path().join("new.rs");
    fs::write(&source, "pub fn old() {}\n").unwrap();
    for i in 0..6 {
        fs::write(dir.path().join(format!("consumer-{i}.rs")), "mod old;\n").unwrap();
    }
    for hints in [
        json!({}),
        json!({"paths": []}),
        json!({"paths": [dir.path()], "includes": ["["]}),
        json!({"paths": [dir.path()], "max_results": 0}),
    ] {
        let failed = move_file::execute(
            &json!({"source": source, "destination": destination, "reference_hints": hints}),
        )
        .await
        .unwrap();
        assert_eq!(failed["error_code"], "invalid_reference_hints", "{failed}");
        assert!(source.exists());
        assert!(!destination.exists());
    }
    let (preview, error) = call(
        "move_file",
        json!({
            "source": source, "destination": destination, "dry_run": true,
            "reference_hints": {"paths": [dir.path()], "max_results": 2}
        }),
    )
    .await;
    assert!(!error, "{preview}");
    let hints = &preview["reference_hints"];
    assert!(hints["matches"].as_array().unwrap().len() <= 2);
    assert!(hints["files"].as_array().unwrap().len() <= 2);
    assert_eq!(hints["limit_reached"], true);
    assert_eq!(hints["complete"], false);
    assert!(source.exists());
    assert!(!destination.exists());
}

#[tokio::test]
async fn move_reference_hints_use_the_parent_module_for_index_files() {
    let dir = tempdir().unwrap();
    register(dir.path());
    let module = dir.path().join("legacy");
    fs::create_dir(&module).unwrap();
    let source = module.join("index.ts");
    let destination = module.join("entry.ts");
    let consumer = dir.path().join("app.ts");
    fs::write(&source, "export const value = 1;\n").unwrap();
    fs::write(&consumer, "import { value } from './legacy';\n").unwrap();
    let (preview, error) = call(
        "move_file",
        json!({
            "source": source, "destination": destination, "dry_run": true,
            "reference_hints": {"paths": [consumer]}
        }),
    )
    .await;
    assert!(!error, "{preview}");
    assert!(
        preview["reference_hints"]["queries"]
            .as_array()
            .unwrap()
            .contains(&json!("legacy"))
    );
    assert_eq!(preview["reference_hints"]["files"], json!(["app.ts"]));
    assert_eq!(preview["reference_hints"]["complete"], true);
}

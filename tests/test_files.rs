use codeloupe_mcp::tools::{
    count_file_lines, file_hash, file_summary, find_definition, find_references, read_file,
    read_snippets, validate_json, workspace_stats,
};
use serde_json::json;
use std::fs::File;
use std::io::{BufWriter, Write};
use tempfile::tempdir;

#[tokio::test]
async fn test_read_file_range_with_encoding() {
    let dir = tempdir().unwrap();

    let utf8_path = dir.path().join("utf8.txt");
    let mut f1 = File::create(&utf8_path).unwrap();
    writeln!(f1, "Line 1\nLine 2\nTiếng Việt\nLine 4\nLine 5").unwrap();

    let args1 = json!({
        "path": utf8_path.to_str().unwrap(),
        "start_line": 2,
        "end_line": 4
    });

    let res1 = read_file::execute(&args1).await.unwrap();
    let content1 = res1.get("content").unwrap().as_str().unwrap();
    assert_eq!(content1, "Line 2\nTiếng Việt\nLine 4");
    assert_eq!(res1.get("encoding").unwrap().as_str().unwrap(), "UTF-8");

    let args2 = json!({
        "path": utf8_path.to_str().unwrap(),
        "start_line": 4,
        "end_line": 100
    });

    let res2 = read_file::execute(&args2).await.unwrap();
    let content2 = res2.get("content").unwrap().as_str().unwrap();
    assert_eq!(content2, "Line 4\nLine 5\n");
    assert_eq!(res2.get("total_lines").unwrap().as_u64().unwrap(), 5);

    let uri_path = dir.path().join("uri file.txt");
    std::fs::write(&uri_path, "uri payload\n").unwrap();
    let file_uri = format!(
        "file:///{}",
        uri_path
            .to_string_lossy()
            .replace('\\', "/")
            .replace(' ', "%20")
    );
    let uri_res = read_file::execute(&json!({ "path": file_uri }))
        .await
        .unwrap();
    assert_eq!(
        uri_res.get("content").and_then(|v| v.as_str()),
        Some("uri payload\n")
    );

    let win1252_path = dir.path().join("win1252.txt");
    let mut f2 = File::create(&win1252_path).unwrap();
    f2.write_all(b"\xC7a va\nline 2").unwrap();

    let args3 = json!({
        "path": win1252_path.to_str().unwrap(),
    });

    let res3 = read_file::execute(&args3).await.unwrap();
    let content3 = res3.get("content").unwrap().as_str().unwrap();
    assert_eq!(
        res3.get("encoding").unwrap().as_str().unwrap(),
        "windows-1252"
    );
    assert_eq!(content3, "Ça va\nline 2");
}

#[tokio::test]
async fn test_file_hash_and_read_file_range_return_full_file_sha256() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("hash.txt");
    std::fs::write(&path, "abc").unwrap();
    let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    let hash = file_hash::execute(&json!({ "path": path.to_str().unwrap() }))
        .await
        .unwrap();
    assert_eq!(
        hash.get("algorithm").and_then(|value| value.as_str()),
        Some("sha256")
    );
    assert_eq!(
        hash.get("sha256").and_then(|value| value.as_str()),
        Some(expected)
    );
    assert_eq!(
        hash.get("size_bytes").and_then(|value| value.as_u64()),
        Some(3)
    );

    let partial = read_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "start_byte": 1,
        "max_bytes": 1
    }))
    .await
    .unwrap();
    assert_eq!(
        partial.get("content").and_then(|value| value.as_str()),
        Some("b")
    );
    assert_eq!(
        partial.get("sha256").and_then(|value| value.as_str()),
        Some(expected)
    );
}

#[tokio::test]
async fn test_validate_json_accepts_files_and_reports_syntax_locations() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("valid.json");
    std::fs::write(&path, "{\"items\":[1,2,3]}").unwrap();

    let valid = validate_json::execute(&json!({ "path": path.to_str().unwrap() }))
        .await
        .unwrap();
    assert_eq!(
        valid.get("valid").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        valid.get("source").and_then(|value| value.as_str()),
        Some("file")
    );

    let invalid = validate_json::execute(&json!({
        "content": "{\n  \"items\": [1,]\n}"
    }))
    .await
    .unwrap();
    assert_eq!(
        invalid.get("valid").and_then(|value| value.as_bool()),
        Some(false)
    );
    assert_eq!(
        invalid.get("source").and_then(|value| value.as_str()),
        Some("content")
    );
    assert_eq!(
        invalid.get("line").and_then(|value| value.as_u64()),
        Some(2)
    );
    assert!(
        invalid
            .get("column")
            .and_then(|value| value.as_u64())
            .is_some()
    );
    assert!(
        invalid
            .get("message")
            .and_then(|value| value.as_str())
            .is_some()
    );
}

#[tokio::test]
async fn test_read_file_range_streams_large_utf8_files() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("large.txt");
    let padding_lines = 50 * 1024;
    let mut writer = BufWriter::new(File::create(&path).unwrap());
    writer.write_all(b"first\nsecond\n").unwrap();
    let padding = format!("{}\n", "x".repeat(1023));
    for _ in 0..padding_lines {
        writer.write_all(padding.as_bytes()).unwrap();
    }
    writer.flush().unwrap();

    let result = read_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "start_line": 1,
        "end_line": 2
    }))
    .await
    .unwrap();

    assert_eq!(
        result.get("content").and_then(|value| value.as_str()),
        Some("first\nsecond")
    );
    assert_eq!(
        result.get("total_lines").and_then(|value| value.as_u64()),
        Some((padding_lines + 2) as u64)
    );
    assert!(
        result
            .get("size_bytes")
            .and_then(|value| value.as_u64())
            .is_some_and(|size| size > 10 * 1024 * 1024)
    );
    assert!(result.get("file_size_bytes").is_none());
}

#[tokio::test]
async fn test_read_file_full_decode_size_limit() {
    let dir = tempdir().unwrap();
    let big_path = dir.path().join("big-utf16.txt");

    {
        let mut f = File::create(&big_path).unwrap();
        f.write_all(&[0xFF, 0xFE]).unwrap();
        f.set_len(12 * 1024 * 1024).unwrap();
    }

    let args = json!({
        "path": big_path.to_str().unwrap(),
    });

    let res = read_file::execute(&args).await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("too large"));
}

#[tokio::test]
async fn test_count_file_lines_handles_empty_plain_utf16_and_binary_files() {
    let dir = tempdir().unwrap();

    let empty_path = dir.path().join("empty.txt");
    std::fs::write(&empty_path, "").unwrap();
    let empty_res = count_file_lines::execute(&json!({
        "path": empty_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(empty_res.get("line_count").unwrap().as_u64().unwrap(), 0);
    assert!(!empty_res.get("is_binary").unwrap().as_bool().unwrap());
    assert_eq!(
        empty_res.get("encoding").unwrap().as_str().unwrap(),
        "UTF-8"
    );

    let plain_path = dir.path().join("plain.txt");
    std::fs::write(&plain_path, "alpha\nbeta\ngamma").unwrap();
    let plain_res = count_file_lines::execute(&json!({
        "path": plain_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(plain_res.get("line_count").unwrap().as_u64().unwrap(), 3);
    assert_eq!(
        plain_res.get("size_bytes").and_then(|value| value.as_u64()),
        Some(16)
    );
    assert!(plain_res.get("file_size_bytes").is_none());
    assert!(
        !plain_res
            .get("ends_with_newline")
            .unwrap()
            .as_bool()
            .unwrap()
    );

    let utf16_path = dir.path().join("utf16.txt");
    let mut utf16_bytes = vec![0xFF, 0xFE];
    for unit in "first\nsecond\n".encode_utf16() {
        utf16_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&utf16_path, utf16_bytes).unwrap();
    let utf16_res = count_file_lines::execute(&json!({
        "path": utf16_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(utf16_res.get("line_count").unwrap().as_u64().unwrap(), 2);
    assert!(!utf16_res.get("is_binary").unwrap().as_bool().unwrap());
    assert_eq!(
        utf16_res.get("encoding").unwrap().as_str().unwrap(),
        "UTF-16LE"
    );
    assert!(
        utf16_res
            .get("ends_with_newline")
            .unwrap()
            .as_bool()
            .unwrap()
    );

    let binary_path = dir.path().join("sample.bin");
    std::fs::write(&binary_path, [0x01u8, 0x00, 0x02, 0x03]).unwrap();
    let binary_res = count_file_lines::execute(&json!({
        "path": binary_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert!(binary_res.get("is_binary").unwrap().as_bool().unwrap());
    assert_eq!(binary_res.get("line_count").unwrap().as_u64().unwrap(), 0);
    assert!(binary_res.get("encoding").unwrap().is_null());
}

#[tokio::test]
async fn test_read_tools_normalize_bom_line_endings_and_binary_files() {
    let dir = tempdir().unwrap();

    let utf8_bom_path = dir.path().join("utf8-bom.txt");
    std::fs::write(&utf8_bom_path, b"\xEF\xBB\xBFalpha\r\nbeta\r").unwrap();

    let read_bom = read_file::execute(&json!({
        "path": utf8_bom_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        read_bom.get("content").and_then(|value| value.as_str()),
        Some("alpha\nbeta\n")
    );
    assert_eq!(
        read_bom.get("bom").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        read_bom.get("is_binary").and_then(|value| value.as_bool()),
        Some(false)
    );
    assert_eq!(
        read_bom.get("total_lines").and_then(|value| value.as_u64()),
        Some(2)
    );

    let count_bom = count_file_lines::execute(&json!({
        "path": utf8_bom_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        count_bom.get("line_count").and_then(|value| value.as_u64()),
        Some(2)
    );
    assert_eq!(
        count_bom.get("bom").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        count_bom
            .get("ends_with_newline")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    let summary_bom = file_summary::execute(&json!({
        "path": utf8_bom_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        summary_bom.get("bom").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        summary_bom
            .get("outline_preview")
            .and_then(|value| value.as_str()),
        Some("alpha\nbeta")
    );
    assert_eq!(
        summary_bom.get("lines").and_then(|value| value.as_i64()),
        Some(2)
    );
    assert!(summary_bom.get("size_bytes").is_some());
    assert!(summary_bom.get("size").is_none());

    let utf16_path = dir.path().join("utf16-crlf.txt");
    let mut utf16_bytes = vec![0xFF, 0xFE];
    for unit in "first\r\nsecond\rthird".encode_utf16() {
        utf16_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&utf16_path, utf16_bytes).unwrap();
    let read_utf16 = read_file::execute(&json!({
        "path": utf16_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        read_utf16.get("content").and_then(|value| value.as_str()),
        Some("first\nsecond\nthird")
    );
    assert_eq!(
        read_utf16.get("bom").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        read_utf16
            .get("total_lines")
            .and_then(|value| value.as_u64()),
        Some(3)
    );

    let binary_path = dir.path().join("read-binary.bin");
    std::fs::write(&binary_path, [b'a', 0, b'b', b'\n']).unwrap();
    let binary = read_file::execute(&json!({
        "path": binary_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        binary.get("is_binary").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        binary.get("content").and_then(|value| value.as_str()),
        Some("")
    );
    assert!(binary.get("encoding").is_some_and(|value| value.is_null()));
}

#[tokio::test]
async fn test_read_file_tail_and_byte_ranges_are_bounded_and_resumable() {
    let dir = tempdir().unwrap();
    let lines_path = dir.path().join("lines.log");
    std::fs::write(&lines_path, "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();

    let tail = read_file::execute(&json!({
        "path": lines_path.to_str().unwrap(),
        "tail": 2
    }))
    .await
    .unwrap();
    assert_eq!(
        tail.get("content").and_then(|value| value.as_str()),
        Some("delta\nepsilon\n")
    );
    assert_eq!(
        tail.get("start_line").and_then(|value| value.as_u64()),
        Some(4)
    );
    assert_eq!(
        tail.get("end_line").and_then(|value| value.as_u64()),
        Some(5)
    );
    assert_eq!(
        tail.get("total_lines").and_then(|value| value.as_u64()),
        Some(5)
    );

    let limited_tail = read_snippets::execute(&json!({
        "requests": [{
            "path": lines_path.to_str().unwrap(),
            "tail": 3,
            "max_lines": 1
        }]
    }))
    .await
    .unwrap();
    let continuation = limited_tail
        .pointer("/continuations/0/suggested_request")
        .unwrap();
    assert!(continuation.get("tail").is_none());
    assert_eq!(
        continuation
            .get("start_line")
            .and_then(|value| value.as_u64()),
        Some(4)
    );

    let long_path = dir.path().join("long-line.txt");
    std::fs::write(&long_path, "x".repeat(2 * 1024 * 1024)).unwrap();
    let line_range = read_file::execute(&json!({
        "path": long_path.to_str().unwrap(),
        "max_bytes": 16
    }))
    .await
    .unwrap();
    assert_eq!(
        line_range.get("content").and_then(|value| value.as_str()),
        Some("xxxxxxxxxxxxxxxx")
    );
    assert_eq!(
        line_range
            .get("line_truncated")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        line_range
            .get("next_start_byte")
            .and_then(|value| value.as_u64()),
        Some(16)
    );
    assert!(line_range.get("next_start_line").is_none());

    let line_snippets = read_snippets::execute(&json!({
        "requests": [{
            "path": long_path.to_str().unwrap(),
            "max_bytes": 8
        }]
    }))
    .await
    .unwrap();
    assert_eq!(
        line_snippets
            .pointer("/continuations/0/suggested_request/start_byte")
            .and_then(|value| value.as_u64()),
        Some(8)
    );
    assert!(
        line_snippets
            .pointer("/continuations/0/suggested_request/start_line")
            .is_none()
    );

    let byte_range = read_file::execute(&json!({
        "path": long_path.to_str().unwrap(),
        "start_byte": 1_000_000,
        "max_bytes": 16
    }))
    .await
    .unwrap();
    assert_eq!(
        byte_range.get("content").and_then(|value| value.as_str()),
        Some("xxxxxxxxxxxxxxxx")
    );
    assert_eq!(
        byte_range
            .get("start_byte")
            .and_then(|value| value.as_u64()),
        Some(1_000_000)
    );
    assert_eq!(
        byte_range.get("end_byte").and_then(|value| value.as_u64()),
        Some(1_000_016)
    );
    assert_eq!(
        byte_range
            .get("next_start_byte")
            .and_then(|value| value.as_u64()),
        Some(1_000_016)
    );
    assert_eq!(
        byte_range
            .get("truncated")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    let byte_snippets = read_snippets::execute(&json!({
        "requests": [{
            "path": long_path.to_str().unwrap(),
            "start_byte": 100,
            "max_bytes": 8
        }]
    }))
    .await
    .unwrap();
    assert_eq!(
        byte_snippets
            .pointer("/continuations/0/suggested_request/start_byte")
            .and_then(|value| value.as_u64()),
        Some(108)
    );
}

#[tokio::test]
async fn test_read_file_rejects_conflicting_range_modes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("conflict.txt");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();

    let tail_conflict = read_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "tail": 1,
        "start_line": 1
    }))
    .await
    .unwrap_err();
    assert!(tail_conflict.to_string().contains("tail"));

    let byte_conflict = read_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "start_byte": 1,
        "end_line": 2
    }))
    .await
    .unwrap_err();
    assert!(byte_conflict.to_string().contains("start_byte"));
}

#[tokio::test]
async fn test_read_file_streams_rfc6901_json_pointer_values() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("large.json");
    let ignored = (0..200_000)
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let document = format!(
        "{{\"ignored\":[{ignored}],\"nested\":{{\"a/b\":{{\"~key\":[1,{{\"value\":\"ok\"}}]}}}}}}"
    );
    std::fs::write(&path, document).unwrap();

    let result = read_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "json_pointer": "/nested/a~1b/~0key/1/value"
    }))
    .await
    .unwrap();
    assert_eq!(
        result.get("content").and_then(|value| value.as_str()),
        Some("\"ok\"")
    );
    assert_eq!(
        result.get("value_type").and_then(|value| value.as_str()),
        Some("string")
    );
    assert_eq!(
        result.get("truncated").and_then(|value| value.as_bool()),
        Some(false)
    );

    let array_value = read_snippets::execute(&json!({
        "requests": [{
            "path": path.to_str().unwrap(),
            "json_pointer": "/nested/a~1b/~0key/1"
        }]
    }))
    .await
    .unwrap();
    assert_eq!(
        array_value
            .pointer("/results/0/content")
            .and_then(|value| value.as_str()),
        Some("{\"value\":\"ok\"}")
    );

    let missing = read_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "json_pointer": "/nested/missing"
    }))
    .await
    .unwrap_err();
    assert!(missing.to_string().contains("not found"));

    let too_small = read_file::execute(&json!({
        "path": path.to_str().unwrap(),
        "json_pointer": "/nested/a~1b/~0key/1",
        "max_bytes": 4
    }))
    .await
    .unwrap_err();
    assert!(too_small.to_string().contains("too large to return"));
}

#[tokio::test]
async fn test_size_limited_scans_report_skipped_file_paths() {
    let dir = tempdir().unwrap();
    let expected_root = codeloupe_mcp::common::normalize_display_path(
        &codeloupe_mcp::common::canonicalize_if_exists(dir.path().to_path_buf()),
    );
    let large_path = dir.path().join("large.rs");
    let mut large = File::create(&large_path).unwrap();
    large.write_all(b"fn target() {}\n").unwrap();
    large.set_len(6 * 1024 * 1024).unwrap();

    let definitions = find_definition::execute(&json!({
        "symbol": "target",
        "paths": [large_path.to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        definitions.get("root").and_then(|value| value.as_str()),
        Some(expected_root.as_str())
    );
    assert_eq!(
        definitions
            .pointer("/diagnostics/skipped_large_files/0/path")
            .and_then(|value| value.as_str()),
        Some("large.rs")
    );
    assert_eq!(
        definitions
            .pointer("/diagnostics/skipped_large_files/0/limit_bytes")
            .and_then(|value| value.as_u64()),
        Some(2 * 1024 * 1024)
    );

    let references = find_references::execute(&json!({
        "symbol": "target",
        "paths": [large_path.to_str().unwrap()]
    }))
    .await
    .unwrap();
    assert_eq!(
        references.get("root").and_then(|value| value.as_str()),
        Some(expected_root.as_str())
    );
    assert_eq!(
        references
            .pointer("/diagnostics/skipped_large_files/0/path")
            .and_then(|value| value.as_str()),
        Some("large.rs")
    );
    assert_eq!(
        references
            .pointer("/diagnostics/skipped_large_files/0/size_bytes")
            .and_then(|value| value.as_u64()),
        Some(6 * 1024 * 1024)
    );

    let stats = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "max_line_count_bytes": 16
    }))
    .await
    .unwrap();
    assert_eq!(
        stats
            .pointer("/diagnostics/line_counting/skipped_large_count")
            .and_then(|value| value.as_u64()),
        Some(1)
    );
    assert!(stats.pointer("/largest_files/0/size_bytes").is_some());
    assert!(stats.pointer("/largest_files/0/size").is_none());
    assert_eq!(
        stats
            .pointer("/largest_files/0/path")
            .and_then(|value| value.as_str()),
        Some("large.rs")
    );
    assert_eq!(
        stats
            .pointer("/diagnostics/line_counting/skipped_large_files/0/limit_bytes")
            .and_then(|value| value.as_u64()),
        Some(16)
    );
}

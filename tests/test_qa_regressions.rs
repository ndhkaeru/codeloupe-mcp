use codeloupe_mcp::tools::{
    self, batch_tool_call, convert_file_format, file_summary, peek_archive, read_file,
    read_snippets, workspace_stats,
};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::tempdir;
use zip::write::SimpleFileOptions;

const INDEX_SETTLE_MAX_POLLS: i64 = 40;

fn server_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_codeloupe-mcp").map(PathBuf::from)
        && path.exists()
    {
        return path;
    }

    let exe_name = if cfg!(windows) {
        "codeloupe-mcp.exe"
    } else {
        "codeloupe-mcp"
    };
    let current_exe = std::env::current_exe().unwrap();
    let debug_dir = current_exe
        .parent()
        .and_then(|parent| parent.parent())
        .unwrap();
    let candidate = debug_dir.join(exe_name);
    assert!(
        candidate.exists(),
        "could not locate codeloupe-mcp binary at {}",
        candidate.display()
    );
    candidate
}

fn isolate_server_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let normalized = key.to_string_lossy().to_ascii_uppercase();
        if normalized.starts_with("CODELOUPE_MCP_") || normalized.starts_with("CODEBASE_MCP_") {
            command.env_remove(key);
        }
    }
}

fn run_binary_with_input(current_dir: &Path, input: &[u8]) -> Output {
    let exe = server_binary();
    let mut command = Command::new(&exe);
    isolate_server_environment(&mut command);
    command
        .current_dir(current_dir)
        .env(
            "CODELOUPE_MCP_INDEX_DIR",
            current_dir.join("codeloupe-test-index"),
        )
        .env("CODELOUPE_MCP_INDEX_MODE", "auto")
        .env("LOCALAPPDATA", current_dir.join("fake-local-app-data"))
        .env("APPDATA", current_dir.join("fake-app-data"))
        .env("USERPROFILE", current_dir.join("fake-home/nested"))
        .env("HOME", current_dir.join("fake-home/nested"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

#[test]
fn test_binary_rejects_invalid_jsonrpc_and_pre_initialize_requests() {
    let dir = tempdir().unwrap();
    let input = format!(
        "{}\n{}\n",
        json!({"jsonrpc":"1.0","id":1,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
    );
    let output = run_binary_with_input(dir.path(), input.as_bytes());

    assert!(output.status.success());
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        responses[0].pointer("/error/code").and_then(Value::as_i64),
        Some(-32600)
    );
    assert_eq!(
        responses[1].pointer("/error/code").and_then(Value::as_i64),
        Some(-32002)
    );
}

#[test]
fn test_binary_ping_protocol_negotiation_and_notification_silence() {
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    let notification_target = workspace.path().join("notification-created.txt");
    let input = [
        json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{
                "protocolVersion":"2025-06-18",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"0"},
                "workspaceFolders":[{"uri":file_uri_for_test(workspace.path())}]
            }
        }),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","method":"notifications/unknown","params":{}}),
        json!({
            "jsonrpc":"2.0",
            "method":"tools/call",
            "params":{
                "name":"create_file",
                "arguments":{
                    "path":notification_target.to_str().unwrap(),
                    "content":"must not be written"
                }
            }
        }),
        json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
    ]
    .into_iter()
    .map(|value| value.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";

    let output = run_binary_with_input(workspace.path(), input.as_bytes());
    assert!(output.status.success());
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| line.trim_start().starts_with('{'))
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].get("id").and_then(Value::as_i64), Some(1));
    assert_eq!(
        responses[0]
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str),
        Some("2025-06-18")
    );
    assert_eq!(responses[1].get("id").and_then(Value::as_i64), Some(2));
    assert_eq!(responses[1].get("result"), Some(&json!({})));
    assert!(!notification_target.exists());
}

#[test]
fn test_cancelled_text_search_sends_no_response_and_ping_stays_responsive() {
    let workspace = tempdir().unwrap();
    for index in 0..2_000 {
        fs::write(
            workspace.path().join(format!("file-{index:04}.txt")),
            "haystack without the requested token\n".repeat(20),
        )
        .unwrap();
    }

    let exe = server_binary();
    let mut command = Command::new(&exe);
    isolate_server_environment(&mut command);
    let mut child = command
        .current_dir(workspace.path())
        .env(
            "CODELOUPE_MCP_INDEX_DIR",
            workspace.path().join("codeloupe-test-index"),
        )
        .env("CODELOUPE_MCP_INDEX_MODE", "auto")
        .env("LOCALAPPDATA", workspace.path().join("fake-local-app-data"))
        .env("APPDATA", workspace.path().join("fake-app-data"))
        .env("USERPROFILE", workspace.path().join("fake-home/nested"))
        .env("HOME", workspace.path().join("fake-home/nested"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let started_at = std::time::Instant::now();
    for message in [
        json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{
                "protocolVersion":"2025-06-18",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"0"}
            }
        }),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params":{
                "name":"text_search",
                "arguments":{
                    "query":"needle-that-does-not-exist",
                    "paths":[workspace.path().to_str().unwrap()],
                    "allow_expensive_fallback":true
                }
            }
        }),
        json!({
            "jsonrpc":"2.0",
            "method":"notifications/cancelled",
            "params":{"requestId":2,"reason":"test"}
        }),
        json!({"jsonrpc":"2.0","id":3,"method":"ping"}),
    ] {
        writeln!(stdin, "{message}").unwrap();
    }
    stdin.flush().unwrap();

    let mut responses = Vec::new();
    while !responses
        .iter()
        .any(|response: &Value| response.get("id").and_then(Value::as_i64) == Some(3))
    {
        let mut line = String::new();
        assert!(stdout.read_line(&mut line).unwrap() > 0);
        if let Ok(response) = serde_json::from_str::<Value>(&line) {
            responses.push(response);
        }
    }
    assert!(started_at.elapsed() < Duration::from_secs(2));

    thread::sleep(Duration::from_millis(300));
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":4,"method":"ping"})).unwrap();
    stdin.flush().unwrap();
    while !responses
        .iter()
        .any(|response| response.get("id").and_then(Value::as_i64) == Some(4))
    {
        let mut line = String::new();
        assert!(stdout.read_line(&mut line).unwrap() > 0);
        if let Ok(response) = serde_json::from_str::<Value>(&line) {
            responses.push(response);
        }
    }

    drop(stdin);
    let mut remaining = String::new();
    stdout.read_to_string(&mut remaining).unwrap();
    responses.extend(
        remaining
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok()),
    );
    assert!(child.wait().unwrap().success());
    assert!(
        responses
            .iter()
            .all(|response| response.get("id").and_then(Value::as_i64) != Some(2))
    );
}

#[test]
fn test_initialized_index_loading_does_not_block_ping() {
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("lib.rs"), "fn main() {}\n").unwrap();

    let exe = server_binary();
    let mut command = Command::new(&exe);
    isolate_server_environment(&mut command);
    let mut child = command
        .current_dir(workspace.path())
        .env(
            "CODELOUPE_MCP_INDEX_DIR",
            workspace.path().join("codeloupe-test-index"),
        )
        .env("CODELOUPE_MCP_INDEX_MODE", "auto")
        .env("CODELOUPE_MCP_WORKSPACE", workspace.path())
        .env("CODELOUPE_MCP_TEST_INDEX_LOAD_DELAY_MS", "3000")
        .env("LOCALAPPDATA", workspace.path().join("fake-local-app-data"))
        .env("APPDATA", workspace.path().join("fake-app-data"))
        .env("USERPROFILE", workspace.path().join("fake-home/nested"))
        .env("HOME", workspace.path().join("fake-home/nested"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{
                "protocolVersion":"2025-06-18",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"0"}
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let _ = read_rpc_response(&mut stdout, 1);

    let started_at = std::time::Instant::now();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":2,"method":"ping"})).unwrap();
    stdin.flush().unwrap();
    let ping = read_rpc_response(&mut stdout, 2);
    assert_eq!(ping.get("result"), Some(&json!({})));
    assert!(
        started_at.elapsed() < Duration::from_secs(1),
        "ping was blocked by startup index loading for {:?}",
        started_at.elapsed()
    );

    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn test_runtime_index_cache_respects_loaded_workspace_budget() {
    let current_dir = tempdir().unwrap();
    let workspaces = (0..3)
        .map(|index| {
            let root = current_dir.path().join(format!("workspace-{index}"));
            fs::create_dir_all(root.join(".git")).unwrap();
            fs::write(
                root.join("lib.rs"),
                format!("fn workspace_{index}() {{}}\n"),
            )
            .unwrap();
            root
        })
        .collect::<Vec<_>>();
    let mut args = Vec::new();
    for root in &workspaces {
        args.push("--workspace".to_string());
        args.push(root.to_string_lossy().into_owned());
    }

    let health = call_binary_server_with_options(
        current_dir.path(),
        json!({
            "protocolVersion":"2025-06-18",
            "capabilities":{},
            "clientInfo":{"name":"test","version":"0"}
        }),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask".to_string()),
            ("CODELOUPE_MCP_INDEX_MAX_LOADED_WORKSPACES", "2".to_string()),
        ],
        &args,
    );

    assert_eq!(
        health
            .get("configured_workspaces")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(3)
    );
    assert_eq!(
        health.get("index_workspace_count").and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        health
            .pointer("/index_gc_policy/max_loaded_workspaces")
            .and_then(Value::as_u64),
        Some(2)
    );
}

#[test]
fn test_binary_bounds_and_reports_malformed_framed_requests() {
    let dir = tempdir().unwrap();

    let missing_length = run_binary_with_input(dir.path(), b"X-Test: y\r\n\r\n");
    assert!(missing_length.status.success());
    let missing_stdout = String::from_utf8(missing_length.stdout).unwrap();
    assert!(missing_stdout.contains("Missing Content-Length"));
    assert!(missing_stdout.contains("\"code\":-32700"));

    let oversized = run_binary_with_input(dir.path(), b"Content-Length: 17000000\r\n\r\n");
    assert!(oversized.status.success());
    let oversized_stdout = String::from_utf8(oversized.stdout).unwrap();
    assert!(oversized_stdout.contains("MCP payload exceeds"));
    assert!(oversized_stdout.contains("\"code\":-32700"));

    let mut invalid_utf8 = b"Content-Length: 1\r\n\r\n".to_vec();
    invalid_utf8.push(0xff);
    invalid_utf8.extend_from_slice(
        format!(
            "{}\n",
            json!({
                "jsonrpc":"2.0",
                "id":3,
                "method":"initialize",
                "params":{
                    "protocolVersion":"2024-11-05",
                    "capabilities":{},
                    "clientInfo":{"name":"test","version":"0"}
                }
            })
        )
        .as_bytes(),
    );
    let invalid_output = run_binary_with_input(dir.path(), &invalid_utf8);
    assert!(invalid_output.status.success());
    let invalid_stdout = String::from_utf8(invalid_output.stdout).unwrap();
    assert!(invalid_stdout.contains("Invalid UTF-8 in framed payload"));
    assert!(invalid_stdout.contains("\"id\":3"));
}

fn call_binary_server(current_dir: &Path, initialize_params: Value) -> Value {
    let wait_for_workspace = codeloupe_mcp::common::looks_like_workspace_root(current_dir)
        || ["workspaceFolders", "roots"].into_iter().any(|field| {
            initialize_params
                .get(field)
                .and_then(Value::as_array)
                .is_some_and(|roots| !roots.is_empty())
        });
    call_binary_server_with_options_and_wait(
        current_dir,
        initialize_params,
        &[("CODELOUPE_MCP_INDEX_MODE", "auto".to_string())],
        &[],
        wait_for_workspace,
    )
}

fn call_binary_server_with_options(
    current_dir: &Path,
    initialize_params: Value,
    extra_env: &[(&str, String)],
    args: &[String],
) -> Value {
    call_binary_server_with_options_and_wait(current_dir, initialize_params, extra_env, args, false)
}

fn call_binary_server_with_options_and_wait(
    current_dir: &Path,
    initialize_params: Value,
    extra_env: &[(&str, String)],
    args: &[String],
    wait_for_workspace: bool,
) -> Value {
    let exe = server_binary();
    let mut command = Command::new(&exe);
    isolate_server_environment(&mut command);
    command
        .current_dir(current_dir)
        .args(args)
        .env_remove("CODELOUPE_MCP_INDEX_MODE")
        .env(
            "CODELOUPE_MCP_INDEX_DIR",
            current_dir.join("codeloupe-test-index"),
        )
        .env("LOCALAPPDATA", current_dir.join("fake-local-app-data"))
        .env("APPDATA", current_dir.join("fake-app-data"))
        .env("USERPROFILE", current_dir.join("fake-home/nested"))
        .env("HOME", current_dir.join("fake-home/nested"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":initialize_params})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    stdin.flush().unwrap();
    let _ = read_rpc_response(&mut stdout, 1);

    let mut result = Value::Null;
    for request_id in 2..=(2 + INDEX_SETTLE_MAX_POLLS) {
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":request_id,"method":"tools/call","params":{"name":"server_health","arguments":{}}})
        )
        .unwrap();
        stdin.flush().unwrap();
        result = decode_tool_rpc_response(&read_rpc_response(&mut stdout, request_id));
        if !health_indexing_in_progress(&result, wait_for_workspace) {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "server exited unsuccessfully while polling health"
    );
    result
}

fn call_binary_server_tool_then_health(
    current_dir: &Path,
    initialize_params: Value,
    extra_env: &[(&str, &str)],
    tool_name: &str,
    tool_arguments: Value,
    settle_ms: u64,
) -> (Value, Value) {
    let exe = server_binary();
    let mut command = Command::new(&exe);
    isolate_server_environment(&mut command);
    command
        .current_dir(current_dir)
        .env(
            "CODELOUPE_MCP_INDEX_DIR",
            current_dir.join("codeloupe-test-index"),
        )
        .env("CODELOUPE_MCP_INDEX_MODE", "auto")
        .env("LOCALAPPDATA", current_dir.join("fake-local-app-data"))
        .env("APPDATA", current_dir.join("fake-app-data"))
        .env("USERPROFILE", current_dir.join("fake-home/nested"))
        .env("HOME", current_dir.join("fake-home/nested"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":initialize_params})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    stdin.flush().unwrap();
    thread::sleep(Duration::from_millis(settle_ms));
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool_name,"arguments":tool_arguments}})
    )
    .unwrap();
    stdin.flush().unwrap();

    let tool_rpc = read_rpc_response(&mut stdout, 2);
    let tool_result = decode_tool_rpc_response(&tool_rpc);
    let scheduled_index = tool_result.get("outcome").and_then(Value::as_str) == Some("scheduled");
    let mut request_id = 3;
    let health_rpc = loop {
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":request_id,"method":"tools/call","params":{"name":"server_health","arguments":{}}})
        )
        .unwrap();
        stdin.flush().unwrap();
        let health_rpc = read_rpc_response(&mut stdout, request_id);
        let health = decode_tool_rpc_response(&health_rpc);
        if !health_indexing_in_progress(&health, scheduled_index)
            || request_id == 3 + INDEX_SETTLE_MAX_POLLS
        {
            break health_rpc;
        }
        request_id += 1;
        thread::sleep(Duration::from_millis(250));
    };

    drop(stdin);
    assert!(child.wait().unwrap().success());

    (tool_result, decode_tool_rpc_response(&health_rpc))
}

fn read_rpc_response(stdout: &mut BufReader<impl Read>, request_id: i64) -> Value {
    loop {
        let response = read_next_json_rpc(stdout);
        if response.get("id").and_then(Value::as_i64) == Some(request_id) {
            return response;
        }
    }
}

fn read_next_json_rpc(stdout: &mut BufReader<impl Read>) -> Value {
    loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "server closed stdout before the next JSON-RPC message"
        );
        if let Ok(response) = serde_json::from_str::<Value>(&line) {
            return response;
        }
    }
}

fn health_indexing_in_progress(health: &Value, wait_for_scheduled_index: bool) -> bool {
    let workspace_count = health
        .get("index_workspace_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let candidate_is_starting = workspace_count == 0
        && health
            .get("index_candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate.get("status").and_then(Value::as_str) == Some("indexing")
                })
            });
    let runtime_is_refreshing = health
        .get("index_workspaces")
        .and_then(Value::as_array)
        .is_some_and(|workspaces| {
            workspaces.iter().any(|workspace| {
                workspace.get("refresh_running").and_then(Value::as_bool) == Some(true)
            })
        });
    let waiting_for_scheduled_index = wait_for_scheduled_index && workspace_count == 0;
    candidate_is_starting || runtime_is_refreshing || waiting_for_scheduled_index
}

fn call_binary_server_tools(
    current_dir: &Path,
    initialize_params: Value,
    tool_calls: Vec<(&str, Value)>,
    settle_ms: u64,
) -> Vec<Value> {
    call_binary_server_tools_with_options(
        current_dir,
        initialize_params,
        tool_calls,
        settle_ms,
        &[],
    )
}

fn call_binary_server_tools_with_options(
    current_dir: &Path,
    initialize_params: Value,
    tool_calls: Vec<(&str, Value)>,
    settle_ms: u64,
    extra_env: &[(&str, &str)],
) -> Vec<Value> {
    let exe = server_binary();
    let mut command = Command::new(&exe);
    isolate_server_environment(&mut command);
    command
        .current_dir(current_dir)
        .env(
            "CODELOUPE_MCP_INDEX_DIR",
            current_dir.join("codeloupe-test-index"),
        )
        .env("CODELOUPE_MCP_INDEX_MODE", "auto")
        .env("LOCALAPPDATA", current_dir.join("fake-local-app-data"))
        .env("APPDATA", current_dir.join("fake-app-data"))
        .env("USERPROFILE", current_dir.join("fake-home/nested"))
        .env("HOME", current_dir.join("fake-home/nested"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":initialize_params})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    stdin.flush().unwrap();
    thread::sleep(Duration::from_millis(settle_ms));

    let mut responses = Vec::new();
    let mut workspace_index_scheduled = false;
    for (index, (tool_name, tool_arguments)) in tool_calls.into_iter().enumerate() {
        let request_id = index as i64 + 2;
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":request_id,"method":"tools/call","params":{"name":tool_name,"arguments":tool_arguments}})
        )
        .unwrap();
        stdin.flush().unwrap();

        let response = read_rpc_response(&mut stdout, request_id);
        let mut decoded = decode_tool_rpc_response(&response);
        if tool_name == "server_health" && workspace_index_scheduled {
            for attempt in 0..INDEX_SETTLE_MAX_POLLS {
                if !health_indexing_in_progress(&decoded, true) {
                    break;
                }
                thread::sleep(Duration::from_millis(250));
                let poll_id = 10_000 + (index as i64 * 20) + attempt;
                writeln!(
                    stdin,
                    "{}",
                    json!({"jsonrpc":"2.0","id":poll_id,"method":"tools/call","params":{"name":"server_health","arguments":{}}})
                )
                .unwrap();
                stdin.flush().unwrap();
                decoded = decode_tool_rpc_response(&read_rpc_response(&mut stdout, poll_id));
            }
            workspace_index_scheduled = false;
        }
        if tool_name == "workspace_index"
            && decoded.get("outcome").and_then(Value::as_str) == Some("scheduled")
        {
            workspace_index_scheduled = true;
        }
        responses.push(decoded);
    }

    drop(stdin);
    assert!(child.wait().unwrap().success());
    responses
}

fn decode_tool_rpc_response(rpc: &Value) -> Value {
    let result = rpc.get("result").unwrap();
    let text = result
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|v| v.as_str())
        .unwrap();
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        if let Ok(Value::Object(mut payload)) = serde_json::from_str::<Value>(text) {
            payload.insert("is_error".to_string(), Value::Bool(true));
            payload.insert("message".to_string(), Value::String(text.to_string()));
            return Value::Object(payload);
        }
        return json!({"is_error": true, "message": text});
    }

    serde_json::from_str(text).unwrap()
}

fn file_uri_for_test(path: &Path) -> String {
    format!(
        "file:///{}",
        path.to_string_lossy()
            .replace('\\', "/")
            .replace(' ', "%20")
    )
}

fn canonical_display_path(path: &Path) -> String {
    codeloupe_mcp::common::normalize_display_path(
        &codeloupe_mcp::common::canonicalize_with_existing_ancestor(path),
    )
}

fn create_directory_link(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        let output = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to create junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[tokio::test]
async fn test_convert_file_format_writes_real_utf16le() {
    let dir = tempdir().unwrap();
    codeloupe_mcp::indexer::ensure_workspace_index(
        dir.path().to_path_buf(),
        "write_tool_test".to_string(),
    );
    codeloupe_mcp::workspace_control::register_configured_workspace(
        dir.path().to_path_buf(),
        "write_tool_test",
        true,
    );
    let path = dir.path().join("convert.txt");
    fs::write(&path, "line-a\nline-b\n").unwrap();

    let result = convert_file_format::execute(&json!({
        "path": path.to_str().unwrap(),
        "target_encoding": "UTF-16LE",
        "target_line_ending": "crlf"
    }))
    .await
    .unwrap();

    assert_eq!(result.get("success").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        result.get("target_encoding").and_then(|v| v.as_str()),
        Some("UTF-16LE")
    );

    let bytes = fs::read(&path).unwrap();
    assert!(bytes.starts_with(&[0xFF, 0xFE]));

    let read_back = read_file::execute(&json!({
        "path": path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        read_back.get("content").and_then(|v| v.as_str()),
        Some("line-a\nline-b\n")
    );
    assert_eq!(read_back.get("bom").and_then(|v| v.as_bool()), Some(true));
}

#[tokio::test]
async fn test_trailing_newline_line_counts_are_not_off_by_one() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("multi.txt");
    fs::write(&path, "first\nsecond\nthird\n").unwrap();

    let read_result = read_file::execute(&json!({
        "path": path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        read_result.get("total_lines").and_then(|v| v.as_u64()),
        Some(3)
    );
    assert_eq!(
        read_result.get("returned_lines").and_then(|v| v.as_u64()),
        Some(3)
    );
    assert_eq!(
        read_result.get("content").and_then(|v| v.as_str()),
        Some("first\nsecond\nthird\n")
    );

    let snippets_result = read_snippets::execute(&json!({
        "requests": [{"path": path.to_str().unwrap()}]
    }))
    .await
    .unwrap();
    let snippet = snippets_result
        .get("results")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .cloned()
        .unwrap();
    assert_eq!(snippet.get("total_lines").and_then(|v| v.as_u64()), Some(3));

    let summary = file_summary::execute(&json!({
        "path": path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(summary.get("lines").and_then(|v| v.as_i64()), Some(3));
}

#[tokio::test]
async fn test_peek_archive_accepts_forward_slash_inner_path() {
    let dir = tempdir().unwrap();
    let archive_path = dir.path().join("sample.zip");
    let file = fs::File::create(&archive_path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    writer
        .start_file("nested\\inside.txt", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"nested archive payload\n").unwrap();
    writer.finish().unwrap();

    let list_result = peek_archive::execute(&json!({
        "archive_path": archive_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    let entries = list_result
        .get("entries")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(
        entries[0].get("name").and_then(|v| v.as_str()),
        Some("nested/inside.txt")
    );
    assert_eq!(
        entries[0].get("size_bytes").and_then(|v| v.as_u64()),
        Some(23)
    );
    assert!(entries[0].get("size").is_none());

    let extract_result = peek_archive::execute(&json!({
        "archive_path": archive_path.to_str().unwrap(),
        "inner_path": "nested/inside.txt"
    }))
    .await
    .unwrap();
    assert_eq!(
        extract_result.get("content").and_then(|v| v.as_str()),
        Some("nested archive payload\n")
    );
}

#[tokio::test]
async fn test_peek_archive_bounds_entry_listing() {
    let dir = tempdir().unwrap();
    let listing_path = dir.path().join("many.zip");
    let listing_file = fs::File::create(&listing_path).unwrap();
    let mut listing_writer = zip::ZipWriter::new(listing_file);
    for index in 0..1_001 {
        listing_writer
            .start_file(
                format!("entries/{index:04}.txt"),
                SimpleFileOptions::default(),
            )
            .unwrap();
    }
    listing_writer.finish().unwrap();

    let list_result = peek_archive::execute(&json!({
        "archive_path": listing_path.to_str().unwrap()
    }))
    .await
    .unwrap();
    assert_eq!(
        list_result
            .get("entries_returned")
            .and_then(|value| value.as_u64()),
        Some(1_000)
    );
    assert_eq!(
        list_result
            .get("entries_complete")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    assert!(list_result.get("entries_truncated").is_none());
    assert_eq!(
        list_result
            .get("total_entries")
            .and_then(|value| value.as_u64()),
        Some(1_001)
    );
}

#[tokio::test]
async fn test_peek_archive_bounds_inner_file_reads() {
    let dir = tempdir().unwrap();
    let large_path = dir.path().join("large.zip");
    let large_file = fs::File::create(&large_path).unwrap();
    let mut large_writer = zip::ZipWriter::new(large_file);
    large_writer
        .start_file("large.bin", SimpleFileOptions::default())
        .unwrap();
    large_writer
        .write_all(&vec![0u8; 10 * 1024 * 1024 + 1])
        .unwrap();
    large_writer.finish().unwrap();

    let error = peek_archive::execute(&json!({
        "archive_path": large_path.to_str().unwrap(),
        "inner_path": "large.bin"
    }))
    .await
    .unwrap_err();
    assert!(error.to_string().contains("too large"));
}

#[tokio::test]
async fn test_workspace_stats_reports_total_and_per_language_lines() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "fn one() {}\nfn two() {}\n").unwrap();
    fs::write(
        dir.path().join("main.py"),
        "print('a')\nprint('b')\nprint('c')\n",
    )
    .unwrap();

    let result = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap()
    }))
    .await
    .unwrap();

    assert_eq!(result.get("total_lines").and_then(|v| v.as_u64()), Some(5));
    assert_eq!(
        result.get("complete").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        result
            .get("limit_reached")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    assert!(result.get("truncated").is_none());
    assert!(result.get("diagnostics").is_none());
    let breakdown = result
        .get("languages_breakdown")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(breakdown.iter().any(|item| {
        item.get("language").and_then(|v| v.as_str()) == Some("Rust")
            && item.get("lines").and_then(|v| v.as_u64()) == Some(2)
    }));
    assert!(breakdown.iter().any(|item| {
        item.get("language").and_then(|v| v.as_str()) == Some("Python")
            && item.get("lines").and_then(|v| v.as_u64()) == Some(3)
    }));

    let limited = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "max_files": 1
    }))
    .await
    .unwrap();
    assert_eq!(
        limited.get("complete").and_then(|value| value.as_bool()),
        Some(false)
    );
    assert_eq!(
        limited
            .get("limit_reached")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        limited.get("limit_reason").and_then(|value| value.as_str()),
        Some("max_files")
    );
}

#[tokio::test]
async fn test_workspace_stats_distinguishes_walked_and_matched_files() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "fn one() {}\n").unwrap();
    fs::write(dir.path().join("notes.txt"), "notes\n").unwrap();

    let result = workspace_stats::execute(&json!({
        "path": dir.path().to_str().unwrap(),
        "includes": ["**/*.rs"],
        "verbose": true
    }))
    .await
    .unwrap();

    let files_walked = result
        .pointer("/diagnostics/files_walked")
        .and_then(Value::as_u64)
        .unwrap();
    let total_files = result.get("total_files").and_then(Value::as_u64).unwrap();
    let skipped_files = result
        .pointer("/diagnostics/files_skipped_by_patterns")
        .and_then(Value::as_u64)
        .unwrap();
    assert_eq!(files_walked, total_files + skipped_files);
    assert_eq!(total_files, 1);
    assert!(result.get("files_seen").is_none());
}

#[tokio::test]
async fn test_batch_tool_call_flattens_inner_tool_payloads() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.txt");
    fs::write(&path, "hello\n").unwrap();

    let result = batch_tool_call::execute(&json!({
        "calls": [{
            "tool": "resolve_path",
            "args": { "path": path.to_str().unwrap() }
        }]
    }))
    .await
    .unwrap();

    let first = result
        .get("results")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .cloned()
        .unwrap();
    assert_eq!(first.get("status").and_then(|v| v.as_str()), Some("ok"));
    assert!(
        first
            .get("result")
            .and_then(|v| v.get("canonical_path"))
            .is_some()
    );
    assert!(first.get("result").and_then(|v| v.get("content")).is_none());
}

#[tokio::test]
async fn test_batch_tool_call_dispatches_summary_first() {
    let response = tools::call_tool(json!({
        "name": "batch_tool_call",
        "arguments": { "calls": [] }
    }))
    .await
    .unwrap();
    let text = response
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap();

    assert!(
        text.starts_with("{\"summary\":"),
        "unexpected batch text: {text}"
    );
}

#[tokio::test]
async fn test_batch_tool_call_bounds_large_results_and_preserves_later_calls() {
    let dir = tempdir().unwrap();
    let large_path = dir.path().join("large.txt");
    let small_path = dir.path().join("small.txt");
    fs::write(&large_path, "large payload line\n".repeat(2_000)).unwrap();
    fs::write(&small_path, "small\n").unwrap();

    let result = batch_tool_call::execute(&json!({
        "max_output_bytes": 2048,
        "calls": [
            {
                "tool": "read_file_range",
                "args": {"path": large_path.to_str().unwrap(), "max_lines": 2000},
                "max_output_bytes": 256
            },
            {
                "tool": "resolve_path",
                "args": {"path": small_path.to_str().unwrap()}
            }
        ]
    }))
    .await
    .unwrap();

    let summary = result.get("summary").and_then(Value::as_array).unwrap();
    let results = result.get("results").and_then(Value::as_array).unwrap();
    assert_eq!(summary.len(), 2);
    assert_eq!(results.len(), 2);
    assert_eq!(
        summary[0].get("truncated").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        results[0].get("truncated").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(results[1].get("status").and_then(Value::as_str), Some("ok"));
    assert!(
        results[1]
            .get("result")
            .and_then(|value| value.get("canonical_path"))
            .is_some()
    );
    assert!(
        result
            .get("output_bytes_used")
            .and_then(Value::as_u64)
            .is_some_and(|bytes| bytes <= 2048)
    );
}

#[tokio::test]
async fn test_batch_tool_call_bounds_large_errors_and_preserves_later_calls() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    let result = batch_tool_call::execute(&json!({
        "max_output_bytes": 2048,
        "calls": [
            {
                "tool": "get_call_graph",
                "args": {
                    "file_path": source.to_str().unwrap(),
                    "symbol": "missing_function_".to_string() + &("\n\"\\x".repeat(1_000))
                },
                "max_output_bytes": 128
            },
            {
                "tool": "resolve_path",
                "args": {"path": source.to_str().unwrap()}
            }
        ]
    }))
    .await
    .unwrap();

    let summary = result.get("summary").and_then(Value::as_array).unwrap();
    let results = result.get("results").and_then(Value::as_array).unwrap();
    assert_eq!(
        summary[0].get("truncated").and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        results[0]
            .get("error")
            .is_some_and(|error| serde_json::to_vec(error).unwrap().len() <= 128)
    );
    assert!(
        summary[0]
            .get("returned_bytes")
            .and_then(Value::as_u64)
            .is_some_and(|bytes| bytes <= 128)
    );
    assert_eq!(results[1].get("status").and_then(Value::as_str), Some("ok"));
    assert!(
        result
            .get("output_bytes_used")
            .and_then(Value::as_u64)
            .is_some_and(|bytes| bytes <= 2048)
    );
}

#[tokio::test]
async fn test_batch_tool_call_preserves_structured_error_codes() {
    let result = batch_tool_call::execute(&json!({
        "calls": [{
            "tool": "text_search",
            "args": { "query": "" }
        }]
    }))
    .await
    .unwrap();

    assert_eq!(
        result
            .pointer("/results/0/error/code")
            .and_then(Value::as_str),
        Some("invalid_argument")
    );
    assert!(
        result
            .pointer("/results/0/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("Query cannot be empty"))
    );
}

#[tokio::test]
async fn test_batch_tool_call_returns_skipped_deadline_without_starting_calls() {
    let dir = tempdir().unwrap();
    let result = batch_tool_call::execute(&json!({
        "deadline_ms": 0,
        "calls": [
            {"tool": "resolve_path", "args": {"path": dir.path().to_str().unwrap()}},
            {"tool": "file_summary", "args": {"path": dir.path().join("missing").to_str().unwrap()}}
        ]
    }))
    .await
    .unwrap();

    assert_eq!(result.get("completed").and_then(Value::as_u64), Some(0));
    assert_eq!(result.get("partial").and_then(Value::as_bool), Some(true));
    assert!(
        result
            .get("summary")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .all(|item| item.get("status").and_then(Value::as_str) == Some("skipped_deadline"))
    );
}

#[test]
fn test_server_health_disables_index_when_process_cwd_is_not_a_workspace() {
    let dir = tempdir().unwrap();
    let result = call_binary_server(dir.path(), json!({}));

    assert_eq!(
        result.get("index_status").and_then(|v| v.as_str()),
        Some("disabled")
    );
    // Null fields are dropped from tool output.
    assert!(result.get("active_index_workspace_source").is_none());
    assert!(result.get("active_index_workspace_root").is_none());
}

#[test]
fn test_default_ask_mode_keeps_client_workspace_as_candidate() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    let result = call_binary_server_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[],
        &[],
    );

    assert_eq!(
        result.get("index_mode").and_then(Value::as_str),
        Some("ask")
    );
    assert_eq!(
        result.get("index_workspace_count").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        result
            .pointer("/configured_workspaces/0/index_approved")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        result
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("candidate")
    );
}

#[test]
fn test_off_mode_reports_candidate_without_indexing() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    let result = call_binary_server_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[("CODELOUPE_MCP_INDEX_MODE", "off".to_string())],
        &[],
    );

    assert_eq!(
        result.get("index_mode").and_then(Value::as_str),
        Some("off")
    );
    assert_eq!(
        result.get("index_workspace_count").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        result
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("mode_off")
    );
}

#[test]
fn test_cli_and_environment_workspaces_are_explicitly_approved() {
    let current_dir = tempdir().unwrap();
    let cli_workspace = tempdir().unwrap();
    let env_workspace = tempdir().unwrap();
    fs::create_dir(cli_workspace.path().join(".git")).unwrap();
    fs::create_dir(env_workspace.path().join(".git")).unwrap();
    let result = call_binary_server_with_options(
        current_dir.path(),
        json!({}),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask".to_string()),
            (
                "CODELOUPE_MCP_WORKSPACE",
                env_workspace.path().to_string_lossy().into_owned(),
            ),
        ],
        &[
            "--workspace".to_string(),
            cli_workspace.path().to_string_lossy().into_owned(),
        ],
    );

    assert_eq!(
        result
            .get("configured_workspaces")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(2)
    );
    assert!(
        result
            .get("configured_workspaces")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .all(
                |workspace| workspace.get("index_approved").and_then(Value::as_bool) == Some(true)
            )
    );
    assert_eq!(
        result.get("index_workspace_count").and_then(Value::as_u64),
        Some(2)
    );
}

#[test]
fn test_metadata_scan_budget_stops_without_persisting_index() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("a.rs"), "fn a() {}\n").unwrap();
    fs::write(workspace.path().join("b.rs"), "fn b() {}\n").unwrap();
    let result = call_binary_server_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "auto".to_string()),
            ("CODELOUPE_MCP_INDEX_MAX_ENTRIES", "1".to_string()),
        ],
        &[],
    );

    assert_eq!(
        result
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("too_large")
    );
    let storage_dir = result
        .pointer("/index_workspaces/0/storage_dir")
        .and_then(Value::as_str)
        .unwrap();
    assert!(!Path::new(storage_dir).exists());

    let decisions_path = current_dir
        .path()
        .join("codeloupe-test-index/index-v2/workspaces.json");
    let decisions: Value = serde_json::from_slice(&fs::read(&decisions_path).unwrap()).unwrap();
    assert_eq!(
        decisions
            .pointer("/workspaces/0/decision")
            .and_then(Value::as_str),
        Some("too_large")
    );

    let reloaded = call_binary_server_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[("CODELOUPE_MCP_INDEX_MODE", "auto".to_string())],
        &[],
    );
    assert_eq!(
        reloaded
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("too_large")
    );
    assert_eq!(
        reloaded
            .get("index_workspace_count")
            .and_then(Value::as_u64),
        Some(0)
    );
}

#[test]
fn test_disk_budget_stops_index_before_storage_is_created() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("lib.rs"), "fn sample() {}\n").unwrap();
    let result = call_binary_server_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "auto".to_string()),
            ("CODELOUPE_MCP_INDEX_MAX_TOTAL_BYTES", "1".to_string()),
        ],
        &[],
    );

    assert_eq!(
        result
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("disk_budget")
    );
    let storage_dir = result
        .pointer("/index_workspaces/0/storage_dir")
        .and_then(Value::as_str)
        .unwrap();
    assert!(!Path::new(storage_dir).exists());
}

#[test]
fn test_blocked_client_root_is_reported_without_indexing() {
    let current_dir = tempdir().unwrap();
    let blocked = current_dir.path().join("fake-home/nested");
    fs::create_dir_all(blocked.join(".git")).unwrap();
    let result = call_binary_server_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(&blocked)}]
        }),
        &[("CODELOUPE_MCP_INDEX_MODE", "auto".to_string())],
        &[],
    );

    assert_eq!(
        result.get("index_workspace_count").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        result
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("blocked_path")
    );
}

#[test]
fn test_application_data_workspace_requires_explicit_approval() {
    let current_dir = tempdir().unwrap();
    let local_app_data = current_dir.path().join("local-app-data");
    let workspace = local_app_data.join("temporary-checkout");
    fs::create_dir_all(workspace.join(".git")).unwrap();
    fs::write(workspace.join("lib.rs"), "fn sample() {}\n").unwrap();
    let local_app_data_text = local_app_data.to_string_lossy().into_owned();

    let automatic = call_binary_server_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(&workspace)}]
        }),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "auto".to_string()),
            ("LOCALAPPDATA", local_app_data_text.clone()),
        ],
        &[],
    );
    assert_eq!(
        automatic
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("approval_required")
    );
    assert_eq!(
        automatic
            .get("index_workspace_count")
            .and_then(Value::as_u64),
        Some(0)
    );

    let configured = call_binary_server_with_options(
        current_dir.path(),
        json!({}),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask".to_string()),
            ("LOCALAPPDATA", local_app_data_text),
        ],
        &[
            "--workspace".to_string(),
            workspace.to_string_lossy().into_owned(),
        ],
    );
    assert_eq!(
        configured
            .pointer("/configured_workspaces/0/index_approved")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        configured
            .get("index_workspace_count")
            .and_then(Value::as_u64),
        Some(1)
    );
}

#[test]
fn test_application_data_estimate_requires_approval_and_explicit_enable_succeeds() {
    let current_dir = tempdir().unwrap();
    let local_app_data = current_dir.path().join("local-app-data");
    let workspace = local_app_data.join("temporary-checkout");
    fs::create_dir_all(workspace.join(".git")).unwrap();
    fs::write(workspace.join("lib.rs"), "fn sample() {}\n").unwrap();
    let local_app_data_text = local_app_data.to_string_lossy().into_owned();
    let workspace_text = workspace.to_string_lossy().into_owned();

    let responses = call_binary_server_tools_with_options(
        current_dir.path(),
        json!({}),
        vec![
            (
                "workspace_index",
                json!({"action": "estimate", "path": workspace_text.clone()}),
            ),
            (
                "workspace_index",
                json!({"action": "enable", "path": workspace_text}),
            ),
            ("server_health", json!({})),
        ],
        50,
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("LOCALAPPDATA", local_app_data_text.as_str()),
        ],
    );

    assert_eq!(
        responses[0]
            .pointer("/estimate/recommendation")
            .and_then(Value::as_str),
        Some("approval_required")
    );
    assert_eq!(
        responses[1].get("outcome").and_then(Value::as_str),
        Some("scheduled")
    );
    assert_eq!(
        responses[1]
            .get("approval_recorded")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        responses[2]
            .get("index_workspace_count")
            .and_then(Value::as_u64),
        Some(1)
    );
}

#[test]
fn test_exact_temp_root_remains_blocked_when_explicitly_configured() {
    let current_dir = tempdir().unwrap();
    let temp_root = current_dir.path().join("configured-temp-root");
    fs::create_dir_all(temp_root.join(".git")).unwrap();
    fs::write(temp_root.join("lib.rs"), "fn sample() {}\n").unwrap();
    let temp_root_text = temp_root.to_string_lossy().into_owned();

    let result = call_binary_server_with_options(
        current_dir.path(),
        json!({}),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask".to_string()),
            ("TEMP", temp_root_text.clone()),
            ("TMP", temp_root_text.clone()),
            ("TMPDIR", temp_root_text),
        ],
        &[
            "--workspace".to_string(),
            temp_root.to_string_lossy().into_owned(),
        ],
    );
    assert_eq!(
        result.get("index_workspace_count").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        result
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("blocked_path")
    );
}

#[test]
fn test_exact_temp_root_is_blocked_for_estimate_and_enable() {
    let current_dir = tempdir().unwrap();
    let temp_root = current_dir.path().join("configured-temp-root");
    fs::create_dir_all(temp_root.join(".git")).unwrap();
    fs::write(temp_root.join("lib.rs"), "fn sample() {}\n").unwrap();
    let temp_root_text = temp_root.to_string_lossy().into_owned();

    let responses = call_binary_server_tools_with_options(
        current_dir.path(),
        json!({}),
        vec![
            (
                "workspace_index",
                json!({"action": "estimate", "path": temp_root_text.clone()}),
            ),
            (
                "workspace_index",
                json!({"action": "enable", "path": temp_root_text.clone()}),
            ),
            ("server_health", json!({})),
        ],
        50,
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("TEMP", temp_root_text.as_str()),
            ("TMP", temp_root_text.as_str()),
            ("TMPDIR", temp_root_text.as_str()),
        ],
    );

    assert_eq!(
        responses[0]
            .pointer("/estimate/recommendation")
            .and_then(Value::as_str),
        Some("blocked")
    );
    assert_eq!(
        responses[1].get("is_error").and_then(Value::as_bool),
        Some(true)
    );
    let message = responses[1].get("message").and_then(Value::as_str).unwrap();
    let estimate: Value = serde_json::from_str(message.trim_start_matches("Error: ")).unwrap();
    assert_eq!(
        estimate.get("recommendation").and_then(Value::as_str),
        Some("blocked")
    );
    assert_eq!(
        responses[2]
            .get("index_workspace_count")
            .and_then(Value::as_u64),
        Some(0)
    );
}

#[test]
fn test_workspace_index_enable_is_persisted_and_reloaded() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("lib.rs"), "fn sample() {}\n").unwrap();
    let workspace_text = workspace.path().to_string_lossy().into_owned();
    let (enable, health) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[("CODELOUPE_MCP_INDEX_MODE", "ask")],
        "workspace_index",
        json!({"action": "enable", "path": workspace_text}),
        500,
    );
    assert_eq!(
        enable.get("approval_recorded").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        health.get("index_workspace_count").and_then(Value::as_u64),
        Some(1)
    );
    assert!(
        current_dir
            .path()
            .join("codeloupe-test-index/index-v2/workspaces.json")
            .is_file()
    );

    let reloaded = call_binary_server_with_options(current_dir.path(), json!({}), &[], &[]);
    assert_eq!(
        reloaded
            .get("index_workspace_count")
            .and_then(Value::as_u64),
        Some(1)
    );
}

#[test]
fn test_workspace_index_estimate_is_read_only() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("lib.rs"), "fn sample() {}\n").unwrap();

    let (estimate, health) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({}),
        &[("CODELOUPE_MCP_INDEX_MODE", "ask")],
        "workspace_index",
        json!({"action": "estimate", "path": workspace.path().to_str().unwrap()}),
        50,
    );

    assert_eq!(
        estimate.get("action").and_then(Value::as_str),
        Some("estimate")
    );
    assert_eq!(
        health.get("index_workspace_count").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        health
            .get("index_storage_entry_count")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert!(
        !current_dir
            .path()
            .join("codeloupe-test-index/index-v2/workspaces.json")
            .exists()
    );
}

#[test]
fn test_workspace_index_rejects_too_large_with_narrower_candidates() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::create_dir(workspace.path().join("src")).unwrap();
    fs::create_dir(workspace.path().join("tests")).unwrap();
    fs::write(workspace.path().join("src/lib.rs"), "fn sample() {}\n").unwrap();
    fs::write(
        workspace.path().join("tests/test.rs"),
        "fn test_sample() {}\n",
    )
    .unwrap();

    let (result, health) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({}),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("CODELOUPE_MCP_INDEX_MAX_ENTRIES", "2"),
        ],
        "workspace_index",
        json!({"action": "enable", "path": workspace.path().to_str().unwrap()}),
        50,
    );

    assert_eq!(result.get("is_error").and_then(Value::as_bool), Some(true));
    let message = result.get("message").and_then(Value::as_str).unwrap();
    let estimate: Value = serde_json::from_str(message.trim_start_matches("Error: ")).unwrap();
    assert_eq!(
        estimate.get("recommendation").and_then(Value::as_str),
        Some("too_large")
    );
    assert!(
        estimate
            .get("narrower_candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| !candidates.is_empty())
    );
    assert_eq!(
        health
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("too_large")
    );
    assert_eq!(
        health.get("index_workspace_count").and_then(Value::as_u64),
        Some(0)
    );
}

#[test]
fn test_workspace_index_disable_removes_index_and_suppresses_advice() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("lib.rs"), "fn sample() {}\n").unwrap();
    let workspace_text = workspace.path().to_string_lossy().into_owned();

    let (_enable, enabled_health) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({}),
        &[("CODELOUPE_MCP_INDEX_MODE", "ask")],
        "workspace_index",
        json!({"action": "enable", "path": workspace_text}),
        50,
    );
    assert_eq!(
        enabled_health
            .get("index_workspace_count")
            .and_then(Value::as_u64),
        Some(1)
    );

    let (disabled, disabled_health) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({}),
        &[("CODELOUPE_MCP_INDEX_MODE", "off")],
        "workspace_index",
        json!({"action": "disable", "path": workspace.path().to_str().unwrap()}),
        50,
    );
    assert_eq!(
        disabled.get("outcome").and_then(Value::as_str),
        Some("disabled")
    );
    assert_eq!(
        disabled_health
            .get("index_workspace_count")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        disabled_health
            .get("index_storage_entry_count")
            .and_then(Value::as_u64),
        Some(0)
    );

    let (stats, health) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("CODELOUPE_MCP_INDEX_ADVICE_MIN_FILES", "1"),
        ],
        "workspace_stats",
        json!({"path": workspace.path().to_str().unwrap()}),
        50,
    );
    assert!(stats.get("index_advice").is_none());
    assert_eq!(
        health
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("declined")
    );
}

#[test]
fn test_workspace_index_disable_releases_same_process_handles() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let source_dir = workspace.path().join("src");
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::create_dir(&source_dir).unwrap();
    fs::write(source_dir.join("lib.rs"), "fn sample() {}\n").unwrap();
    let workspace_text = workspace.path().to_string_lossy().into_owned();
    let source_text = source_dir.to_string_lossy().into_owned();

    let responses = call_binary_server_tools_with_options(
        current_dir.path(),
        json!({}),
        vec![
            (
                "workspace_index",
                json!({"action": "enable", "path": workspace_text}),
            ),
            (
                "warm_content_index",
                json!({
                    "paths": [source_text],
                    "include_ignored": false,
                    "wait_ms": 30_000
                }),
            ),
            (
                "workspace_index",
                json!({"action": "disable", "path": workspace.path().to_str().unwrap()}),
            ),
            ("server_health", json!({})),
        ],
        50,
        &[("CODELOUPE_MCP_INDEX_MODE", "ask")],
    );

    assert_eq!(
        responses[0].get("outcome").and_then(Value::as_str),
        Some("scheduled")
    );
    assert_eq!(
        responses[1].get("outcome").and_then(Value::as_str),
        Some("ready")
    );
    assert_eq!(
        responses[2].get("outcome").and_then(Value::as_str),
        Some("disabled")
    );
    assert_eq!(
        responses[3]
            .get("index_workspace_count")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        responses[3]
            .get("index_storage_entry_count")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        responses[3]
            .pointer("/configured_workspaces/0/index_approved")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        responses[3]
            .pointer("/index_candidates/0/status")
            .and_then(Value::as_str),
        Some("declined")
    );
}

#[test]
fn test_large_scan_response_attaches_index_advice_once_per_session() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("lib.rs"), "fn sample() {}\n").unwrap();
    let responses = call_binary_server_tools_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        vec![
            (
                "workspace_stats",
                json!({"path": workspace.path().to_str().unwrap()}),
            ),
            (
                "workspace_stats",
                json!({"path": workspace.path().to_str().unwrap()}),
            ),
        ],
        50,
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("CODELOUPE_MCP_INDEX_ADVICE_MIN_FILES", "1"),
        ],
    );

    assert_eq!(
        responses[0]
            .pointer("/index_advice/recommendation")
            .and_then(Value::as_str),
        Some("index")
    );
    assert!(responses[1].get("index_advice").is_none());
}

#[test]
fn test_hidden_scan_diagnostics_still_feed_index_advice_and_health() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    for index in 0..3 {
        fs::write(
            workspace.path().join(format!("file-{index}.rs")),
            "fn sample() {}\n",
        )
        .unwrap();
    }
    let workspace_text = workspace.path().to_string_lossy().into_owned();
    let responses = call_binary_server_tools_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        vec![
            (
                "fuzzy_find",
                json!({
                    "pattern": "no-match",
                    "paths": [workspace_text],
                    "max_results": 10
                }),
            ),
            ("server_health", json!({})),
        ],
        50,
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("CODELOUPE_MCP_INDEX_ADVICE_MIN_FILES", "3"),
        ],
    );

    assert!(responses[0].get("diagnostics").is_none());
    assert_eq!(
        responses[0]
            .pointer("/index_advice/recommendation")
            .and_then(Value::as_str),
        Some("index")
    );
    assert!(
        responses[1]
            .pointer("/index_candidates/0/files_seen")
            .and_then(Value::as_u64)
            .is_some_and(|count| count >= 3)
    );
}

#[test]
fn test_repeated_small_workspace_scans_do_not_emit_index_advice() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    for index in 0..3 {
        fs::write(
            workspace.path().join(format!("file-{index}.rs")),
            "fn sample() {}\n",
        )
        .unwrap();
    }
    let workspace_text = workspace.path().to_string_lossy().into_owned();
    let responses = call_binary_server_tools_with_options(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        vec![
            (
                "fuzzy_find",
                json!({"pattern": "no-match", "paths": [workspace_text.clone()]}),
            ),
            (
                "fuzzy_find",
                json!({"pattern": "no-match", "paths": [workspace_text.clone()]}),
            ),
            (
                "fuzzy_find",
                json!({"pattern": "no-match", "paths": [workspace_text]}),
            ),
            ("server_health", json!({})),
        ],
        50,
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("CODELOUPE_MCP_INDEX_ADVICE_MIN_FILES", "100"),
        ],
    );

    assert!(
        responses[..3]
            .iter()
            .all(|response| response.get("index_advice").is_none())
    );
}

#[cfg(debug_assertions)]
#[test]
fn test_timeout_only_suppresses_success_advice_but_keeps_timeout_advice() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("lib.rs"), "fn sample() {}\n").unwrap();

    let (success, _) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("CODELOUPE_MCP_INDEX_ADVICE", "timeout_only"),
            ("CODELOUPE_MCP_INDEX_ADVICE_MIN_FILES", "1"),
        ],
        "workspace_stats",
        json!({"path": workspace.path().to_str().unwrap()}),
        50,
    );
    assert!(success.get("index_advice").is_none());

    let (timed_out, _) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("CODELOUPE_MCP_INDEX_ADVICE", "timeout_only"),
            ("CODELOUPE_MCP_INDEX_ADVICE_MIN_FILES", "1"),
            ("CODELOUPE_MCP_TEST_TOOL_TIMEOUT_NAME", "workspace_stats"),
            ("CODELOUPE_MCP_TEST_TOOL_TIMEOUT_MS", "1000"),
            ("CODELOUPE_MCP_TEST_SCAN_DELAY_MS", "3000"),
        ],
        "workspace_stats",
        json!({"path": workspace.path().to_str().unwrap()}),
        50,
    );
    assert_eq!(
        timed_out.get("is_error").and_then(Value::as_bool),
        Some(true)
    );
    let details: Value =
        serde_json::from_str(timed_out.get("message").and_then(Value::as_str).unwrap()).unwrap();
    assert_eq!(
        details.get("error").and_then(Value::as_str),
        Some("tool_timeout")
    );
    assert_eq!(
        details.get("timeout_milliseconds").and_then(Value::as_u64),
        Some(1000)
    );
    assert!(
        details
            .get("files_scanned")
            .and_then(Value::as_u64)
            .is_some_and(|count| count >= 1)
    );
    assert!(details.get("narrower_candidates").is_some());
    assert_eq!(
        details
            .pointer("/index_advice/recommendation")
            .and_then(Value::as_str),
        Some("index")
    );
}

#[cfg(debug_assertions)]
#[test]
fn test_timeout_without_workspace_marker_has_no_index_advice() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join("src")).unwrap();
    fs::write(workspace.path().join("src/lib.rs"), "fn sample() {}\n").unwrap();

    let (timed_out, _) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        &[
            ("CODELOUPE_MCP_INDEX_MODE", "ask"),
            ("CODELOUPE_MCP_INDEX_ADVICE_MIN_FILES", "1"),
            ("CODELOUPE_MCP_TEST_TOOL_TIMEOUT_NAME", "workspace_stats"),
            ("CODELOUPE_MCP_TEST_TOOL_TIMEOUT_MS", "1000"),
            ("CODELOUPE_MCP_TEST_SCAN_DELAY_MS", "3000"),
        ],
        "workspace_stats",
        json!({"path": workspace.path().to_str().unwrap()}),
        50,
    );
    let details: Value =
        serde_json::from_str(timed_out.get("message").and_then(Value::as_str).unwrap()).unwrap();
    assert!(details.get("index_advice").is_none());
    assert!(
        details
            .get("narrower_candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| !candidates.is_empty())
    );
}

#[test]
fn test_tool_path_without_workspace_marker_does_not_register_index() {
    let current_dir = tempdir().unwrap();
    let unmarked_dir = tempdir().unwrap();
    let path = unmarked_dir.path().join("sample.txt");
    fs::write(&path, "unmarked workspace\n").unwrap();

    let (read_result, health) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({}),
        &[],
        "read_file_range",
        json!({ "path": path.to_str().unwrap() }),
        50,
    );

    assert_eq!(
        read_result.get("content").and_then(Value::as_str),
        Some("unmarked workspace\n")
    );
    assert_eq!(
        health.get("index_status").and_then(Value::as_str),
        Some("disabled")
    );
    assert_eq!(
        health.get("index_workspace_count").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        health
            .get("index_storage_entry_count")
            .and_then(Value::as_u64),
        Some(0)
    );
}

#[test]
fn test_server_health_counts_unloaded_orphaned_index_storage() {
    let current_dir = tempdir().unwrap();
    let storage_dir = current_dir
        .path()
        .join("codeloupe-test-index/index-v2/orphan-fixture");
    fs::create_dir_all(&storage_dir).unwrap();
    fs::write(
        storage_dir.join("meta.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "workspace_root": current_dir.path().join("missing-workspace").to_string_lossy(),
            "saved_at": 1,
            "scan_complete": true,
            "indexed_entries_count": 1,
            "indexed_files_count": 1,
            "indexed_dirs_count": 0,
            "last_full_scan_at": 1,
            "content_index_enabled": false,
            "content_index_status": "disabled",
            "content_index_zones": [],
            "content_index_partial": false,
            "indexed_content_files": 0,
            "indexed_content_bytes": 0
        }))
        .unwrap(),
    )
    .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(storage_dir.join("last-used"), now.to_string()).unwrap();
    fs::write(storage_dir.join("payload.bin"), vec![0u8; 256]).unwrap();

    let result = call_binary_server(current_dir.path(), json!({}));

    assert_eq!(
        result
            .get("index_storage_entry_count")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        result
            .get("index_storage_unloaded_count")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        result
            .get("index_storage_orphaned_count")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert!(
        result
            .get("index_storage_size_bytes")
            .and_then(Value::as_u64)
            .is_some_and(|size| size >= 256)
    );
}

#[test]
fn test_startup_gc_removes_expired_orphaned_index() {
    let current_dir = tempdir().unwrap();
    let storage_dir = current_dir
        .path()
        .join("codeloupe-test-index/index-v2/expired-orphan");
    fs::create_dir_all(&storage_dir).unwrap();
    fs::write(
        storage_dir.join("meta.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "workspace_root": current_dir.path().join("deleted-workspace").to_string_lossy(),
            "saved_at": 1,
            "scan_complete": true,
            "indexed_entries_count": 1,
            "indexed_files_count": 1,
            "indexed_dirs_count": 0,
            "last_full_scan_at": 1,
            "content_index_enabled": false,
            "content_index_status": "disabled",
            "content_index_zones": [],
            "content_index_partial": false,
            "indexed_content_files": 0,
            "indexed_content_bytes": 0
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(storage_dir.join("last-used"), "1").unwrap();
    fs::write(storage_dir.join("payload.bin"), vec![0u8; 256]).unwrap();

    let result = call_binary_server(current_dir.path(), json!({}));

    assert!(!storage_dir.exists());
    assert_eq!(
        result
            .get("index_storage_entry_count")
            .and_then(Value::as_u64),
        Some(0)
    );
}

#[test]
fn test_server_health_can_fallback_to_project_like_process_cwd() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"qa\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    let result = call_binary_server(dir.path(), json!({}));

    let status = result.get("index_status").and_then(|v| v.as_str()).unwrap();
    assert!(matches!(status, "idle" | "active"));
    assert_eq!(
        result
            .get("active_index_workspace_source")
            .and_then(|v| v.as_str()),
        Some("process_current_dir")
    );
    let root_value = result
        .get("active_index_workspace_root")
        .and_then(|v| v.as_str())
        .unwrap();
    let actual = Path::new(root_value)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(root_value).to_path_buf());
    assert_eq!(actual, dir.path().canonicalize().unwrap());
}

#[test]
fn test_server_health_uses_client_workspace_root() {
    let current_dir = tempdir().unwrap();
    let workspace_root = tempdir().unwrap();
    let init = json!({
        "workspaceFolders": [
            { "uri": file_uri_for_test(workspace_root.path()) }
        ]
    });
    let result = call_binary_server(current_dir.path(), init);

    assert_eq!(
        result
            .get("active_index_workspace_source")
            .and_then(|v| v.as_str()),
        Some("client_initialize")
    );
    let root_value = result
        .get("active_index_workspace_root")
        .and_then(|v| v.as_str())
        .unwrap();
    let expected = workspace_root.path().canonicalize().unwrap();
    let actual = Path::new(root_value)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(root_value).to_path_buf());
    assert_eq!(actual, expected);

    let storage_dir = result
        .pointer("/index_workspaces/0/storage_dir")
        .and_then(|value| value.as_str())
        .unwrap();
    let expected_index_root = current_dir
        .path()
        .join("codeloupe-test-index")
        .join("index-v2");
    assert!(
        Path::new(storage_dir).starts_with(&expected_index_root),
        "test index escaped fixture directory: {storage_dir}"
    );
}

#[test]
fn test_standard_tantivy_environment_variable_is_honored() {
    let current_dir = tempdir().unwrap();
    fs::create_dir(current_dir.path().join(".git")).unwrap();
    fs::write(current_dir.path().join("lib.rs"), "fn main() {}\n").unwrap();

    let (_, health) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"roots": {"listChanged": true}},
            "clientInfo": {"name": "test", "version": "0"},
            "roots": [{"uri": current_dir.path().to_str().unwrap(), "name": "fixture"}]
        }),
        &[("CODELOUPE_MCP_TANTIVY_ENABLED", "false")],
        "file_summary",
        json!({"path": current_dir.path().join("lib.rs").to_str().unwrap()}),
        50,
    );

    assert_eq!(
        health
            .pointer("/index_workspaces/0/content_index/backend")
            .and_then(Value::as_str),
        Some("disabled")
    );
}

#[test]
fn test_server_health_reports_multiple_client_workspaces() {
    let current_dir = tempdir().unwrap();
    let workspace_a = tempdir().unwrap();
    let workspace_b = tempdir().unwrap();
    let init = json!({
        "workspaceFolders": [
            { "uri": format!("file:///{}", workspace_a.path().to_string_lossy().replace('\\', "/")) },
            { "uri": format!("file:///{}", workspace_b.path().to_string_lossy().replace('\\', "/")) }
        ]
    });

    let result = call_binary_server(current_dir.path(), init);

    assert_eq!(
        result.get("index_workspace_count").and_then(|v| v.as_u64()),
        Some(2)
    );

    let workspaces = result
        .get("index_workspaces")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(workspaces.len(), 2);
    assert!(workspaces.iter().all(|workspace| {
        workspace.get("workspace_source").and_then(|v| v.as_str()) == Some("client_initialize")
    }));

    let mut roots = workspaces
        .iter()
        .filter_map(|workspace| workspace.get("workspace_root").and_then(|v| v.as_str()))
        .map(|root| {
            Path::new(root)
                .canonicalize()
                .unwrap_or_else(|_| Path::new(root).to_path_buf())
        })
        .collect::<Vec<_>>();
    roots.sort();

    let mut expected = vec![
        workspace_a.path().canonicalize().unwrap(),
        workspace_b.path().canonicalize().unwrap(),
    ];
    expected.sort();

    assert_eq!(roots, expected);
}

#[test]
fn test_nested_client_workspaces_share_parent_index() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let nested = workspace.path().join("nested");
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::create_dir_all(&nested).unwrap();
    fs::write(
        nested.join("pyproject.toml"),
        "[project]\nname = 'nested'\n",
    )
    .unwrap();
    let init = json!({
        "workspaceFolders": [
            { "uri": file_uri_for_test(&nested) },
            { "uri": file_uri_for_test(workspace.path()) }
        ]
    });

    let result = call_binary_server(current_dir.path(), init);

    assert_eq!(
        result.get("index_workspace_count").and_then(Value::as_u64),
        Some(1)
    );
    let indexed_root = result
        .pointer("/index_workspaces/0/workspace_root")
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(
        Path::new(indexed_root).canonicalize().unwrap(),
        workspace.path().canonicalize().unwrap()
    );
}

#[test]
fn test_tool_call_auto_indexes_workspace_from_request_path() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"qa\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(workspace.path().join("src")).unwrap();
    let file_path = workspace.path().join("src/lib.rs");
    fs::write(&file_path, "fn sample() {}\n").unwrap();

    let (_tool_result, health) = call_binary_server_tool_then_health(
        current_dir.path(),
        json!({}),
        &[],
        "read_file_range",
        json!({ "path": file_path.to_str().unwrap() }),
        150,
    );

    let active_root = health
        .get("active_index_workspace_root")
        .and_then(|v| v.as_str())
        .unwrap();
    let actual = Path::new(active_root)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(active_root).to_path_buf());
    assert_eq!(actual, workspace.path().canonicalize().unwrap());
    assert_eq!(
        health
            .get("active_index_workspace_source")
            .and_then(|v| v.as_str()),
        Some("tool_call:read_file_range")
    );
}

#[test]
fn test_relative_paths_can_target_sibling_workspace_after_active_child_changes() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let api_dir = workspace.path().join("workboardapi");
    let ui_dir = workspace.path().join("workboardui");
    let api_search_dir = api_dir.join("src/redmine-sync");
    let ui_search_dir = ui_dir.join("src/app/features/task-current");
    fs::create_dir_all(&api_search_dir).unwrap();
    fs::create_dir_all(&ui_search_dir).unwrap();
    fs::write(
        api_dir.join("Cargo.toml"),
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(ui_dir.join("package.json"), "{\"name\":\"ui\"}\n").unwrap();
    fs::write(api_search_dir.join("import.rs"), "fn basicImport() {}\n").unwrap();
    fs::write(
        ui_search_dir.join("import.ts"),
        "export const basicImport = true;\n",
    )
    .unwrap();

    let responses = call_binary_server_tools(
        current_dir.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![
            ("resolve_path", json!({ "path": api_dir.to_str().unwrap() })),
            (
                "text_search",
                json!({
                    "paths": [
                        "workboardapi/src/redmine-sync",
                        "workboardui/src/app/features/task-current"
                    ],
                    "query": "basicImport",
                    "max_results": 50,
                    "context_lines": 2,
                    "verbose": true
                }),
            ),
        ],
        300,
    );

    let text_search_result = responses.last().unwrap();
    assert_eq!(
        text_search_result
            .get("total_returned")
            .and_then(|v| v.as_u64()),
        Some(2)
    );
    assert_eq!(
        text_search_result
            .pointer("/diagnostics/files_searched")
            .and_then(|v| v.as_u64()),
        Some(2)
    );
}

#[test]
fn test_tool_call_switches_active_workspace_context() {
    let current_dir = tempdir().unwrap();
    let workspace_a = tempdir().unwrap();
    let workspace_b = tempdir().unwrap();

    fs::write(
        workspace_a.path().join("Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        workspace_b.path().join("Cargo.toml"),
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(workspace_b.path().join("src")).unwrap();
    let file_b = workspace_b.path().join("src/lib.rs");
    fs::write(&file_b, "fn current_workspace() {}\n").unwrap();

    let init = json!({
        "workspaceFolders": [
            { "uri": format!("file:///{}", workspace_a.path().to_string_lossy().replace('\\', "/")) }
        ]
    });
    let (_tool_result, health) = call_binary_server_tool_then_health(
        current_dir.path(),
        init,
        &[],
        "read_file_range",
        json!({ "path": file_b.to_str().unwrap() }),
        150,
    );

    let active_root = health
        .get("active_index_workspace_root")
        .and_then(|v| v.as_str())
        .unwrap();
    let actual = Path::new(active_root)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(active_root).to_path_buf());
    assert_eq!(actual, workspace_b.path().canonicalize().unwrap());
    assert_eq!(
        health.get("index_workspace_count").and_then(|v| v.as_u64()),
        Some(2)
    );
}

#[test]
fn test_tool_calls_resolve_relative_paths_against_client_workspace_root_only() {
    let current_dir = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"pathing\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(workspace.path().join("src")).unwrap();
    fs::write(workspace.path().join("src/lib.rs"), "fn sample() {}\n").unwrap();
    let init = json!({
        "workspaceFolders": [
            { "uri": file_uri_for_test(workspace.path()) }
        ]
    });

    let (read_result, _health) = call_binary_server_tool_then_health(
        current_dir.path(),
        init.clone(),
        &[],
        "read_file_range",
        json!({ "path": "src/lib.rs" }),
        150,
    );
    assert_eq!(
        read_result.get("content").and_then(|v| v.as_str()),
        Some("fn sample() {}\n")
    );

    let (resolve_result, _health) = call_binary_server_tool_then_health(
        current_dir.path(),
        init,
        &[],
        "resolve_path",
        json!({ "path": "src/lib.rs" }),
        150,
    );
    assert_eq!(
        resolve_result
            .get("resolution_basis")
            .and_then(|v| v.as_str()),
        Some("active_workspace")
    );
    assert_eq!(
        resolve_result
            .get("workspace_root")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .and_then(|path| path.canonicalize().ok()),
        Some(workspace.path().canonicalize().unwrap())
    );
}

#[test]
fn test_tool_calls_resolve_relative_paths_against_client_workspace_root() {
    let current_dir = tempdir().unwrap();
    let workspace_parent = tempdir().unwrap();
    let workspace = workspace_parent.path().join("space root");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"client-pathing\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(workspace.join("src/lib.rs"), "fn client_workspace() {}\n").unwrap();

    let init = json!({
        "workspaceFolders": [
            { "uri": file_uri_for_test(&workspace) }
        ]
    });

    let (read_result, _health) = call_binary_server_tool_then_health(
        current_dir.path(),
        init.clone(),
        &[],
        "read_file_range",
        json!({ "path": "src/lib.rs" }),
        150,
    );
    assert_eq!(
        read_result.get("content").and_then(|v| v.as_str()),
        Some("fn client_workspace() {}\n")
    );

    let (resolve_result, _health) = call_binary_server_tool_then_health(
        current_dir.path(),
        init,
        &[],
        "resolve_path",
        json!({ "path": "src/lib.rs" }),
        150,
    );
    assert_eq!(
        resolve_result
            .get("resolution_basis")
            .and_then(|v| v.as_str()),
        Some("active_workspace")
    );
    assert_eq!(
        resolve_result
            .get("workspace_root")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .and_then(|path| path.canonicalize().ok()),
        Some(workspace.canonicalize().unwrap())
    );
}

#[test]
fn test_relative_reads_prefer_existing_configured_workspace_path() {
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::create_dir_all(workspace.path().join("packages/npm")).unwrap();
    fs::write(
        workspace.path().join("packages/npm/fixture.txt"),
        "configured workspace\n",
    )
    .unwrap();

    let deep_parent = tempdir().unwrap();
    let deep_workspace = deep_parent.path().join("scratch/a/b/c/d/e/f/g/h/i/deep");
    fs::create_dir_all(deep_workspace.join(".git")).unwrap();
    fs::write(deep_workspace.join("deep.txt"), "deep workspace\n").unwrap();

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![
            (
                "read_file_range",
                json!({"path": deep_workspace.join("deep.txt").to_str().unwrap()}),
            ),
            ("server_health", json!({})),
            (
                "read_file_range",
                json!({"path": "packages/npm/fixture.txt"}),
            ),
        ],
        200,
    );

    assert_eq!(
        responses[0].get("content").and_then(Value::as_str),
        Some("deep workspace\n")
    );
    assert_eq!(
        responses[1]
            .get("active_index_workspace_root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .and_then(|path| path.canonicalize().ok()),
        Some(deep_workspace.canonicalize().unwrap())
    );
    assert_eq!(
        responses[2].get("content").and_then(Value::as_str),
        Some("configured workspace\n")
    );
}

#[test]
fn test_missing_and_glob_paths_do_not_create_content_zones() {
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("existing.rs"), "fn existing() {}\n").unwrap();
    let absolute_missing = workspace.path().join("missing/absolute");

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![
            ("warm_content_index", json!({"paths": ["missing/**/*.rs"]})),
            (
                "warm_content_index",
                json!({"paths": [absolute_missing.to_str().unwrap()]}),
            ),
            (
                "text_search",
                json!({"query": "needle", "paths": ["missing/search-root"]}),
            ),
            ("server_health", json!({})),
        ],
        200,
    );

    assert_eq!(
        responses[0].get("is_error").and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        responses[0]
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("includes"))
    );
    for response in &responses[1..=2] {
        assert_eq!(
            response.get("is_error").and_then(Value::as_bool),
            Some(true),
            "unexpected response: {response:#}"
        );
        assert!(
            response
                .get("message")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("resolved to"))
        );
    }
    assert_eq!(
        responses[3]
            .pointer("/index_workspaces/0/content_index/zones")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );
}

#[test]
fn test_write_tools_warn_by_risk_without_policy_blocks() {
    let workspace = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("shared.txt"), "configured\n").unwrap();
    fs::write(outside.path().join("secret.txt"), "outside\n").unwrap();
    create_directory_link(&workspace.path().join("escape"), outside.path());

    let deep_parent = tempdir().unwrap();
    let deep_workspace = deep_parent.path().join("scratch/a/b/c/d/deep");
    fs::create_dir_all(deep_workspace.join(".git")).unwrap();
    fs::write(deep_workspace.join("shared.txt"), "deep\n").unwrap();

    let outside_new = outside.path().join("new.txt");
    let generated_target = workspace.path().join("node_modules/pkg/generated.txt");
    let outside_warning = format!(
        "low write risk: outside write roots: {}",
        canonical_display_path(&outside_new)
    );
    let system_target = if cfg!(windows) {
        PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:/Windows".into()))
            .join("codeloupe-must-not-write.txt")
    } else {
        PathBuf::from("/etc/codeloupe-must-not-write.txt")
    };
    let responses = call_binary_server_tools(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![
            (
                "read_file_range",
                json!({"path": deep_workspace.join("shared.txt").to_str().unwrap()}),
            ),
            (
                "read_file_range",
                json!({"path": outside.path().join("secret.txt").to_str().unwrap()}),
            ),
            (
                "edit_file",
                json!({"path": "shared.txt", "mode": "append", "content": "edited\n"}),
            ),
            (
                "create_file",
                json!({"path": outside_new.to_str().unwrap(), "content": "warned\n"}),
            ),
            (
                "create_file",
                json!({"path": ".git/hooks/pre-commit", "content": "acknowledged\n", "create_parents": true, "acknowledge_risk": true}),
            ),
            (
                "create_file",
                json!({"path": generated_target.to_str().unwrap(), "content": "generated\n"}),
            ),
            (
                "edit_file",
                json!({"path": "escape/secret.txt", "mode": "append", "content": "acknowledged\n", "acknowledge_risk": true}),
            ),
        ],
        200,
    );

    assert!(
        responses[2]
            .get("sha256_after")
            .and_then(Value::as_str)
            .is_some(),
        "unexpected edit response: {:#}",
        responses[2]
    );
    assert!(responses[2].get("success").is_none());
    assert_eq!(
        fs::read_to_string(workspace.path().join("shared.txt")).unwrap(),
        "configured\nedited\n"
    );
    assert_eq!(
        fs::read_to_string(deep_workspace.join("shared.txt")).unwrap(),
        "deep\n"
    );

    assert!(
        responses[3]
            .get("sha256_after")
            .and_then(Value::as_str)
            .is_some(),
        "outside-workspace write failed: {:#}",
        responses[3]
    );
    assert!(
        responses[3]
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings
                .iter()
                .any(|warning| { warning.as_str() == Some(outside_warning.as_str()) })),
        "outside-workspace warning was missing or too verbose: {:#}",
        responses[3]
    );
    assert!(
        responses[4]
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.starts_with("critical write risk: .git path:")))),
        "missing .git risk warning: {:#}",
        responses[4]
    );
    assert!(
        responses[5]
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings
                .iter()
                .any(|warning| warning.as_str().is_some_and(|warning| warning
                    .starts_with("high write risk: matched configured risk pattern")))),
        "missing configured-pattern risk warning: {:#}",
        responses[5]
    );
    assert!(
        responses[6]
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings
                .iter()
                .any(|warning| warning.as_str().is_some_and(|warning| warning
                    .starts_with("critical write risk: symlink or junction escapes")))),
        "missing symlink risk warning: {:#}",
        responses[6]
    );
    assert_eq!(fs::read_to_string(outside_new).unwrap(), "warned\n");
    assert_eq!(
        fs::read_to_string(workspace.path().join(".git/hooks/pre-commit")).unwrap(),
        "acknowledged\n"
    );
    assert_eq!(fs::read_to_string(generated_target).unwrap(), "generated\n");
    assert_eq!(
        fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
        "outside\nacknowledged\n"
    );
    let system_classification =
        codeloupe_mcp::security::path_guard::GUARD.classify_path(&system_target);
    assert_eq!(
        system_classification.tier,
        codeloupe_mcp::security::path_guard::Tier::CriticalRiskWarn
    );
    assert!(
        system_classification.warning.is_some_and(
            |warning| warning.starts_with("critical write risk: operating-system path:")
        )
    );
}

#[test]
fn test_critical_write_requires_preflight_acknowledgement() {
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join(".git/config"), "[core]\n").unwrap();
    let hook = workspace.path().join(".git/hooks/pre-commit");

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![
            (
                "create_file",
                json!({
                    "path": hook.to_str().unwrap(),
                    "content": "first\n",
                    "create_parents": true
                }),
            ),
            ("list_history", json!({})),
            (
                "create_file",
                json!({
                    "path": hook.to_str().unwrap(),
                    "content": "confirmed\n",
                    "create_parents": true,
                    "acknowledge_risk": true
                }),
            ),
            ("list_history", json!({})),
            (
                "edit_file",
                json!({
                    "path": workspace.path().join(".git/config").to_str().unwrap(),
                    "mode": "append",
                    "content": "changed\n",
                    "expected_hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "acknowledge_risk": true
                }),
            ),
        ],
        200,
    );

    assert_eq!(
        responses[0].pointer("/error/code").and_then(Value::as_str),
        Some("risk_confirmation_required"),
        "unexpected preflight response: {:#}",
        responses[0]
    );
    assert_eq!(
        responses[0]
            .get("warnings")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1),
        "critical warning should not be duplicated: {:#}",
        responses[0]
    );
    assert_eq!(
        responses[1].get("total").and_then(Value::as_u64),
        Some(0),
        "unacknowledged write created history: {:#}",
        responses[1]
    );
    assert!(responses[2].get("sha256_after").is_some());
    assert!(
        responses[2]
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.starts_with("critical write risk: .git path:"))))
    );
    assert_eq!(fs::read_to_string(&hook).unwrap(), "confirmed\n");
    assert_eq!(responses[3].get("total").and_then(Value::as_u64), Some(1));
    assert_eq!(
        responses[4].pointer("/error/code").and_then(Value::as_str),
        Some("hash_mismatch")
    );
    assert!(responses[4].get("actual_hash").is_none());
    assert_eq!(
        fs::read_to_string(workspace.path().join(".git/config")).unwrap(),
        "[core]\n"
    );
}

#[test]
fn test_batch_preflight_blocks_all_writes_before_critical_acknowledgement() {
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    let safe = workspace.path().join("safe.txt");
    let hook = workspace.path().join(".git/hooks/pre-commit");

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![(
            "batch_tool_call",
            json!({
                "calls": [
                    {
                        "tool": "create_file",
                        "args": {"path": safe.to_str().unwrap(), "content": "safe\n"}
                    },
                    {
                        "tool": "create_file",
                        "args": {
                            "path": hook.to_str().unwrap(),
                            "content": "hook\n",
                            "create_parents": true
                        }
                    }
                ]
            }),
        )],
        200,
    );

    assert_eq!(
        responses[0].pointer("/error/code").and_then(Value::as_str),
        Some("risk_confirmation_required")
    );
    assert!(!safe.exists());
    assert!(!hook.exists());
}

#[test]
fn test_unknown_workspace_junction_warning_shows_canonical_target() {
    let workspace = tempdir().unwrap();
    let lexical_parent = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    let link = lexical_parent.path().join("junction");
    create_directory_link(&link, outside.path());
    let requested = link.join("escaped.txt");
    let canonical = outside.path().join("escaped.txt");

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![(
            "create_file",
            json!({"path": requested.to_str().unwrap(), "content": "escaped\n"}),
        )],
        200,
    );

    let canonical_display = canonical_display_path(&canonical);
    assert!(
        responses[0]
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.contains(&canonical_display)))),
        "canonical target missing from warning: {:#}",
        responses[0]
    );
    assert_eq!(fs::read_to_string(canonical).unwrap(), "escaped\n");
}

#[test]
fn test_path_alias_scopes_plural_path_tools() {
    let workspace = tempdir().unwrap();
    let scoped = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("wrong.rs"), "fn wrong_helper() {}\n").unwrap();
    fs::write(
        scoped.path().join("unique_external.rs"),
        "fn external_helper() {}\n",
    )
    .unwrap();

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![
            (
                "find_definition",
                json!({
                    "symbol": "external_helper",
                    "path": scoped.path().to_str().unwrap()
                }),
            ),
            (
                "fuzzy_find",
                json!({
                    "pattern": "unique_external",
                    "path": scoped.path().to_str().unwrap()
                }),
            ),
        ],
        200,
    );

    assert_eq!(
        responses[0].get("total_returned").and_then(Value::as_u64),
        Some(1),
        "singular path alias did not scope find_definition: {:#}",
        responses[0]
    );
    assert!(
        responses[1]
            .get("results")
            .and_then(Value::as_array)
            .is_some_and(|matches| matches.iter().any(|matched| matched
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("unique_external.rs")))),
        "singular path alias did not scope fuzzy_find: {:#}",
        responses[1]
    );
    for response in responses {
        assert!(
            !response.to_string().contains("Unknown argument 'path'"),
            "path alias still emitted an unknown-argument warning: {response:#}"
        );
    }
}

#[test]
fn test_project_map_lists_directory_links_as_directories() {
    let workspace = tempdir().unwrap();
    let target = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(target.path().join("target.txt"), "target\n").unwrap();
    let link = workspace.path().join("linked-directory");
    create_directory_link(&link, target.path());

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![(
            "project_map",
            json!({"path": workspace.path().to_str().unwrap(), "max_depth": 2}),
        )],
        200,
    );

    assert!(
        responses[0]
            .pointer("/tree_representation/./dirs")
            .and_then(Value::as_array)
            .is_some_and(|dirs| dirs.iter().any(|entry| {
                entry.get("name").and_then(Value::as_str) == Some("linked-directory")
            })),
        "directory link was not represented as a directory: {:#}",
        responses[0]
    );
    assert!(
        !responses[0]
            .pointer("/tree_representation/./files")
            .and_then(Value::as_array)
            .is_some_and(|files| files.iter().any(|entry| {
                entry.get("name").and_then(Value::as_str) == Some("linked-directory")
            }))
    );
}

#[test]
fn test_cli_help_version_and_unknown_option_exit_without_starting_server() {
    let exe = server_binary();
    let version = Command::new(&exe).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("codeloupe-mcp {}", env!("CARGO_PKG_VERSION"))
    );

    let help = Command::new(&exe).arg("--help").output().unwrap();
    assert!(help.status.success());
    let help_stdout = String::from_utf8_lossy(&help.stdout);
    assert!(help_stdout.contains("Usage: codeloupe-mcp [OPTIONS]"));
    assert!(help_stdout.contains("--workspace <PATH>"));

    let unknown = Command::new(&exe)
        .arg("--definitely-unknown")
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("Unknown option"));
}

#[test]
fn test_workspace_index_enable_does_not_declare_a_write_root() {
    let workspace = tempdir().unwrap();
    let indexed_only = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::create_dir(indexed_only.path().join(".git")).unwrap();
    fs::write(indexed_only.path().join("lib.rs"), "fn indexed_only() {}\n").unwrap();
    let target = indexed_only.path().join("should-not-write.txt");

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        vec![
            (
                "workspace_index",
                json!({
                    "action": "enable",
                    "path": indexed_only.path().to_str().unwrap()
                }),
            ),
            (
                "create_file",
                json!({"path": target.to_str().unwrap(), "content": "warned\n"}),
            ),
            ("server_health", json!({})),
        ],
        200,
    );

    assert_eq!(
        responses[0]
            .get("approval_recorded")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        responses[1]
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| {
                warning.as_str().is_some_and(|warning| {
                    warning.starts_with("low write risk: outside write roots:")
                })
            })),
        "index-only workspace write did not warn: {:#}",
        responses[1]
    );
    assert!(
        responses[2]
            .get("configured_workspaces")
            .and_then(Value::as_array)
            .is_some_and(|workspaces| workspaces.iter().any(|workspace| {
                workspace.get("workspace_root").and_then(Value::as_str)
                    == Some(canonical_display_path(indexed_only.path()).as_str())
                    && workspace.get("index_approved").and_then(Value::as_bool) == Some(true)
                    && workspace.get("write_allowed").and_then(Value::as_bool) == Some(false)
            }))
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "warned\n");
}

#[test]
fn test_workspace_index_lifecycle_preserves_implicit_cwd_write_scope() {
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::write(workspace.path().join("lib.rs"), "fn sample() {}\n").unwrap();
    let target = workspace.path().join("after-index-disable.txt");

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({}),
        vec![
            (
                "workspace_index",
                json!({
                    "action": "enable",
                    "path": workspace.path().to_str().unwrap()
                }),
            ),
            ("server_health", json!({})),
            (
                "workspace_index",
                json!({
                    "action": "disable",
                    "path": workspace.path().to_str().unwrap()
                }),
            ),
            (
                "create_file",
                json!({"path": target.to_str().unwrap(), "content": "allowed\n"}),
            ),
        ],
        200,
    );

    assert_eq!(
        responses[0].get("outcome").and_then(Value::as_str),
        Some("scheduled")
    );
    assert_eq!(
        responses[2].get("outcome").and_then(Value::as_str),
        Some("disabled")
    );
    assert!(responses[3].get("error").is_none(), "{:#}", responses[3]);
    assert_eq!(fs::read_to_string(target).unwrap(), "allowed\n");
}

#[test]
fn test_write_tools_allow_explicit_additional_root() {
    let workspace = tempdir().unwrap();
    let additional_root = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    let target = additional_root.path().join("allowed.txt");
    let additional_root_value = additional_root.path().to_string_lossy().into_owned();

    let (response, _health) = call_binary_server_tool_then_health(
        workspace.path(),
        json!({
            "workspaceFolders": [
                { "uri": file_uri_for_test(workspace.path()) }
            ]
        }),
        &[("CODELOUPE_MCP_WRITE_ROOTS", additional_root_value.as_str())],
        "create_file",
        json!({"path": target.to_str().unwrap(), "content": "allowed\n"}),
        200,
    );

    assert!(response.get("success").is_none());
    assert_eq!(
        response.get("path").and_then(Value::as_str),
        Some(codeloupe_mcp::common::normalize_display_path(&target).as_str())
    );
    assert!(
        response
            .get("sha256_after")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(
        response
            .get("history_entry_id")
            .and_then(Value::as_str)
            .is_some()
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "allowed\n");
}

#[test]
fn test_home_roots_are_not_implicitly_write_allowed() {
    let fake_home = tempdir().unwrap();
    fs::create_dir(fake_home.path().join(".git")).unwrap();
    let home_text = fake_home.path().to_string_lossy().into_owned();
    let app_data_text = fake_home
        .path()
        .join("AppData/Roaming")
        .to_string_lossy()
        .into_owned();
    let local_app_data_text = fake_home
        .path()
        .join("AppData/Local")
        .to_string_lossy()
        .into_owned();
    let cwd_target = fake_home.path().join("ordinary-cwd.txt");
    let client_target = fake_home.path().join("ordinary-client.txt");
    let environment = [
        ("HOME", home_text.as_str()),
        ("USERPROFILE", home_text.as_str()),
        ("APPDATA", app_data_text.as_str()),
        ("LOCALAPPDATA", local_app_data_text.as_str()),
    ];

    let cwd_responses = call_binary_server_tools_with_options(
        fake_home.path(),
        json!({}),
        vec![(
            "create_file",
            json!({"path": cwd_target.to_str().unwrap(), "content": "warned\n"}),
        )],
        50,
        &environment,
    );
    assert!(
        cwd_responses[0]
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| {
                warning.as_str().is_some_and(|warning| {
                    warning.starts_with("low write risk: outside write roots:")
                })
            }))
    );
    assert_eq!(fs::read_to_string(&cwd_target).unwrap(), "warned\n");

    let process_cwd = tempdir().unwrap();
    let client_root_responses = call_binary_server_tools_with_options(
        process_cwd.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(fake_home.path())}]
        }),
        vec![
            (
                "create_file",
                json!({"path": client_target.to_str().unwrap(), "content": "warned\n"}),
            ),
            ("server_health", json!({})),
        ],
        50,
        &environment,
    );
    assert!(
        client_root_responses[0]
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| {
                warning.as_str().is_some_and(|warning| {
                    warning.starts_with("low write risk: outside write roots:")
                })
            }))
    );
    assert!(
        client_root_responses[1]
            .get("configured_workspaces")
            .and_then(Value::as_array)
            .is_some_and(|workspaces| workspaces.iter().any(|workspace| {
                workspace.get("workspace_root").and_then(Value::as_str)
                    == Some(canonical_display_path(fake_home.path()).as_str())
                    && workspace.get("write_allowed").and_then(Value::as_bool) == Some(false)
            }))
    );
    assert_eq!(fs::read_to_string(client_target).unwrap(), "warned\n");
}

#[test]
fn test_sensitive_home_paths_require_critical_acknowledgement() {
    let workspace = tempdir().unwrap();
    let fake_home = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    fs::create_dir_all(fake_home.path().join(".ssh")).unwrap();
    let home_alias = workspace.path().join("home-link");
    create_directory_link(&home_alias, fake_home.path());
    let home_text = home_alias.to_string_lossy().into_owned();
    let app_data = home_alias.join("AppData/Roaming");
    let app_data_text = app_data.to_string_lossy().into_owned();
    let local_app_data_text = home_alias
        .join("AppData/Local")
        .to_string_lossy()
        .into_owned();
    let normal = home_alias.join("ordinary.txt");
    let mut sensitive_targets = vec![
        home_alias.join(".ssh").join("authorized_keys"),
        home_alias.join(".bashrc"),
        home_alias
            .join(".config")
            .join("service")
            .join("credentials"),
        home_alias
            .join("Documents")
            .join("PowerShell")
            .join("Microsoft.PowerShell_profile.ps1"),
        app_data
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("agent.cmd"),
    ];

    let raw_sensitive_link = home_alias.join(".ssh").join("project-link");
    create_directory_link(&raw_sensitive_link, workspace.path());
    sensitive_targets.push(raw_sensitive_link.join("raw-blocked.txt"));
    let canonical_sensitive_link = workspace.path().join("ssh-link");
    create_directory_link(&canonical_sensitive_link, &fake_home.path().join(".ssh"));
    sensitive_targets.push(canonical_sensitive_link.join("canonical-blocked.txt"));

    let mut calls = vec![(
        "create_file",
        json!({"path": normal.to_str().unwrap(), "content": "allowed\n"}),
    )];
    for target in &sensitive_targets {
        calls.push((
            "create_file",
            json!({"path": target.to_str().unwrap(), "content": "warned\n"}),
        ));
    }

    let responses = call_binary_server_tools_with_options(
        workspace.path(),
        json!({
            "workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]
        }),
        calls,
        50,
        &[
            ("HOME", home_text.as_str()),
            ("USERPROFILE", home_text.as_str()),
            ("APPDATA", app_data_text.as_str()),
            ("LOCALAPPDATA", local_app_data_text.as_str()),
            ("CODELOUPE_MCP_WRITE_ROOTS", home_text.as_str()),
        ],
    );

    assert_eq!(
        fs::read_to_string(&normal).unwrap_or_default(),
        "allowed\n",
        "ordinary explicit-root write failed: {:#}",
        responses[0]
    );
    for (response, target) in responses[1..].iter().zip(&sensitive_targets) {
        assert_eq!(
            response.pointer("/error/code").and_then(Value::as_str),
            Some("risk_confirmation_required"),
            "unexpected response for {}: {response:#}",
            target.display()
        );
        assert!(
            response
                .get("warnings")
                .and_then(Value::as_array)
                .is_some_and(|warnings| warnings.iter().any(|warning| warning
                    .as_str()
                    .is_some_and(
                        |warning| warning.starts_with("critical write risk: sensitive home path:")
                    ))),
            "unexpected response for {}: {response:#}",
            target.display()
        );
        assert!(
            !target.exists(),
            "sensitive target was written before acknowledgement: {}",
            target.display()
        );
    }
}

#[test]
fn test_mcp_roots_list_updates_and_replaces_client_roots() {
    let process_cwd = tempdir().unwrap();
    let repository = tempdir().unwrap();
    fs::create_dir(repository.path().join(".git")).unwrap();
    let first_root = repository.path().join("first-root");
    let second_root = repository.path().join("second-root");
    fs::create_dir(&first_root).unwrap();
    fs::create_dir(&second_root).unwrap();
    let first_target = first_root.join("first.txt");
    let old_target = first_root.join("old.txt");
    let second_target = second_root.join("second.txt");

    let exe = server_binary();
    let mut command = Command::new(&exe);
    isolate_server_environment(&mut command);
    let mut child = command
        .current_dir(process_cwd.path())
        .env(
            "CODELOUPE_MCP_INDEX_DIR",
            process_cwd.path().join("codeloupe-test-index"),
        )
        .env("CODELOUPE_MCP_INDEX_MODE", "off")
        .env(
            "LOCALAPPDATA",
            process_cwd.path().join("fake-local-app-data"),
        )
        .env("APPDATA", process_cwd.path().join("fake-app-data"))
        .env("USERPROFILE", process_cwd.path().join("fake-home/nested"))
        .env("HOME", process_cwd.path().join("fake-home/nested"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"roots": {"listChanged": true}},
                "clientInfo": {"name": "roots-test", "version": "0"}
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    assert_eq!(
        read_rpc_response(&mut stdout, 1)
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str),
        Some("2025-06-18")
    );

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    stdin.flush().unwrap();
    let first_roots_request = read_next_json_rpc(&mut stdout);
    assert_eq!(
        first_roots_request.get("method").and_then(Value::as_str),
        Some("roots/list")
    );
    let first_roots_request_id = first_roots_request.get("id").cloned().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": first_roots_request_id,
            "result": {"roots": [{"uri": file_uri_for_test(&first_root)}]}
        })
    )
    .unwrap();

    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "create_file", "arguments": {
                "path": first_target.to_str().unwrap(),
                "content": "first\n"
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let first_write = decode_tool_rpc_response(&read_rpc_response(&mut stdout, 2));
    assert!(first_write.get("sha256_after").is_some());
    assert_eq!(fs::read_to_string(&first_target).unwrap(), "first\n");

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"})
    )
    .unwrap();
    stdin.flush().unwrap();
    let second_roots_request = read_next_json_rpc(&mut stdout);
    assert_eq!(
        second_roots_request.get("method").and_then(Value::as_str),
        Some("roots/list")
    );
    let second_roots_request_id = second_roots_request.get("id").cloned().unwrap();
    assert_ne!(second_roots_request_id, first_roots_request_id);
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": second_roots_request_id,
            "result": {"roots": [{"uri": file_uri_for_test(&second_root)}]}
        })
    )
    .unwrap();

    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "create_file", "arguments": {
                "path": old_target.to_str().unwrap(),
                "content": "updated\n"
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let old_write = decode_tool_rpc_response(&read_rpc_response(&mut stdout, 3));
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "create_file", "arguments": {
                "path": second_target.to_str().unwrap(),
                "content": "updated\n"
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let second_write = decode_tool_rpc_response(&read_rpc_response(&mut stdout, 4));
    assert!(old_write.get("sha256_after").is_some(), "{old_write:#}");
    assert!(
        old_write
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| {
                warning
                    .as_str()
                    .is_some_and(|warning| warning.contains("outside declared roots:"))
            })),
        "revoked root should fall back to a medium-risk warning: {old_write:#}"
    );
    assert!(second_write.get("sha256_after").is_some());
    assert_eq!(fs::read_to_string(&old_target).unwrap(), "updated\n");
    assert_eq!(fs::read_to_string(&second_target).unwrap(), "updated\n");

    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {"name": "server_health", "arguments": {}}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let health = decode_tool_rpc_response(&read_rpc_response(&mut stdout, 5));
    let configured = health
        .get("configured_workspaces")
        .and_then(Value::as_array)
        .unwrap();
    let inferred_root = canonical_display_path(repository.path());
    let workspace = configured
        .iter()
        .find(|workspace| {
            workspace.get("workspace_root").and_then(Value::as_str) == Some(inferred_root.as_str())
        })
        .unwrap();
    assert_eq!(
        workspace.get("source").and_then(Value::as_str),
        Some("client_roots_list")
    );
    assert_eq!(
        workspace.get("write_allowed").and_then(Value::as_bool),
        Some(true)
    );
    let declared_roots = workspace["declared_roots"].as_array().unwrap();
    assert_eq!(declared_roots.len(), 1);
    assert_eq!(
        declared_roots[0].as_str(),
        Some(canonical_display_path(&second_root).as_str())
    );
    assert_eq!(workspace["write_roots"], workspace["declared_roots"]);

    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn test_write_policy_is_risk_aware_even_when_client_supports_elicitation() {
    let repository = tempdir().unwrap();
    fs::create_dir(repository.path().join(".git")).unwrap();
    let declared_root = repository.path().join("declared");
    let undeclared_directory = repository.path().join("tools");
    fs::create_dir(&declared_root).unwrap();
    fs::create_dir(&undeclared_directory).unwrap();
    let target = undeclared_directory.join("first.txt");

    let exe = server_binary();
    let mut command = Command::new(&exe);
    isolate_server_environment(&mut command);
    let mut child = command
        .current_dir(repository.path())
        .env(
            "CODELOUPE_MCP_INDEX_DIR",
            repository.path().join("codeloupe-test-index"),
        )
        .env("CODELOUPE_MCP_INDEX_MODE", "off")
        .env(
            "LOCALAPPDATA",
            repository.path().join("fake-local-app-data"),
        )
        .env("APPDATA", repository.path().join("fake-app-data"))
        .env("USERPROFILE", repository.path().join("fake-home/nested"))
        .env("HOME", repository.path().join("fake-home/nested"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"elicitation": {}},
                "clientInfo": {"name": "warning-only-test", "version": "0"},
                "workspaceFolders": [{"uri": file_uri_for_test(&declared_root)}]
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    assert_eq!(
        read_rpc_response(&mut stdout, 1)
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str),
        Some("2025-06-18")
    );

    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "create_file", "arguments": {
                "path": target.to_str().unwrap(),
                "content": "written\n"
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let written = decode_tool_rpc_response(&read_rpc_response(&mut stdout, 2));
    assert!(written.get("sha256_after").is_some(), "{written:#}");
    assert!(
        written
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(
                |warnings| warnings
                    .iter()
                    .any(|warning| warning
                        .as_str()
                        .is_some_and(|warning| warning
                            .starts_with("medium write risk: outside declared roots:")))
            ),
        "missing medium risk warning: {written:#}"
    );

    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "server_health", "arguments": {}}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let health = decode_tool_rpc_response(&read_rpc_response(&mut stdout, 3));
    assert_eq!(health["write_scope"], json!("risk_aware"));
    assert_eq!(health["write_elicitation_supported"], json!(false));
    assert_eq!(health["write_approved_directories"], json!([]));
    assert_eq!(health["write_declined_directories"], json!([]));

    drop(stdin);
    assert!(child.wait().unwrap().success());
    assert_eq!(fs::read_to_string(target).unwrap(), "written\n");
}

#[test]
fn test_declared_subdirectories_allow_warned_writes_in_inferred_repo_root() {
    let repository = tempdir().unwrap();
    fs::create_dir(repository.path().join(".git")).unwrap();
    let browser_root = repository.path().join("automative_browser");
    let server_root = repository.path().join("mcp_server");
    let cache_root = repository.path().join("dist/package/__pycache__");
    fs::create_dir_all(&browser_root).unwrap();
    fs::create_dir_all(&server_root).unwrap();
    fs::create_dir_all(&cache_root).unwrap();

    let declared_file = browser_root.join("README.md");
    let repository_file = repository.path().join("README.md");
    let tools_dir = repository.path().join("tools");
    let tools_file = tools_dir.join("package_automative_browser.ps1");
    fs::create_dir(&tools_dir).unwrap();
    fs::write(&declared_file, "declared\n").unwrap();
    fs::write(&repository_file, "repository\n").unwrap();
    fs::write(&tools_file, "tools\n").unwrap();
    let responses = call_binary_server_tools(
        repository.path(),
        json!({
            "workspaceFolders": [
                {"uri": file_uri_for_test(&browser_root)},
                {"uri": file_uri_for_test(&server_root)},
                {"uri": file_uri_for_test(&cache_root)}
            ]
        }),
        vec![
            (
                "edit_file",
                json!({
                    "path": declared_file.to_str().unwrap(),
                    "mode": "append",
                    "content": "declared-edit\n"
                }),
            ),
            (
                "edit_file",
                json!({
                    "path": repository_file.to_str().unwrap(),
                    "mode": "append",
                    "content": "repository-edit\n"
                }),
            ),
            (
                "edit_file",
                json!({
                    "path": tools_file.to_str().unwrap(),
                    "mode": "append",
                    "content": "tools-edit\n"
                }),
            ),
            ("list_history", json!({})),
            ("server_health", json!({})),
        ],
        200,
    );

    assert!(responses[0].get("sha256_after").is_some());
    assert!(responses[0].get("warnings").is_none());
    for response in &responses[1..=2] {
        assert!(response.get("sha256_after").is_some(), "{response:#}");
        assert!(
            response
                .get("warnings")
                .and_then(Value::as_array)
                .is_some_and(|warnings| warnings.iter().any(|warning| {
                    warning
                        .as_str()
                        .is_some_and(|warning| warning.contains("outside declared roots:"))
                })),
            "missing repo warning: {response:#}"
        );
    }
    assert_eq!(
        fs::read_to_string(&declared_file).unwrap(),
        "declared\ndeclared-edit\n"
    );
    assert_eq!(
        fs::read_to_string(&repository_file).unwrap(),
        "repository\nrepository-edit\n"
    );
    assert_eq!(
        fs::read_to_string(&tools_file).unwrap(),
        "tools\ntools-edit\n"
    );

    let history = responses[3]["entries"].as_array().unwrap();
    assert_eq!(history[0]["outside_declared"], json!(true));
    assert_eq!(history[1]["outside_declared"], json!(true));
    assert_eq!(history[2]["outside_declared"], json!(false));

    let inferred_root = canonical_display_path(repository.path());
    assert_eq!(responses[4]["write_scope"], json!("risk_aware"));
    assert_eq!(responses[4]["write_approved_directories"], json!([]));
    assert_eq!(responses[4]["write_declined_directories"], json!([]));
    let configured = responses[4]
        .get("configured_workspaces")
        .and_then(Value::as_array)
        .unwrap();
    let workspace = configured
        .iter()
        .find(|workspace| {
            workspace.get("workspace_root").and_then(Value::as_str) == Some(inferred_root.as_str())
        })
        .unwrap();
    let mut declared_roots = workspace["declared_roots"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    declared_roots.sort();
    let mut expected_roots = [&browser_root, &server_root, &cache_root]
        .into_iter()
        .map(|path| canonical_display_path(path))
        .collect::<Vec<_>>();
    expected_roots.sort();
    assert_eq!(declared_roots, expected_roots);
    assert_eq!(workspace["write_roots"], workspace["declared_roots"]);
}

#[test]
fn test_write_tool_errors_drop_legacy_top_level_fields() {
    let workspace = tempdir().unwrap();
    fs::create_dir(workspace.path().join(".git")).unwrap();
    let existing_file = workspace.path().join("existing.txt");
    let existing_dir = workspace.path().join("existing-dir");
    let missing_file = workspace.path().join("missing.txt");
    fs::write(&existing_file, "existing\n").unwrap();
    fs::create_dir(&existing_dir).unwrap();

    let responses = call_binary_server_tools(
        workspace.path(),
        json!({"workspaceFolders": [{"uri": file_uri_for_test(workspace.path())}]}),
        vec![
            (
                "create_file",
                json!({"path": existing_file.to_str().unwrap(), "content": "new\n"}),
            ),
            (
                "create_directory",
                json!({"path": existing_dir.to_str().unwrap(), "allow_existing": false}),
            ),
            (
                "edit_file",
                json!({
                    "path": existing_file.to_str().unwrap(),
                    "mode": "replace",
                    "content": "new\n",
                    "expected_hash": "0".repeat(64)
                }),
            ),
            (
                "edit_files",
                json!({"files": [{
                    "path": existing_file.to_str().unwrap(),
                    "expected_hash": "0".repeat(64),
                    "edits": [{"find": "existing", "replace": "new", "expected_replacements": 1}]
                }]}),
            ),
            (
                "delete_file",
                json!({
                    "path": existing_file.to_str().unwrap(),
                    "expected_hash": "0".repeat(64)
                }),
            ),
            (
                "convert_file_format",
                json!({"path": missing_file.to_str().unwrap(), "target_encoding": "UTF-8"}),
            ),
            ("undo_change", json!({"entry_id": "missing-history-entry"})),
        ],
        200,
    );

    for response in responses {
        let payload = response
            .get("message")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or(response);
        assert!(
            payload.get("error").is_some_and(Value::is_object),
            "missing structured error in {payload:#}"
        );
        for legacy_field in ["success", "error_code", "message", "reason"] {
            assert!(
                payload.get(legacy_field).is_none(),
                "legacy field {legacy_field} remained in {payload:#}"
            );
        }
    }
}

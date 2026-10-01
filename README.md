## codeloupe-mcp

A Model Context Protocol (MCP) server that gives coding agents fast, local-first access to large codebases. It enables LLMs to inspect files, search text, compare directories, read symbols, and apply structured edits through bounded tool responses instead of dumping whole files or repository trees into context.

### codeloupe-mcp vs shell tools

This package provides an MCP interface into repository exploration and code editing.

If you are using a **coding agent**, shell commands are still useful for concise build/test workflows. `codeloupe-mcp` is most useful when the model needs structured, repeatable, repository-aware code context across a chat session.

- **Shell / CLI**: best for deterministic commands such as `cargo test`, `npm run lint`, project scripts, one-off `rg`, and build logs that are already compact.
- **MCP**: best for agent loops that benefit from tool schemas, workspace-aware path resolution, bounded output, persistent indexes, and precise reads such as symbol bodies, snippets, shallow project maps, and directory diffs.

Use `codeloupe-mcp` when you want an agent to ask for exactly the code context it needs: a line range, a symbol body, a bounded search result, a fuzzy file match, a workspace summary, or a source-directory comparison.

### Key Features

- **Fast and lightweight**. Local Rust server over `stdio`; no external indexing service, no network calls, no telemetry.
- **LLM-friendly output**. Tools return bounded JSON/Markdown designed for agent context windows.
- **Repository-aware indexing**. LMDB path metadata plus optional Tantivy content indexing per workspace.
- **Precise code reads**. Read focused line ranges, snippets, symbol bodies, imports, exports, call graphs, definitions, and references.
- **Scoped search**. Literal and regex search with path scopes, include/exclude globs, fallback diagnostics, and large-repo safeguards.
- **Directory comparison**. Compare two source directories with added/deleted/modified/renamed summaries and bounded diffs.
- **Structured edits**. Create, edit, convert, delete, batch-edit, and undo files with predictable success/error metadata.

### Requirements

- An MCP client that supports local `stdio` servers.
- Node.js 18 or newer when using `npx`.
- No Rust toolchain is needed when using the published npm package or a native release binary.
- Rust 1.98.1 is required only when building from source; `rust-toolchain.toml` installs the pinned toolchain through rustup.

### Getting started

First, install the `codeloupe-mcp` server with your client.

**Standard config** works in most MCP clients:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

You can also use a native binary from the [GitHub Releases](https://github.com/ndhkaeru/codeloupe-mcp/releases) page:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "/absolute/path/to/codeloupe-mcp",
      "args": []
    }
  }
}
```

On Windows native installs, point `command` to the `.exe` file, for example `C:\path\to\codeloupe-mcp.exe`.

<details>
<summary>Amp</summary>

Add via the Amp VS Code extension settings screen or by updating your settings JSON:

```json
"amp.mcpServers": {
  "codeloupe-mcp": {
    "command": "npx",
    "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
  }
}
```

Amp CLI setup:

```bash
amp mcp add codeloupe-mcp -- npx -y @ndhkaeru/codeloupe-mcp@latest
```

</details>

<details>
<summary>Antigravity</summary>

Add via Antigravity settings or by updating your MCP configuration file:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

</details>

<details>
<summary>Claude Code</summary>

Use the Claude Code CLI:

```bash
claude mcp add codeloupe-mcp -- npx -y @ndhkaeru/codeloupe-mcp@latest
```

Native binary alternative:

```bash
claude mcp add codeloupe-mcp -- /absolute/path/to/codeloupe-mcp
```

</details>

<details>
<summary>Claude Desktop</summary>

Follow the MCP install guide and use the standard config:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

Native Windows binary alternative:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "C:\\path\\to\\codeloupe-mcp.exe",
      "args": []
    }
  }
}
```

</details>

<details>
<summary>Cline</summary>

Follow Cline's MCP server configuration flow and add this to `cline_mcp_settings.json`:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "type": "stdio",
      "command": "npx",
      "timeout": 60,
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"],
      "disabled": false
    }
  }
}
```

</details>

<details>
<summary>Codex</summary>

Use the Codex CLI:

```bash
codex mcp add codeloupe-mcp -- npx -y @ndhkaeru/codeloupe-mcp@latest
```

Alternatively, create or edit `~/.codex/config.toml`:

```toml
[mcp_servers.codeloupe-mcp]
command = "npx"
args = ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
```

</details>

<details>
<summary>Copilot</summary>

Use the Copilot MCP add flow, or add this to your MCP config:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "type": "local",
      "command": "npx",
      "tools": ["*"],
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

</details>

<details>
<summary>Cursor</summary>

Go to `Cursor Settings` -> `MCP` -> `Add new MCP Server`.

Use command type with:

```bash
npx -y @ndhkaeru/codeloupe-mcp@latest
```

Or paste the standard config:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

</details>

<details>
<summary>Factory</summary>

Use the Factory CLI:

```bash
droid mcp add codeloupe-mcp "npx -y @ndhkaeru/codeloupe-mcp@latest"
```

Alternatively, open Factory's MCP UI and paste the standard config.

</details>

<details>
<summary>Gemini CLI</summary>

Use the Gemini CLI MCP configuration file and add:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

</details>

<details>
<summary>Goose</summary>

Go to `Advanced settings` -> `Extensions` -> `Add custom extension`.

Use type `STDIO` and command:

```bash
npx -y @ndhkaeru/codeloupe-mcp@latest
```

</details>

<details>
<summary>Junie</summary>

Use Junie's MCP flow, or add to `.junie/mcp/mcp.json`:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

</details>

<details>
<summary>Kiro</summary>

Follow Kiro's MCP server documentation and add:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

</details>

<details>
<summary>LM Studio</summary>

Go to `Program` in the right sidebar -> `Install` -> `Edit mcp.json`.

Use the standard config:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

</details>

<details>
<summary>opencode</summary>

Add to `~/.config/opencode/opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "codeloupe-mcp": {
      "type": "local",
      "command": ["npx", "-y", "@ndhkaeru/codeloupe-mcp@latest"],
      "enabled": true
    }
  }
}
```

</details>

<details>
<summary>Qodo Gen</summary>

Open Qodo Gen chat panel in VS Code or IntelliJ, choose `Connect more tools` -> `+ Add new MCP`, then paste the standard config.

</details>

<details>
<summary>VS Code</summary>

Use the MCP server configuration:

```json
{
  "servers": {
    "codeloupe-mcp": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

You can also install with the VS Code CLI:

```bash
code --add-mcp '{"name":"codeloupe-mcp","command":"npx","args":["-y","@ndhkaeru/codeloupe-mcp@latest"]}'
```

</details>

<details>
<summary>Warp</summary>

Go to `Settings` -> `AI` -> `Manage MCP Servers` -> `+ Add`, or use the `/add-mcp` slash command and paste the standard config.

</details>

<details>
<summary>Windsurf</summary>

Follow Windsurf's MCP documentation and use the standard config:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"]
    }
  }
}
```

</details>

### Configuration

`codeloupe-mcp` accepts repeatable `--workspace <path>` / `--workspace=<path>` arguments and environment variables in the MCP server process. Explicit CLI and environment workspaces are approved for indexing unless index mode is `off`. `--help` and `--version` print immediately without starting the stdio server; unknown CLI options fail instead of silently starting it.

After upgrading the server, reconnect or restart the MCP client so it fetches the current `tools/list` schema. Long-running clients such as Claude Code can keep the previous session's tool parameters until they reconnect.

| Variable | Description |
|----------|-------------|
| `CODELOUPE_MCP_INDEX_DIR` | Override the persistent index cache directory. |
| `CODELOUPE_MCP_WORKSPACES` | Explicit workspace roots encoded as a platform path list (`;` on Windows, `:` on Unix). |
| `CODELOUPE_MCP_WORKSPACE` | Compatibility singular form of `CODELOUPE_MCP_WORKSPACES`; it also accepts a platform path list. |
| `CODELOUPE_WORKSPACE` | Legacy alias for explicit workspace roots; distinct roots are retained alongside the `CODELOUPE_MCP_*` forms. |
| `CODELOUPE_MCP_INDEX_MODE` | Workspace indexing policy: `ask` (default), `auto`, or `off`. |
| `CODELOUPE_MCP_INDEX_ADVICE` | Set to `timeout_only` to suppress successful-call advice while retaining timeout advice. Defaults to all eligible advice moments. |
| `CODELOUPE_MCP_INDEX_ADVICE_MIN_FILES` | Minimum observed files before index advice is eligible. Defaults to `20000`. |
| `CODELOUPE_MCP_INDEX_MAX_ENTRIES` | Maximum entries in one metadata scan. Defaults to `300000`. |
| `CODELOUPE_MCP_INDEX_MAX_SCAN_SECONDS` | Maximum duration of one metadata scan. Defaults to `60`. |
| `CODELOUPE_MCP_INDEX_MAX_LOADED_WORKSPACES` | Maximum workspace index runtimes retained in memory. Defaults to `16`; least-recently-used inactive runtimes are evicted without deleting their on-disk indexes. |
| `CODELOUPE_MCP_INDEX_MAX_TOTAL_BYTES` | Maximum total persistent index storage. Defaults to 2 GiB and is enforced before writes as well as during GC. |
| `CODELOUPE_MCP_INDEX_MIN_FREE_BYTES` | Minimum free space retained on the index volume. Defaults to 5 GiB; the effective floor is the larger of this value and 10% of the volume. |
| `CODELOUPE_MCP_TANTIVY_ENABLED` | Enable/disable the Tantivy content sidecar. Defaults to `true`. |
| `CODELOUPE_MCP_WALK_THREADS` | Filesystem walk parallelism. Values are clamped to `1..=6`. |
| `CODELOUPE_MCP_WRITE_ROOTS` | Additional filesystem roots that write tools may modify, encoded as a platform path list (`;` on Windows, `:` on Unix). |
| `CODELOUPE_MCP_BINARY` | npm wrapper override for a custom local binary path. |
| `CODELOUPE_MCP_RUNTIME_COPY` | Windows npm launcher only. Set to `0` to run the native binary directly from `node_modules` instead of a versioned runtime copy. |
| `CODELOUPE_MCP_RUNTIME_DIR` | Windows npm launcher only. Directory for runtime copies of the native binary. Defaults to `%LOCALAPPDATA%\codeloupe-mcp\runtime`. |

Legacy `codeloupe_mcp_*` and `CODEBASE_MCP_*` names remain supported for compatibility; the uppercase `CODELOUPE_MCP_*` names take precedence.

On Windows, the npm launcher copies the native binary to `%LOCALAPPDATA%\codeloupe-mcp\runtime\<version>-<sha256 prefix>\` and runs that copy. Windows locks running executables, so running directly from `node_modules` made `npx -y @ndhkaeru/codeloupe-mcp@latest` upgrades fail with `EBUSY` or leave npm temp directories behind while another MCP session was still using the server. Copies unused for a day are removed on a later start. Servers started through launcher 1.1.1 or earlier still run from `node_modules`; if an upgrade reports `EBUSY`, stop those `codeloupe-mcp.exe` processes once and reconnect.

Workspace sources are processed in this order: repeatable CLI arguments, explicit workspace environment variables, MCP client roots (`workspaceFolders`, `roots`, `rootUri`, or `clientInfo.workspaceRoot`), a project-like process current directory, then workspace roots inferred from tool paths. In `ask` mode, only CLI/environment roots and remembered `workspace_index(action="enable")` decisions are approved for indexing; client roots, cwd, and tool paths remain candidates. `auto` indexes eligible candidates within the same protected-path, scan, and disk budgets. `off` disables indexing and advice. Relative tool paths prefer the current active workspace context, then an active index workspace, a workspace discovered from cwd, cwd itself, other configured workspaces, and finally loaded index workspaces.

Write roots are the exact MCP client roots and explicit `--workspace`/workspace-environment roots after canonicalization, the project-like process cwd when no roots were configured, plus `CODELOUPE_MCP_WRITE_ROOTS`. Writes inside a write root carry no location warning; other targets get a risk warning as described under [Tools](#tools). Discovering an ancestor repository root may expand index/search context, but never expands the write roots beyond the declared root. `workspace_index(action="enable")` grants indexing only and never adds a write root. Writes under `.git` and canonical junction/symlink escapes from a write root are critical-risk: they stop with `risk_confirmation_required` before any filesystem change until the call is retried with `acknowledge_risk=true`.

Example with a custom index directory:

```json
{
  "mcpServers": {
    "codeloupe-mcp": {
      "command": "npx",
      "args": ["-y", "@ndhkaeru/codeloupe-mcp@latest"],
      "env": {
        "CODELOUPE_MCP_INDEX_DIR": "/path/to/cache"
      }
    }
  }
}
```

### Tools

Tools are grouped by purpose. Each response uses the standard MCP `content` array shape and keeps output bounded for agent context.

The server negotiates supported MCP protocol versions (`2024-11-05`, `2025-03-26`, `2025-06-18`, and `2025-11-25`), returns an empty object for `ping`, and never responds to notifications. Notification-form `tools/call` messages are ignored without execution. `notifications/cancelled` suppresses the matching active response and propagates cooperative cancellation; multi-file scan tools report cooperative progress and stop early when cancelled. As a server extension, request-form `tools/call` may set `params.timeout_seconds` to an integer from 60 through 600; out-of-range or non-integer values return JSON-RPC `-32602` instead of being silently adjusted. Timeout responses are structured JSON containing elapsed time, observed scan progress, narrower candidate paths, and conditional `index_advice`.

<details>
<summary>Files and edits</summary>

| Tool | Description |
|------|-------------|
| `resolve_path` | Normalize a path and return preflight accessibility metadata. |
| `read_file_range` | Read a file or line range with encoding detection and truncation metadata. |
| `count_file_lines` | Count lines in a text file with encoding and binary detection. |
| `read_snippets` | Read multiple file ranges in one request. |
| `file_summary` | Return file metadata, binary detection, and a short preview. |
| `file_hash` | Compute a file's SHA-256 digest with streaming I/O. |
| `validate_json` | Validate JSON syntax from a UTF-8 file or inline content. |
| `convert_file_format` | Rewrite a file with normalized encoding and line endings. |
| `create_file` | Create or overwrite a file. |
| `create_directory` | Create a directory with optional parent creation. |
| `edit_file` | Edit a file with replace, append, prepend, or find-replace. |
| `edit_files` | Apply bounded exact edits across multiple existing text files with preflight validation and rollback. |
| `delete_file` | Delete one file with structured success/error metadata. |
| `list_history` | List metadata for recent process-local write snapshots without returning file contents. |
| `undo_change` | Restore one recorded before-snapshot when the current path still matches its recorded after-state. |

With `target_line_ending="preserve"`, `edit_file` normalizes appended, prepended, and find-replace fragments to an existing LF or CRLF file. Responses include `line_endings_normalized` and `mixed_line_endings`; mixed files are reported without silently rewriting their existing line endings.

`edit_file` and `convert_file_format` preserve the detected existing encoding when `target_encoding` is omitted. They support UTF-8, BOM-tagged UTF-16LE/UTF-16BE, and Windows-1252; responses expose `previous_encoding`, `target_encoding`, and `encoding_changed` when an explicit conversion is requested.

`edit_files` accepts up to 20 files and 100 ordered exact edits per call. Every edit requires `expected_replacements`; all paths, counts, encodings, and supported JSON/Tree-sitter syntax are preflighted before any write. Changed files are written atomically one at a time, with earlier writes rolled back if a later write fails. This is rollback-backed transaction behavior, not a filesystem-wide atomic commit.

`file_hash` and `read_file_range` compute a full-file SHA-256 digest with bounded-memory streaming I/O. `edit_file`, `delete_file`, and each `edit_files.files` entry accept that digest as `expected_hash`; stale or malformed preconditions are rejected before mutation, and single-file tools recheck the digest immediately before writing or deleting. Successful edit responses include before/after digests.

Successful write responses are compact and omit redundant `success`, `message`, and default-valued status fields. File mutations return `path`, `history_entry_id`, and `sha256_after` when a file exists after the operation, plus operation-specific fields such as `replacements`, `bytes_removed`, or `undone_entry_id`; exceptional conditions are reported only when they occur. `edit_files` applies the same contract to each changed file instead of repeating default encoding, validation, and history metadata.

Write authorization uses the `risk_aware` scope. Paths inside exact declared MCP/CLI/environment roots are written without a location warning. Other targets receive one short warning based on the highest detected risk: `low` outside known write roots, `medium` elsewhere in an inferred repository, `high` for configured sensitive patterns or a detected symlink/junction target, and `critical` for `.git`, sensitive home credential/profile locations, protected operating-system directories, or canonical escapes from a declared root/repository. Low, medium, and high-risk writes proceed immediately. A critical-risk call returns `risk_confirmation_required` before mutation; retry the same call with `acknowledge_risk=true` to proceed while retaining the warning. Sensitive hash-mismatch responses do not expose `actual_hash`. History entries outside declared roots include `outside_declared: true`; `workspace_index` controls indexing only and does not declare a write root.

`validate_json` performs streaming syntax validation without materializing the document. It accepts exactly one UTF-8 file path or inline JSON string, reports parser line/column details for invalid input, and does not perform JSON Schema validation.

Write history is in memory and local to the running server process. It retains at most 200 entries and 256 MiB of snapshot bytes, evicting the oldest entries first. `list_history` returns metadata only, and `undo_change` refuses to overwrite a path whose current contents or canonical location no longer match the recorded after-snapshot. History does not survive a server restart.

`read_file_range` supports line ranges, `tail` for the final N lines, bounded `start_byte` windows for very long lines, and streaming `json_pointer` selection using RFC 6901. Text responses remove a leading BOM, normalize CRLF and classic CR line endings to LF, report `bom`/`is_binary`, and never return NUL-containing binary content. Line, tail, and `start_byte` reads default to a 64 KiB output window when `max_bytes` is omitted; oversized first lines return a prefix with `line_truncated: true` and `next_start_byte`. `json_pointer` defaults to a 1 MiB serialized-value budget. `read_snippets` accepts the same per-request selectors and returns line- or byte-based continuations.

</details>

<details>
<summary>Search and workspace</summary>

| Tool | Description |
|------|-------------|
| `search_workspace` | Run path, symbol-definition, and exact text searches concurrently with grouped engine status. |
| `text_search` | Literal or regex search with path scopes, include/exclude filters, and fallback diagnostics. |
| `fuzzy_find` | Fuzzy path and file-name search across scoped roots. |
| `project_map` | Bounded tree view with optional size metadata. |
| `workspace_stats` | File, line, language, and size summaries. |
| `compare_directories` | Compare two source directory trees with inventory-first summaries and optional bounded diffs. |
| `workspace_index` | Estimate, enable, or disable indexing for one workspace without bypassing server safety budgets. |

Literal `text_search` uses Tantivy only to shortlist files, then always runs exact grep verification when the scope is allowed. A warmed search therefore reports `search_strategy: "mixed"` instead of treating the content index as the source of truth. Core index summary fields (`index_used`, `index_complete`, `index_age_secs`) remain at the top level. Timestamps, fallback reasons, candidate counters, bounded `unindexed_files_in_scope`, and related details move under `diagnostics` when `complete=false`, `explain_no_results=true`, warnings exist, or `verbose=true`. File-writing tools schedule refreshes for already-warmed content zones.

`search_workspace` does not choose an engine heuristically. It runs `fuzzy_find`, `find_definition`, and `text_search` concurrently, returns one shared `root` plus fixed `path`, `symbol`, and `text` groups with explicit `source`, `status`, `strategy`, `complete`, and limit fields, and preserves successful groups when another engine fails. `max_results` applies per group, while `max_output_bytes` bounds the complete serialized response and fairly removes oversized result items without dropping group status.

`text_search` accepts `output_format="json" | "markdown" | "compact"`. Markdown and compact text group matches by file and emit one line per match. All formats return a shared `root`, use paths relative to that root, report `complete`, and omit detailed diagnostics on normal complete results unless `verbose=true`.

`project_map`, `fuzzy_find`, `text_search`, `workspace_stats`, `find_definition`, `find_references`, and `search_workspace` accept `include_ignored=true` to include paths filtered by `.gitignore`, `.git/info/exclude`, global gitignore, or `.ignore` files. The same tools accept `include_hidden=true` to include hidden files and directories. Tools whose schema uses `paths` also accept `path` as a singular alias; passing both is an error. Inclusive scans bypass metadata/content indexes and use a filesystem walk. VCS metadata directories such as `.git`, `.hg`, and `.svn` remain excluded from root scans even with `include_hidden=true`, but an explicitly scoped hidden or ignored path remains searchable. Zero-result searches add bounded diagnostics when ignored or hidden entries were skipped and suggest the relevant inclusive flag. Search/discovery responses expose consistent `index_used`, `index_complete`, and `index_age_secs` fields. `project_map` tree keys are relative to `root` (`.` is the root directory); each key maps to separate `dirs` and `files` arrays so entries do not repeat a constant type field. Directory symlinks/junctions are represented in `dirs` without being traversed. File entries include `size_bytes` only when `show_sizes=true`. `fuzzy_find` result `path` values are relative to its single shared `root`; detailed traversal counters, timestamps, fallback reasons, skipped-item details, and warnings move under `diagnostics` when `complete=false` or `verbose=true`.

`workspace_stats` reports bounded `excluded_files.ignored` and `excluded_files.hidden` counts when default filters omit files. `counts_complete=false` means those counts are lower bounds because the filter probe reached its 50,000-file cap.

Path glob filters use the same `globset` semantics across filesystem-walk and metadata-index paths: brace alternation such as `*.{rs,py}` is supported, backslashes are normalized to `/`, matching is case-insensitive on Windows, and a pattern without `/` matches a file or directory name at any depth. A leading `!` is rejected instead of being treated inconsistently as a literal or negation; use the tool's `excludes` field explicitly. Directory excludes prune descendants in both walk and index modes.

Eligible multi-file scan responses may include `index_advice` when an unapproved marked workspace is large enough and indexing could help the current call. Advice is emitted at most once per workspace per server session, never by single-file tools, and is lifted to the top level of `batch_tool_call`. Use `workspace_index(action="estimate")` for a bounded read-only estimate, `enable` to persist index approval and start indexing, or `disable` to remove the index and remember the decline for 30 days. Index approval is separate from write authorization. `blocked` and `too_large` recommendations cannot be overridden through tool arguments. Paths inside application-data trees return `approval_required`; an agent may enable one only when the user explicitly requested work at that exact path, and the normal scan and disk budgets still apply.

</details>

<details>
<summary>Code intelligence</summary>

| Tool | Description |
|------|-------------|
| `get_symbols` | Extract Tree-sitter symbols, qualified names, and declaration lines from a source file. |
| `read_symbol_body` | Resolve qualified or unqualified symbols with AST-first resolution and fallback heuristics. |
| `find_definition` | Find case-sensitive qualified or unqualified definitions with AST-first resolution and labeled heuristic fallback. |
| `find_references` | Find token matches and classify AST-supported results as code, comment, or string. |
| `list_imports` | List imports for supported languages. |
| `list_exports` | List exports for supported languages. |
| `get_call_graph` | List outbound calls made by a qualified or unqualified function or method. |
| `compare_symbols` | Compare two unambiguous resolved symbols and return compact metadata plus a unified diff. |

</details>

Symbol queries accept both `Class.method` and `Class::method`, including deeper qualified names. `get_symbols` returns `qualified_name` and `start_line`; `read_symbol_body`, `get_call_graph`, and each `compare_symbols` side accept an optional `line` selector. An unqualified query with multiple AST matches returns `ambiguous: true` plus `candidates` instead of silently selecting the first match. `compare_symbols` omits `same_content` and `unified_diff` when either side is ambiguous.

`find_definition` uses Tree-sitter for supported languages, matches symbol names case-sensitively, and labels those results with `resolution: "ast"`. Unsupported code extensions use a declaration-regex fallback labeled `resolution: "heuristic"`. `find_references` performs token matching rather than binding or type resolution; Tree-sitter-supported files return `match_kind: "code" | "comment" | "string"` with `classification: "ast"`, mark a declaration-name match with `is_definition: true`, and leave ordinary references unmarked. Unsupported extensions return `match_kind: "unknown"` with `classification: "text_fallback"`.

`find_definition` and `find_references` keep result data plus `complete`/limit metadata at the top level. Scan counters, path-index/filesystem strategy, fallback counts, skipped-file details, and cancellation state appear under `diagnostics` only for incomplete results, zero-result filter hints, or when `verbose=true`. Default per-result metadata is omitted where it can be inferred; for example, ordinary AST code references do not repeat `classification: "ast"`, `match_kind: "code"`, or `line_truncated: false`.

Code-intelligence outputs use `path` consistently. Single-file tools return one normalized path, while `find_definition` and `find_references` return a shared `root` plus relative paths for matches and skipped-file details. Ambiguous `get_call_graph` candidates omit repeated paths because the top-level `path` applies to every candidate.

`compare_directories` defaults to an inventory-only JSON result; set `include_content_diff=true` when unified diffs are needed. `compare_symbols` returns the unified diff and symbol metadata without repeating both complete symbol bodies in `left` and `right`.

Byte-size output fields use the `_bytes` suffix consistently: `size_bytes` for one item and `left_size_bytes`/`right_size_bytes` for directory comparisons.

Bounded collection outputs use `complete`, `limit_reached`, and `limit_reason`. Bounded nested lists use names such as `entries_complete`, `details_complete`, or `candidates_complete`; `truncated` is reserved for returned text or byte content that was actually shortened.

<details>
<summary>Data and diagnostics</summary>

| Tool | Description |
|------|-------------|
| `peek_archive` | List up to 1,000 archive entries or read one inner file up to 10 MiB. |
| `server_health` | Report uptime, workspace, and index health. |
| `index_gc` | Inspect persistent index storage and optionally delete selected stale/orphaned indexes. |
| `content_index_status` | Report Tantivy content-index status for scoped files or directories. |
| `warm_content_index` | Warm Tantivy content index zones for repeated searches. |
| `batch_tool_call` | Run up to 20 ordered tools with a shared deadline, cancellation, fair output budgets, and bounded per-call results. |

Content-index status distinguishes a registered workspace that is still being scanned (`workspace_scanning`) from a path outside every registered workspace. A persisted `warming` state from an interrupted process is reloaded as `stale` and can be warmed again. Refreshing one zone does not make other indexed zones unavailable. A ready parent zone covers nested scopes, and nested warm/search/write refreshes reuse that parent instead of creating redundant child zones.

Tool execution failures use a machine-readable `{ "error": { "code", "message" } }` payload, including per-item `read_snippets` failures and `batch_tool_call` subcall failures. Structured write-tool failures are marked as MCP errors and omit the legacy top-level `success`, `error_code`, `message`, and `reason` aliases.

`tools/list` keeps each tool's description and complete validation constraints, but omits redundant titles and per-property prose. The server retains the full internal schemas for argument normalization and validation.

`warm_content_index` returns `outcome` (`scheduled`, `ready`, or `nothing_to_warm`) plus a readable `message`. Ignored scopes return `ignored_by_ignore_rules` unless `include_ignored=true`; candidates from inclusive warms are still filtered out of ordinary parent-scope searches. If every requested path is unavailable, the MCP result is marked `isError: true` while retaining structured per-path statuses. Content-index writes are serialized across processes; a busy Tantivy writer is retried instead of immediately marking the workspace as failed. Default source-content budgets are 2 MiB per file, 128 MiB per zone, and 256 MiB cumulatively per workspace; capped zones report `partial: true`.

`batch_tool_call` executes subcalls sequentially under a 55-second default batch deadline and a shared 1 MiB output budget. Its text payload starts with a compact `summary`; successful results and error messages both consume the fair per-call budget, oversized payloads are replaced with bounded previews, and calls that cannot start are reported as `skipped_deadline`, `skipped_cancelled`, or `skipped_output_budget` instead of disappearing.

`server_health` reports configured workspaces, active workspace context, index candidates and rejection reasons, safety budgets, the total persistent index-root size, and counts of loaded, unloaded, and orphaned workspace indexes. Each configured workspace reports its inferred `workspace_root` separately from exact `declared_roots` and `write_roots`. It reports `write_scope: "risk_aware"`; compatibility fields for write elicitation remain disabled and empty. `index_gc` is a dry-run unless `apply=true`; it can target workspace roots and applies the same default retention policy used at startup. Active indexes are protected by cross-process shared locks.

</details>

### Supported languages

AST-backed tools use Tree-sitter for Rust, C, C++, Go, Java, C#, PHP, Ruby, JavaScript, TypeScript, Python, Swift, and Objective-C.

`list_imports` and `list_exports` support Rust, JavaScript/TypeScript, Swift, Objective-C, Python, Go, Java, and C#. Python exports follow the public-name convention (top-level declarations not beginning with `_`); Go exports follow exported-identifier capitalization. File/search/workspace tools work on any text file regardless of language.

### Shared file limits

File-content limits are named centrally in `src/limits.rs` and are reported in schemas or responses where they affect results.

| Operation | Default or maximum |
|-----------|--------------------|
| UTF-8 line-range and tail reads | Streamed; no input-file size cap |
| Full-file text decoding, single-file edits, transaction files, symbol fallback reads, and archive entries | 10 MiB |
| AST parsing, definition candidates, and default compare content reads | 2 MiB per file |
| Reference candidates | 5 MiB per file |
| `file_summary` full line count | 50 MiB; larger files return `line_count_skipped: true` |
| `workspace_stats` line count | 2 MiB per file by default, configurable up to 20 MiB |
| `start_byte` output window | 64 KiB by default |
| `json_pointer` serialized value | 1 MiB by default |

Scan tools that skip oversized files return bounded path/size/limit details plus an omitted-detail count when needed.

### Large repository guidance

- Scope `paths` to the smallest useful subtree.
- Prefer literal `text_search` when possible so Tantivy can shortlist files.
- Use concrete basenames or path tokens with `fuzzy_find`.
- Keep `project_map` shallow, for example `max_depth <= 2`.
- Use recursive excludes such as `third_party/**`, `node_modules/**`, `target/**`, `dist/**`, and `out/**`.
- Use `read_snippets` after search results instead of reading whole files.

`text_search`, `compare_directories`, and content indexing share the same generated/vendor directory policy, including `build`, `dist`, `obj`, `out`, `target`, `node_modules`, `vendor`, and `third_party`. `bin` is treated as build output only for recognized .NET/Java workspaces so Node, Ruby, and similar source `bin/` directories remain searchable. Directly scoping `text_search` to an excluded directory bypasses these defaults.

### Index storage

The server keeps one index per canonical approved workspace root. Nested client roots may share an inferred ancestor index/search workspace, and discovery prefers an ancestor with a VCS marker (`.git`, `.hg`, or `.svn`) over a nested language manifest. Inferred ancestors do not become declared roots; writes elsewhere in that repository receive the medium-risk warning described above. Index approval decisions are stored in the human-readable `index-v2/workspaces.json`; enabled workspaces reload across sessions, while declines suppress advice for 30 days.

- Windows: `%LOCALAPPDATA%\codeloupe-mcp\index-v2\<workspace_hash>\`
- Linux/macOS: `$XDG_CACHE_HOME/codeloupe-mcp/index-v2/<workspace_hash>/` or `~/.cache/codeloupe-mcp/index-v2/<workspace_hash>/`
- Custom root: `<CODELOUPE_MCP_INDEX_DIR>\index-v2\<workspace_hash>\`
- Tantivy sidecar: `<workspace_index_dir>\tantivy-content\`
- Last-used sidecar: `<workspace_index_dir>\last-used`

`<workspace_hash>` is a stable FNV-1a hash of the normalized canonical root, so changing Rust toolchains does not rename every cache directory. When a matching pre-1.0 cache directory is found by its `meta.json`, it is renamed to the stable hash before opening.

At startup, index GC removes missing-workspace indexes after a 7-day grace period, removes indexes unused for 90 days, and enforces a 2 GiB total cap by least-recently-used order. The same total-size and free-space budgets are checked before new index writes. Metadata scans stop at the configured entry or time budget and persist `too_large` without creating a workspace index directory. Loaded or cross-process-locked indexes are never deleted. Tantivy stores searchable fields and file metadata, but not file content or path-token payloads.

Workspace roots are dynamic: explicit CLI/environment roots are approved first; client roots, a project-like process current directory, and workspace roots inferred from tool paths update the active context. Index loading runs in background so initialization and unrelated requests remain responsive; tools fall back to bounded filesystem access while a runtime is loading. Loaded runtimes use an in-memory LRU budget and can be reopened from their on-disk indexes after eviction. Recognized manifests include Rust, Node, Python, Go, Java/Gradle, Bazel, .NET (`*.sln`, `*.slnx`, `global.json`, `Directory.Build.props`), CMake, PHP Composer, Ruby, Elixir, and Scala/SBT projects. Tool paths without a recognized workspace marker still work through bounded filesystem access but never receive index advice. Explicit client roots remain valid active contexts even without markers; whether they index depends on `CODELOUPE_MCP_INDEX_MODE` or a persisted approval.

### Shipping

Releases are distributed through two channels:

- **GitHub Releases**: a semver tag such as `v1.0.0` builds native archives/installers for macOS, Linux, and Windows on x64/arm64. Linux archives use static musl targets so they do not require a recent glibc and can run on musl-based distributions such as Alpine.
- **npm/npx**: `@ndhkaeru/codeloupe-mcp` is a small JavaScript launcher. npm installs one OS/CPU-specific optional package containing only the matching native binary instead of downloading all six platforms.

Release checklist:

1. Update versions in `Cargo.toml`, `Cargo.lock`, `packages/npm/package.json`, every `packages/npm-platforms/*/package.json`, and `server.json`.
2. Run the checks from `rust.yml`: format, clippy, tests, audit, deny, and typos.
3. Push `main`, then check GitHub Actions for failures.
4. Push a semver tag such as `v1.0.0`.
5. Confirm release assets, all six platform npm packages, and the launcher npm package publish successfully.
6. Smoke test `npx -y @ndhkaeru/codeloupe-mcp@<version>`.

### Development

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --release
```

### License

Apache-2.0. See [LICENSE](./LICENSE).

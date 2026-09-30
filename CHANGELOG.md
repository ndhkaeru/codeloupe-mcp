# Changelog

All notable changes to `codeloupe-mcp` are documented in this file.

## [Unreleased]

### Security

- Critical-risk writes now stop before mutation and return `risk_confirmation_required`; callers must retry with `acknowledge_risk=true`. Low, medium, and high-risk writes still proceed with warnings.
- Junction/symlink warnings include the canonical target, and critical-path hash mismatch errors no longer expose `actual_hash`.

### Fixed

- Bounded default line/tail reads to 64 KiB with byte continuations for oversized lines, and reject byte limits too small to advance a UTF-8 continuation.
- Added a validated singular `path` alias for plural-path tools, immediate CLI `--help`/`--version`, definition marking in `find_references`, correct directory-link representation in `project_map`, and source-checkout gating for the npm launcher fallback.

### Distribution

- Split npm native binaries into six OS/CPU-specific optional packages so the launcher installation downloads only the current platform, with a packed-install E2E check for launcher resolution.
- Switched Linux release targets from glibc-linked GNU binaries to static musl binaries for older distributions and Alpine compatibility.

## [1.1.0] - 2026-09-29

### Breaking Changes

- Compact tool responses now omit redundant success flags, messages, aliases, default-valued metadata, and repeated absolute roots. Consumers that parsed the previous verbose payloads must use the documented compact fields instead.
- Search, symbol, comparison, history, batch, and write responses use consistent root-relative paths and nested diagnostics. Several legacy top-level counters and path aliases were removed.
- Successful file mutations use a shared compact contract built around `path`, `history_entry_id`, and `sha256_after`, plus operation-specific fields.
- Failed write-tool responses use the canonical nested `error` object and omit legacy top-level `success`, `error_code`, `message`, and `reason` fields.
- `tools/list` schemas are compact but retain validation constraints. MCP clients must reconnect after upgrading to load new parameters such as `include_hidden` and `include_ignored`.

### Added

- Added `search_workspace` for concurrent path, symbol-definition, and text search with independent completeness and strategy metadata.
- Added consistent `include_ignored` and `include_hidden` support across discovery, search, symbol, and workspace-stat tools, with zero-result guidance and bounded excluded-file counts.
- Added MCP client Roots support: the server sends `roots/list` after initialization, refreshes on `notifications/roots/list_changed`, and replaces only the dynamic roots from that capability.
- Added warning-only write classification with short `low`, `medium`, `high`, and `critical` risk messages based on the target path.
- Added resumable byte-window reads for oversized lines, expanded AST-aware symbol support, transactional multi-file edits, bounded undo history, hash preconditions, JSON validation, and workspace index lifecycle controls.

### Security

- Separated indexing approval from write authorization and restricted implicit write roots to project-like directories rather than home or filesystem roots.
- Replaced path-policy blocks and write elicitation with the `warning_only` scope. `.git`, sensitive home paths, operating-system directories, configured sensitive patterns, and canonical symlink/junction escapes now emit high or critical warnings instead of `path_blocked`; filesystem permissions still apply.
- Hardened JSON-RPC framing, initialization lifecycle, cancellation, archive reads, scan budgets, disk budgets, path normalization, and workspace boundary enforcement.

### Fixed

- Corrected stale fuzzy-find metadata, long-line continuations, root-search thresholds, index-advice telemetry, ignored/hidden coverage reporting, Windows index removal, UTF-16 preservation, glob consistency, content-index refresh behavior, and large-file search/read correctness.
- Reduced repeated response data while preserving completeness, truncation, fallback, and error evidence needed by agents.

## [1.0.0] - 2026-07-09

- Initial public release.

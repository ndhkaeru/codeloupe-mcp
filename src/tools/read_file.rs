use anyhow::{Context, Result};
use encoding_rs::{UTF_16BE, UTF_16LE, WINDOWS_1252};
use serde::Deserialize;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::common::insert_object_field;
use crate::limits::{
    BINARY_PROBE_BYTES, DEFAULT_BYTE_RANGE_BYTES, DEFAULT_JSON_POINTER_OUTPUT_BYTES,
    MAX_IN_MEMORY_TEXT_FILE_BYTES,
};

pub fn schema() -> Value {
    json!({
        "name": "read_file_range",
        "title": "Read file range",
        "description": "Read one focused file range with encoding detection, full-file SHA-256, and truncation metadata. Use after search/path discovery; prefer line ranges over whole-file reads.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to read. Relative paths resolve against the active workspace." },
                "start_line": { "type": "integer", "minimum": 1, "description": "1-indexed inclusive start line. Defaults to 1." },
                "end_line": { "type": "integer", "description": "1-indexed inclusive end line. Omit to read until max_lines/max_bytes or EOF." },
                "tail": { "type": "integer", "minimum": 1, "description": "Read the final N text lines. Mutually exclusive with start_line, end_line, and start_byte." },
                "start_byte": { "type": "integer", "minimum": 0, "description": "Read a bounded text window from this zero-based raw byte offset. Mutually exclusive with line ranges and tail. Defaults max_bytes to 65536." },
                "json_pointer": { "type": "string", "description": "Read one JSON branch selected by an RFC 6901 pointer. Non-target branches are skipped while parsing. Mutually exclusive with line, tail, and byte ranges." },
                "max_lines": { "type": "integer", "description": "Maximum number of lines to return. Applied before/alongside max_bytes for bounded output." },
                "max_bytes": { "type": "integer", "description": "Maximum UTF-8 output bytes to return. If both max_lines and max_bytes are set, output stops at the first limit reached." },
                "include_line_numbers": { "type": "boolean", "description": "Prefix returned lines with 1-indexed line numbers when true. Defaults to false." }
            },
            "required": ["path"]
        }
    })
}

struct LimitedContent {
    content: String,
    returned_lines: usize,
    truncated: bool,
    line_truncated: bool,
    omitted_lines: usize,
    next_start_line: Option<usize>,
    next_start_byte: Option<u64>,
    end_line: usize,
}

pub fn decode_fuzzy(buffer: &[u8]) -> (String, &'static str) {
    if buffer.starts_with(&[0xFF, 0xFE]) {
        let (cow, _, _) = UTF_16LE.decode(&buffer[2..]);
        return (cow.into_owned(), "UTF-16LE");
    }

    if buffer.starts_with(&[0xFE, 0xFF]) {
        let (cow, _, _) = UTF_16BE.decode(&buffer[2..]);
        return (cow.into_owned(), "UTF-16BE");
    }

    match std::str::from_utf8(buffer) {
        Ok(s) => (s.to_string(), "UTF-8"),
        Err(_) => {
            let (cow, encoding, _) = WINDOWS_1252.decode(buffer);
            (cow.into_owned(), encoding.name())
        }
    }
}

pub(crate) fn has_utf8_bom(buffer: &[u8]) -> bool {
    buffer.starts_with(&[0xEF, 0xBB, 0xBF])
}

pub(crate) fn has_utf16_bom(buffer: &[u8]) -> bool {
    buffer.starts_with(&[0xFF, 0xFE]) || buffer.starts_with(&[0xFE, 0xFF])
}

pub(crate) fn has_text_bom(buffer: &[u8]) -> bool {
    has_utf8_bom(buffer) || has_utf16_bom(buffer)
}

pub(crate) fn is_probably_binary(buffer: &[u8]) -> bool {
    !has_utf16_bom(buffer) && buffer.contains(&b'\x00')
}

pub(crate) fn normalize_read_line_endings(content: &str) -> String {
    if content.contains('\r') {
        content.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        content.to_string()
    }
}

pub(crate) fn decode_for_read(buffer: &[u8]) -> (String, &'static str, bool) {
    let bom = has_text_bom(buffer);
    let (mut content, encoding) = decode_fuzzy(buffer);
    if content.starts_with('\u{feff}') {
        content.remove(0);
    }
    (normalize_read_line_endings(&content), encoding, bom)
}

fn split_text_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        Vec::new()
    } else {
        content.split_terminator('\n').collect()
    }
}

pub(crate) struct TextFileInspection {
    pub(crate) encoding: Option<&'static str>,
    pub(crate) bom: bool,
    pub(crate) is_binary: bool,
    pub(crate) line_count: usize,
    pub(crate) ends_with_newline: bool,
}

pub(crate) fn inspect_text_file(path: &Path) -> Result<TextFileInspection> {
    let mut probe_file = File::open(path)?;
    let mut probe = vec![0u8; BINARY_PROBE_BYTES];
    let probe_len = probe_file.read(&mut probe)?;
    probe.truncate(probe_len);
    let bom = has_text_bom(&probe);

    if has_utf16_bom(&probe) {
        return inspect_utf16_file(path, probe.starts_with(&[0xFF, 0xFE]), bom);
    }
    if is_probably_binary(&probe) {
        return Ok(binary_inspection(bom));
    }

    let utf8_probe = probe.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&probe);
    let encoding = if std::str::from_utf8(utf8_probe).is_ok() {
        "UTF-8"
    } else {
        WINDOWS_1252.name()
    };
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(if has_utf8_bom(&probe) { 3 } else { 0 }))?;
    let mut reader = BufReader::new(file);
    let mut buffer = [0u8; 64 * 1024];
    let mut counter = LineCounter::default();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for &byte in &buffer[..read] {
            if byte == 0 {
                return Ok(binary_inspection(bom));
            }
            counter.push(byte as u16);
        }
    }
    let (line_count, ends_with_newline) = counter.finish();
    Ok(TextFileInspection {
        encoding: Some(encoding),
        bom,
        is_binary: false,
        line_count,
        ends_with_newline,
    })
}

fn inspect_utf16_file(path: &Path, little_endian: bool, bom: bool) -> Result<TextFileInspection> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(2))?;
    let mut reader = BufReader::new(file);
    let mut buffer = [0u8; 64 * 1024];
    let mut carry = None;
    let mut counter = LineCounter::default();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let mut index = 0usize;
        if let Some(first) = carry.take() {
            let unit = if little_endian {
                u16::from_le_bytes([first, buffer[0]])
            } else {
                u16::from_be_bytes([first, buffer[0]])
            };
            counter.push(unit);
            index = 1;
        }
        while index + 1 < read {
            let pair = [buffer[index], buffer[index + 1]];
            counter.push(if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            });
            index += 2;
        }
        if index < read {
            carry = Some(buffer[index]);
        }
    }
    let (line_count, ends_with_newline) = counter.finish();
    Ok(TextFileInspection {
        encoding: Some(if little_endian {
            "UTF-16LE"
        } else {
            "UTF-16BE"
        }),
        bom,
        is_binary: false,
        line_count,
        ends_with_newline,
    })
}

fn binary_inspection(bom: bool) -> TextFileInspection {
    TextFileInspection {
        encoding: None,
        bom,
        is_binary: true,
        line_count: 0,
        ends_with_newline: false,
    }
}

#[derive(Default)]
struct LineCounter {
    separators: usize,
    saw_unit: bool,
    pending_cr: bool,
    ends_with_newline: bool,
}

impl LineCounter {
    fn push(&mut self, unit: u16) {
        self.saw_unit = true;
        if self.pending_cr {
            self.separators += 1;
            self.pending_cr = false;
            self.ends_with_newline = true;
            if unit == b'\n' as u16 {
                return;
            }
        }
        match unit {
            unit if unit == b'\r' as u16 => self.pending_cr = true,
            unit if unit == b'\n' as u16 => {
                self.separators += 1;
                self.ends_with_newline = true;
            }
            _ => self.ends_with_newline = false,
        }
    }

    fn finish(mut self) -> (usize, bool) {
        if self.pending_cr {
            self.separators += 1;
            self.ends_with_newline = true;
        }
        let line_count = if self.saw_unit {
            self.separators + usize::from(!self.ends_with_newline)
        } else {
            0
        };
        (line_count, self.ends_with_newline)
    }
}

pub async fn execute(args: &Value) -> Result<Value> {
    execute_sync(args)
}

pub(crate) fn execute_sync(args: &Value) -> Result<Value> {
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .context("Missing path")?;
    let path = crate::common::resolve_tool_path(path_str);

    if !path.exists() || !path.is_file() {
        return Err(anyhow::anyhow!(
            "File does not exist or is not a file: {}",
            path_str
        ));
    }

    let meta = std::fs::metadata(&path)?;

    let has_start_line = args.get("start_line").is_some();
    let has_end_line = args.get("end_line").is_some();
    let start_line = args.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    let end_line = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX) as usize;
    let max_lines = args
        .get("max_lines")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize);
    let max_bytes = args
        .get("max_bytes")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize);
    let include_line_numbers = args
        .get("include_line_numbers")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let tail = args
        .get("tail")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let start_byte = args.get("start_byte").and_then(Value::as_u64);
    let json_pointer = args.get("json_pointer").and_then(Value::as_str);

    if start_line == 0 {
        return Err(anyhow::anyhow!("start_line must be >= 1"));
    }
    if has_end_line && end_line < start_line {
        return Err(anyhow::anyhow!("end_line must be >= start_line"));
    }
    if tail == Some(0) {
        return Err(anyhow::anyhow!("tail must be >= 1"));
    }
    if tail.is_some()
        && (has_start_line || has_end_line || start_byte.is_some() || json_pointer.is_some())
    {
        return Err(anyhow::anyhow!(
            "tail is mutually exclusive with start_line, end_line, and start_byte"
        ));
    }
    if start_byte.is_some()
        && (has_start_line
            || has_end_line
            || tail.is_some()
            || json_pointer.is_some()
            || max_lines.is_some()
            || include_line_numbers)
    {
        return Err(anyhow::anyhow!(
            "start_byte is mutually exclusive with line ranges, tail, max_lines, and include_line_numbers"
        ));
    }
    if json_pointer.is_some()
        && (has_start_line
            || has_end_line
            || tail.is_some()
            || start_byte.is_some()
            || max_lines.is_some()
            || include_line_numbers)
    {
        return Err(anyhow::anyhow!(
            "json_pointer is mutually exclusive with line ranges, tail, start_byte, max_lines, and include_line_numbers"
        ));
    }
    let sha256 = super::file_hash::sha256_file(&path)?;

    if let Some(start_byte) = start_byte {
        let mut response = read_byte_range(&path, meta.len(), start_byte, max_bytes)?;
        insert_object_field(&mut response, "sha256", json!(sha256));
        return Ok(response);
    }
    if let Some(json_pointer) = json_pointer {
        let mut response = read_json_pointer(&path, meta.len(), json_pointer, max_bytes)?;
        insert_object_field(&mut response, "sha256", json!(sha256));
        return Ok(response);
    }

    let selection = match tail {
        Some(lines) => LineSelection::Tail { lines },
        None => LineSelection::Range {
            start_line,
            end_line,
        },
    };
    let line_max_bytes = max_bytes.or(Some(DEFAULT_BYTE_RANGE_BYTES));

    let streaming_result = read_utf8_streaming(
        &path,
        selection,
        include_line_numbers,
        max_lines,
        line_max_bytes,
    )?;
    let read_result = match streaming_result {
        StreamingRead::Text(result) => result,
        StreamingRead::Binary { bom } => {
            let mut response = binary_response(meta.len(), bom);
            insert_object_field(&mut response, "sha256", json!(sha256));
            return Ok(response);
        }
        StreamingRead::NeedsFullDecode => {
            if meta.len() > MAX_IN_MEMORY_TEXT_FILE_BYTES {
                return Err(anyhow::anyhow!(
                    "File is too large for full-file decoding ({} bytes > {} byte limit)",
                    meta.len(),
                    MAX_IN_MEMORY_TEXT_FILE_BYTES
                ));
            }
            read_range_full_decode(
                &path,
                selection,
                include_line_numbers,
                max_lines,
                line_max_bytes,
            )?
        }
    };

    let mut response = json!({
        "content": read_result.limited.content,
        "start_line": read_result.start_line,
        "end_line": read_result.limited.end_line,
        "total_lines": read_result.total_lines,
        "encoding": read_result.encoding,
        "bom": read_result.bom,
        "is_binary": false,
        "size_bytes": meta.len(),
        "truncated": read_result.limited.truncated,
        "omitted_lines": read_result.limited.omitted_lines,
        "returned_lines": read_result.limited.returned_lines
    });
    insert_object_field(&mut response, "sha256", json!(sha256));

    if let Some(next_start_line) = read_result.limited.next_start_line {
        insert_object_field(&mut response, "next_start_line", json!(next_start_line));
    }
    if read_result.limited.line_truncated {
        insert_object_field(&mut response, "line_truncated", json!(true));
    }
    if let Some(next_start_byte) = read_result.limited.next_start_byte {
        insert_object_field(&mut response, "next_start_byte", json!(next_start_byte));
    }

    Ok(response)
}

struct ReadRangeResult {
    limited: LimitedContent,
    start_line: usize,
    total_lines: usize,
    encoding: &'static str,
    bom: bool,
}

#[derive(Clone, Copy)]
enum LineSelection {
    Range { start_line: usize, end_line: usize },
    Tail { lines: usize },
}

enum StreamingRead {
    Text(ReadRangeResult),
    Binary { bom: bool },
    NeedsFullDecode,
}

fn binary_response(file_size_bytes: u64, bom: bool) -> Value {
    json!({
        "content": "",
        "encoding": Value::Null,
        "bom": bom,
        "is_binary": true,
        "size_bytes": file_size_bytes,
        "truncated": false,
        "returned_lines": 0,
        "omitted_lines": 0,
        "warnings": ["This appears to be a binary file; text content was not returned."]
    })
}

fn read_utf8_streaming(
    path: &Path,
    selection: LineSelection,
    include_line_numbers: bool,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> Result<StreamingRead> {
    match selection {
        LineSelection::Range {
            start_line,
            end_line,
        } => read_utf8_range_streaming(
            path,
            start_line,
            end_line,
            include_line_numbers,
            max_lines,
            max_bytes,
        ),
        LineSelection::Tail { lines } => {
            read_utf8_tail_streaming(path, lines, include_line_numbers, max_lines, max_bytes)
        }
    }
}

fn read_utf8_range_streaming(
    path: &Path,
    start_line: usize,
    end_line: usize,
    include_line_numbers: bool,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> Result<StreamingRead> {
    let line_limit = max_lines.unwrap_or(usize::MAX);
    let byte_limit = max_bytes.unwrap_or(usize::MAX);
    let mut rendered_lines = Vec::<String>::new();
    let mut selected_lines = 0usize;
    let mut used_bytes = 0usize;
    let mut output_limit_reached = false;
    let mut line_truncated = false;
    let mut next_start_byte = None;
    let mut non_progressing_limit = false;

    let outcome = visit_utf8_lines(path, |line_number, line_start_byte, line| {
        if line_number < start_line || line_number > end_line {
            return;
        }
        selected_lines += 1;

        if output_limit_reached || rendered_lines.len() >= line_limit {
            output_limit_reached = true;
            return;
        }

        let rendered = render_line(line_number, line, include_line_numbers);
        let separator_bytes = usize::from(!rendered_lines.is_empty());
        if used_bytes
            .saturating_add(separator_bytes)
            .saturating_add(rendered.len())
            > byte_limit
        {
            if rendered_lines.is_empty() && rendered.len() > byte_limit {
                let prefix = utf8_prefix(&rendered, byte_limit);
                let rendered_prefix_bytes = if include_line_numbers {
                    format!("{}: ", line_number).len()
                } else {
                    0
                };
                let raw_bytes_consumed = prefix
                    .len()
                    .saturating_sub(rendered_prefix_bytes)
                    .min(line.len());
                if raw_bytes_consumed == 0 {
                    non_progressing_limit = true;
                } else {
                    rendered_lines.push(prefix.to_string());
                    line_truncated = true;
                    next_start_byte =
                        Some(line_start_byte.saturating_add(raw_bytes_consumed as u64));
                }
            }
            output_limit_reached = true;
            return;
        }

        used_bytes += separator_bytes + rendered.len();
        rendered_lines.push(rendered);
    })?;

    let info = match outcome {
        Utf8VisitOutcome::Text(info) => info,
        Utf8VisitOutcome::Binary { bom } => return Ok(StreamingRead::Binary { bom }),
        Utf8VisitOutcome::NeedsFullDecode => return Ok(StreamingRead::NeedsFullDecode),
    };
    if non_progressing_limit {
        return Err(anyhow::anyhow!(
            "max_bytes is too small to return any complete UTF-8 content from the selected line"
        ));
    }
    let actual_start = std::cmp::min(start_line - 1, info.total_lines) + 1;
    let actual_end = std::cmp::min(end_line, info.total_lines);
    let limited = finish_streamed_content(
        rendered_lines,
        selected_lines,
        actual_start,
        info.ends_with_newline && actual_end == info.total_lines,
        line_truncated,
        next_start_byte,
    );

    Ok(StreamingRead::Text(ReadRangeResult {
        limited,
        start_line: actual_start,
        total_lines: info.total_lines,
        encoding: "UTF-8",
        bom: info.bom,
    }))
}

fn read_utf8_tail_streaming(
    path: &Path,
    tail_lines: usize,
    include_line_numbers: bool,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> Result<StreamingRead> {
    let mut tail = VecDeque::<(usize, u64, String)>::with_capacity(tail_lines.min(4096));
    let outcome = visit_utf8_lines(path, |line_number, line_start_byte, line| {
        if tail.len() == tail_lines {
            tail.pop_front();
        }
        tail.push_back((line_number, line_start_byte, line.to_string()));
    })?;
    let info = match outcome {
        Utf8VisitOutcome::Text(info) => info,
        Utf8VisitOutcome::Binary { bom } => return Ok(StreamingRead::Binary { bom }),
        Utf8VisitOutcome::NeedsFullDecode => return Ok(StreamingRead::NeedsFullDecode),
    };

    let actual_start = tail
        .front()
        .map(|(line_number, _, _)| *line_number)
        .unwrap_or(info.total_lines + 1);
    let first_line_start_byte = tail.front().map(|(_, start_byte, _)| *start_byte);
    let line_refs = tail
        .iter()
        .map(|(_, _, line)| line.as_str())
        .collect::<Vec<_>>();
    let limited = apply_limits(
        &line_refs,
        actual_start,
        include_line_numbers,
        max_lines,
        max_bytes,
        info.ends_with_newline && !line_refs.is_empty(),
        first_line_start_byte,
    )?;

    Ok(StreamingRead::Text(ReadRangeResult {
        limited,
        start_line: actual_start,
        total_lines: info.total_lines,
        encoding: "UTF-8",
        bom: info.bom,
    }))
}

fn finish_streamed_content(
    rendered_lines: Vec<String>,
    selected_lines: usize,
    actual_start: usize,
    append_terminal_newline: bool,
    line_truncated: bool,
    next_start_byte: Option<u64>,
) -> LimitedContent {
    let returned_lines = rendered_lines.len();
    let completed_lines = returned_lines.saturating_sub(usize::from(line_truncated));
    let omitted_lines = selected_lines.saturating_sub(completed_lines);
    let truncated = line_truncated || omitted_lines > 0;
    let next_start_line = (truncated && !line_truncated).then_some(actual_start + returned_lines);
    let end_line = returned_lines
        .checked_sub(1)
        .map(|offset| actual_start + offset)
        .unwrap_or_else(|| actual_start.saturating_sub(1));
    let mut content = rendered_lines.join("\n");
    if append_terminal_newline && !truncated && returned_lines > 0 {
        content.push('\n');
    }
    LimitedContent {
        content,
        returned_lines,
        truncated,
        line_truncated,
        omitted_lines,
        next_start_line,
        next_start_byte,
        end_line,
    }
}

struct Utf8StreamInfo {
    total_lines: usize,
    ends_with_newline: bool,
    bom: bool,
}

enum Utf8VisitOutcome {
    Text(Utf8StreamInfo),
    Binary { bom: bool },
    NeedsFullDecode,
}

fn visit_utf8_lines(
    path: &Path,
    mut visitor: impl FnMut(usize, u64, &str),
) -> Result<Utf8VisitOutcome> {
    let mut file = File::open(path)?;
    let mut prefix = [0u8; 3];
    let prefix_len = file.read(&mut prefix)?;
    if prefix_len >= 2 && has_utf16_bom(&prefix[..prefix_len]) {
        return Ok(Utf8VisitOutcome::NeedsFullDecode);
    }
    let bom = prefix_len >= 3 && has_utf8_bom(&prefix);
    file.seek(SeekFrom::Start(if bom { 3 } else { 0 }))?;

    let mut reader = BufReader::new(file);
    let mut chunk = [0u8; 64 * 1024];
    let mut current_line = Vec::<u8>::new();
    let mut total_lines = 0usize;
    let mut pending_cr = false;
    let mut ends_with_newline = false;
    let mut byte_offset = if bom { 3u64 } else { 0u64 };
    let mut line_start_byte = byte_offset;

    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        for &byte in &chunk[..read] {
            if byte == 0 {
                return Ok(Utf8VisitOutcome::Binary { bom });
            }
            if pending_cr {
                if !emit_utf8_line(
                    &mut current_line,
                    &mut total_lines,
                    line_start_byte,
                    &mut visitor,
                ) {
                    return Ok(Utf8VisitOutcome::NeedsFullDecode);
                }
                pending_cr = false;
                ends_with_newline = true;
                if byte == b'\n' {
                    byte_offset = byte_offset.saturating_add(1);
                    line_start_byte = byte_offset;
                    continue;
                }
                line_start_byte = byte_offset;
            }

            match byte {
                b'\r' => {
                    pending_cr = true;
                    byte_offset = byte_offset.saturating_add(1);
                }
                b'\n' => {
                    if !emit_utf8_line(
                        &mut current_line,
                        &mut total_lines,
                        line_start_byte,
                        &mut visitor,
                    ) {
                        return Ok(Utf8VisitOutcome::NeedsFullDecode);
                    }
                    byte_offset = byte_offset.saturating_add(1);
                    line_start_byte = byte_offset;
                    ends_with_newline = true;
                }
                _ => {
                    current_line.push(byte);
                    byte_offset = byte_offset.saturating_add(1);
                    ends_with_newline = false;
                }
            }
        }
    }

    if pending_cr {
        if !emit_utf8_line(
            &mut current_line,
            &mut total_lines,
            line_start_byte,
            &mut visitor,
        ) {
            return Ok(Utf8VisitOutcome::NeedsFullDecode);
        }
        ends_with_newline = true;
    } else if !current_line.is_empty() {
        if !emit_utf8_line(
            &mut current_line,
            &mut total_lines,
            line_start_byte,
            &mut visitor,
        ) {
            return Ok(Utf8VisitOutcome::NeedsFullDecode);
        }
        ends_with_newline = false;
    }

    Ok(Utf8VisitOutcome::Text(Utf8StreamInfo {
        total_lines,
        ends_with_newline,
        bom,
    }))
}

fn emit_utf8_line(
    current_line: &mut Vec<u8>,
    total_lines: &mut usize,
    line_start_byte: u64,
    visitor: &mut impl FnMut(usize, u64, &str),
) -> bool {
    let Ok(line) = std::str::from_utf8(current_line) else {
        return false;
    };
    *total_lines += 1;
    visitor(*total_lines, line_start_byte, line);
    current_line.clear();
    true
}

fn read_range_full_decode(
    path: &Path,
    selection: LineSelection,
    include_line_numbers: bool,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> Result<ReadRangeResult> {
    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    if is_probably_binary(&buffer) {
        return Err(anyhow::anyhow!("File appears to be binary"));
    }
    let (content, encoding, bom) = decode_for_read(&buffer);
    let lines = split_text_lines(&content);
    let total_lines = lines.len();
    let ends_with_newline = content.ends_with('\n');
    let (actual_start, actual_end) = match selection {
        LineSelection::Range {
            start_line,
            end_line,
        } => (
            std::cmp::min(start_line - 1, total_lines),
            std::cmp::min(end_line, total_lines),
        ),
        LineSelection::Tail { lines } => (total_lines.saturating_sub(lines), total_lines),
    };
    let selected_lines = if actual_start <= actual_end {
        &lines[actual_start..actual_end]
    } else {
        &[]
    };
    let limited = apply_limits(
        selected_lines,
        actual_start + 1,
        include_line_numbers,
        max_lines,
        max_bytes,
        ends_with_newline && actual_end == total_lines,
        None,
    )?;

    Ok(ReadRangeResult {
        limited,
        start_line: actual_start + 1,
        total_lines,
        encoding,
        bom,
    })
}

fn read_byte_range(
    path: &Path,
    file_size_bytes: u64,
    requested_start_byte: u64,
    max_bytes: Option<usize>,
) -> Result<Value> {
    let mut probe_file = File::open(path)?;
    let mut probe = vec![0u8; BINARY_PROBE_BYTES.min(file_size_bytes as usize)];
    let probe_len = probe_file.read(&mut probe)?;
    probe.truncate(probe_len);
    let bom = has_text_bom(&probe);
    if is_probably_binary(&probe) {
        return Ok(binary_response(file_size_bytes, bom));
    }
    if has_utf16_bom(&probe) {
        return Err(anyhow::anyhow!(
            "start_byte is supported only for UTF-8 or Windows-1252 text files"
        ));
    }

    let byte_limit = max_bytes.unwrap_or(DEFAULT_BYTE_RANGE_BYTES);
    if byte_limit == 0 {
        return Err(anyhow::anyhow!(
            "max_bytes must be >= 1 when start_byte is used"
        ));
    }

    let mut actual_start_byte = requested_start_byte.min(file_size_bytes);
    if actual_start_byte == 0 && has_utf8_bom(&probe) {
        actual_start_byte = 3.min(file_size_bytes);
    }
    let available = file_size_bytes.saturating_sub(actual_start_byte);
    let read_limit = available.min(byte_limit.saturating_add(4) as u64) as usize;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(actual_start_byte))?;
    let mut buffer = vec![0u8; read_limit];
    let bytes_read = file.read(&mut buffer)?;
    buffer.truncate(bytes_read);
    if buffer.contains(&0) {
        return Ok(binary_response(file_size_bytes, bom));
    }

    let mut leading_continuation_bytes = 0usize;
    while leading_continuation_bytes < buffer.len()
        && leading_continuation_bytes < 3
        && buffer[leading_continuation_bytes] & 0b1100_0000 == 0b1000_0000
    {
        leading_continuation_bytes += 1;
    }
    actual_start_byte = actual_start_byte.saturating_add(leading_continuation_bytes as u64);
    let candidate = &buffer[leading_continuation_bytes..];
    let requested_len = byte_limit.min(candidate.len());
    let window = &candidate[..requested_len];

    let (content, encoding, raw_bytes_consumed) = match std::str::from_utf8(window) {
        Ok(text) => (normalize_read_line_endings(text), "UTF-8", window.len()),
        Err(error) if error.error_len().is_none() => {
            let valid_len = error.valid_up_to();
            if valid_len == 0 && !window.is_empty() {
                return Err(anyhow::anyhow!(
                    "max_bytes is too small to include the next complete UTF-8 character"
                ));
            }
            let text = std::str::from_utf8(&window[..valid_len])?;
            (normalize_read_line_endings(text), "UTF-8", valid_len)
        }
        Err(_) => {
            let (decoded, encoding, _) = WINDOWS_1252.decode(window);
            (
                normalize_read_line_endings(&decoded),
                encoding.name(),
                window.len(),
            )
        }
    };
    let end_byte = actual_start_byte.saturating_add(raw_bytes_consumed as u64);
    let truncated = end_byte < file_size_bytes;
    let mut response = json!({
        "content": content,
        "start_byte": actual_start_byte,
        "end_byte": end_byte,
        "requested_start_byte": requested_start_byte,
        "returned_bytes": content.len(),
        "raw_bytes_consumed": raw_bytes_consumed,
        "encoding": encoding,
        "bom": bom,
        "is_binary": false,
        "size_bytes": file_size_bytes,
        "truncated": truncated,
        "max_bytes": byte_limit,
        "max_bytes_defaulted": max_bytes.is_none()
    });
    if truncated {
        insert_object_field(&mut response, "next_start_byte", json!(end_byte));
    }
    Ok(response)
}

fn read_json_pointer(
    path: &Path,
    file_size_bytes: u64,
    pointer: &str,
    max_bytes: Option<usize>,
) -> Result<Value> {
    let tokens = parse_json_pointer(pointer)?;
    if tokens.is_empty() && file_size_bytes > MAX_IN_MEMORY_TEXT_FILE_BYTES {
        return Err(anyhow::anyhow!(
            "The root JSON value is too large to materialize ({} bytes > {} byte limit); use a narrower json_pointer",
            file_size_bytes,
            MAX_IN_MEMORY_TEXT_FILE_BYTES
        ));
    }

    let mut probe_file = File::open(path)?;
    let mut probe = vec![0u8; BINARY_PROBE_BYTES.min(file_size_bytes as usize)];
    let probe_len = probe_file.read(&mut probe)?;
    probe.truncate(probe_len);
    let bom = has_text_bom(&probe);
    if is_probably_binary(&probe) {
        return Ok(binary_response(file_size_bytes, bom));
    }
    if has_utf16_bom(&probe) {
        return Err(anyhow::anyhow!(
            "json_pointer streaming currently supports UTF-8 JSON files only"
        ));
    }
    let utf8_probe = probe.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&probe);
    std::str::from_utf8(utf8_probe)
        .context("json_pointer streaming requires UTF-8 encoded JSON")?;

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(if has_utf8_bom(&probe) { 3 } else { 0 }))?;
    let reader = BufReader::new(file);
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let value = JsonPointerSeed { tokens: &tokens }
        .deserialize(&mut deserializer)
        .with_context(|| format!("Invalid JSON while reading pointer '{pointer}'"))?;
    deserializer
        .end()
        .with_context(|| format!("Invalid trailing JSON data while reading pointer '{pointer}'"))?;
    let value = value.ok_or_else(|| anyhow::anyhow!("JSON pointer not found: {pointer}"))?;
    let content = serde_json::to_string(&value)?;
    let output_limit = max_bytes.unwrap_or(DEFAULT_JSON_POINTER_OUTPUT_BYTES);
    if content.len() > output_limit {
        return Err(anyhow::anyhow!(
            "JSON pointer value is too large to return ({} UTF-8 bytes > {} byte limit); use a narrower json_pointer or increase max_bytes",
            content.len(),
            output_limit
        ));
    }

    Ok(json!({
        "content": content,
        "json_pointer": pointer,
        "value_type": json_value_type(&value),
        "returned_bytes": content.len(),
        "encoding": "UTF-8",
        "bom": bom,
        "is_binary": false,
        "size_bytes": file_size_bytes,
        "truncated": false,
        "max_bytes": output_limit,
        "max_bytes_defaulted": max_bytes.is_none()
    }))
}

fn parse_json_pointer(pointer: &str) -> Result<Vec<String>> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    if !pointer.starts_with('/') {
        return Err(anyhow::anyhow!(
            "json_pointer must be empty or start with '/' according to RFC 6901"
        ));
    }

    pointer[1..]
        .split('/')
        .map(|token| {
            let mut decoded = String::with_capacity(token.len());
            let mut chars = token.chars();
            while let Some(character) = chars.next() {
                if character != '~' {
                    decoded.push(character);
                    continue;
                }
                match chars.next() {
                    Some('0') => decoded.push('~'),
                    Some('1') => decoded.push('/'),
                    Some(other) => {
                        return Err(anyhow::anyhow!(
                            "invalid RFC 6901 escape '~{other}' in json_pointer"
                        ));
                    }
                    None => {
                        return Err(anyhow::anyhow!(
                            "json_pointer cannot end with an incomplete '~' escape"
                        ));
                    }
                }
            }
            Ok(decoded)
        })
        .collect()
}

fn json_value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

struct JsonPointerSeed<'a> {
    tokens: &'a [String],
}

impl<'de> DeserializeSeed<'de> for JsonPointerSeed<'_> {
    type Value = Option<Value>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.tokens.is_empty() {
            return Value::deserialize(deserializer).map(Some);
        }
        deserializer.deserialize_any(JsonPointerVisitor {
            tokens: self.tokens,
        })
    }
}

struct JsonPointerVisitor<'a> {
    tokens: &'a [String],
}

impl<'de> Visitor<'de> for JsonPointerVisitor<'_> {
    type Value = Option<Value>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object or array containing the requested pointer")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let target = &self.tokens[0];
        let mut matched = None;
        while let Some(key) = map.next_key::<String>()? {
            if &key == target {
                matched = map.next_value_seed(JsonPointerSeed {
                    tokens: &self.tokens[1..],
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(matched)
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let target_index = self.tokens[0].parse::<usize>().ok();
        let mut matched = None;
        let mut index = 0usize;
        loop {
            let has_value = if target_index == Some(index) {
                match sequence.next_element_seed(JsonPointerSeed {
                    tokens: &self.tokens[1..],
                })? {
                    Some(value) => {
                        matched = value;
                        true
                    }
                    None => false,
                }
            } else {
                sequence.next_element::<IgnoredAny>()?.is_some()
            };
            if !has_value {
                break;
            }
            index += 1;
        }
        Ok(matched)
    }

    fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E>(self, _value: u64) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_str<E>(self, _value: &str) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(None)
    }

    fn visit_string<E>(self, _value: String) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(None)
    }
}

fn apply_limits(
    lines: &[&str],
    first_line_number: usize,
    include_line_numbers: bool,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
    append_terminal_newline: bool,
    first_line_start_byte: Option<u64>,
) -> Result<LimitedContent> {
    let line_limit = max_lines.unwrap_or(usize::MAX);
    let byte_limit = max_bytes.unwrap_or(usize::MAX);
    let mut rendered_lines = Vec::new();
    let mut used_bytes = 0usize;
    let mut line_truncated = false;
    let mut next_start_byte = None;

    for (index, line) in lines.iter().enumerate() {
        if rendered_lines.len() >= line_limit {
            break;
        }

        let line_number = first_line_number + index;
        let rendered = render_line(line_number, line, include_line_numbers);
        let rendered_bytes = rendered.len();
        let separator_bytes = if rendered_lines.is_empty() { 0 } else { 1 };

        if used_bytes
            .saturating_add(separator_bytes)
            .saturating_add(rendered_bytes)
            > byte_limit
        {
            if rendered_lines.is_empty() && rendered_bytes > byte_limit {
                let prefix = utf8_prefix(&rendered, byte_limit);
                let rendered_prefix_bytes = if include_line_numbers {
                    format!("{}: ", line_number).len()
                } else {
                    0
                };
                let raw_bytes_consumed = prefix
                    .len()
                    .saturating_sub(rendered_prefix_bytes)
                    .min(line.len());
                if raw_bytes_consumed == 0 {
                    return Err(anyhow::anyhow!(
                        "max_bytes is too small to return any complete UTF-8 content from the selected line"
                    ));
                }
                rendered_lines.push(prefix.to_string());
                line_truncated = true;
                next_start_byte = first_line_start_byte
                    .map(|start_byte| start_byte.saturating_add(raw_bytes_consumed as u64));
            }
            break;
        }

        used_bytes += separator_bytes + rendered_bytes;
        rendered_lines.push(rendered);
    }

    let returned_lines = rendered_lines.len();
    let completed_lines = returned_lines.saturating_sub(usize::from(line_truncated));
    let omitted_lines = lines.len().saturating_sub(completed_lines);
    let truncated = line_truncated || omitted_lines > 0;
    let next_start_line = if truncated && !line_truncated {
        Some(first_line_number + returned_lines)
    } else {
        None
    };
    let end_line = returned_lines
        .checked_sub(1)
        .map(|offset| first_line_number + offset)
        .unwrap_or_else(|| first_line_number.saturating_sub(1));

    let mut content = rendered_lines.join("\n");
    if append_terminal_newline && !truncated && returned_lines > 0 {
        content.push('\n');
    }

    Ok(LimitedContent {
        content,
        returned_lines,
        truncated,
        line_truncated,
        omitted_lines,
        next_start_line,
        next_start_byte,
        end_line,
    })
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn render_line(line_number: usize, line: &str, include_line_numbers: bool) -> String {
    if include_line_numbers {
        format!("{}: {}", line_number, line)
    } else {
        line.to_string()
    }
}

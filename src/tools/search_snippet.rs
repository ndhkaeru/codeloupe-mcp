use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, Sink, SinkMatch};
use std::io;

#[derive(Debug)]
pub(crate) struct SnippetMatch {
    pub line: u64,
    pub text: String,
    pub line_truncated: bool,
    pub match_column: usize,
    pub absolute_byte_offset: u64,
}

pub(crate) struct LossyMatchSink<'matcher> {
    matcher: &'matcher RegexMatcher,
    max_results: usize,
    max_line_length: usize,
    matches: Vec<SnippetMatch>,
}

impl<'matcher> LossyMatchSink<'matcher> {
    pub(crate) fn new(
        matcher: &'matcher RegexMatcher,
        max_results: usize,
        max_line_length: usize,
    ) -> Self {
        Self {
            matcher,
            max_results,
            max_line_length,
            matches: Vec::new(),
        }
    }

    pub(crate) fn into_matches(self) -> Vec<SnippetMatch> {
        self.matches
    }
}

impl Sink for LossyMatchSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> io::Result<bool> {
        let line = mat
            .line_number()
            .ok_or_else(|| io::Error::other("line numbers not enabled"))?;
        let Some(rendered) = render_match_line(self.matcher, mat.bytes(), self.max_line_length)?
        else {
            return Ok(true);
        };

        self.matches.push(SnippetMatch {
            line,
            text: rendered.text,
            line_truncated: rendered.line_truncated,
            match_column: rendered.match_column,
            absolute_byte_offset: mat.absolute_byte_offset() + rendered.match_byte_offset as u64,
        });
        Ok(self.matches.len() < self.max_results)
    }
}

pub(crate) struct RenderedMatch {
    pub text: String,
    pub line_truncated: bool,
    pub match_column: usize,
    match_byte_offset: usize,
}

pub(crate) fn render_match_line(
    matcher: &RegexMatcher,
    bytes: &[u8],
    max_chars: usize,
) -> io::Result<Option<RenderedMatch>> {
    let bytes = trim_line_ending(bytes);
    let Some(found) = matcher
        .find(bytes)
        .map_err(|error| io::Error::other(error.to_string()))?
    else {
        return Ok(None);
    };

    Ok(Some(render_match_range(
        bytes,
        found.start(),
        found.end(),
        max_chars,
    )))
}

pub(crate) fn render_match_range(
    bytes: &[u8],
    match_start_byte: usize,
    match_end_byte: usize,
    max_chars: usize,
) -> RenderedMatch {
    let bytes = trim_line_ending(bytes);
    let match_start_byte = match_start_byte.min(bytes.len());
    let match_end_byte = match_end_byte.max(match_start_byte).min(bytes.len());

    let text = String::from_utf8_lossy(bytes);
    let total_chars = text.chars().count();
    let match_start = String::from_utf8_lossy(&bytes[..match_start_byte])
        .chars()
        .count();
    let match_end = String::from_utf8_lossy(&bytes[..match_end_byte])
        .chars()
        .count();
    let max_chars = max_chars.max(1);

    if total_chars <= max_chars {
        return RenderedMatch {
            text: text.into_owned(),
            line_truncated: false,
            match_column: match_start + 1,
            match_byte_offset: match_start_byte,
        };
    }

    let match_len = match_end.saturating_sub(match_start).min(max_chars);
    let surrounding_chars = max_chars.saturating_sub(match_len);
    let max_start = total_chars.saturating_sub(max_chars);
    let mut window_start = match_start
        .saturating_sub(surrounding_chars / 2)
        .min(max_start);
    if match_end > window_start + max_chars {
        window_start = match_end.saturating_sub(max_chars).min(max_start);
    }
    let window_end = (window_start + max_chars).min(total_chars);

    let mut rendered = String::new();
    if window_start > 0 {
        rendered.push_str("...");
    }
    rendered.extend(
        text.chars()
            .skip(window_start)
            .take(window_end - window_start),
    );
    if window_end < total_chars {
        rendered.push_str("...");
    }

    RenderedMatch {
        text: rendered,
        line_truncated: true,
        match_column: match_start + 1,
        match_byte_offset: match_start_byte,
    }
}

pub(crate) fn render_line_start(bytes: &[u8], max_chars: usize) -> (String, bool) {
    let text = String::from_utf8_lossy(trim_line_ending(bytes));
    let max_chars = max_chars.max(1);
    if text.chars().count() <= max_chars {
        return (text.into_owned(), false);
    }

    let mut rendered = text.chars().take(max_chars).collect::<String>();
    rendered.push_str("...");
    (rendered, true)
}

fn trim_line_ending(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.last(), Some(b'\r' | b'\n')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

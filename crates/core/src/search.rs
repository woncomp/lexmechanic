use crate::model::{Cancellation, Role, TimeMode};
use serde::{Deserialize, Serialize};
use std::ops::Range;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub fn normalize(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    text.nfkc().flat_map(char::to_lowercase).collect()
}

pub fn first_range(text: &str, query: &str) -> Option<Range<usize>> {
    let query = normalize(query);
    if query.is_empty() {
        return None;
    }
    let folded = normalize(text);
    let start = folded.find(&query)?;
    let end = start + query.len();
    let mut position = 0;
    let mut original_start = None;
    for (offset, grapheme) in text.grapheme_indices(true) {
        let length = normalize(grapheme).len();
        if original_start.is_none() && position + length > start {
            original_start = Some(offset);
        }
        position += length;
        if position >= end {
            return Some(original_start?..offset + grapheme.len());
        }
    }
    None
}

/// Returns original UTF-8 ranges, even when normalization changes byte lengths.
pub fn find_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    let query = normalize(query);
    if query.is_empty() {
        return Vec::new();
    }
    let mut folded = String::new();
    let mut mapping = Vec::new();
    for (offset, grapheme) in text.grapheme_indices(true) {
        let value = normalize(grapheme);
        mapping.extend(std::iter::repeat_n(
            offset..offset + grapheme.len(),
            value.len(),
        ));
        folded.push_str(&value);
    }
    folded
        .match_indices(&query)
        .map(|(start, _)| mapping[start].start..mapping[start + query.len() - 1].end)
        .collect()
}

#[derive(Clone, Debug, Default)]
pub struct SearchQuery {
    pub text: String,
    pub workspace: Option<String>,
    pub role: Option<Role>,
    pub session: Option<String>,
    pub mode: TimeMode,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub session: String,
    pub message_id: i64,
    pub ordinal: usize,
    pub role: Role,
    pub snippet: String,
    pub offset: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub scanned: usize,
    pub next: Option<SearchCursor>,
}

#[derive(Clone, Debug, Default)]
pub struct SearchCursor {
    pub offset: usize,
    pub occurrence: usize,
    pub after: Option<SearchPosition>,
}

#[derive(Clone, Debug)]
pub struct SearchPosition {
    pub session: String,
    pub ordinal: i64,
    pub time: Option<i64>,
}

pub(crate) fn fts_phrase(query: &str) -> Option<String> {
    let value = normalize(query);
    (value.chars().count() >= 3 && value.len() < 4096)
        .then(|| format!("\"{}\"", value.replace('"', "\"\"")))
}

pub(crate) fn snippet(content: &str, offset: usize) -> String {
    let mut start = offset.saturating_sub(80);
    while !content.is_char_boundary(start) {
        start += 1;
    }
    let end = content[start..]
        .char_indices()
        .nth(220)
        .map_or(content.len(), |(end, _)| start + end);
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        content[start..end].replace(['\n', '\r'], " "),
        if end < content.len() { "…" } else { "" }
    )
}

pub fn check_cancel(cancel: &Cancellation) -> anyhow::Result<()> {
    cancel.check()
}

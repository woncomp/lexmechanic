//! Line-oriented transcript engine shared by the Claude and Codex readers.
//!
//! Plain `.jsonl` files are streamed and resumed by byte cursor (`ReadMode::Append`). Compressed
//! `.jsonl.zst` files are decoded whole and replayed as a snapshot (`ReadMode::Snapshot`); their
//! message offsets address the decompressed stream while the batch cursor stays in file bytes.

use crate::model::*;
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

const PROBE_BYTES: usize = 512 * 1024;
const BLOCK_BYTES: u64 = 64 * 1024;
// Claude logs interleave very large request blobs, so the tail needs more room than the head.
const TAIL_BYTES: usize = 8 * 1024 * 1024;
const DECODED_LIMIT: u64 = 1024 * 1024 * 1024;

/// Session-level facts that records carry besides messages.
#[derive(Default)]
pub(super) struct Header {
    pub created: Option<String>,
    pub workspace: Option<String>,
}

pub(super) trait Format {
    /// The visible message carried by one record, if any.
    fn message(record: &Value, offset: u64, end: u64) -> Option<Message>;
    /// The unfiltered text behind a visible message, for the Raw view.
    fn raw(record: &Value) -> Option<String>;
    /// Fills the fields of `header` that this record knows and are still empty.
    fn header(record: &Value, header: &mut Header);
}

pub(super) fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "zst")
}

enum Line {
    Blank,
    Record(Value),
    Partial,
    Invalid,
}

fn parse_line(line: &[u8]) -> Line {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Line::Blank;
    }
    match serde_json::from_slice(line) {
        Ok(record) => Line::Record(record),
        Err(error) if !line.ends_with(b"\n") && error.is_eof() => Line::Partial,
        Err(_) => Line::Invalid,
    }
}

fn decode(path: &Path, size: u64) -> Result<Vec<u8>> {
    let file =
        File::open(path).with_context(|| format!("Cannot read transcript {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.len() >= size,
        "Transcript was truncated during this operation; refresh to retry"
    );
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(file.take(size))
        .map_err(|error| anyhow::anyhow!("Cannot decompress {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    decoder
        .by_ref()
        .take(DECODED_LIMIT + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Cannot decompress {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= DECODED_LIMIT,
        "Compressed transcript exceeds the 1 GiB decoded budget"
    );
    Ok(bytes)
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(100)
        .collect()
}

pub(super) fn summarize<F: Format>(
    candidate: &Candidate,
    name: Option<String>,
    cancel: &Cancellation,
) -> Result<Session> {
    let mut summary = Session {
        candidate: candidate.clone(),
        name: name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_default(),
        workspace: None,
        created: MessageTime::Missing,
        latest: MessageTime::Pending,
        first_assistant: MessageTime::Pending,
        indexed: false,
        available: true,
        content_error: None,
    };
    let compressed = is_compressed(&candidate.events_path);
    let mut header = Header::default();
    let mut latest = MessageTime::Missing;
    let mut reader: Box<dyn BufRead> = if compressed {
        Box::new(std::io::Cursor::new(decode(
            &candidate.events_path,
            candidate.size,
        )?))
    } else {
        Box::new(BufReader::new(
            File::open(&candidate.events_path)?.take(candidate.size),
        ))
    };
    let mut line = Vec::new();
    let mut offset = 0_u64;
    let mut exhausted = false;
    loop {
        cancel.check()?;
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            exhausted = true;
            break;
        }
        if let Line::Record(record) = parse_line(&line) {
            F::header(&record, &mut header);
            if let Some(message) = F::message(&record, offset, offset + count as u64) {
                if summary.name.is_empty() && message.role == Role::User {
                    summary.name = first_line(&message.content);
                }
                if message.time.value().is_some() {
                    latest = message.time.clone();
                }
                if message.role == Role::Assistant
                    && matches!(summary.first_assistant, MessageTime::Pending)
                {
                    summary.first_assistant = message.time;
                    // A plain file only needs its head; the tail is found separately.
                    if !compressed {
                        break;
                    }
                }
            }
        }
        offset += count as u64;
        if !compressed && offset >= PROBE_BYTES as u64 {
            break;
        }
    }
    if exhausted && matches!(summary.first_assistant, MessageTime::Pending) {
        summary.first_assistant = MessageTime::Missing;
    }
    summary.created = MessageTime::parse(header.created.as_deref());
    summary.workspace = header.workspace.filter(|path| !path.is_empty());
    if summary.name.is_empty() {
        summary.name = candidate.key.native.chars().take(8).collect();
    }
    summary.latest = if compressed {
        latest
    } else {
        match tail_time::<F>(&candidate.events_path, candidate.size, cancel)? {
            // No message near the end: fall back to the creation time rather than stay unknown.
            MessageTime::Pending => MessageTime::Missing,
            time => time,
        }
    };
    Ok(summary)
}

pub(super) fn read_batch<F: Format>(
    candidate: &Candidate,
    cursor: u64,
    budget: usize,
    cancel: &Cancellation,
) -> Result<ReadBatch> {
    let mut batch = ReadBatch {
        messages: Vec::new(),
        cursor,
        eof: false,
        incomplete_tail: false,
        diagnostics: Vec::new(),
    };
    if is_compressed(&candidate.events_path) {
        anyhow::ensure!(
            cursor == 0 || cursor == candidate.size,
            "Compressed transcript changed; replay must restart from its snapshot"
        );
        batch.cursor = candidate.size;
        batch.eof = true;
        if cursor != 0 {
            return Ok(batch);
        }
        let bytes = decode(&candidate.events_path, candidate.size)?;
        let mut offset = 0_u64;
        for line in bytes.split_inclusive(|&byte| byte == b'\n') {
            cancel.check()?;
            let end = offset + line.len() as u64;
            match parse_line(line) {
                Line::Record(record) => batch.messages.extend(F::message(&record, offset, end)),
                Line::Partial => batch.incomplete_tail = true,
                Line::Invalid => batch.diagnostics.push(invalid(candidate, offset)),
                Line::Blank => {}
            }
            offset = end;
        }
        return Ok(batch);
    }
    let mut file = File::open(&candidate.events_path)?;
    anyhow::ensure!(
        file.metadata()?.len() >= candidate.size && cursor <= candidate.size,
        "Transcript was truncated during this operation; refresh to retry"
    );
    file.seek(SeekFrom::Start(cursor))?;
    let mut reader = BufReader::new(file.take(candidate.size - cursor));
    let mut line = Vec::new();
    let mut bytes = 0;
    while bytes < budget {
        cancel.check()?;
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            batch.eof = true;
            break;
        }
        let offset = batch.cursor;
        match parse_line(&line) {
            Line::Record(record) => {
                batch
                    .messages
                    .extend(F::message(&record, offset, offset + count as u64));
            }
            Line::Partial => {
                batch.incomplete_tail = true;
                batch.eof = true;
                break;
            }
            Line::Invalid => batch.diagnostics.push(invalid(candidate, offset)),
            Line::Blank => {}
        }
        batch.cursor += count as u64;
        bytes += count;
    }
    Ok(batch)
}

fn invalid(candidate: &Candidate, offset: u64) -> Diagnostic {
    Diagnostic {
        path: candidate.events_path.clone(),
        offset: Some(offset),
        description: "Invalid event JSON; this record was not imported".into(),
    }
}

pub(super) fn original_message<F: Format>(
    candidate: &Candidate,
    cached: &Message,
) -> Result<String> {
    let length = usize::try_from(cached.end.saturating_sub(cached.offset))?;
    anyhow::ensure!(
        length <= 128 * 1024 * 1024,
        "Raw event exceeds the 128 MiB view budget; inspect the source file instead"
    );
    let bytes = if is_compressed(&candidate.events_path) {
        let decoded = decode(&candidate.events_path, candidate.size)?;
        let start = usize::try_from(cached.offset)?;
        decoded
            .get(start..start + length)
            .context("Source event is no longer available")?
            .to_vec()
    } else {
        let mut file = File::open(&candidate.events_path)?;
        file.seek(SeekFrom::Start(cached.offset))?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        bytes
    };
    let record: Value =
        serde_json::from_slice(&bytes).context("Source event is no longer available")?;
    let current = F::message(&record, cached.offset, cached.end)
        .context("Source message changed; refresh the conversation")?;
    anyhow::ensure!(
        current.id == cached.id && current.content == cached.content,
        "Source message changed; refresh the conversation"
    );
    F::raw(&record).context("Source message has no text content")
}

/// Time of the last timestamped message, found by scanning backwards from `limit`.
fn tail_time<F: Format>(path: &Path, limit: u64, cancel: &Cancellation) -> Result<MessageTime> {
    let mut file = File::open(path)?;
    anyhow::ensure!(
        file.metadata()?.len() >= limit,
        "Transcript was truncated during summary reading"
    );
    let mut position = limit;
    let mut remainder = Vec::new();
    let mut bytes = 0;
    let last = |line: &[u8]| match parse_line(line) {
        Line::Record(record) => {
            F::message(&record, 0, 0).filter(|message| message.time.value().is_some())
        }
        _ => None,
    };
    while position > 0 && bytes < TAIL_BYTES {
        cancel.check()?;
        let length = position.min(BLOCK_BYTES);
        position -= length;
        file.seek(SeekFrom::Start(position))?;
        let mut block = vec![0; length as usize];
        file.read_exact(&mut block)?;
        bytes += block.len();
        block.extend_from_slice(&remainder);
        let mut end = block.len();
        for start in (0..block.len()).rev().filter(|&i| block[i] == b'\n') {
            if let Some(message) = last(&block[start + 1..end]) {
                return Ok(message.time);
            }
            end = start;
        }
        remainder = block[..end].to_vec();
    }
    if position == 0 {
        return Ok(last(&remainder)
            .map(|message| message.time)
            .unwrap_or(MessageTime::Missing));
    }
    Ok(MessageTime::Pending)
}

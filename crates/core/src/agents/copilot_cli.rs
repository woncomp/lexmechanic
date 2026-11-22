use super::AgentAdapter;
use crate::model::*;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const PROBE_BYTES: usize = 512 * 1024;
const BLOCK_BYTES: u64 = 64 * 1024;

pub fn default_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("COPILOT_HOME").filter(|value| !value.is_empty()) {
        return Ok(home.into());
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    home.map(|home| PathBuf::from(home).join(".copilot"))
        .context("Cannot locate the user home directory; choose a Copilot data directory")
}

#[derive(Clone)]
pub struct CopilotCli {
    source: Source,
}

#[derive(Default, Deserialize)]
pub(super) struct WorkspaceMetadata {
    client_name: Option<String>,
    last_client_name: Option<String>,
    name: Option<String>,
    pub(super) cwd: Option<String>,
}

impl WorkspaceMetadata {
    pub(super) fn read(directory: &Path) -> Result<Self> {
        let path = directory.join("workspace.yaml");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot read {}", path.display()));
            }
        };
        serde_yaml::from_str(&text)
            .with_context(|| format!("Invalid workspace metadata {}", path.display()))
    }

    pub(super) fn continued_in_cli(&self) -> bool {
        self.last_client_name.as_deref() == Some("github/cli")
    }

    fn originated_in_cli(&self) -> bool {
        self.client_name.as_deref() == Some("github/cli")
    }
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "agentId")]
    agent_id: Option<Value>,
    id: Option<String>,
    timestamp: Option<String>,
    #[serde(default)]
    data: Value,
}

impl CopilotCli {
    pub fn for_candidate(candidate: &Candidate) -> Result<Self> {
        let root = candidate
            .events_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .context("Invalid Copilot session location")?;
        let adapter = Self::new(root)?;
        anyhow::ensure!(
            adapter.source.id == candidate.key.source,
            "Session location no longer matches its registered source"
        );
        Ok(adapter)
    }

    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        Ok(Self {
            source: super::source(&root, "copilot-cli")?,
        })
    }
}

fn visible_content(raw: &str) -> String {
    // Only strip complete, known wrappers. Preserve user text following a wrapper.
    let mut content = raw.trim_start_matches('\u{feff}').to_owned();
    loop {
        let trimmed = content.trim_start();
        let tag = ["system_reminder", "system-reminder"]
            .into_iter()
            .find(|tag| trimmed.starts_with(&format!("<{tag}>")));
        let Some(tag) = tag else { break };
        let close = format!("</{tag}>");
        let Some(end) = trimmed.find(&close) else {
            break;
        };
        content = trimmed[end + close.len()..].trim_start().to_owned();
    }
    content
}

fn top_level(event: &Event) -> bool {
    match event.agent_id.as_ref() {
        None | Some(Value::Null) => true,
        Some(Value::String(id)) => id.is_empty(),
        _ => false,
    }
}

fn message(event: &Event, offset: u64, end: u64) -> Option<Message> {
    if !top_level(event) {
        return None;
    }
    let role = match event.kind.as_str() {
        "user.message" => Role::User,
        "assistant.message" => Role::Assistant,
        _ => return None,
    };
    if role == Role::User && event.data.get("source").and_then(Value::as_str) == Some("memory") {
        return None;
    }
    let content = visible_content(
        event
            .data
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let attachments = event
        .data
        .get("attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if content.trim().is_empty() && attachments.is_empty() {
        return None;
    }
    Some(Message {
        id: event.id.clone().unwrap_or_else(|| format!("byte-{offset}")),
        role,
        time: MessageTime::parse(event.timestamp.as_deref()),
        content,
        attachments,
        offset,
        end,
    })
}

pub fn original_message(path: &Path, cached: &Message) -> Result<String> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(cached.offset))?;
    let length = usize::try_from(cached.end.saturating_sub(cached.offset))?;
    anyhow::ensure!(
        length <= 128 * 1024 * 1024,
        "Raw event exceeds the 128 MiB view budget; inspect the source file instead"
    );
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    let event: Event =
        serde_json::from_slice(&bytes).context("Source event is no longer available")?;
    let current = message(&event, cached.offset, cached.end)
        .context("Source message changed; refresh the conversation")?;
    anyhow::ensure!(
        current.id == cached.id && current.content == cached.content,
        "Source message changed; refresh the conversation"
    );
    event
        .data
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("Source message has no text content")
}

impl AgentAdapter for CopilotCli {
    fn original_message(&self, candidate: &Candidate, cached: &Message) -> Result<String> {
        original_message(&candidate.events_path, cached)
    }

    fn source(&self) -> &Source {
        &self.source
    }

    fn discover(&self, cancel: &Cancellation) -> Result<Discovery> {
        let root = self.source.root.join("session-state");
        let mut candidates = Vec::new();
        let mut diagnostics = Vec::new();
        let mut continuations = Vec::new();
        let app_ids = if self.source.root.join("data.db").is_file() {
            match super::copilot_app::registered_ids(&self.source.root, cancel) {
                Ok(ids) => ids,
                Err(error) => {
                    diagnostics.push(Diagnostic {
                        path: self.source.root.join("data.db"),
                        offset: None,
                        description: format!("Cannot classify App sessions: {error:#}"),
                    });
                    HashSet::new()
                }
            }
        } else {
            HashSet::new()
        };
        for entry in
            fs::read_dir(&root).with_context(|| format!("Cannot scan {}", root.display()))?
        {
            cancel.check()?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    diagnostics.push(Diagnostic {
                        path: root.clone(),
                        offset: None,
                        description: error.to_string(),
                    });
                    continue;
                }
            };
            let candidate = (|| -> Result<Option<Candidate>> {
                if !entry.file_type()?.is_dir() {
                    return Ok(None);
                }
                let native = entry.file_name().to_string_lossy().to_string();
                if uuid::Uuid::parse_str(&native).is_err() {
                    return Ok(None);
                }
                let events_path = entry.path().join("events.jsonl");
                match events_path.metadata() {
                    Ok(metadata) if metadata.is_file() => {}
                    Ok(_) => return Ok(None),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error).context("Cannot inspect session log"),
                }
                let candidate =
                    super::candidate(&self.source, native, &events_path, ReadMode::Append)?;
                let metadata = WorkspaceMetadata::read(&entry.path())?;
                if metadata.continued_in_cli() {
                    continuations.push(candidate.key.clone());
                } else if !metadata.originated_in_cli()
                    || app_ids.contains(&candidate.key.native.to_lowercase())
                {
                    return Ok(None);
                }
                Ok(Some(candidate))
            })();
            match candidate {
                Ok(Some(candidate)) => candidates.push(candidate),
                Ok(None) => {}
                Err(error) => diagnostics.push(Diagnostic {
                    path: entry.path(),
                    offset: None,
                    description: format!("{error:#}"),
                }),
            }
        }
        candidates.sort_unstable_by(|a, b| {
            b.modified_ns
                .cmp(&a.modified_ns)
                .then_with(|| a.key.cmp(&b.key))
        });
        let reclassified = if continuations.is_empty() {
            Vec::new()
        } else {
            let app = super::source(&self.source.root, "copilot-app")?;
            continuations
                .into_iter()
                .map(|current| ReclassifiedSession {
                    previous: SessionKey {
                        agent: app.agent.clone(),
                        source: app.id.clone(),
                        native: current.native.to_lowercase(),
                    },
                    current,
                })
                .collect()
        };
        Ok(Discovery {
            candidates,
            diagnostics,
            reclassified,
        })
    }

    fn summarize(&self, candidate: &Candidate, cancel: &Cancellation) -> Result<Session> {
        let directory = candidate
            .events_path
            .parent()
            .context("Invalid Copilot session location")?;
        let metadata = WorkspaceMetadata::read(directory)?;
        anyhow::ensure!(
            metadata.continued_in_cli() || metadata.originated_in_cli(),
            "Not a Copilot CLI session (neither client_name nor last_client_name is github/cli)"
        );
        summarize_log(candidate, metadata.name, cancel)
    }

    fn read_batch(
        &self,
        candidate: &Candidate,
        cursor: u64,
        budget: usize,
        cancel: &Cancellation,
    ) -> Result<ReadBatch> {
        read_log(candidate, cursor, budget, cancel)
    }
}

pub(super) fn summarize_log(
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
    let mut reader = BufReader::new(File::open(&candidate.events_path)?.take(candidate.size));
    let mut line = Vec::new();
    let mut offset = 0;
    loop {
        cancel.check()?;
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            summary.first_assistant = MessageTime::Missing;
            break;
        }
        if let Ok(event) = serde_json::from_slice::<Event>(&line) {
            if event.kind == "session.start" {
                summary.created = MessageTime::parse(
                    event
                        .data
                        .get("startTime")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .or(event.timestamp.as_deref()),
                );
                summary.workspace = event
                    .data
                    .pointer("/context/cwd")
                    .and_then(Value::as_str)
                    .filter(|path| !path.is_empty())
                    .or_else(|| {
                        event
                            .data
                            .get("cwd")
                            .and_then(Value::as_str)
                            .filter(|path| !path.is_empty())
                    })
                    .map(str::to_owned);
            }
            if let Some(message) = message(&event, offset, offset + count as u64) {
                if summary.name.is_empty() && message.role == Role::User {
                    summary.name = message
                        .content
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .take(100)
                        .collect();
                }
                if message.role == Role::Assistant {
                    summary.first_assistant = message.time;
                    break;
                }
            }
        }
        offset += count as u64;
        if offset >= PROBE_BYTES as u64 {
            break;
        }
    }
    if summary.name.is_empty() {
        summary.name = candidate.key.native.chars().take(8).collect();
    }
    summary.latest = tail_time(&candidate.events_path, candidate.size, PROBE_BYTES, cancel)?;
    Ok(summary)
}

pub(super) fn read_log(
    candidate: &Candidate,
    cursor: u64,
    budget: usize,
    cancel: &Cancellation,
) -> Result<ReadBatch> {
    let mut file = File::open(&candidate.events_path)?;
    anyhow::ensure!(
        file.metadata()?.len() >= candidate.size && cursor <= candidate.size,
        "Transcript was truncated during this operation; refresh to retry"
    );
    file.seek(SeekFrom::Start(cursor))?;
    let mut reader = BufReader::new(file.take(candidate.size - cursor));
    let mut batch = ReadBatch {
        messages: Vec::new(),
        cursor,
        eof: false,
        incomplete_tail: false,
        diagnostics: Vec::new(),
    };
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
        match serde_json::from_slice::<Event>(&line) {
            Ok(event) => {
                if matches!(event.kind.as_str(), "user.message" | "assistant.message")
                    && top_level(&event)
                    && event
                        .data
                        .get("content")
                        .is_some_and(|content| !content.is_null() && !content.is_string())
                {
                    batch.diagnostics.push(Diagnostic {
                        path: candidate.events_path.clone(),
                        offset: Some(offset),
                        description:
                            "Unsupported non-text message payload; inspect the original log".into(),
                    });
                }
                if let Some(message) = message(&event, offset, offset + count as u64) {
                    batch.messages.push(message);
                }
            }
            Err(error) if !line.ends_with(b"\n") && error.is_eof() => {
                batch.incomplete_tail = true;
                batch.eof = true;
                break;
            }
            Err(_) if line.iter().all(u8::is_ascii_whitespace) => {}
            Err(_) => batch.diagnostics.push(Diagnostic {
                path: candidate.events_path.clone(),
                offset: Some(offset),
                description: "Invalid event JSON; this record was not imported".into(),
            }),
        }
        batch.cursor += count as u64;
        bytes += count;
    }
    Ok(batch)
}

fn tail_time(path: &Path, limit: u64, budget: usize, cancel: &Cancellation) -> Result<MessageTime> {
    let mut file = File::open(path)?;
    anyhow::ensure!(
        file.metadata()?.len() >= limit,
        "Transcript was truncated during summary reading"
    );
    let mut position = limit;
    let mut remainder = Vec::new();
    let mut bytes = 0;
    while position > 0 && bytes < budget {
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
            let line = &block[start + 1..end];
            if let Ok(event) = serde_json::from_slice::<Event>(line)
                && let Some(message) = message(&event, 0, 0)
                && message.time.value().is_some()
            {
                return Ok(message.time);
            }
            end = start;
        }
        remainder = block[..end].to_vec();
    }
    if position == 0 {
        if let Ok(event) = serde_json::from_slice::<Event>(&remainder)
            && let Some(message) = message(&event, 0, 0)
        {
            return Ok(message.time);
        }
        Ok(MessageTime::Missing)
    } else {
        Ok(MessageTime::Pending)
    }
}

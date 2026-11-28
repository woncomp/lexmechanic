use super::{AgentAdapter, copilot_app::parse_database_time, open_read_only};
use crate::model::*;
use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const SESSION_INDEX_KEY: &str = "chat.ChatSessionStore.index";

fn user_root_of(product: &str) -> Result<PathBuf> {
    let roaming = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|home| PathBuf::from(home).join("AppData").join("Roaming"))
        })
        .context("Cannot locate VS Code user data; select its User directory")?;
    Ok(roaming.join(product).join("User"))
}

pub fn default_user_root() -> Result<PathBuf> {
    user_root_of("Code")
}

/// Stable and Insiders keep separate user data; either may be absent.
pub fn default_user_roots() -> Result<Vec<PathBuf>> {
    Ok(vec![
        user_root_of("Code")?,
        user_root_of("Code - Insiders")?,
    ])
}

pub fn session_store(root: &Path) -> PathBuf {
    root.join("globalStorage")
        .join("github.copilot-chat")
        .join("session-store.db")
}

#[derive(Clone, Default)]
struct Metadata {
    title: Option<String>,
    workspace: Option<String>,
}

struct Transcript {
    state: Value,
    cursor: u64,
    incomplete: bool,
}

pub struct VsCode {
    source: Source,
    records: Mutex<HashMap<String, Metadata>>,
    // At most one replayed session is retained, never the entire history.
    replay: Mutex<Option<(Candidate, Arc<Transcript>)>>,
}

impl VsCode {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            source: super::source(&root.into(), "vscode")?,
            records: Mutex::new(HashMap::new()),
            replay: Mutex::new(None),
        })
    }

    pub fn for_candidate(candidate: &Candidate) -> Result<Self> {
        let root = candidate
            .events_path
            .ancestors()
            .nth(4)
            .context("Invalid VS Code transcript location")?;
        let adapter = Self::new(root)?;
        anyhow::ensure!(
            adapter.source.id == candidate.key.source,
            "VS Code source location changed"
        );
        Ok(adapter)
    }

    fn transcript(&self, candidate: &Candidate, cancel: &Cancellation) -> Result<Arc<Transcript>> {
        cancel.check()?;
        if let Some((cached, transcript)) = self
            .replay
            .lock()
            .map_err(|_| anyhow::anyhow!("VS Code replay cache failed"))?
            .as_ref()
            && cached == candidate
        {
            return Ok(transcript.clone());
        }
        let transcript = Arc::new(replay(&candidate.events_path, candidate.size, cancel)?);
        *self
            .replay
            .lock()
            .map_err(|_| anyhow::anyhow!("VS Code replay cache failed"))? =
            Some((candidate.clone(), transcript.clone()));
        Ok(transcript)
    }
}

impl AgentAdapter for VsCode {
    fn source(&self) -> &Source {
        &self.source
    }

    fn discover(&self, cancel: &Cancellation) -> Result<Discovery> {
        let store_path = session_store(&self.source.root);
        let mut records: HashMap<String, Metadata> = HashMap::new();
        let store = open_read_only(&store_path)?;
        let mut statement = store.prepare(
            "SELECT id,cwd FROM sessions WHERE host_type='vscode' AND agent_name='GitHub Copilot Chat'")?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })? {
            cancel.check()?;
            let (id, cwd) = row?;
            if uuid::Uuid::parse_str(&id).is_ok() {
                records.insert(
                    id.to_lowercase(),
                    Metadata {
                        workspace: cwd,
                        title: None,
                    },
                );
            }
        }
        let storage = self.source.root.join("workspaceStorage");
        let mut candidates: HashMap<String, Candidate> = HashMap::new();
        let mut diagnostics = Vec::new();
        for directory in fs::read_dir(&storage).with_context(|| {
            format!(
                "Cannot enumerate VS Code workspace storage {}",
                storage.display()
            )
        })? {
            cancel.check()?;
            let directory = match directory {
                Ok(entry) => entry.path(),
                Err(error) => {
                    diagnostics.push(Diagnostic {
                        path: storage.clone(),
                        offset: None,
                        description: error.to_string(),
                    });
                    continue;
                }
            };
            if !directory.is_dir() {
                continue;
            }
            let database = directory.join("state.vscdb");
            let titles = match read_titles(&database) {
                Ok(titles) => titles,
                Err(error) => {
                    diagnostics.push(Diagnostic {
                        path: database,
                        offset: None,
                        description: format!("{error:#}"),
                    });
                    HashMap::new()
                }
            };
            let chats = directory.join("chatSessions");
            if !chats.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&chats)? {
                cancel.check()?;
                let result = (|| -> Result<Option<Candidate>> {
                    let path = entry?.path();
                    if path
                        .extension()
                        .is_none_or(|extension| extension != "jsonl")
                    {
                        return Ok(None);
                    }
                    let Some(native) = path
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .map(str::to_lowercase)
                        .filter(|id| records.contains_key(id))
                    else {
                        return Ok(None);
                    };
                    Ok(Some(super::candidate(
                        &self.source,
                        native,
                        &path,
                        ReadMode::Snapshot,
                    )?))
                })();
                match result {
                    Ok(Some(candidate)) => {
                        let native = candidate.key.native.clone();
                        let replace = candidates.get(&native).is_none_or(|existing| {
                            (candidate.modified_ns, &candidate.events_path)
                                > (existing.modified_ns, &existing.events_path)
                        });
                        if let Some(existing) = candidates.get(&native) {
                            diagnostics.push(Diagnostic { path: candidate.events_path.clone(), offset: None,
                                description: format!("Duplicate VS Code transcript; using newest copy (also found {})",
                                    existing.events_path.display()) });
                        }
                        if replace {
                            if let Some(record) = records.get_mut(&native) {
                                record.title = titles.get(&native).cloned();
                            }
                            candidates.insert(native, candidate);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => diagnostics.push(Diagnostic {
                        path: chats.clone(),
                        offset: None,
                        description: format!("{error:#}"),
                    }),
                }
            }
        }
        *self
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!("VS Code metadata reader failed"))? = records;
        let mut candidates: Vec<_> = candidates.into_values().collect();
        candidates
            .sort_unstable_by(|a, b| b.modified_ns.cmp(&a.modified_ns).then(a.key.cmp(&b.key)));
        Ok(Discovery {
            candidates,
            diagnostics,
            reclassified: Vec::new(),
        })
    }

    fn summarize(&self, candidate: &Candidate, cancel: &Cancellation) -> Result<Session> {
        let transcript = self.transcript(candidate, cancel)?;
        let record = self
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!("VS Code metadata reader failed"))?
            .get(&candidate.key.native)
            .cloned()
            .unwrap_or_default();
        let messages = messages(&transcript.state, cancel)?;
        let name = record
            .title
            .or_else(|| {
                transcript
                    .state
                    .get("customTitle")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .map(str::to_owned)
            })
            .or_else(|| {
                messages
                    .iter()
                    .find(|message| message.role == Role::User)
                    .map(|message| {
                        message
                            .content
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .chars()
                            .take(100)
                            .collect()
                    })
            })
            .unwrap_or_else(|| candidate.key.native.chars().take(8).collect());
        Ok(Session {
            candidate: candidate.clone(),
            name,
            workspace: record.workspace,
            created: milliseconds(transcript.state.get("creationDate")),
            latest: messages
                .iter()
                .rev()
                .find(|message| message.time.value().is_some())
                .map(|message| message.time.clone())
                .unwrap_or(MessageTime::Missing),
            first_assistant: messages
                .iter()
                .find(|message| message.role == Role::Assistant)
                .map(|message| message.time.clone())
                .unwrap_or(MessageTime::Missing),
            indexed: false,
            available: true,
            content_error: None,
        })
    }

    fn read_batch(
        &self,
        candidate: &Candidate,
        cursor: u64,
        _: usize,
        cancel: &Cancellation,
    ) -> Result<ReadBatch> {
        let transcript = self.transcript(candidate, cancel)?;
        anyhow::ensure!(
            cursor == 0 || cursor == transcript.cursor,
            "VS Code transcript changed; replay must restart from its snapshot"
        );
        Ok(ReadBatch {
            messages: if cursor == 0 {
                messages(&transcript.state, cancel)?
            } else {
                Vec::new()
            },
            cursor: transcript.cursor,
            eof: true,
            incomplete_tail: transcript.incomplete,
            diagnostics: Vec::new(),
        })
    }

    fn original_message(&self, candidate: &Candidate, cached: &Message) -> Result<String> {
        let transcript = replay(
            &candidate.events_path,
            candidate.size,
            &Cancellation::default(),
        )?;
        let current = messages(&transcript.state, &Cancellation::default())?
            .into_iter()
            .find(|message| message.id == cached.id)
            .context("VS Code message is no longer present")?;
        anyhow::ensure!(
            current.content == cached.content,
            "VS Code message changed; refresh the conversation"
        );
        Ok(current.content)
    }
}

fn read_titles(database: &Path) -> Result<HashMap<String, String>> {
    if !database.is_file() {
        return Ok(HashMap::new());
    }
    let connection = open_read_only(database)?;
    let text = connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key=?1",
            [SESSION_INDEX_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(text) = text else {
        return Ok(HashMap::new());
    };
    let value: Value = serde_json::from_str(&text).context("Invalid VS Code title index JSON")?;
    let entries = value
        .get("entries")
        .and_then(Value::as_object)
        .context("VS Code title index has no entries object")?;
    Ok(entries
        .iter()
        .filter_map(|(id, entry)| {
            entry
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.trim().is_empty())
                .map(|title| (id.to_lowercase(), title.to_owned()))
        })
        .collect())
}

fn milliseconds(value: Option<&Value>) -> MessageTime {
    match value {
        None | Some(Value::Null) => MessageTime::Missing,
        Some(Value::String(value)) => parse_database_time(Some(value)),
        Some(value) => value
            .as_i64()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(MessageTime::Known)
            .unwrap_or(MessageTime::Invalid),
    }
}

fn messages(state: &Value, cancel: &Cancellation) -> Result<Vec<Message>> {
    let Some(requests) = state.get("requests") else {
        return Ok(Vec::new());
    };
    let requests = requests
        .as_array()
        .context("VS Code requests must be an array")?;
    let mut messages = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        cancel.check()?;
        if !request.is_object()
            || request.get("hiddenFromTranscript").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let user = request
            .pointer("/message/text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let response = request
            .get("response")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part.get("kind").is_none_or(Value::is_null))
                    .filter_map(|part| part.get("value").and_then(Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default();
        let native = request
            .get("requestId")
            .or_else(|| request.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("request-{index}"));
        for (role, content, field) in [
            (Role::User, user, "timestamp"),
            (Role::Assistant, response, "responseTimestamp"),
        ] {
            if content.is_empty() {
                continue;
            }
            let offset = u64::try_from(index)?
                .checked_mul(2)
                .context("Too many VS Code requests")?
                + u64::from(role == Role::Assistant);
            messages.push(Message {
                id: format!("{native}:{}", role.label()),
                role,
                time: milliseconds(request.get(field)),
                content,
                attachments: Vec::new(),
                offset,
                end: offset + 1,
            });
        }
    }
    Ok(messages)
}

fn replay(path: &Path, limit: u64, cancel: &Cancellation) -> Result<Transcript> {
    let file = File::open(path)
        .with_context(|| format!("Cannot read VS Code transcript {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.len() >= limit,
        "VS Code transcript was truncated; refresh to retry"
    );
    let mut reader = BufReader::new(file.take(limit));
    let mut state = Value::Null;
    let mut line = Vec::new();
    let mut cursor = 0;
    let mut line_number = 0;
    let mut incomplete = false;
    loop {
        cancel.check()?;
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }
        line_number += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            cursor += bytes as u64;
            continue;
        }
        let event: Value = match serde_json::from_slice(&line) {
            Ok(event) => event,
            Err(error) if !line.ends_with(b"\n") && error.is_eof() => {
                incomplete = true;
                break;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("{}: invalid JSON at line {line_number}", path.display())
                });
            }
        };
        apply_delta(&mut state, event)
            .with_context(|| format!("{}: invalid delta at line {line_number}", path.display()))?;
        cursor += bytes as u64;
    }
    anyhow::ensure!(
        state.is_object(),
        "VS Code transcript has no valid session snapshot"
    );
    Ok(Transcript {
        state,
        cursor,
        incomplete,
    })
}

fn segment_key(segment: &Value) -> Result<String> {
    match segment {
        Value::String(key) => Ok(key.clone()),
        Value::Number(number) if number.as_u64().is_some() => Ok(number.to_string()),
        _ => anyhow::bail!("Delta path segment must be a key or nonnegative array index"),
    }
}

fn index(segment: &Value) -> Result<usize> {
    let index = segment_key(segment)?
        .parse::<usize>()
        .context("Expected an array index")?;
    anyhow::ensure!(
        index < 1_000_000,
        "Delta array index exceeds the supported safety limit"
    );
    Ok(index)
}

fn container(next: &Value) -> Value {
    if index(next).is_ok() {
        Value::Array(Vec::new())
    } else {
        Value::Object(Map::new())
    }
}

fn set_path(root: &mut Value, path: &[Value], value: Value) -> Result<()> {
    let Some((last, parents)) = path.split_last() else {
        *root = value;
        return Ok(());
    };
    let mut current = root;
    for (position, segment) in parents.iter().enumerate() {
        match current {
            Value::Array(array) => {
                let index = index(segment)?;
                if array.len() <= index {
                    array.resize(index + 1, Value::Null);
                }
                if array[index].is_null() {
                    array[index] = container(&path[position + 1]);
                }
                current = &mut array[index];
            }
            Value::Object(object) => {
                current = object
                    .entry(segment_key(segment)?)
                    .or_insert_with(|| container(&path[position + 1]));
            }
            _ => anyhow::bail!("Delta path traverses a non-container value"),
        }
    }
    match current {
        Value::Array(array) => {
            let index = index(last)?;
            if array.len() <= index {
                array.resize(index + 1, Value::Null);
            }
            array[index] = value;
        }
        Value::Object(object) => {
            object.insert(segment_key(last)?, value);
        }
        _ => anyhow::bail!("Delta path targets a non-container value"),
    }
    Ok(())
}

fn apply_delta(state: &mut Value, mut event: Value) -> Result<()> {
    let kind = event
        .get("kind")
        .and_then(Value::as_u64)
        .context("Missing delta kind")?;
    let value = event.get_mut("v").map(Value::take).unwrap_or(Value::Null);
    if kind == 0 {
        anyhow::ensure!(value.is_object(), "Session snapshot is not an object");
        *state = value;
        return Ok(());
    }
    anyhow::ensure!(state.is_object(), "Delta appears before a session snapshot");
    let path = event
        .get("k")
        .and_then(Value::as_array)
        .context("Missing delta path")?;
    anyhow::ensure!(path.len() <= 128, "Delta path is too deeply nested");
    match kind {
        1 => set_path(state, path, value),
        2 => {
            let mut current = state;
            for segment in path {
                current = match current {
                    Value::Array(array) => array
                        .get_mut(index(segment)?)
                        .context("Missing array entry")?,
                    Value::Object(object) => object
                        .get_mut(&segment_key(segment)?)
                        .context("Missing object key")?,
                    _ => anyhow::bail!("Invalid append delta path"),
                };
            }
            let array = current
                .as_array_mut()
                .context("Append target is not an array")?;
            match value {
                Value::Array(values) => array.extend(values),
                value => array.push(value),
            };
            Ok(())
        }
        _ => anyhow::bail!("Unsupported delta kind {kind}"),
    }
}

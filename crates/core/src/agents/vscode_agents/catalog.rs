//! Reads the VS Code agent host's catalog and locates each session's native transcript.

use super::Driver;
use crate::agents::jsonl::{Segment, is_compressed};
use crate::{agents::open_read_only, model::*};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

pub fn catalog_path(user_root: &Path) -> PathBuf {
    user_root.join("globalStorage").join("agent-host.db")
}

/// A session created in the Agents window (not one the host merely discovered on disk).
pub(super) struct Entry {
    pub id: String,
    pub sdk_id: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub modified_ms: i64,
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

fn parse(text: Option<String>) -> Value {
    text.and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

pub(super) fn entries(
    user_root: &Path,
    driver: Driver,
    cancel: &Cancellation,
) -> Result<Vec<Entry>> {
    let database = open_read_only(&catalog_path(user_root))?;
    let mut statement = database.prepare(
        "SELECT s.session_uri, s.modified_time, s.payload, c.provider_data, c.metadata
         FROM sessions_v2 s
         JOIN chats_v2 c ON c.owner_session_uri = s.session_uri AND c.tombstoned = 0 AND c.chat_order = 0
         WHERE s.provider = ?1 AND COALESCE(s.external, 0) = 0",
    ).context("Cannot read the VS Code agent host catalog")?;
    let provider = driver.provider();
    let rows = statement.query_map([provider], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        cancel.check()?;
        let (uri, modified_ms, payload, provider_data, metadata) = row?;
        let Some(id) = uri
            .strip_prefix(provider)
            .and_then(|rest| rest.strip_prefix(":/"))
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            .map(str::to_lowercase)
        else {
            continue;
        };
        let payload = parse(payload);
        let workspace = payload
            .pointer("/data/workingDirectories/0")
            .and_then(|directory| {
                directory
                    .as_str()
                    .or_else(|| directory.get("uri")?.as_str())
            })
            .map(file_uri_to_path);
        entries.push(Entry {
            sdk_id: text(parse(provider_data).get("sdkSessionId")).map(|id| id.to_lowercase()),
            title: text(payload.pointer("/data/summary"))
                .or_else(|| text(parse(metadata).pointer("/data/summary"))),
            id,
            workspace,
            modified_ms,
        });
    }
    Ok(entries)
}

/// `file:///d%3A/Workspace/x` becomes `d:\Workspace\x` on Windows.
pub(super) fn file_uri_to_path(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_owned();
    };
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                index += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    let path = String::from_utf8_lossy(&decoded).into_owned();
    let drive = path.as_bytes();
    let path = if drive.len() > 2 && drive[0] == b'/' && drive[2] == b':' {
        path[1..].to_owned()
    } else {
        path
    };
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path
    }
}

/// `projects/<slug>/<sdk-session>.jsonl`, keyed by the sdk session id.
pub(super) fn claude_transcripts(home: &Path) -> HashMap<String, PathBuf> {
    let mut found = HashMap::new();
    let Ok(projects) = fs::read_dir(home.join("projects")) else {
        return found;
    };
    for project in projects.flatten().filter(|entry| entry.path().is_dir()) {
        for file in fs::read_dir(project.path()).into_iter().flatten().flatten() {
            let path = file.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && uuid::Uuid::parse_str(stem).is_ok()
            {
                found.insert(stem.to_lowercase(), path);
            }
        }
    }
    found
}

/// The Codex thread an agent-host session runs, from the host's per-session store.
pub(super) fn codex_thread(user_root: &Path, session: &str) -> Option<String> {
    let store = user_root
        .parent()?
        .join("agentSessionData")
        .join(session)
        .join("session.db");
    if !store.is_file() {
        return None;
    }
    let value: String = open_read_only(&store)
        .ok()?
        .query_row(
            "SELECT value FROM session_metadata WHERE key = 'codex.threadId' AND turn_id IS NULL",
            [],
            |row| row.get(0),
        )
        .ok()?;
    let thread = value.trim().trim_matches('"').to_lowercase();
    uuid::Uuid::parse_str(&thread).is_ok().then_some(thread)
}

#[derive(Default)]
pub(super) struct Rollouts {
    /// Every rollout file by its own id: the thread id for a first file, the page id for a page.
    pub files: HashMap<String, PathBuf>,
    /// The newest file of each thread, which holds its latest history.
    pub heads: HashMap<String, PathBuf>,
}

/// Indexes `sessions/Y/M/D/` and `archived_sessions/` rollouts.
pub(super) fn codex_rollouts(home: &Path) -> Rollouts {
    let mut directories = vec![home.join("archived_sessions")];
    for years in fs::read_dir(home.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for months in fs::read_dir(years.path()).into_iter().flatten().flatten() {
            for days in fs::read_dir(months.path()).into_iter().flatten().flatten() {
                directories.push(days.path());
            }
        }
    }
    let mut rollouts = Rollouts::default();
    let mut newest: HashMap<String, (String, PathBuf)> = HashMap::new();
    // Active sessions come first so that they win over an archived copy of the same file.
    directories.rotate_left(1);
    for directory in directories {
        for file in fs::read_dir(directory).into_iter().flatten().flatten() {
            let path = file.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(stem) = name
                .strip_prefix("rollout-")
                .and_then(|name| name.strip_suffix(".zst").or(Some(name)))
                .and_then(|name| name.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let Some((thread, own)) = ids_of(stem) else {
                continue;
            };
            rollouts.files.entry(own).or_insert_with(|| path.clone());
            // Names start with the creation time, so the greatest name is the latest page.
            if newest
                .get(&thread)
                .is_none_or(|(latest, _)| stem > latest.as_str())
            {
                newest.insert(thread, (stem.to_owned(), path));
            }
        }
    }
    rollouts.heads = newest
        .into_iter()
        .map(|(thread, (_, path))| (thread, path))
        .collect();
    rollouts
}

/// Splits `<timestamp>-<thread>` or `<timestamp>-<thread>_<page>` into the thread id and the
/// file's own id (the page id for a page, otherwise the thread id).
fn ids_of(stem: &str) -> Option<(String, String)> {
    let uuid = |end: usize| {
        let start = end.checked_sub(36)?;
        let candidate = stem.get(start..end)?;
        (uuid::Uuid::parse_str(candidate).is_ok() && stem.get(..start)?.ends_with(['-', '_']))
            .then(|| candidate.to_lowercase())
    };
    let last = uuid(stem.len())?;
    if stem.get(..stem.len() - 36)?.ends_with('-') {
        return Some((last.clone(), last));
    }
    // `…-<thread>_<page>`: the character before the page uuid is the underscore.
    let thread = uuid(stem.len() - 37)?;
    Some((thread, last))
}

/// The earlier files that a rollout's history continues from, oldest first.
pub(super) struct Chain {
    pub base: Vec<Segment>,
    /// Set when the history could not be completed, so the reader shows only part of it.
    pub warning: Option<String>,
}

pub(super) fn chain(rollouts: &Rollouts, head: &Path) -> Chain {
    let mut base = Vec::new();
    let mut seen = HashSet::from([head.to_owned()]);
    let mut current = head.to_owned();
    let warning = loop {
        let (from, limit) = match history_base(&current) {
            Ok(Some(found)) => found,
            Ok(None) => break None,
            Err(error) => {
                break Some(format!(
                    "Cannot read the history base of this rollout: {error:#}"
                ));
            }
        };
        let Some(path) = rollouts.files.get(&from) else {
            break Some(format!(
                "Codex history continues from {from}, which is not on disk; only the later part is shown"
            ));
        };
        if base.len() >= 32 || !seen.insert(path.clone()) {
            break Some(
                "Codex history chain is cyclic or too long; only part of it is shown".into(),
            );
        }
        base.push(Segment {
            path: path.clone(),
            limit,
        });
        current = path.clone();
    };
    base.reverse();
    Chain { base, warning }
}

/// `history_base` of the rollout's first record: the file it continues and where that file's
/// share of the history ends.
fn history_base(path: &Path) -> Result<Option<(String, u64)>> {
    let file = fs::File::open(path)?;
    let mut reader: Box<dyn BufRead> = if is_compressed(path) {
        let decoder = ruzstd::decoding::StreamingDecoder::new(file)
            .map_err(|error| anyhow::anyhow!("Cannot decompress: {error}"))?;
        Box::new(BufReader::new(decoder))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut line = Vec::new();
    // The first record carries the whole system prompt, but never approaches this bound.
    (&mut reader)
        .take(16 * 1024 * 1024)
        .read_until(b'\n', &mut line)?;
    let record: Value = serde_json::from_slice(&line).context("Invalid first record")?;
    let Some(base) = record.pointer("/payload/history_base") else {
        return Ok(None);
    };
    let thread = base
        .get("thread_id")
        .and_then(Value::as_str)
        .context("history_base has no thread_id")?
        .to_lowercase();
    let offset = base
        .get("end_byte_offset")
        .and_then(Value::as_u64)
        .context("history_base has no end_byte_offset")?;
    Ok(Some((thread, offset)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_become_windows_paths() {
        if cfg!(windows) {
            assert_eq!(
                file_uri_to_path("file:///d%3A/Workspace/agent-chat-reader"),
                "d:\\Workspace\\agent-chat-reader"
            );
            assert_eq!(file_uri_to_path("file:///r%3A/a%20b"), "r:\\a b");
        }
        assert_eq!(file_uri_to_path("plain"), "plain");
    }

    #[test]
    fn rollout_names_map_to_threads_and_own_ids() {
        let thread = "01a1f711-81a5-7723-ad58-21611e1d29db";
        let page = "01a1f78c-7b37-70a1-ad85-0e71e723a7e5";
        // A first file is its own thread; a page belongs to the thread but has its own id.
        assert_eq!(
            ids_of(&format!("2026-11-20T11-48-02-{thread}")),
            Some((thread.into(), thread.into()))
        );
        assert_eq!(
            ids_of(&format!("2026-11-20T14-02-21-{thread}_{page}")),
            Some((thread.into(), page.into()))
        );
        assert_eq!(ids_of("2026-11-20T14-02-21-not-a-uuid"), None);
    }
}

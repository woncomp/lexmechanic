//! Codex sessions: `rollout-<time>-<thread>[_<page>].jsonl[.zst]` under `sessions/Y/M/D/` and
//! `archived_sessions/` of the Codex home.
//!
//! The rollout format and the page chain are shared with the VS Code Agents adapter, which reads
//! the rollouts of sessions created in the Agents window; this adapter lists the others.

use super::{
    AgentAdapter, candidate,
    copilot_cli::visible_content,
    jsonl::{self, Format, Header, Segment, is_compressed},
    open_read_only, source,
};
use crate::model::*;
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// The `originator` of rollouts written for sessions created in the VS Code Agents window.
const AGENT_HOST: &str = "vscode_agent_host";

pub fn default_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Ok(home.into());
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    home.map(|home| PathBuf::from(home).join(".codex"))
        .context("Cannot locate the user home directory; choose a Codex data directory")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The `item_completed` events carry exactly the user prompts and agent messages. The parallel
/// `response_item` records also hold injected AGENTS.md and developer text.
pub(super) struct Rollout;

fn item(record: &Value) -> Option<(Role, String, Vec<Value>)> {
    if record.get("type").and_then(Value::as_str)? != "event_msg" {
        return None;
    }
    let payload = record.get("payload")?;
    if payload.get("type").and_then(Value::as_str)? != "item_completed" {
        return None;
    }
    let item = payload.get("item")?;
    let role = match item.get("type").and_then(Value::as_str)? {
        "UserMessage" => Role::User,
        "AgentMessage" => Role::Assistant,
        _ => return None,
    };
    let mut text = Vec::new();
    let mut attachments = Vec::new();
    for part in item.get("content")?.as_array()? {
        match part.get("type").and_then(Value::as_str) {
            Some(kind) if kind.eq_ignore_ascii_case("text") => {
                text.extend(part.get("text").and_then(Value::as_str));
            }
            Some(kind @ ("local_image" | "image")) => {
                attachments.push(serde_json::json!({ "type": kind, "path": part.get("path") }));
            }
            _ => {}
        }
    }
    Some((role, text.join("\n\n"), attachments))
}

impl Format for Rollout {
    fn message(record: &Value, offset: u64, end: u64) -> Option<Message> {
        let (role, raw, attachments) = item(record)?;
        let content = visible_content(&raw);
        if content.trim().is_empty() && attachments.is_empty() {
            return None;
        }
        Some(Message {
            // Agent message ids are long opaque tokens; the per-file ordinal is stable and short.
            id: record.get("ordinal").and_then(Value::as_u64).map_or_else(
                || format!("byte-{offset}"),
                |ordinal| format!("ordinal-{ordinal}"),
            ),
            role,
            time: MessageTime::parse(record.get("timestamp").and_then(Value::as_str)),
            content,
            attachments,
            offset,
            end,
        })
    }

    fn raw(record: &Value) -> Option<String> {
        item(record).map(|(_, raw, _)| raw)
    }

    fn wants(line: &[u8]) -> bool {
        // Most lines are tool traffic and token counts; only these two kinds are read.
        contains(line, b"\"item_completed\"") || contains(line, b"\"session_meta\"")
    }

    fn header(record: &Value, header: &mut Header) {
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            return;
        }
        let Some(meta) = record.get("payload") else {
            return;
        };
        if header.created.is_none() {
            header.created = meta
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if header.workspace.is_none() {
            // Some writers store extended-length paths; show the ordinary form.
            header.workspace = meta
                .get("cwd")
                .and_then(Value::as_str)
                .map(|cwd| cwd.strip_prefix(r"\\?\").unwrap_or(cwd).to_owned());
        }
    }
}

#[derive(Default)]
pub(super) struct Rollouts {
    /// Every rollout file by its own id: the thread id for a first file, the page id for a page.
    pub files: HashMap<String, PathBuf>,
    /// The newest file of each thread, which holds its latest history.
    pub heads: HashMap<String, PathBuf>,
}

/// Indexes `sessions/Y/M/D/` and `archived_sessions/` rollouts.
pub(super) fn index_rollouts(home: &Path) -> Rollouts {
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

/// What a rollout's first record says about the thread.
#[derive(Default)]
pub(super) struct Meta {
    pub originator: Option<String>,
    /// Spawned by another thread (sub-agents, guardian reviews), not started by the user.
    pub subagent: bool,
    /// The file this one continues and where that file's share of the history ends.
    pub base: Option<(String, u64)>,
}

pub(super) fn first_meta(path: &Path) -> Result<Meta> {
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
    let Some(meta) = record.get("payload") else {
        return Ok(Meta::default());
    };
    let base = match meta.get("history_base") {
        None | Some(Value::Null) => None,
        Some(base) => Some((
            base.get("thread_id")
                .and_then(Value::as_str)
                .context("history_base has no thread_id")?
                .to_lowercase(),
            base.get("end_byte_offset")
                .and_then(Value::as_u64)
                .context("history_base has no end_byte_offset")?,
        )),
    };
    Ok(Meta {
        originator: meta
            .get("originator")
            .and_then(Value::as_str)
            .map(str::to_owned),
        subagent: meta
            .get("source")
            .is_some_and(|source| source.get("subagent").is_some()),
        base,
    })
}

/// The earlier files that a rollout's history continues from, oldest first.
pub(super) struct Chain {
    pub base: Vec<Segment>,
    /// Set when the history could not be completed, so the reader shows only part of it.
    pub warning: Option<String>,
}

pub(super) fn chain(
    rollouts: &Rollouts,
    head: &Path,
    meta_of: &dyn Fn(&Path) -> Result<Arc<Meta>>,
) -> Chain {
    let mut base = Vec::new();
    let mut seen = HashSet::from([head.to_owned()]);
    let mut current = head.to_owned();
    let warning = loop {
        let (from, limit) = match meta_of(&current).as_deref() {
            Ok(Meta {
                base: Some(found), ..
            }) => found.clone(),
            Ok(_) => break None,
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

/// The rollout index and history chains of one Codex home, shared by both Codex adapters.
pub(super) struct Files {
    /// The Codex home, or empty when it must be inferred from a rollout path.
    home: PathBuf,
    rollouts: Mutex<Option<Arc<Rollouts>>>,
    chains: Mutex<HashMap<PathBuf, Vec<Segment>>>,
    metas: Mutex<HashMap<PathBuf, Arc<Meta>>>,
}

impl Files {
    pub(super) fn new(home: PathBuf) -> Self {
        Self {
            home,
            rollouts: Mutex::new(None),
            chains: Mutex::new(HashMap::new()),
            metas: Mutex::new(HashMap::new()),
        }
    }

    /// A rollout's first-record facts, read once per scan.
    pub(super) fn meta(&self, path: &Path) -> Result<Arc<Meta>> {
        if let Some(meta) = self
            .metas
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex rollout metadata failed"))?
            .get(path)
        {
            return Ok(meta.clone());
        }
        let meta = Arc::new(first_meta(path)?);
        self.metas
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex rollout metadata failed"))?
            .insert(path.to_owned(), meta.clone());
        Ok(meta)
    }

    /// The Codex data directory: the configured one, else the one that holds this rollout.
    fn home_of(&self, rollout: &Path) -> Option<PathBuf> {
        if !self.home.as_os_str().is_empty() {
            return Some(self.home.clone());
        }
        let folder = rollout.parent()?.file_name()?;
        let home = if folder == "archived_sessions" {
            rollout.ancestors().nth(2)?
        } else {
            // sessions/<year>/<month>/<day>/rollout-*.jsonl
            let sessions = rollout.ancestors().nth(4)?;
            if sessions.file_name()? != "sessions" {
                return None;
            }
            sessions.parent()?
        };
        Some(home.to_owned())
    }

    /// Re-reads what is on disk and forgets every resolved chain.
    pub(super) fn rescan(&self) -> Result<Arc<Rollouts>> {
        let rollouts = Arc::new(index_rollouts(&self.home));
        *self
            .rollouts
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex rollout index failed"))? = Some(rollouts.clone());
        self.chains
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex history chain failed"))?
            .clear();
        self.metas
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex rollout metadata failed"))?
            .clear();
        Ok(rollouts)
    }

    fn rollouts(&self, rollout: &Path) -> Result<Arc<Rollouts>> {
        let mut cached = self
            .rollouts
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex rollout index failed"))?;
        if let Some(index) = cached.as_ref() {
            return Ok(index.clone());
        }
        let home = self
            .home_of(rollout)
            .context("Cannot locate the Codex data directory for this rollout")?;
        Ok(cached.insert(Arc::new(index_rollouts(&home))).clone())
    }

    /// The earlier files that a rollout continues from, oldest first, and a warning when the
    /// history could not be completed. `rescan` re-reads the files' own history markers.
    pub(super) fn base(
        &self,
        rollout: &Path,
        rescan: bool,
    ) -> Result<(Vec<Segment>, Option<String>)> {
        if !rescan
            && let Some(base) = self
                .chains
                .lock()
                .map_err(|_| anyhow::anyhow!("Codex history chain failed"))?
                .get(rollout)
        {
            return Ok((base.clone(), None));
        }
        let index = self.rollouts(rollout)?;
        let chain = chain(&index, rollout, &|path| self.meta(path));
        self.chains
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex history chain failed"))?
            .insert(rollout.to_owned(), chain.base.clone());
        Ok((chain.base, chain.warning))
    }

    /// The session's candidate: its own file, with the change time of everything it replays.
    pub(super) fn candidate(
        &self,
        source: &Source,
        native: &str,
        path: &Path,
        rescan: bool,
    ) -> Result<(Candidate, Option<String>)> {
        let (base, warning) = self.base(path, rescan)?;
        let mode = if base.is_empty() && !is_compressed(path) {
            ReadMode::Append
        } else {
            ReadMode::Snapshot
        };
        let mut found = candidate(source, native.to_owned(), path, mode)?;
        for segment in &base {
            let earlier = candidate(source, native.to_owned(), &segment.path, mode)?;
            found.modified_ns = found.modified_ns.max(earlier.modified_ns);
        }
        Ok((found, warning))
    }
}

/// What Codex's own bookkeeping says about a thread, without opening its rollout.
struct Known {
    name: Option<String>,
    /// A sub-agent or the Agents window's own session: not listed here.
    skip: bool,
}

/// Thread names and kinds. The app's index names win over the state database's.
fn known(home: &Path, diagnostics: &mut Vec<Diagnostic>) -> HashMap<String, Known> {
    let mut known = HashMap::new();
    // `state_<n>.sqlite` is versioned; the highest version is the live one.
    let state = fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let version = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            Some((version, entry.path()))
        })
        .max();
    if let Some((_, path)) = state {
        let read = || -> Result<Vec<(String, String, String, String)>> {
            let database = open_read_only(&path)?;
            let mut statement = database.prepare(
                "SELECT id, COALESCE(name, ''), COALESCE(source, ''), COALESCE(originator, '') FROM threads",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        };
        match read() {
            Ok(rows) => {
                for (id, name, source, originator) in rows {
                    let subagent = serde_json::from_str::<Value>(&source)
                        .is_ok_and(|source| source.get("subagent").is_some());
                    known.insert(
                        id.to_lowercase(),
                        Known {
                            name: Some(name).filter(|name| !name.trim().is_empty()),
                            skip: subagent || originator == AGENT_HOST,
                        },
                    );
                }
            }
            Err(error) => diagnostics.push(Diagnostic {
                path,
                offset: None,
                description: format!("Cannot read Codex thread names: {error:#}"),
            }),
        }
    }
    let index = home.join("session_index.jsonl");
    if let Ok(text) = fs::read_to_string(&index) {
        // Later entries supersede earlier ones for the same thread.
        for entry in text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            if let (Some(id), Some(name)) = (
                entry.get("id").and_then(Value::as_str),
                entry
                    .get("thread_name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty()),
            ) {
                known
                    .entry(id.to_lowercase())
                    .or_insert(Known {
                        name: None,
                        skip: false,
                    })
                    .name = Some(name.to_owned());
            }
        }
    }
    known
}

pub struct Codex {
    source: Source,
    files: Files,
    titles: Mutex<HashMap<String, String>>,
}

impl Codex {
    pub fn new(home: impl Into<PathBuf>) -> Result<Self> {
        let home = home.into();
        Ok(Self {
            source: source(&home, "codex")?,
            files: Files::new(home),
            titles: Mutex::new(HashMap::new()),
        })
    }

    /// A reader for an already-discovered session; it only needs the rollout files.
    pub fn for_candidate(candidate: &Candidate) -> Result<Self> {
        let root = candidate
            .events_path
            .parent()
            .context("Invalid Codex rollout location")?
            .to_owned();
        Ok(Self {
            source: Source {
                id: candidate.key.source.clone(),
                agent: candidate.key.agent.clone(),
                root,
            },
            files: Files::new(PathBuf::new()),
            titles: Mutex::new(HashMap::new()),
        })
    }
}

impl AgentAdapter for Codex {
    fn source(&self) -> &Source {
        &self.source
    }

    fn probe(&self, previous: &Candidate, cancel: &Cancellation) -> Result<Candidate> {
        cancel.check()?;
        Ok(self
            .files
            .candidate(
                &self.source,
                &previous.key.native,
                &previous.events_path,
                true,
            )?
            .0)
    }

    fn discover(&self, cancel: &Cancellation) -> Result<Discovery> {
        let rollouts = self.files.rescan()?;
        let mut diagnostics = Vec::new();
        let known = known(&self.source.root, &mut diagnostics);
        let mut candidates = Vec::new();
        let mut kept = HashMap::new();
        for (thread, head) in &rollouts.heads {
            cancel.check()?;
            // Sub-agent and review threads are not conversations of their own, and sessions from
            // the Agents window belong to the VS Code Agents adapter. Codex's own records say
            // so without opening the rollout; the rollout's first record decides otherwise.
            let skip = match known.get(thread) {
                Some(known) => known.skip,
                None => match self.files.meta(head) {
                    Ok(meta) => meta.subagent || meta.originator.as_deref() == Some(AGENT_HOST),
                    Err(error) => {
                        diagnostics.push(Diagnostic {
                            path: head.clone(),
                            offset: None,
                            description: format!("{error:#}"),
                        });
                        true
                    }
                },
            };
            if skip {
                continue;
            }
            match self.files.candidate(&self.source, thread, head, false) {
                Ok((found, warning)) => {
                    diagnostics.extend(warning.map(|description| Diagnostic {
                        path: head.clone(),
                        offset: None,
                        description,
                    }));
                    if let Some(name) = known.get(thread).and_then(|known| known.name.clone()) {
                        kept.insert(thread.clone(), name);
                    }
                    candidates.push(found);
                }
                Err(error) => diagnostics.push(Diagnostic {
                    path: head.clone(),
                    offset: None,
                    description: format!("{error:#}"),
                }),
            }
        }
        *self
            .titles
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex name reader failed"))? = kept;
        candidates
            .sort_unstable_by(|a, b| b.modified_ns.cmp(&a.modified_ns).then(a.key.cmp(&b.key)));
        Ok(Discovery {
            candidates,
            diagnostics,
            reclassified: Vec::new(),
        })
    }

    fn summarize(&self, candidate: &Candidate, cancel: &Cancellation) -> Result<Session> {
        let name = self
            .titles
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex name reader failed"))?
            .get(&candidate.key.native)
            .cloned();
        let (base, _) = self.files.base(&candidate.events_path, false)?;
        jsonl::summarize::<Rollout>(candidate, &base, name, cancel)
    }

    fn read_batch(
        &self,
        candidate: &Candidate,
        cursor: u64,
        budget: usize,
        cancel: &Cancellation,
    ) -> Result<ReadBatch> {
        let (base, _) = self.files.base(&candidate.events_path, false)?;
        jsonl::read_batch::<Rollout>(candidate, &base, cursor, budget, cancel)
    }

    fn original_message(&self, candidate: &Candidate, cached: &Message) -> Result<String> {
        let (base, _) = self.files.base(&candidate.events_path, false)?;
        jsonl::original_message::<Rollout>(candidate, &base, cached)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_completed_prompts_and_agent_messages_only() {
        let user: Value = serde_json::from_str(
            r#"{"timestamp":"2026-11-20T03:48:05.524Z","ordinal":9,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"u","content":[{"type":"text","text":"hello"},{"type":"local_image","path":"C:\\a.png"}]}}}"#,
        )
        .unwrap();
        let message = Rollout::message(&user, 0, 1).unwrap();
        assert_eq!(
            (message.role, message.content.as_str(), message.id.as_str()),
            (Role::User, "hello", "ordinal-9")
        );
        assert_eq!(message.attachments[0]["path"], "C:\\a.png");

        let agent: Value = serde_json::from_str(
            r#"{"timestamp":"2026-11-20T03:48:09.686Z","ordinal":12,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"x","phase":"commentary","content":[{"type":"Text","text":"on it"}]}}}"#,
        )
        .unwrap();
        assert_eq!(Rollout::message(&agent, 0, 1).unwrap().content, "on it");

        for skipped in [
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}]}}"##,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count"}}"#,
        ] {
            let record: Value = serde_json::from_str(skipped).unwrap();
            assert!(Rollout::message(&record, 0, 0).is_none(), "{skipped}");
        }
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

    #[test]
    fn extended_length_workspace_paths_are_shown_in_the_ordinary_form() {
        let record: Value = serde_json::from_str(
            r#"{"type":"session_meta","payload":{"timestamp":"2026-11-20T03:48:02.389Z","cwd":"\\\\?\\D:\\Work"}}"#,
        )
        .unwrap();
        let mut header = Header::default();
        Rollout::header(&record, &mut header);
        assert_eq!(header.workspace.as_deref(), Some("D:\\Work"));
    }
}

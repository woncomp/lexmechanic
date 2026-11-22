use super::{AgentAdapter, copilot_cli, open_read_only};
use crate::model::*;
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::Connection;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Default)]
struct AppRecord {
    title: Option<String>,
    names: Vec<String>,
    workspace: Option<String>,
    latest: MessageTime,
    shared: bool,
}

pub struct CopilotApp {
    source: Source,
    records: Mutex<HashMap<String, AppRecord>>,
}

pub(super) fn registered_ids(home: &Path, cancel: &Cancellation) -> Result<HashSet<String>> {
    let connection = open_read_only(&home.join("data.db"))?;
    let mut statement = connection.prepare("SELECT id FROM sessions")?;
    let mut ids = HashSet::new();
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        cancel.check()?;
        let id = row?;
        if uuid::Uuid::parse_str(&id).is_ok() {
            ids.insert(id.to_lowercase());
        }
    }
    Ok(ids)
}

pub(super) fn parse_database_time(value: Option<&str>) -> MessageTime {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return MessageTime::Missing;
    };
    if let Ok(time) = DateTime::parse_from_rfc3339(value.trim()) {
        return MessageTime::Known(time.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(time) = NaiveDateTime::parse_from_str(value.trim(), format) {
            return MessageTime::Known(time.and_utc());
        }
    }
    MessageTime::Invalid
}

fn app_records(
    connection: &Connection,
    cancel: &Cancellation,
) -> Result<HashMap<String, AppRecord>> {
    let mut records = HashMap::new();
    let mut statement = connection.prepare("SELECT id,title FROM sessions")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        cancel.check()?;
        let (id, title) = row?;
        if uuid::Uuid::parse_str(&id).is_ok() {
            records.insert(
                id.to_lowercase(),
                AppRecord {
                    title,
                    ..Default::default()
                },
            );
        }
    }
    let mut statement = connection
        .prepare("SELECT session_id,name FROM workspaces WHERE session_id IS NOT NULL")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        cancel.check()?;
        let (id, name) = row?;
        if let Some(record) = records.get_mut(&id.to_lowercase())
            && let Some(name) = name.filter(|name| !name.trim().is_empty())
            && !record.names.contains(&name)
        {
            record.names.push(name);
        }
    }
    Ok(records)
}

impl CopilotApp {
    pub fn new(home: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            source: super::source(&home.into(), "copilot-app")?,
            records: Mutex::new(HashMap::new()),
        })
    }

    pub fn for_candidate(candidate: &Candidate) -> Result<Self> {
        let home = candidate
            .events_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .context("Invalid Copilot App transcript location")?;
        let adapter = Self::new(home)?;
        anyhow::ensure!(
            adapter.source.id == candidate.key.source,
            "Copilot App source location changed"
        );
        Ok(adapter)
    }

    fn record(&self, native: &str) -> Result<AppRecord> {
        self.records
            .lock()
            .map_err(|_| anyhow::anyhow!("Copilot App metadata reader failed"))?
            .get(&native.to_lowercase())
            .cloned()
            .context("Copilot App session is no longer registered")
    }
}

impl AgentAdapter for CopilotApp {
    fn source(&self) -> &Source {
        &self.source
    }

    fn discover(&self, cancel: &Cancellation) -> Result<Discovery> {
        let database = self.source.root.join("data.db");
        let connection = open_read_only(&database)?;
        let mut records = app_records(&connection, cancel)
            .with_context(|| format!("Cannot read Copilot App metadata {}", database.display()))?;
        let mut diagnostics = Vec::new();
        let shared = self.source.root.join("session-store.db");
        if shared.is_file() {
            let result = (|| -> Result<()> {
                let connection = open_read_only(&shared)?;
                let mut statement = connection.prepare(
                    "SELECT s.id,s.cwd,(SELECT timestamp FROM turns t WHERE t.session_id=s.id
                     ORDER BY julianday(timestamp) DESC LIMIT 1) FROM sessions s",
                )?;
                for row in statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })? {
                    cancel.check()?;
                    let (id, cwd, time) = row?;
                    if let Some(record) = records.get_mut(&id.to_lowercase()) {
                        record.workspace = cwd.filter(|path| !path.is_empty());
                        record.shared = record.workspace.is_some();
                        record.latest = parse_database_time(time.as_deref());
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                cancel.check()?;
                diagnostics.push(Diagnostic {
                    path: shared,
                    offset: None,
                    description: format!("{error:#}"),
                });
            }
        }
        let mut directories = HashMap::new();
        let state_root = self.source.root.join("session-state");
        if state_root.is_dir() {
            for entry in fs::read_dir(&state_root)? {
                cancel.check()?;
                match entry {
                    Ok(entry) => {
                        directories.insert(
                            entry.file_name().to_string_lossy().to_lowercase(),
                            entry.path(),
                        );
                    }
                    Err(error) => diagnostics.push(Diagnostic {
                        path: state_root.clone(),
                        offset: None,
                        description: error.to_string(),
                    }),
                }
            }
        }
        let mut candidates = Vec::new();
        for id in records.keys() {
            cancel.check()?;
            let directory = directories
                .get(id)
                .cloned()
                .unwrap_or_else(|| state_root.join(id));
            if !directory.is_dir() && !records[id].shared {
                continue;
            }
            match copilot_cli::WorkspaceMetadata::read(&directory) {
                Ok(metadata) if metadata.continued_in_cli() => continue,
                Ok(_) => {}
                Err(error) => {
                    diagnostics.push(Diagnostic {
                        path: directory.join("workspace.yaml"),
                        offset: None,
                        description: format!("{error:#}"),
                    });
                    continue;
                }
            }
            let path = directory.join("events.jsonl");
            let result = super::candidate(&self.source, id.clone(), &path, ReadMode::Append);
            match result {
                Ok(candidate) => candidates.push(candidate),
                Err(_) if !path.exists() => {
                    let mut candidate =
                        super::candidate(&self.source, id.clone(), &database, ReadMode::Append)?;
                    candidate.events_path = path;
                    candidate.size = 0;
                    candidate.file_identity = None;
                    candidates.push(candidate);
                }
                Err(error) => diagnostics.push(Diagnostic {
                    path,
                    offset: None,
                    description: format!("{error:#}"),
                }),
            }
        }
        candidates
            .sort_unstable_by(|a, b| b.modified_ns.cmp(&a.modified_ns).then(a.key.cmp(&b.key)));
        *self
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!("Copilot App metadata reader failed"))? = records;
        let legacy = super::source(&self.source.root, "copilot-cli")?;
        let reclassified = candidates
            .iter()
            .map(|candidate| ReclassifiedSession {
                previous: SessionKey {
                    agent: legacy.agent.clone(),
                    source: legacy.id.clone(),
                    native: candidate.key.native.clone(),
                },
                current: candidate.key.clone(),
            })
            .collect();
        Ok(Discovery {
            candidates,
            diagnostics,
            reclassified,
        })
    }

    fn summarize(&self, candidate: &Candidate, cancel: &Cancellation) -> Result<Session> {
        let record = self.record(&candidate.key.native)?;
        let name = if record.names.len() == 1 {
            record.names.first().cloned()
        } else {
            record.title.filter(|title| !title.trim().is_empty())
        };
        let mut session = if candidate.events_path.is_file() {
            copilot_cli::summarize_log(candidate, name.clone(), cancel)?
        } else {
            Session {
                candidate: candidate.clone(),
                name: name.unwrap_or_else(|| candidate.key.native.chars().take(8).collect()),
                workspace: None,
                created: MessageTime::Missing,
                latest: record.latest,
                first_assistant: MessageTime::Missing,
                indexed: false,
                available: false,
            }
        };
        let directory = candidate
            .events_path
            .parent()
            .context("Invalid Copilot App transcript location")?;
        let cwd = copilot_cli::WorkspaceMetadata::read(directory)?.cwd;
        session.workspace = cwd
            .filter(|path| !path.is_empty())
            .or(record.workspace)
            .or(session.workspace);
        Ok(session)
    }

    fn read_batch(
        &self,
        candidate: &Candidate,
        cursor: u64,
        budget: usize,
        cancel: &Cancellation,
    ) -> Result<ReadBatch> {
        copilot_cli::read_log(candidate, cursor, budget, cancel)
    }

    fn original_message(&self, candidate: &Candidate, message: &Message) -> Result<String> {
        copilot_cli::original_message(&candidate.events_path, message)
    }
}

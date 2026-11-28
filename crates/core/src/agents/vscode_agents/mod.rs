//! Sessions created in the VS Code Agents window.
//!
//! The agent host (`globalStorage/agent-host.db`) catalogs every session with its title and
//! workspace, but the conversation lives in each driver's own transcript: Copilot CLI
//! `events.jsonl`, a Claude SDK `.jsonl` or a Codex rollout. One adapter instance serves one
//! driver, so each driver is its own agent with its own icon and its own source id.

mod catalog;
mod claude;
mod codex;

use super::{AgentAdapter, candidate, copilot_cli, jsonl, source};
use crate::model::*;
use anyhow::{Context, Result};
use catalog::Entry;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

pub use catalog::catalog_path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    Copilot,
    Claude,
    Codex,
}

impl Driver {
    pub const ALL: [Driver; 3] = [Driver::Copilot, Driver::Claude, Driver::Codex];

    pub fn agent(self) -> &'static str {
        match self {
            Self::Copilot => "vscode-agents-copilot",
            Self::Claude => "vscode-agents-claude",
            Self::Codex => "vscode-agents-codex",
        }
    }

    pub fn from_agent(agent: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|driver| driver.agent() == agent)
    }

    /// The provider name the agent host files its sessions under.
    fn provider(self) -> &'static str {
        match self {
            Self::Copilot => "copilotcli",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// Where each driver keeps its own data.
#[derive(Clone, Debug)]
pub struct Homes {
    pub copilot: PathBuf,
    pub claude: PathBuf,
    pub codex: PathBuf,
}

impl Homes {
    pub fn detect() -> Result<Self> {
        let user = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
        let home = |variable: &str, folder: &str| -> Result<PathBuf> {
            if let Some(value) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
                return Ok(value.into());
            }
            user.as_ref()
                .map(|user| user.join(folder))
                .context("Cannot locate the user home directory")
        };
        Ok(Self {
            copilot: copilot_cli::default_home()?,
            claude: home("CLAUDE_CONFIG_DIR", ".claude")?,
            codex: home("CODEX_HOME", ".codex")?,
        })
    }
}

#[derive(Clone, Default)]
struct Record {
    title: Option<String>,
    workspace: Option<String>,
}

pub struct VsCodeAgents {
    source: Source,
    driver: Driver,
    homes: Homes,
    records: Mutex<HashMap<String, Record>>,
}

const MISSING: &str = "Local session transcript was not found";

impl VsCodeAgents {
    pub fn new(user_root: impl Into<PathBuf>, driver: Driver, homes: Homes) -> Result<Self> {
        Ok(Self {
            source: source(&user_root.into(), driver.agent())?,
            driver,
            homes,
            records: Mutex::new(HashMap::new()),
        })
    }

    /// A reader for an already-discovered session. It only needs the transcript, so it does not
    /// require the catalog or the original data directories to be present.
    pub fn for_candidate(candidate: &Candidate) -> Result<Self> {
        let driver = Driver::from_agent(&candidate.key.agent)
            .context("This agent provider is not installed")?;
        let root = candidate
            .events_path
            .parent()
            .context("Invalid VS Code Agents transcript location")?
            .to_owned();
        Ok(Self {
            source: Source {
                id: candidate.key.source.clone(),
                agent: candidate.key.agent.clone(),
                root,
            },
            driver,
            homes: Homes {
                copilot: PathBuf::new(),
                claude: PathBuf::new(),
                codex: PathBuf::new(),
            },
            records: Mutex::new(HashMap::new()),
        })
    }

    /// Transcripts by session id, plus the sessions another adapter already lists.
    fn transcripts(
        &self,
        entries: &[Entry],
        diagnostics: &mut Vec<Diagnostic>,
    ) -> (HashMap<String, PathBuf>, HashSet<String>) {
        let user_root = &self.source.root;
        let mut found = HashMap::new();
        let mut foreign = HashSet::new();
        match self.driver {
            Driver::Copilot => {
                for entry in entries {
                    let directory = self
                        .homes
                        .copilot
                        .join("session-state")
                        .join(entry.sdk_id.as_deref().unwrap_or(&entry.id));
                    let claimed = copilot_cli::WorkspaceMetadata::read(&directory)
                        .map(|metadata| !metadata.claimed_by_cli())
                        .unwrap_or(true);
                    let events = directory.join("events.jsonl");
                    if !events.is_file() {
                        continue;
                    }
                    if claimed {
                        found.insert(entry.id.clone(), events);
                    } else {
                        foreign.insert(entry.id.clone());
                    }
                }
            }
            Driver::Claude => {
                let files = catalog::claude_transcripts(&self.homes.claude);
                for entry in entries {
                    if let Some(path) = entry.sdk_id.as_ref().and_then(|sdk| files.get(sdk)) {
                        found.insert(entry.id.clone(), path.clone());
                    }
                }
            }
            Driver::Codex => {
                let rollouts = catalog::codex_rollouts(&self.homes.codex);
                for entry in entries {
                    let Some(thread) = catalog::codex_thread(user_root, &entry.id) else {
                        continue;
                    };
                    let Some(path) = rollouts.primary.get(&thread) else {
                        continue;
                    };
                    if let Some(pages) = rollouts.pages.get(&thread) {
                        diagnostics.push(Diagnostic {
                            path: path.clone(),
                            offset: None,
                            description: format!(
                                "Codex stored {pages} continuation page(s) for this thread in separate files; only the first file is shown"
                            ),
                        });
                    }
                    found.insert(entry.id.clone(), path.clone());
                }
            }
        }
        (found, foreign)
    }

    fn unavailable(candidate: &Candidate, record: Record) -> Session {
        let mut session = Session::unreadable(candidate.clone(), MISSING.into());
        if let Some(title) = record.title {
            session.name = title;
        }
        session.workspace = record.workspace;
        session
    }
}

impl AgentAdapter for VsCodeAgents {
    fn source(&self) -> &Source {
        &self.source
    }

    fn probe(&self, previous: &Candidate, cancel: &Cancellation) -> Result<Candidate> {
        cancel.check()?;
        // A missing transcript is represented by the catalog file, which must never be read as one.
        anyhow::ensure!(is_transcript(&previous.events_path), MISSING);
        candidate(
            &self.source,
            previous.key.native.clone(),
            &previous.events_path,
            previous.read_mode,
        )
    }

    fn discover(&self, cancel: &Cancellation) -> Result<Discovery> {
        let entries = catalog::entries(&self.source.root, self.driver, cancel)?;
        let mut diagnostics = Vec::new();
        let (transcripts, foreign) = self.transcripts(&entries, &mut diagnostics);
        let mut records = HashMap::new();
        let mut candidates = Vec::new();
        for entry in entries {
            cancel.check()?;
            // Sessions the Copilot CLI adapter owns, and empty stubs, are not worth a row.
            if foreign.contains(&entry.id)
                || (entry.title.is_none() && !transcripts.contains_key(&entry.id))
            {
                continue;
            }
            let found = match transcripts.get(&entry.id) {
                Some(path) => {
                    let mode = if jsonl::is_compressed(path) {
                        ReadMode::Snapshot
                    } else {
                        ReadMode::Append
                    };
                    candidate(&self.source, entry.id.clone(), path, mode)
                }
                // The catalog outlives deleted transcripts; keep the row but mark it unreadable.
                None => candidate(
                    &self.source,
                    entry.id.clone(),
                    &catalog::catalog_path(&self.source.root),
                    ReadMode::Snapshot,
                )
                .map(|mut missing| {
                    missing.size = 0;
                    missing.file_identity = None;
                    missing.modified_ns = i128::from(entry.modified_ms) * 1_000_000;
                    missing
                }),
            };
            match found {
                Ok(found) => {
                    candidates.push(found);
                    records.insert(
                        entry.id,
                        Record {
                            title: entry.title,
                            workspace: entry.workspace,
                        },
                    );
                }
                Err(error) => diagnostics.push(Diagnostic {
                    path: catalog::catalog_path(&self.source.root),
                    offset: None,
                    description: format!("{error:#}"),
                }),
            }
        }
        *self
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!("VS Code Agents metadata reader failed"))? = records;
        candidates
            .sort_unstable_by(|a, b| b.modified_ns.cmp(&a.modified_ns).then(a.key.cmp(&b.key)));
        Ok(Discovery {
            candidates,
            diagnostics,
            reclassified: Vec::new(),
        })
    }

    fn summarize(&self, candidate: &Candidate, cancel: &Cancellation) -> Result<Session> {
        let record = self
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!("VS Code Agents metadata reader failed"))?
            .get(&candidate.key.native)
            .cloned()
            .unwrap_or_default();
        if candidate.size == 0 && !is_transcript(&candidate.events_path) {
            return Ok(Self::unavailable(candidate, record));
        }
        let mut session = match self.driver {
            Driver::Copilot => copilot_cli::summarize_log(candidate, record.title, cancel)?,
            Driver::Claude => jsonl::summarize::<claude::Claude>(candidate, record.title, cancel)?,
            Driver::Codex => jsonl::summarize::<codex::Codex>(candidate, record.title, cancel)?,
        };
        // The transcript knows the working directory in the same form as the other agents.
        session.workspace = session.workspace.or(record.workspace);
        Ok(session)
    }

    fn read_batch(
        &self,
        candidate: &Candidate,
        cursor: u64,
        budget: usize,
        cancel: &Cancellation,
    ) -> Result<ReadBatch> {
        match self.driver {
            Driver::Copilot => copilot_cli::read_log(candidate, cursor, budget, cancel),
            Driver::Claude => {
                jsonl::read_batch::<claude::Claude>(candidate, cursor, budget, cancel)
            }
            Driver::Codex => jsonl::read_batch::<codex::Codex>(candidate, cursor, budget, cancel),
        }
    }

    fn original_message(&self, candidate: &Candidate, cached: &Message) -> Result<String> {
        match self.driver {
            Driver::Copilot => copilot_cli::original_message(&candidate.events_path, cached),
            Driver::Claude => jsonl::original_message::<claude::Claude>(candidate, cached),
            Driver::Codex => jsonl::original_message::<codex::Codex>(candidate, cached),
        }
    }
}

/// Transcripts are `.jsonl` / `.jsonl.zst`; the placeholder for a missing one is the catalog.
fn is_transcript(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl" || extension == "zst")
}

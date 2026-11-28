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
use catalog::{Entry, Rollouts};
use jsonl::Segment;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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
    /// Codex rollouts as of the last discovery, and the history chain resolved for each head.
    rollouts: Mutex<Option<Arc<Rollouts>>>,
    chains: Mutex<HashMap<PathBuf, Vec<Segment>>>,
}

const MISSING: &str = "Local session transcript was not found";

impl VsCodeAgents {
    pub fn new(user_root: impl Into<PathBuf>, driver: Driver, homes: Homes) -> Result<Self> {
        Ok(Self {
            source: source(&user_root.into(), driver.agent())?,
            driver,
            homes,
            records: Mutex::new(HashMap::new()),
            rollouts: Mutex::new(None),
            chains: Mutex::new(HashMap::new()),
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
            rollouts: Mutex::new(None),
            chains: Mutex::new(HashMap::new()),
        })
    }

    /// The Codex data directory: the configured one, else the one that holds this rollout.
    fn codex_home(&self, rollout: &Path) -> Option<PathBuf> {
        if !self.homes.codex.as_os_str().is_empty() {
            return Some(self.homes.codex.clone());
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

    fn rollouts(&self, rollout: &Path) -> Result<Arc<Rollouts>> {
        let mut cached = self
            .rollouts
            .lock()
            .map_err(|_| anyhow::anyhow!("VS Code Agents rollout index failed"))?;
        if let Some(index) = cached.as_ref() {
            return Ok(index.clone());
        }
        let home = self
            .codex_home(rollout)
            .context("Cannot locate the Codex data directory for this rollout")?;
        Ok(cached
            .insert(Arc::new(catalog::codex_rollouts(&home)))
            .clone())
    }

    /// The earlier files that a Codex rollout continues from, oldest first, and a warning when
    /// the history could not be completed. `rescan` re-reads the files' own history markers.
    fn base(&self, rollout: &Path, rescan: bool) -> Result<(Vec<Segment>, Option<String>)> {
        if self.driver != Driver::Codex {
            return Ok((Vec::new(), None));
        }
        if !rescan
            && let Some(base) = self
                .chains
                .lock()
                .map_err(|_| anyhow::anyhow!("VS Code Agents history chain failed"))?
                .get(rollout)
        {
            return Ok((base.clone(), None));
        }
        let index = self.rollouts(rollout)?;
        let chain = catalog::chain(&index, rollout);
        self.chains
            .lock()
            .map_err(|_| anyhow::anyhow!("VS Code Agents history chain failed"))?
            .insert(rollout.to_owned(), chain.base.clone());
        Ok((chain.base, chain.warning))
    }

    /// The session's candidate: its own file, with the change time of everything it replays.
    fn build_candidate(
        &self,
        native: &str,
        path: &Path,
        rescan: bool,
    ) -> Result<(Candidate, Option<String>)> {
        let (base, warning) = self.base(path, rescan)?;
        let mode = if base.is_empty() && !jsonl::is_compressed(path) {
            ReadMode::Append
        } else {
            ReadMode::Snapshot
        };
        let mut found = candidate(&self.source, native.to_owned(), path, mode)?;
        for segment in &base {
            let earlier = candidate(&self.source, native.to_owned(), &segment.path, mode)?;
            found.modified_ns = found.modified_ns.max(earlier.modified_ns);
        }
        Ok((found, warning))
    }

    /// Transcripts by session id, plus the sessions another adapter already lists.
    fn transcripts(&self, entries: &[Entry]) -> (HashMap<String, PathBuf>, HashSet<String>) {
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
                // Each discovery starts from what is on disk now.
                let rollouts = Arc::new(catalog::codex_rollouts(&self.homes.codex));
                if let Ok(mut cached) = self.rollouts.lock() {
                    *cached = Some(rollouts.clone());
                }
                if let Ok(mut chains) = self.chains.lock() {
                    chains.clear();
                }
                for entry in entries {
                    let Some(thread) = catalog::codex_thread(user_root, &entry.id) else {
                        continue;
                    };
                    // The newest page holds the thread's latest history; `base` finds the rest.
                    if let Some(path) = rollouts.heads.get(&thread) {
                        found.insert(entry.id.clone(), path.clone());
                    }
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
        Ok(self
            .build_candidate(&previous.key.native, &previous.events_path, true)?
            .0)
    }

    fn discover(&self, cancel: &Cancellation) -> Result<Discovery> {
        let entries = catalog::entries(&self.source.root, self.driver, cancel)?;
        let mut diagnostics = Vec::new();
        let (transcripts, foreign) = self.transcripts(&entries);
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
                    self.build_candidate(&entry.id, path, false)
                        .map(|(found, warning)| {
                            diagnostics.extend(warning.map(|description| Diagnostic {
                                path: path.clone(),
                                offset: None,
                                description,
                            }));
                            found
                        })
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
            Driver::Claude => {
                jsonl::summarize::<claude::Claude>(candidate, &[], record.title, cancel)?
            }
            Driver::Codex => {
                let (base, _) = self.base(&candidate.events_path, false)?;
                jsonl::summarize::<codex::Codex>(candidate, &base, record.title, cancel)?
            }
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
                jsonl::read_batch::<claude::Claude>(candidate, &[], cursor, budget, cancel)
            }
            Driver::Codex => {
                let (base, _) = self.base(&candidate.events_path, false)?;
                jsonl::read_batch::<codex::Codex>(candidate, &base, cursor, budget, cancel)
            }
        }
    }

    fn original_message(&self, candidate: &Candidate, cached: &Message) -> Result<String> {
        match self.driver {
            Driver::Copilot => copilot_cli::original_message(&candidate.events_path, cached),
            Driver::Claude => jsonl::original_message::<claude::Claude>(candidate, &[], cached),
            Driver::Codex => {
                let (base, _) = self.base(&candidate.events_path, false)?;
                jsonl::original_message::<codex::Codex>(candidate, &base, cached)
            }
        }
    }
}

/// Transcripts are `.jsonl` / `.jsonl.zst`; the placeholder for a missing one is the catalog.
fn is_transcript(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl" || extension == "zst")
}

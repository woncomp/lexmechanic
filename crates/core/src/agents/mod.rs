pub mod copilot_app;
pub mod copilot_cli;
pub mod vscode;

use crate::model::{
    Cancellation, Candidate, Discovery, Message, ReadBatch, ReadMode, Session, SessionKey, Source,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

pub fn adapter_for(candidate: &Candidate) -> anyhow::Result<Arc<dyn AgentAdapter>> {
    match candidate.key.agent.as_str() {
        "copilot-cli" => Ok(Arc::new(copilot_cli::CopilotCli::for_candidate(candidate)?)),
        "copilot-app" => Ok(Arc::new(copilot_app::CopilotApp::for_candidate(candidate)?)),
        "vscode" => Ok(Arc::new(vscode::VsCode::for_candidate(candidate)?)),
        _ => anyhow::bail!("This agent provider is not installed"),
    }
}

pub fn agent_name(id: &str) -> &str {
    match id {
        "copilot-cli" => "Copilot CLI",
        "copilot-app" => "Copilot App",
        "vscode" => "VS Code",
        _ => id,
    }
}

pub(super) fn source(root: &Path, agent: &str) -> Result<Source> {
    let root = std::fs::canonicalize(root).with_context(|| {
        format!(
            "Cannot access {} directory {}",
            agent_name(agent),
            root.display()
        )
    })?;
    let identity = root.to_string_lossy().to_string();
    #[cfg(windows)]
    let identity = identity.to_lowercase();
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    Ok(Source {
        id: if agent == "copilot-cli" {
            digest
        } else {
            format!("{agent}-{digest}")
        },
        agent: agent.into(),
        root,
    })
}

pub(super) fn candidate(
    source: &Source,
    native: String,
    path: &Path,
    read_mode: ReadMode,
) -> Result<Candidate> {
    let metadata = path
        .metadata()
        .with_context(|| format!("Cannot inspect {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "Not a transcript file: {}",
        path.display()
    );
    let modified_ns = match metadata.modified()?.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos())?,
        Err(error) => -i128::try_from(error.duration().as_nanos())?,
    };
    Ok(Candidate {
        key: SessionKey {
            agent: source.agent.clone(),
            source: source.id.clone(),
            native,
        },
        events_path: path.to_owned(),
        size: metadata.len(),
        modified_ns,
        file_identity: metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos()),
        read_mode,
    })
}

pub(super) fn open_read_only(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("Cannot read source database {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(1))?;
    Ok(connection)
}

/// Adapters are synchronous; the scheduler owns their worker threads.
pub trait AgentAdapter: Send + Sync + 'static {
    fn source(&self) -> &Source;
    fn discover(&self, cancel: &Cancellation) -> anyhow::Result<Discovery>;
    fn summarize(&self, candidate: &Candidate, cancel: &Cancellation) -> anyhow::Result<Session>;
    fn read_batch(
        &self,
        candidate: &Candidate,
        cursor: u64,
        budget: usize,
        cancel: &Cancellation,
    ) -> anyhow::Result<ReadBatch>;
    fn original_message(
        &self,
        _candidate: &Candidate,
        _message: &Message,
    ) -> anyhow::Result<String> {
        anyhow::bail!("This agent does not support original-text viewing")
    }
}

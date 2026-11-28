pub mod codex;
pub mod copilot_app;
pub mod copilot_cli;
mod jsonl;
pub mod vscode;
pub mod vscode_agents;

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
        "codex" => Ok(Arc::new(codex::Codex::for_candidate(candidate)?)),
        "vscode" => Ok(Arc::new(vscode::VsCode::for_candidate(candidate)?)),
        agent if vscode_agents::Driver::from_agent(agent).is_some() => Ok(Arc::new(
            vscode_agents::VsCodeAgents::for_candidate(candidate)?,
        )),
        _ => anyhow::bail!("This agent provider is not installed"),
    }
}

pub fn agent_name(id: &str) -> &str {
    match id {
        "copilot-cli" => "Copilot CLI",
        "copilot-app" => "Copilot App",
        "codex" => "Codex",
        "vscode" => "VS Code",
        "vscode-agents-copilot" => "VS Code Agents - Copilot",
        "vscode-agents-codex" => "VS Code Agents - Codex",
        "vscode-agents-claude" => "VS Code Agents - Claude",
        _ => id,
    }
}

/// The agent of a source id made by [`source`]: a bare digest for Copilot CLI, otherwise
/// `{agent}-{digest}`.
pub fn source_agent(id: &str) -> &str {
    let is_digest = |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit());
    if is_digest(id) {
        return "copilot-cli";
    }
    match id.rsplit_once('-') {
        Some((agent, digest)) if is_digest(digest) => agent,
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
    fn probe(&self, previous: &Candidate, cancel: &Cancellation) -> Result<Candidate> {
        cancel.check()?;
        candidate(
            self.source(),
            previous.key.native.clone(),
            &previous.events_path,
            previous.read_mode,
        )
    }
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

#[cfg(test)]
mod tests {
    use super::source_agent;

    #[test]
    fn source_ids_name_their_agent() {
        let digest = "a".repeat(64);
        assert_eq!(source_agent(&digest), "copilot-cli");
        assert_eq!(
            source_agent(&format!("vscode-agents-claude-{digest}")),
            "vscode-agents-claude"
        );
        assert_eq!(source_agent("custom"), "custom");
    }
}

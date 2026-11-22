use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, AtomicOrdering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(AtomicOrdering::Relaxed)
    }

    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.is_cancelled(), "Operation cancelled");
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct SessionKey {
    pub agent: String,
    pub source: String,
    pub native: String,
}

impl SessionKey {
    pub fn storage_key(&self) -> String {
        format!("{}:{}:{}", self.agent, self.source, self.native)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageTime {
    #[default]
    Pending,
    Known(DateTime<Utc>),
    Missing,
    Invalid,
}

impl MessageTime {
    pub fn parse(value: Option<&str>) -> Self {
        match value.filter(|value| !value.trim().is_empty()) {
            None => Self::Missing,
            Some(value) => match DateTime::parse_from_rfc3339(value) {
                Ok(time) => Self::Known(time.with_timezone(&Utc)),
                Err(_) => Self::Invalid,
            },
        }
    }

    pub fn value(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Known(time) => Some(*time),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum TimeMode {
    #[default]
    Latest,
    FirstAssistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub agent: String,
    pub root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub key: SessionKey,
    pub events_path: PathBuf,
    pub size: u64,
    pub modified_ns: i128,
    #[serde(default)]
    pub file_identity: Option<u128>,
    #[serde(default)]
    pub read_mode: ReadMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadMode {
    #[default]
    Append,
    Snapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub candidate: Candidate,
    pub name: String,
    pub workspace: Option<String>,
    pub created: MessageTime,
    pub latest: MessageTime,
    pub first_assistant: MessageTime,
    pub indexed: bool,
    pub available: bool,
    #[serde(default)]
    pub content_error: Option<String>,
}

impl Session {
    pub fn unreadable(candidate: Candidate, reason: String) -> Self {
        Self {
            name: candidate.key.native.chars().take(8).collect(),
            candidate,
            workspace: None,
            created: MessageTime::Missing,
            latest: MessageTime::Missing,
            first_assistant: MessageTime::Missing,
            indexed: false,
            available: false,
            content_error: Some(reason),
        }
    }

    pub fn time(&self, mode: TimeMode) -> Option<DateTime<Utc>> {
        match mode {
            TimeMode::Latest => self.latest.value().or_else(|| {
                (!matches!(self.latest, MessageTime::Pending))
                    .then(|| self.created.value())
                    .flatten()
            }),
            TimeMode::FirstAssistant => self.first_assistant.value(),
        }
    }

    pub fn compare(&self, other: &Self, mode: TimeMode) -> Ordering {
        other
            .time(mode)
            .cmp(&self.time(mode))
            .then_with(|| self.candidate.key.cmp(&other.candidate.key))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Assistant => "Assistant",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub time: MessageTime,
    pub content: String,
    pub attachments: Vec<serde_json::Value>,
    pub offset: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub offset: Option<u64>,
    pub description: String,
}

pub struct Discovery {
    pub candidates: Vec<Candidate>,
    pub diagnostics: Vec<Diagnostic>,
    pub reclassified: Vec<ReclassifiedSession>,
}

#[derive(Clone, Debug)]
pub struct ReclassifiedSession {
    pub previous: SessionKey,
    pub current: SessionKey,
}

#[derive(Debug)]
pub struct ReadBatch {
    pub messages: Vec<Message>,
    pub cursor: u64,
    pub eof: bool,
    pub incomplete_tail: bool,
    pub diagnostics: Vec<Diagnostic>,
}

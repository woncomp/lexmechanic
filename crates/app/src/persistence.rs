use crate::tabs::Tab;
use agent_chat_reader_core::model::TimeMode;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub sources: Vec<PathBuf>,
    pub vscode_sources: Option<Vec<PathBuf>>,
    pub mode: TimeMode,
    pub dark: Option<bool>,
    pub font_size: Option<f32>,
    pub tabs: Vec<Tab>,
    pub active: Option<String>,
    pub positions: HashMap<String, ReadingPosition>,
    pub column_widths: Vec<f32>,
    pub sidebar_width: Option<f32>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadingPosition {
    pub message_id: String,
    pub block: usize,
    pub offset: f32,
    pub collapsed: Vec<String>,
}

pub fn load(path: &Path) -> Result<Settings> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("Cannot parse saved application settings")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(error).context("Cannot read saved application settings"),
    }
}

pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    let directory = path.parent().context("Settings path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), settings)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Cannot save application settings")?;
    Ok(())
}

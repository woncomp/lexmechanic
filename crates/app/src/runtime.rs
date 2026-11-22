use crate::persistence::{self, Settings};
use agent_chat_reader_core::{
    agents::{AgentAdapter, copilot_app::CopilotApp, copilot_cli::CopilotCli, vscode::VsCode},
    model::*,
    scan::{self, ScanEvent, SharedWriter},
    search::{SearchCursor, SearchHit, SearchQuery},
    store::{MessageInfo, Store},
};
use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use std::{
    fs,
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug)]
pub struct DocumentMessage {
    pub info: MessageInfo,
    pub native_id: String,
    pub parts: Vec<Range<usize>>,
    pub attachment_names: Vec<String>,
}

pub enum Event {
    Ready {
        sessions: Vec<Session>,
        settings: Settings,
    },
    Cached(Vec<Session>),
    Scan(ScanEvent),
    Document {
        generation: u64,
        key: String,
        messages: Vec<DocumentMessage>,
        complete: bool,
    },
    DocumentFailed {
        generation: u64,
        message: String,
    },
    Chunk {
        generation: u64,
        rowid: i64,
        part: usize,
        text: String,
    },
    Raw {
        generation: u64,
        parts: Vec<String>,
    },
    Hits {
        generation: u64,
        hits: Vec<SearchHit>,
        scanned: usize,
        complete: bool,
        more: Option<SearchCursor>,
    },
    Copied(String),
    Notice(String),
    Error(String),
}

enum ReadCommand {
    Open {
        generation: u64,
        session: Box<Session>,
        cancel: Cancellation,
    },
    Chunk {
        generation: u64,
        rowid: i64,
        part: usize,
        range: Range<usize>,
    },
    Copy(i64),
    Raw {
        generation: u64,
        rowid: i64,
    },
    Export {
        key: String,
        path: PathBuf,
    },
}

enum ControlCommand {
    Refresh(Vec<PathBuf>, Option<Vec<PathBuf>>),
    Save(Settings),
    Clear,
}

pub struct Runtime {
    pub events: Receiver<Event>,
    pub catalog_events: Receiver<Event>,
    control: SyncSender<ControlCommand>,
    reader: SyncSender<ReadCommand>,
    searcher: SyncSender<(u64, SearchQuery, SearchCursor, Cancellation)>,
    stop: Cancellation,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    settings_lock: Arc<Mutex<()>>,
    settings_path: PathBuf,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Ok(mut workers) = self.workers.lock() {
            let handles = std::mem::take(&mut *workers);
            drop(workers);
            for worker in handles {
                if worker.join().is_err() {
                    tracing::error!("Reader worker panicked during shutdown");
                }
            }
        }
    }
}

impl Runtime {
    pub fn new(directory: PathBuf) -> Self {
        let (sender, events) = mpsc::sync_channel(128);
        let (catalog_sender, catalog_events) = mpsc::sync_channel(64);
        let (control, commands) = mpsc::sync_channel(8);
        let (reader, read_commands) = mpsc::sync_channel(64);
        let (searcher, searches) = mpsc::sync_channel(8);
        let stop = Cancellation::default();
        let worker_stop = stop.clone();
        let workers = Arc::new(Mutex::new(Vec::new()));
        let child_workers = workers.clone();
        let settings_lock = Arc::new(Mutex::new(()));
        let worker_settings_lock = settings_lock.clone();
        let settings_path = directory.join("settings.json");
        let catalog = thread::Builder::new()
            .name("catalog".into())
            .spawn(move || {
                let result = (|| {
                    fs::create_dir_all(&directory)?;
                    let path = directory.join("index.sqlite");
                    let writer = Arc::new(Mutex::new(Store::open(&path)?));
                    let settings_path = directory.join("settings.json");
                    let settings = match persistence::load(&settings_path) {
                        Ok(settings) => settings,
                        Err(error) => {
                            emit(&sender, Event::Error(format!("{error:#}")), &worker_stop)?;
                            Settings::default()
                        }
                    };
                    let sessions = writer
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Index writer failed"))?
                        .sessions(settings.mode)?;
                    let reader = spawn_reader(
                        path.clone(),
                        writer.clone(),
                        read_commands,
                        sender.clone(),
                        worker_stop.clone(),
                    )?;
                    let search = spawn_search(path, searches, sender.clone(), worker_stop.clone())?;
                    child_workers
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Worker registry failed"))?
                        .extend([reader, search]);
                    let mut cached = sessions.into_iter();
                    emit(
                        &sender,
                        Event::Ready {
                            sessions: cached.by_ref().take(128).collect(),
                            settings: settings.clone(),
                        },
                        &worker_stop,
                    )?;
                    loop {
                        let batch: Vec<_> = cached.by_ref().take(128).collect();
                        if batch.is_empty() {
                            break;
                        }
                        emit(&catalog_sender, Event::Cached(batch), &worker_stop)?;
                    }
                    let mut scan = Some(begin_scan(
                        &settings.sources,
                        settings.vscode_sources.as_deref(),
                        writer.clone(),
                        &sender,
                        &worker_stop,
                    )?);
                    let dirty = Arc::new(AtomicBool::new(false));
                    let mut roots = settings.sources.clone();
                    let mut vscode_roots = settings.vscode_sources.clone();
                    let mut watcher = watch_sources(&roots, vscode_roots.as_deref(), dirty.clone(), &sender, &worker_stop);
                    let mut watch_due = None;
                    while !worker_stop.is_cancelled() {
                        while let Ok(command) = commands.try_recv() {
                            match command {
                                ControlCommand::Refresh(sources, vscode) => {
                                    scan.take();
                                    scan = Some(begin_scan(
                                        &sources,
                                        vscode.as_deref(),
                                        writer.clone(),
                                        &sender,
                                        &worker_stop,
                                    )?);
                                    roots = sources;
                                    vscode_roots = vscode;
                                    watcher =
                                        watch_sources(&roots, vscode_roots.as_deref(), dirty.clone(), &sender, &worker_stop);
                                }
                                ControlCommand::Save(settings) => {
                                    let _guard = worker_settings_lock
                                        .lock()
                                        .map_err(|_| anyhow::anyhow!("Settings writer failed"))?;
                                    if worker_stop.is_cancelled() {
                                        continue;
                                    }
                                    if let Err(error) = persistence::save(&settings_path, &settings)
                                    {
                                        emit(
                                            &sender,
                                            Event::Error(format!("{error:#}")),
                                            &worker_stop,
                                        )?;
                                    }
                                }
                                ControlCommand::Clear => {
                                    if let Some(handle) = scan.take() {
                                        handle.cancel.cancel();
                                        while !handle.is_finished() {
                                            while handle.events.try_recv().is_ok() {}
                                            thread::sleep(Duration::from_millis(5));
                                        }
                                    }
                                    writer
                                        .lock()
                                        .map_err(|_| anyhow::anyhow!("Index writer failed"))?
                                        .clear()?;
                                    emit(
                                        &sender,
                                        Event::Notice(
                                            "Local index cleared; rebuilding from read-only sources.".into(),
                                        ),
                                        &worker_stop,
                                    )?;
                                    scan = Some(begin_scan(&roots, vscode_roots.as_deref(), writer.clone(), &sender, &worker_stop)?);
                                }
                            }
                        }
                        if dirty.swap(false, Ordering::Relaxed) {
                            watch_due = Some(Instant::now() + Duration::from_millis(500));
                        }
                        if watch_due.is_some_and(|due| Instant::now() >= due) {
                            watch_due = None;
                            scan.take();
                            scan = Some(begin_scan(&roots, vscode_roots.as_deref(), writer.clone(), &sender, &worker_stop)?);
                        }
                        if let Some(handle) = &scan {
                            for _ in 0..64 {
                                match handle.events.try_recv() {
                                    Ok(event) => {
                                        emit(&catalog_sender, Event::Scan(event), &worker_stop)?
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                        thread::sleep(Duration::from_millis(25));
                    }
                    drop(watcher);
                    Ok::<_, anyhow::Error>(())
                })();
                if let Err(error) = result {
                    let _ = emit(&sender, Event::Error(format!("{error:#}")), &worker_stop);
                }
            })
            .expect("Cannot start catalog worker");
        workers
            .lock()
            .expect("Worker registry failed")
            .push(catalog);
        Self {
            events,
            catalog_events,
            control,
            reader,
            searcher,
            stop,
            workers,
            settings_lock,
            settings_path,
        }
    }

    pub fn refresh(&self, sources: Vec<PathBuf>, vscode: Option<Vec<PathBuf>>) -> Result<()> {
        self.control
            .try_send(ControlCommand::Refresh(sources, vscode))
            .context("Refresh queue is busy")
    }

    pub fn save(&self, settings: Settings) -> Result<()> {
        self.control
            .try_send(ControlCommand::Save(settings))
            .context("Settings queue is busy")
    }

    pub fn shutdown(&self, settings: Settings) -> Result<()> {
        self.stop.cancel();
        let _guard = self
            .settings_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("Settings writer failed"))?;
        persistence::save(&self.settings_path, &settings)
    }

    pub fn clear(&self) -> Result<()> {
        self.control
            .try_send(ControlCommand::Clear)
            .context("Index queue is busy")
    }

    pub fn open(&self, generation: u64, session: Session, cancel: Cancellation) -> Result<()> {
        self.reader
            .try_send(ReadCommand::Open {
                generation,
                session: Box::new(session),
                cancel,
            })
            .context("Document queue is busy")
    }

    pub fn chunk(
        &self,
        generation: u64,
        rowid: i64,
        part: usize,
        range: Range<usize>,
    ) -> Result<()> {
        self.reader
            .try_send(ReadCommand::Chunk {
                generation,
                rowid,
                part,
                range,
            })
            .context("Document queue is busy")
    }

    pub fn search(
        &self,
        generation: u64,
        query: SearchQuery,
        cursor: SearchCursor,
        cancel: Cancellation,
    ) -> Result<()> {
        self.searcher
            .try_send((generation, query, cursor, cancel))
            .context("Search queue is busy")
    }

    pub fn copy(&self, rowid: i64) -> Result<()> {
        self.reader
            .try_send(ReadCommand::Copy(rowid))
            .context("Document queue is busy")
    }

    pub fn raw(&self, generation: u64, rowid: i64) -> Result<()> {
        self.reader
            .try_send(ReadCommand::Raw { generation, rowid })
            .context("Document queue is busy")
    }

    pub fn export(&self, key: String, path: PathBuf) -> Result<()> {
        self.reader
            .try_send(ReadCommand::Export { key, path })
            .context("Document queue is busy")
    }
}

fn emit(sender: &SyncSender<Event>, mut event: Event, stop: &Cancellation) -> Result<()> {
    loop {
        stop.check()?;
        match sender.try_send(event) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Full(returned)) => {
                event = returned;
                thread::sleep(Duration::from_millis(5));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => anyhow::bail!("Window closed"),
        }
    }
}

fn begin_scan(
    sources: &[PathBuf],
    vscode_sources: Option<&[PathBuf]>,
    writer: SharedWriter,
    sender: &SyncSender<Event>,
    stop: &Cancellation,
) -> Result<scan::ScanHandle> {
    let roots = if sources.is_empty() {
        vec![agent_chat_reader_core::agents::copilot_cli::default_home()?]
    } else {
        sources.to_vec()
    };
    let mut adapters: Vec<Arc<dyn AgentAdapter>> = Vec::new();
    for root in roots {
        let mut providers: Vec<anyhow::Result<Arc<dyn AgentAdapter>>> = Vec::new();
        if root.join("session-state").is_dir() {
            providers.push(
                CopilotCli::new(&root).map(|adapter| Arc::new(adapter) as Arc<dyn AgentAdapter>),
            );
        }
        if root.join("data.db").is_file() {
            providers.push(
                CopilotApp::new(&root).map(|adapter| Arc::new(adapter) as Arc<dyn AgentAdapter>),
            );
        }
        if providers.is_empty() {
            emit(
                sender,
                Event::Error(format!(
                    "No Copilot session-state or App data.db found in {}",
                    root.display()
                )),
                stop,
            )?;
        }
        for provider in providers {
            match provider {
                Ok(adapter)
                    if !adapters
                        .iter()
                        .any(|existing| existing.source().id == adapter.source().id) =>
                {
                    adapters.push(adapter)
                }
                Ok(_) => {}
                Err(error) => emit(sender, Event::Error(format!("{error:#}")), stop)?,
            }
        }
    }
    let vscode_roots = match vscode_sources {
        Some(roots) => roots.to_vec(),
        None => vec![agent_chat_reader_core::agents::vscode::default_user_root()?],
    };
    for root in vscode_roots {
        if vscode_sources.is_none()
            && !agent_chat_reader_core::agents::vscode::session_store(&root).is_file()
        {
            continue;
        }
        match VsCode::new(&root) {
            Ok(adapter)
                if !adapters
                    .iter()
                    .any(|existing| existing.source().id == adapter.source().id) =>
            {
                adapters.push(Arc::new(adapter))
            }
            Ok(_) => {}
            Err(error) => emit(sender, Event::Error(format!("{error:#}")), stop)?,
        }
    }
    scan::start_with_writer(adapters, writer)
}

fn watch_sources(
    sources: &[PathBuf],
    vscode_sources: Option<&[PathBuf]>,
    dirty: Arc<AtomicBool>,
    sender: &SyncSender<Event>,
    stop: &Cancellation,
) -> Option<notify::RecommendedWatcher> {
    let result = (|| -> Result<_> {
        let error_sender = sender.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event)
                    if matches!(
                        event.kind,
                        notify::EventKind::Modify(_)
                            | notify::EventKind::Create(_)
                            | notify::EventKind::Remove(_)
                    ) && event.paths.iter().any(|path| {
                        path.file_name().is_some_and(|name| {
                            matches!(
                                name.to_str(),
                                Some(
                                    "workspace.yaml"
                                        | "data.db"
                                        | "data.db-wal"
                                        | "session-store.db"
                                        | "session-store.db-wal"
                                        | "state.vscdb"
                                        | "state.vscdb-wal"
                                )
                            )
                        }) || path
                            .extension()
                            .is_some_and(|extension| extension == "jsonl")
                            || path.is_dir()
                    }) =>
                {
                    dirty.store(true, Ordering::Relaxed);
                }
                Err(error)
                    if error_sender
                        .try_send(Event::Error(format!("File watcher failed: {error}")))
                        .is_err() =>
                {
                    tracing::error!("File watcher error could not be delivered to the window");
                }
                _ => {}
            })?;
        let mut roots = if sources.is_empty() {
            vec![agent_chat_reader_core::agents::copilot_cli::default_home()?]
        } else {
            sources.to_vec()
        };
        roots.extend(match vscode_sources {
            Some(roots) => roots.to_vec(),
            None => vec![agent_chat_reader_core::agents::vscode::default_user_root()?],
        });
        for root in roots {
            if root.is_dir()
                && let Err(error) = watcher.watch(&root, RecursiveMode::Recursive)
            {
                emit(
                    sender,
                    Event::Error(format!(
                        "Live updates unavailable for {}: {error}",
                        root.display()
                    )),
                    stop,
                )?;
            }
        }
        Ok(watcher)
    })();
    match result {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            let _ = emit(
                sender,
                Event::Error(format!("Live updates unavailable; use Refresh: {error:#}")),
                stop,
            );
            None
        }
    }
}

fn spawn_reader(
    database: PathBuf,
    writer: SharedWriter,
    commands: Receiver<ReadCommand>,
    sender: SyncSender<Event>,
    stop: Cancellation,
) -> Result<JoinHandle<()>> {
    let handle = thread::Builder::new()
        .name("document-reader".into())
        .spawn(move || {
            let work = || -> Result<()> {
                let store = Store::open(&database)?;
                let mut continuation: Option<(u64, Session, Cancellation)> = None;
                while !stop.is_cancelled() {
                    let command = match commands.recv_timeout(Duration::from_millis(15)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if let Some((generation, session, cancel)) = continuation.take() {
                                ReadCommand::Open {
                                    generation,
                                    session: Box::new(session),
                                    cancel,
                                }
                            } else {
                                continue;
                            }
                        }
                        Err(_) => break,
                    };
                    let result = match command {
                        ReadCommand::Open {
                            generation,
                            session,
                            cancel,
                        } => {
                            if cancel.is_cancelled() {
                                continue;
                            }
                            match open_document(
                                *session, generation, &cancel, &store, &writer, &sender, &stop,
                            ) {
                                Ok(Some(session)) => {
                                    continuation = Some((generation, session, cancel));
                                    Ok(())
                                }
                                Ok(None) => Ok(()),
                                Err(_) if cancel.is_cancelled() => Ok(()),
                                Err(error) => emit(
                                    &sender,
                                    Event::DocumentFailed {
                                        generation,
                                        message: format!("{error:#}"),
                                    },
                                    &stop,
                                ),
                            }
                        }
                        ReadCommand::Chunk {
                            generation,
                            rowid,
                            part,
                            range,
                        } => store.chunk(rowid, range.start, range.end).and_then(|text| {
                            emit(
                                &sender,
                                Event::Chunk {
                                    generation,
                                    rowid,
                                    part,
                                    text,
                                },
                                &stop,
                            )
                        }),
                        ReadCommand::Copy(rowid) => store.message(rowid).and_then(|message| {
                            emit(&sender, Event::Copied(message.content), &stop)
                        }),
                        ReadCommand::Raw { generation, rowid } => {
                            store.original_message(rowid).and_then(|text| {
                                let parts = chunk_ranges(&text)
                                    .into_iter()
                                    .map(|range| text[range].to_owned())
                                    .collect();
                                emit(&sender, Event::Raw { generation, parts }, &stop)
                            })
                        }
                        ReadCommand::Export { key, path } => export_document(&store, &key, &path)
                            .and_then(|()| {
                                emit(
                                    &sender,
                                    Event::Notice(format!("Exported {}", path.display())),
                                    &stop,
                                )
                            }),
                    };
                    if let Err(error) = result {
                        emit(&sender, Event::Error(format!("{error:#}")), &stop)?;
                    }
                }
                Ok(())
            };
            if let Err(error) = work() {
                let _ = emit(&sender, Event::Error(format!("{error:#}")), &stop);
            }
        })?;
    Ok(handle)
}

fn open_document(
    mut session: Session,
    generation: u64,
    cancel: &Cancellation,
    store: &Store,
    writer: &SharedWriter,
    sender: &SyncSender<Event>,
    stop: &Cancellation,
) -> Result<Option<Session>> {
    let key = session.candidate.key.storage_key();
    if let Some(current) = store.session(&key)? {
        session = current;
    } else {
        session.indexed = false;
        writer
            .lock()
            .map_err(|_| anyhow::anyhow!("Index writer failed"))?
            .upsert_session(&session)?;
    }
    let mut continuation = false;
    anyhow::ensure!(
        session.available || session.indexed,
        "No local transcript is available for this session; its metadata is listed for reference"
    );
    if !session.indexed {
        let adapter = agent_chat_reader_core::agents::adapter_for(&session.candidate)?;
        let cursor = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("Index writer failed"))?
            .resume_cursor(&session)?;
        cancel.check()?;
        let batch = adapter.read_batch(&session.candidate, cursor, 256 * 1024, cancel)?;
        let mut writer = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("Index writer failed"))?;
        cancel.check()?;
        writer.commit_batch(&mut session, &batch)?;
        drop(writer);
        continuation = !batch.eof;
    }
    let infos = store.message_infos(&key)?;
    let mut messages = Vec::with_capacity(infos.len());
    for info in infos {
        cancel.check()?;
        let parts = if info.bytes <= 8192 {
            std::iter::once(0..info.bytes).collect()
        } else {
            chunk_ranges(&store.message(info.rowid)?.content)
        };
        let native_id = info.native_id.clone();
        let attachment_names = info.attachment_names.clone();
        messages.push(DocumentMessage {
            info,
            native_id,
            parts,
            attachment_names,
        });
    }
    cancel.check()?;
    emit(
        sender,
        Event::Document {
            generation,
            key,
            messages,
            complete: session.indexed,
        },
        stop,
    )?;
    Ok(continuation.then_some(session))
}

pub fn chunk_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        let end = offset + grapheme.len();
        if end - start > 8192 && offset > start {
            ranges.push(start..offset);
            start = offset;
        }
        if grapheme.ends_with('\n') && end - start >= 4096 {
            ranges.push(start..end);
            start = end;
        }
    }
    if start < text.len() {
        ranges.push(start..text.len());
    }
    if ranges.is_empty() {
        ranges.push(0..0);
    }
    ranges
}

fn spawn_search(
    database: PathBuf,
    commands: Receiver<(u64, SearchQuery, SearchCursor, Cancellation)>,
    sender: SyncSender<Event>,
    stop: Cancellation,
) -> Result<JoinHandle<()>> {
    let handle = thread::Builder::new()
        .name("literal-search".into())
        .spawn(move || {
            let work = || -> Result<()> {
                let store = Store::open(&database)?;
                while !stop.is_cancelled() {
                    let (generation, query, mut cursor, cancel) =
                        match commands.recv_timeout(Duration::from_millis(100)) {
                            Ok(command) => command,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(_) => break,
                        };
                    let mut found = 0;
                    while !stop.is_cancelled() && !cancel.is_cancelled() {
                        match store.search_page(&query, cursor, &cancel) {
                            Ok(page) => {
                                found += page.hits.len();
                                let pause = found >= 512;
                                emit(
                                    &sender,
                                    Event::Hits {
                                        generation,
                                        hits: page.hits,
                                        scanned: page.scanned,
                                        complete: page.next.is_none(),
                                        more: if pause { page.next.clone() } else { None },
                                    },
                                    &stop,
                                )?;
                                if pause {
                                    break;
                                }
                                match page.next {
                                    Some(next) => cursor = next,
                                    None => break,
                                }
                            }
                            Err(error) => {
                                if !cancel.is_cancelled() {
                                    emit(&sender, Event::Error(format!("{error:#}")), &stop)?;
                                }
                                break;
                            }
                        }
                    }
                }
                Ok(())
            };
            if let Err(error) = work() {
                let _ = emit(&sender, Event::Error(format!("{error:#}")), &stop);
            }
        })?;
    Ok(handle)
}

fn export_document(store: &Store, key: &str, path: &Path) -> Result<()> {
    let session = store
        .session(key)?
        .context("Session is no longer indexed")?;
    anyhow::ensure!(
        session.indexed,
        "Wait until this session is completely indexed before export"
    );
    let parent = path.parent().context("Export path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(
        temporary,
        "# {}\n\nMain conversation only; system, tool and sub-agent events excluded.\n",
        session.name.replace(['\r', '\n'], " ")
    )?;
    for info in store.message_infos(key)? {
        let message = store.message(info.rowid)?;
        writeln!(
            temporary,
            "## {}. {} — {}\n\n{}\n",
            info.ordinal + 1,
            info.role.label(),
            info.time
                .value()
                .map(|time| time.to_rfc3339())
                .unwrap_or_else(|| "Unknown time".into()),
            message.content
        )?;
        for _ in &message.attachments {
            writeln!(
                temporary,
                "> Attachment omitted (original file not included).\n"
            )?;
        }
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Cannot write Markdown export")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;

    #[test]
    fn huge_single_paragraph_chunks_preserve_every_unicode_byte() {
        let text = "中e\u{301}🙂".repeat(300_000);
        let ranges = chunk_ranges(&text);
        assert!(ranges.len() > 100);
        assert!(ranges.iter().all(|range| range.len() <= 8192));
        let joined: String = ranges.into_iter().map(|range| &text[range]).collect();
        assert_eq!(text, joined);
    }

    fn next_matching(runtime: &Runtime, predicate: impl Fn(&Event) -> bool) -> Event {
        let started = Instant::now();
        loop {
            if let Ok(event) = runtime
                .events
                .try_recv()
                .or_else(|_| runtime.catalog_events.try_recv())
            {
                match &event {
                    Event::Error(error) | Event::DocumentFailed { message: error, .. } => {
                        panic!("{error}")
                    }
                    Event::Scan(ScanEvent::Diagnostic(error)) => panic!("{}", error.description),
                    _ => {}
                }
                if predicate(&event) {
                    return event;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(12),
                "Runtime event timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn runtime_reads_exports_and_watches_append_in_isolated_local_data() {
        let source = tempfile::tempdir().unwrap();
        let directory = source
            .path()
            .join("session-state")
            .join("00000000-0000-4000-8000-000000000001");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("workspace.yaml"),
            "client_name: github/cli\nname: Synthetic runtime test\n",
        )
        .unwrap();
        let events = directory.join("events.jsonl");
        let raw = "<system_reminder>internal</system_reminder>\nHello 中文";
        fs::write(
            &events,
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"session.start","timestamp":"2026-01-01T00:00:00Z",
                "data":{"context":{"cwd":"D:\\synthetic-private-workspace"}}}),
                serde_json::json!({"type":"user.message","timestamp":"2026-01-01T01:00:00Z",
                "data":{"content":raw}})
            ),
        )
        .unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = Settings {
            sources: vec![source.path().to_owned()],
            vscode_sources: Some(Vec::new()),
            ..Default::default()
        };
        persistence::save(&data.path().join("settings.json"), &settings).unwrap();
        let runtime = Runtime::new(data.path().to_owned());
        let event = next_matching(&runtime, |event| {
            matches!(event,
            Event::Scan(ScanEvent::Session(session)) if session.indexed)
        });
        let Event::Scan(ScanEvent::Session(session)) = event else {
            unreachable!()
        };
        let old_size = session.candidate.size;
        runtime
            .open(1, (*session).clone(), Cancellation::default())
            .unwrap();
        let event = next_matching(&runtime, |event| {
            matches!(event, Event::Document { generation: 1, .. })
        });
        let Event::Document {
            messages, complete, ..
        } = event
        else {
            unreachable!()
        };
        assert!(complete);
        assert_eq!(messages.len(), 1);
        let rowid = messages[0].info.rowid;
        runtime.copy(rowid).unwrap();
        let event = next_matching(&runtime, |event| matches!(event, Event::Copied(_)));
        assert!(matches!(event, Event::Copied(text) if text == "Hello 中文"));
        runtime.raw(1, rowid).unwrap();
        let event = next_matching(&runtime, |event| {
            matches!(event, Event::Raw { generation: 1, .. })
        });
        assert!(matches!(event, Event::Raw { parts, .. } if parts.concat() == raw));
        let output = data.path().join("export.md");
        runtime
            .export(session.candidate.key.storage_key(), output.clone())
            .unwrap();
        next_matching(
            &runtime,
            |event| matches!(event, Event::Notice(text) if text.starts_with("Exported")),
        );
        let exported = fs::read_to_string(output).unwrap();
        assert!(exported.contains("Hello 中文"));
        assert!(!exported.contains("synthetic-private-workspace"));
        let mut file = fs::OpenOptions::new().append(true).open(&events).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"assistant.message",
            "timestamp":"2026-01-02T00:00:00Z","data":{"content":"Appended response"}})
        )
        .unwrap();
        drop(file);
        let event = next_matching(&runtime, |event| {
            matches!(event,
            Event::Scan(ScanEvent::Session(session)) if session.indexed && session.candidate.size > old_size)
        });
        let Event::Scan(ScanEvent::Session(updated)) = event else {
            unreachable!()
        };
        runtime
            .open(2, (*updated).clone(), Cancellation::default())
            .unwrap();
        let event = next_matching(&runtime, |event| {
            matches!(event, Event::Document { generation: 2, .. })
        });
        assert!(
            matches!(event, Event::Document { messages, complete: true, .. } if messages.len() == 2)
        );
        Store::open(&data.path().join("index.sqlite"))
            .unwrap()
            .clear()
            .unwrap();
        runtime.open(3, *updated, Cancellation::default()).unwrap();
        let event = next_matching(&runtime, |event| {
            matches!(event, Event::Document { generation: 3, .. })
        });
        assert!(
            matches!(event, Event::Document { messages, complete: true, .. } if messages.len() == 2)
        );
        fs::remove_file(&events).unwrap();
        let event = next_matching(&runtime, |event| {
            matches!(event,
            Event::Scan(ScanEvent::Session(session)) if !session.available)
        });
        let Event::Scan(ScanEvent::Session(cached)) = event else {
            unreachable!()
        };
        runtime.open(4, *cached, Cancellation::default()).unwrap();
        let event = next_matching(&runtime, |event| {
            matches!(event, Event::Document { generation: 4, .. })
        });
        assert!(
            matches!(event, Event::Document { messages, complete: true, .. } if messages.len() == 2)
        );
        runtime.shutdown(settings).unwrap();
        drop(runtime);
        data.close().unwrap();
        source.close().unwrap();
    }

    #[test]
    fn runtime_opens_both_desktop_providers_and_refreshes_database_titles_and_snapshot_replies() {
        let files = tempfile::tempdir().unwrap();
        let copilot = files.path().join("copilot");
        let native = "00000000-0000-4000-8000-000000000022";
        let events = copilot
            .join("session-state")
            .join(native)
            .join("events.jsonl");
        fs::create_dir_all(events.parent().unwrap()).unwrap();
        fs::write(&events, format!("{}\n{}\n",
            serde_json::json!({"type":"session.start","timestamp":"2026-01-01T00:00:00Z","data":{}}),
            serde_json::json!({"type":"user.message","timestamp":"2026-01-02T00:00:00Z",
                "data":{"content":"App question 中文"}}))).unwrap();
        fs::write(
            events.with_file_name("workspace.yaml"),
            "client_name: github/cli\nname: Wrong CLI title\n",
        )
        .unwrap();
        let app_db = copilot.join("data.db");
        let db = rusqlite_for_tests(&app_db);
        db.execute_batch(
            "CREATE TABLE sessions(id TEXT,title TEXT);
            CREATE TABLE workspaces(session_id TEXT,name TEXT);",
        )
        .unwrap();
        db.execute("INSERT INTO sessions VALUES(?1,'App title')", [native])
            .unwrap();
        drop(db);

        let code = files.path().join("Code").join("User");
        let code_id = "00000000-0000-4000-8000-000000000033";
        let db_path = agent_chat_reader_core::agents::vscode::session_store(&code);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = rusqlite_for_tests(&db_path);
        db.execute_batch("CREATE TABLE sessions(id TEXT,cwd TEXT,host_type TEXT,agent_name TEXT);")
            .unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,'D:\\code','vscode','GitHub Copilot Chat')",
            [code_id],
        )
        .unwrap();
        drop(db);
        let transcript = code
            .join("workspaceStorage")
            .join("one")
            .join("chatSessions")
            .join(format!("{code_id}.jsonl"));
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, format!("{}\n", serde_json::json!({"kind":0,"v":{
            "customTitle":"VS Code title","requests":[{"requestId":"one","timestamp":1767312000000_i64,
                "responseTimestamp":1767398400000_i64,"message":{"text":"Code question 中文"},
                "response":[{"value":"Code response"}]}]}}))).unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = Settings {
            sources: vec![copilot],
            vscode_sources: Some(vec![code]),
            ..Default::default()
        };
        persistence::save(&data.path().join("settings.json"), &settings).unwrap();
        let runtime = Runtime::new(data.path().to_owned());
        let mut providers = HashMap::new();
        while providers.len() < 2 {
            let event = next_matching(&runtime, |event| {
                matches!(event,
                Event::Scan(ScanEvent::Session(session)) if session.indexed)
            });
            let Event::Scan(ScanEvent::Session(session)) = event else {
                unreachable!()
            };
            providers.insert(session.candidate.key.agent.clone(), *session);
        }
        assert!(providers.contains_key("copilot-app"));
        assert!(providers.contains_key("vscode"));
        for (generation, agent) in [(1, "copilot-app"), (2, "vscode")] {
            let session = providers[agent].clone();
            runtime
                .open(generation, session.clone(), Cancellation::default())
                .unwrap();
            let event = next_matching(&runtime, |event| {
                matches!(event,
                Event::Document { generation: actual, complete: true, .. } if *actual == generation)
            });
            let Event::Document { messages, .. } = event else {
                unreachable!()
            };
            assert!(!messages.is_empty());
            let export = data.path().join(format!("{agent}.md"));
            runtime
                .export(session.candidate.key.storage_key(), export.clone())
                .unwrap();
            next_matching(
                &runtime,
                |event| matches!(event, Event::Notice(message) if message.starts_with("Exported")),
            );
            assert!(fs::read_to_string(export).unwrap().contains("中文"));
        }
        let db = rusqlite_for_tests(&app_db);
        db.execute(
            "UPDATE sessions SET title='Renamed App title' WHERE id=?1",
            [native],
        )
        .unwrap();
        drop(db);
        next_matching(&runtime, |event| {
            matches!(event,
            Event::Scan(ScanEvent::Session(session)) if session.indexed && session.name == "Renamed App title")
        });

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"kind":1,"k":["requests",0,"response",0,"value"],
            "v":"Updated Code response 中文"})
        )
        .unwrap();
        drop(file);
        let old_size = providers["vscode"].candidate.size;
        let event = next_matching(&runtime, |event| {
            matches!(event,
            Event::Scan(ScanEvent::Session(session)) if session.indexed
                && session.candidate.key.agent=="vscode" && session.candidate.size > old_size)
        });
        let Event::Scan(ScanEvent::Session(updated)) = event else {
            unreachable!()
        };
        runtime.open(3, *updated, Cancellation::default()).unwrap();
        let event = next_matching(&runtime, |event| {
            matches!(
                event,
                Event::Document {
                    generation: 3,
                    complete: true,
                    ..
                }
            )
        });
        let Event::Document { messages, .. } = event else {
            unreachable!()
        };
        assert_eq!(messages.len(), 2);
        runtime.copy(messages[1].info.rowid).unwrap();
        assert!(
            matches!(next_matching(&runtime,|event| matches!(event,Event::Copied(_))),
            Event::Copied(text) if text=="Updated Code response 中文")
        );
        runtime.shutdown(settings).unwrap();
        drop(runtime);
        data.close().unwrap();
        files.close().unwrap();
    }

    fn rusqlite_for_tests(path: &Path) -> rusqlite::Connection {
        rusqlite::Connection::open(path).unwrap()
    }
}

use crate::persistence::{self, Settings};
use agent_chat_reader_core::{
    agents::{
        AgentAdapter,
        codex::Codex,
        copilot_app::CopilotApp,
        copilot_cli::CopilotCli,
        vscode::VsCode,
        vscode_agents::{self, Driver, Homes, VsCodeAgents},
    },
    model::*,
    scan::{self, ScanEvent, SharedWriter},
    search::{HitKind, SearchCursor, SearchHit, SearchQuery},
    snapshot::{DocumentSnapshot, SearchSnapshot},
    store::{MessageInfo, Store},
};
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    time::Duration,
};
use tempfile::{TempDir, TempPath};
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
    CatalogStarted {
        run_id: u64,
    },
    Scan {
        run_id: u64,
        event: ScanEvent,
    },
    CatalogFinished {
        run_id: u64,
    },
    SearchFailed {
        generation: u64,
        message: String,
    },
    /// Complete sessions found by a global search, added to the results already shown.
    SearchHits {
        generation: u64,
        hits: Vec<SearchHit>,
    },
    SearchDone {
        generation: u64,
        /// The search stopped at the hit limit rather than at the end of the index.
        capped: bool,
    },
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
        complete: bool,
        more: Option<SearchCursor>,
    },
    OpenHit {
        generation: u64,
        hit: SearchHit,
        permanent: bool,
    },
    Copied(String),
    Notice(String),
    Error(String),
}

enum ReadCommand {
    Open {
        generation: u64,
        session: Box<Session>,
        reload: bool,
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
    Find {
        generation: u64,
        query: SearchQuery,
        cancel: Cancellation,
    },
    ValidateHit {
        generation: u64,
        hit: SearchHit,
        permanent: bool,
    },
    Retain(Vec<String>),
}

struct SearchRequest {
    generation: u64,
    query: SearchQuery,
    cancel: Cancellation,
}

enum SearchCommand {
    Global(SearchRequest),
    Document {
        request: SearchRequest,
        file: Arc<TempPath>,
    },
    Page {
        generation: u64,
        offset: usize,
    },
}

enum ControlCommand {
    Refresh(Vec<PathBuf>, Option<Vec<PathBuf>>, Option<Vec<PathBuf>>),
    Save(Settings),
    Clear,
}

pub struct Runtime {
    pub events: Receiver<Event>,
    pub catalog_events: Receiver<Event>,
    control: SyncSender<ControlCommand>,
    reader: SyncSender<ReadCommand>,
    searcher: SyncSender<SearchCommand>,
    stop: Cancellation,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    settings_lock: Arc<Mutex<()>>,
    settings_path: PathBuf,
    search_cancel: Mutex<Cancellation>,
    document_cancel: Mutex<Cancellation>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Ok(cancel) = self.search_cancel.lock() {
            cancel.cancel();
        }
        if let Ok(cancel) = self.document_cancel.lock() {
            cancel.cancel();
        }
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
        let search_sender = searcher.clone();
        // Set while a catalog run is scanning and indexing; a global search finishes after it.
        let indexing = Arc::new(AtomicBool::new(true));
        let search_indexing = indexing.clone();
        let catalog = thread::Builder::new()
            .name("catalog".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    fs::create_dir_all(&directory)?;
                    let snapshots = Arc::new(
                        tempfile::Builder::new()
                            .prefix("snapshots-")
                            .tempdir_in(&directory)?,
                    );
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
                        search_sender.clone(),
                        sender.clone(),
                        snapshots.clone(),
                        worker_stop.clone(),
                    )?;
                    let search = spawn_search(
                        path,
                        searches,
                        sender.clone(),
                        snapshots,
                        search_indexing,
                        worker_stop.clone(),
                    )?;
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
                    let mut roots = settings.sources;
                    let mut vscode_roots = settings.vscode_sources;
                    let mut codex_roots = settings.codex_sources;
                    let mut run_id = 1;
                    emit(&sender, Event::CatalogStarted { run_id }, &worker_stop)?;
                    let mut scan = Some(begin_scan(
                        &roots,
                        vscode_roots.as_deref(),
                        codex_roots.as_deref(),
                        writer.clone(),
                        &sender,
                        &worker_stop,
                    )?);
                    while !worker_stop.is_cancelled() {
                        while let Ok(command) = commands.try_recv() {
                            match command {
                                ControlCommand::Refresh(sources, vscode, codex) => {
                                    scan.take();
                                    indexing.store(true, Ordering::SeqCst);
                                    roots = sources;
                                    vscode_roots = vscode;
                                    codex_roots = codex;
                                    run_id += 1;
                                    emit(&sender, Event::CatalogStarted { run_id }, &worker_stop)?;
                                    scan = Some(begin_scan(
                                        &roots,
                                        vscode_roots.as_deref(),
                                        codex_roots.as_deref(),
                                        writer.clone(),
                                        &sender,
                                        &worker_stop,
                                    )?);
                                }
                                ControlCommand::Save(settings) => {
                                    let _guard = worker_settings_lock
                                        .lock()
                                        .map_err(|_| anyhow::anyhow!("Settings writer failed"))?;
                                    if !worker_stop.is_cancelled()
                                        && let Err(error) =
                                            persistence::save(&settings_path, &settings)
                                    {
                                        emit(
                                            &sender,
                                            Event::Error(format!("{error:#}")),
                                            &worker_stop,
                                        )?;
                                    }
                                }
                                ControlCommand::Clear => {
                                    scan.take();
                                    writer
                                        .lock()
                                        .map_err(|_| anyhow::anyhow!("Index writer failed"))?
                                        .clear()?;
                                    indexing.store(true, Ordering::SeqCst);
                                    run_id += 1;
                                    emit(&sender, Event::CatalogStarted { run_id }, &worker_stop)?;
                                    scan = Some(begin_scan(
                                        &roots,
                                        vscode_roots.as_deref(),
                                        codex_roots.as_deref(),
                                        writer.clone(),
                                        &sender,
                                        &worker_stop,
                                    )?);
                                }
                            }
                        }
                        let mut done = false;
                        if let Some(handle) = &scan {
                            let mut drained = false;
                            for _ in 0..64 {
                                match handle.events.try_recv() {
                                    Ok(event) => {
                                        emit(
                                            &catalog_sender,
                                            Event::Scan { run_id, event },
                                            &worker_stop,
                                        )?;
                                    }
                                    Err(_) => {
                                        drained = true;
                                        break;
                                    }
                                }
                            }
                            done = drained && handle.is_finished();
                        }
                        if done {
                            scan.take();
                            indexing.store(false, Ordering::SeqCst);
                            // This shares the FIFO with the last progress events.
                            emit(
                                &catalog_sender,
                                Event::CatalogFinished { run_id },
                                &worker_stop,
                            )?;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    Ok(())
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
            search_cancel: Mutex::new(Cancellation::default()),
            document_cancel: Mutex::new(Cancellation::default()),
        }
    }

    pub fn refresh(
        &self,
        sources: Vec<PathBuf>,
        vscode: Option<Vec<PathBuf>>,
        codex: Option<Vec<PathBuf>>,
    ) -> Result<()> {
        self.control
            .try_send(ControlCommand::Refresh(sources, vscode, codex))
            .context("Refresh queue is busy")
    }
    pub fn save(&self, settings: Settings) -> Result<()> {
        self.control
            .try_send(ControlCommand::Save(settings))
            .context("Settings queue is busy")
    }
    pub fn shutdown(&self, settings: Settings) -> Result<()> {
        self.stop.cancel();
        self.search_cancel
            .lock()
            .map_err(|_| anyhow::anyhow!("Search cancellation failed"))?
            .cancel();
        self.document_cancel
            .lock()
            .map_err(|_| anyhow::anyhow!("Document cancellation failed"))?
            .cancel();
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
        self.set_document_cancel(&cancel)?;
        self.reader
            .try_send(ReadCommand::Open {
                generation,
                session: Box::new(session),
                reload: false,
                cancel,
            })
            .context("Document queue is busy")
    }
    pub fn reload(&self, generation: u64, session: Session, cancel: Cancellation) -> Result<()> {
        self.set_document_cancel(&cancel)?;
        self.reader
            .try_send(ReadCommand::Open {
                generation,
                session: Box::new(session),
                reload: true,
                cancel,
            })
            .context("Document queue is busy")
    }
    pub fn retain_documents(&self, keys: Vec<String>) -> Result<()> {
        self.reader
            .try_send(ReadCommand::Retain(keys))
            .context("Document queue is busy")
    }
    fn set_document_cancel(&self, cancel: &Cancellation) -> Result<()> {
        let mut previous = self
            .document_cancel
            .lock()
            .map_err(|_| anyhow::anyhow!("Document cancellation failed"))?;
        previous.cancel();
        *previous = cancel.clone();
        Ok(())
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
    pub fn validate_hit(&self, generation: u64, hit: SearchHit, permanent: bool) -> Result<()> {
        self.reader
            .try_send(ReadCommand::ValidateHit {
                generation,
                hit,
                permanent,
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
        if cursor.offset > 0 {
            self.searcher
                .try_send(SearchCommand::Page {
                    generation,
                    offset: cursor.offset,
                })
                .context("Search queue is busy")
        } else {
            let mut previous = self
                .search_cancel
                .lock()
                .map_err(|_| anyhow::anyhow!("Search cancellation failed"))?;
            previous.cancel();
            *previous = cancel.clone();
            drop(previous);
            if query.session.is_some() {
                self.reader
                    .try_send(ReadCommand::Find {
                        generation,
                        query,
                        cancel,
                    })
                    .context("Document queue is busy")
            } else {
                self.searcher
                    .try_send(SearchCommand::Global(SearchRequest {
                        generation,
                        query,
                        cancel,
                    }))
                    .context("Search queue is busy")
            }
        }
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
    codex_sources: Option<&[PathBuf]>,
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
        let mut providers: Vec<Result<Arc<dyn AgentAdapter>>> = Vec::new();
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
        None => agent_chat_reader_core::agents::vscode::default_user_roots()?,
    };
    let mut vscode_adapters: Vec<Result<Arc<dyn AgentAdapter>>> = Vec::new();
    let mut agent_homes = None;
    for root in vscode_roots {
        if vscode_sources.is_some()
            || agent_chat_reader_core::agents::vscode::session_store(&root).is_file()
        {
            vscode_adapters
                .push(VsCode::new(&root).map(|adapter| Arc::new(adapter) as Arc<dyn AgentAdapter>));
        }
        // The Agents window keeps its own catalog; the drivers' transcripts live elsewhere.
        if vscode_agents::catalog_path(&root).is_file() {
            match agent_homes.get_or_insert_with(Homes::detect) {
                Ok(homes) => {
                    for driver in Driver::ALL {
                        vscode_adapters.push(
                            VsCodeAgents::new(&root, driver, homes.clone())
                                .map(|adapter| Arc::new(adapter) as Arc<dyn AgentAdapter>),
                        );
                    }
                }
                Err(error) => emit(sender, Event::Error(format!("{error:#}")), stop)?,
            }
        }
    }
    // Codex keeps its own home; without an explicit choice only an existing default is scanned.
    let codex_roots = match codex_sources {
        Some(roots) => roots.to_vec(),
        None => vec![agent_chat_reader_core::agents::codex::default_home()?],
    };
    for root in codex_roots {
        if codex_sources.is_some()
            || root.join("sessions").is_dir()
            || root.join("archived_sessions").is_dir()
        {
            vscode_adapters
                .push(Codex::new(&root).map(|adapter| Arc::new(adapter) as Arc<dyn AgentAdapter>));
        }
    }
    for adapter in vscode_adapters {
        match adapter {
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
    scan::start_with_writer(adapters, writer)
}

fn spawn_search(
    database: PathBuf,
    commands: Receiver<SearchCommand>,
    sender: SyncSender<Event>,
    directory: Arc<TempDir>,
    indexing: Arc<AtomicBool>,
    stop: Cancellation,
) -> Result<JoinHandle<()>> {
    Ok(thread::Builder::new()
        .name("literal-search".into())
        .spawn(move || {
            let work = || -> Result<()> {
                let store = Store::open(&database)?;
                let mut active: Option<(u64, SearchSnapshot, Cancellation)> = None;
                let mut live: Option<LiveSearch> = None;
                while !stop.is_cancelled() {
                    if active
                        .as_ref()
                        .is_some_and(|(_, _, cancel)| cancel.is_cancelled())
                    {
                        active = None;
                    }
                    if live
                        .as_ref()
                        .is_some_and(|search| search.request.cancel.is_cancelled())
                    {
                        live = None;
                    }
                    // Commands are checked between steps, so a newer query interrupts promptly.
                    let wait = match &live {
                        Some(search) if search.has_work() => Duration::ZERO,
                        Some(_) => Duration::from_millis(300),
                        None => Duration::from_millis(50),
                    };
                    let command = match commands.recv_timeout(wait) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if let Some(search) = &mut live
                                && search.advance(&store, &indexing, &sender, &stop)?
                            {
                                live = None;
                            }
                            continue;
                        }
                        Err(_) => break,
                    };
                    let (generation, result, cancel) = match command {
                        SearchCommand::Global(request) => {
                            if !request.cancel.is_cancelled() {
                                active = None;
                                live = Some(LiveSearch::new(request));
                            }
                            continue;
                        }
                        SearchCommand::Document { request, file } => {
                            if request.cancel.is_cancelled() {
                                continue;
                            }
                            active = None;
                            let result = Store::open(file.as_ref()).and_then(|store| {
                                store.with_read_snapshot(|source| {
                                    SearchSnapshot::capture(
                                        source,
                                        &request.query,
                                        directory.path(),
                                        &request.cancel,
                                    )
                                })
                            });
                            if request.cancel.is_cancelled() {
                                continue;
                            }
                            (request.generation, result, request.cancel)
                        }
                        SearchCommand::Page { generation, offset } => {
                            if let Some((current, snapshot, _)) = &active
                                && *current == generation
                                && let Err(error) =
                                    send_page(&sender, generation, snapshot, offset, &stop)
                            {
                                emit(
                                    &sender,
                                    Event::SearchFailed {
                                        generation,
                                        message: format!("{error:#}"),
                                    },
                                    &stop,
                                )?;
                            }
                            continue;
                        }
                    };
                    match result {
                        Ok(snapshot) => {
                            send_page(&sender, generation, &snapshot, 0, &stop)?;
                            active = Some((generation, snapshot, cancel));
                        }
                        Err(error) => emit(
                            &sender,
                            Event::SearchFailed {
                                generation,
                                message: format!("{error:#}"),
                            },
                            &stop,
                        )?,
                    }
                }
                Ok(())
            };
            if let Err(error) = work() {
                let _ = emit(&sender, Event::Error(format!("{error:#}")), &stop);
            }
        })?)
}

/// Most hits a global search reports. It stops between sessions, so a session crossing the
/// limit is still reported whole; a single session reports at most this many on its own.
pub(crate) const SEARCH_HIT_LIMIT: usize = 5_000;
/// Sessions searched in one step, between checks for newer commands.
const SEARCH_CHUNK: usize = 64;

/// A global search that follows the index. Each session is searched once, as soon as it is
/// completely indexed; the search ends when the catalog run is over and every indexed session
/// has been searched.
struct LiveSearch {
    request: SearchRequest,
    searched: HashSet<String>,
    pending: VecDeque<String>,
    /// The index version when the pending sessions were listed.
    version: Option<i64>,
    hits: usize,
}

impl LiveSearch {
    fn new(request: SearchRequest) -> Self {
        Self {
            request,
            searched: HashSet::new(),
            pending: VecDeque::new(),
            version: None,
            hits: 0,
        }
    }

    fn has_work(&self) -> bool {
        self.version.is_none() || !self.pending.is_empty()
    }

    /// Runs one step and returns whether the search has ended.
    fn advance(
        &mut self,
        store: &Store,
        indexing: &AtomicBool,
        sender: &SyncSender<Event>,
        stop: &Cancellation,
    ) -> Result<bool> {
        let generation = self.request.generation;
        let event = match self.step(store, indexing, sender, stop) {
            Ok(None) => return Ok(false),
            Ok(Some(capped)) => Event::SearchDone { generation, capped },
            Err(_) if self.request.cancel.is_cancelled() => return Ok(true),
            Err(error) => {
                stop.check()?;
                Event::SearchFailed {
                    generation,
                    message: format!("{error:#}"),
                }
            }
        };
        emit(sender, event, stop)?;
        Ok(true)
    }

    /// Searches the next sessions. Returns `Some(capped)` once the search is complete.
    fn step(
        &mut self,
        store: &Store,
        indexing: &AtomicBool,
        sender: &SyncSender<Event>,
        stop: &Cancellation,
    ) -> Result<Option<bool>> {
        if self.pending.is_empty() {
            // Read before listing: a run that ends after the listing is caught on the next step.
            let running = indexing.load(Ordering::SeqCst);
            let version = store.data_version()?;
            if self.version != Some(version) {
                self.version = Some(version);
                self.pending = store
                    .searchable_sessions(self.request.query.mode)?
                    .into_iter()
                    .filter(|key| !self.searched.contains(key))
                    .collect();
            }
            if self.pending.is_empty() {
                return Ok((!running).then_some(false));
            }
        }
        let count = self.pending.len().min(SEARCH_CHUNK);
        let chunk: Vec<String> = self.pending.drain(..count).collect();
        let mut found = Vec::new();
        let mut total = self.hits;
        store.search_sessions(
            &self.request.query,
            &chunk,
            SEARCH_HIT_LIMIT,
            &self.request.cancel,
            |hits| {
                total += hits.len();
                found.extend(hits);
                Ok(total < SEARCH_HIT_LIMIT)
            },
        )?;
        self.hits = total;
        self.searched.extend(chunk);
        if !found.is_empty() {
            emit(
                sender,
                Event::SearchHits {
                    generation: self.request.generation,
                    hits: found,
                },
                stop,
            )?;
        }
        Ok((self.hits >= SEARCH_HIT_LIMIT).then_some(true))
    }
}

fn send_page(
    sender: &SyncSender<Event>,
    generation: u64,
    snapshot: &SearchSnapshot,
    offset: usize,
    stop: &Cancellation,
) -> Result<()> {
    let (hits, next) = snapshot.page(offset)?;
    emit(
        sender,
        Event::Hits {
            generation,
            hits,
            complete: next.is_none(),
            more: next.map(|offset| SearchCursor {
                offset,
                ..Default::default()
            }),
        },
        stop,
    )
}

fn spawn_reader(
    database: PathBuf,
    writer: SharedWriter,
    commands: Receiver<ReadCommand>,
    searcher: SyncSender<SearchCommand>,
    sender: SyncSender<Event>,
    directory: Arc<TempDir>,
    stop: Cancellation,
) -> Result<JoinHandle<()>> {
    Ok(thread::Builder::new()
        .name("document-reader".into())
        .spawn(move || {
            let work = || -> Result<()> {
                let live = Store::open(&database)?;
                let mut documents: HashMap<String, DocumentSnapshot> = HashMap::new();
                let mut active: Option<(u64, String)> = None;
                while !stop.is_cancelled() {
                    let command = match commands.recv_timeout(Duration::from_millis(50)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    };
                    let current = active.as_ref().and_then(|(_, key)| documents.get(key));
                    let result = match command {
                        ReadCommand::Open {
                            generation,
                            session,
                            reload,
                            cancel,
                        } => {
                            if cancel.is_cancelled() {
                                continue;
                            }
                            let key = session.candidate.key.storage_key();
                            let opened = (|| -> Result<Vec<DocumentMessage>> {
                                if reload {
                                    documents.remove(&key);
                                }
                                if !documents.contains_key(&key) {
                                    let mut writer = writer
                                        .lock()
                                        .map_err(|_| anyhow::anyhow!("Index writer failed"))?;
                                    let (prepared, diagnostics) =
                                        scan::prepare_known(&mut writer, &session, &cancel)?;
                                    for diagnostic in diagnostics {
                                        emit(
                                            &sender,
                                            Event::Notice(diagnostic.description),
                                            &stop,
                                        )?;
                                    }
                                    anyhow::ensure!(
                                        prepared.available && prepared.indexed,
                                        "{}",
                                        prepared
                                            .content_error
                                            .as_deref()
                                            .unwrap_or("Local session content is unavailable")
                                    );
                                    let snapshot = DocumentSnapshot::capture(
                                        &writer,
                                        &key,
                                        directory.path(),
                                        &cancel,
                                    )?;
                                    documents.insert(key.clone(), snapshot);
                                }
                                let snapshot = documents
                                    .get(&key)
                                    .context("Document snapshot is unavailable")?;
                                document_messages(&snapshot.store, &key, &cancel)
                            })();
                            if cancel.is_cancelled() {
                                continue;
                            }
                            match opened {
                                Ok(messages) => {
                                    active = Some((generation, key.clone()));
                                    emit(
                                        &sender,
                                        Event::Document {
                                            generation,
                                            key,
                                            messages,
                                            complete: true,
                                        },
                                        &stop,
                                    )
                                }
                                Err(error) => {
                                    active = None;
                                    emit(
                                        &sender,
                                        Event::DocumentFailed {
                                            generation,
                                            message: format!("{error:#}"),
                                        },
                                        &stop,
                                    )
                                }
                            }
                        }
                        ReadCommand::Chunk {
                            generation,
                            rowid,
                            part,
                            range,
                        } => {
                            if active
                                .as_ref()
                                .is_none_or(|(current, _)| *current != generation)
                            {
                                continue;
                            }
                            current
                                .context("Document is not open")
                                .and_then(|document| {
                                    document.store.chunk(rowid, range.start, range.end)
                                })
                                .and_then(|text| {
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
                                })
                        }
                        ReadCommand::Copy(rowid) => current
                            .context("Document is not open")
                            .and_then(|document| document.store.message(rowid))
                            .and_then(|message| {
                                emit(&sender, Event::Copied(message.content), &stop)
                            }),
                        ReadCommand::Raw { generation, rowid } => {
                            if active
                                .as_ref()
                                .is_none_or(|(current, _)| *current != generation)
                            {
                                continue;
                            }
                            current
                                .context("Document is not open")
                                .and_then(|document| document.store.original_message(rowid))
                                .and_then(|text| {
                                    let parts = chunk_ranges(&text)
                                        .into_iter()
                                        .map(|range| text[range].to_owned())
                                        .collect();
                                    emit(&sender, Event::Raw { generation, parts }, &stop)
                                })
                        }
                        ReadCommand::Export { key, path } => documents
                            .get(&key)
                            .context("Document is not open")
                            .and_then(|document| export_document(&document.store, &key, &path))
                            .and_then(|()| {
                                emit(
                                    &sender,
                                    Event::Notice(format!("Exported {}", path.display())),
                                    &stop,
                                )
                            }),
                        ReadCommand::Find {
                            generation,
                            query,
                            cancel,
                        } => {
                            let file = query
                                .session
                                .as_ref()
                                .and_then(|key| documents.get(key))
                                .map(|document| document.file.clone());
                            match file {
                                Some(file) => searcher
                                    .send(SearchCommand::Document {
                                        request: SearchRequest {
                                            generation,
                                            query,
                                            cancel,
                                        },
                                        file,
                                    })
                                    .context("Search worker closed"),
                                None => emit(
                                    &sender,
                                    Event::SearchFailed {
                                        generation,
                                        message: "无法读取 session 内容".into(),
                                    },
                                    &stop,
                                ),
                            }
                        }
                        ReadCommand::ValidateHit {
                            generation,
                            hit,
                            permanent,
                        } => (if hit.kind == HitKind::Title {
                            live.session(&hit.session)
                                .map(|session| session.is_some_and(|s| s.available && s.indexed))
                        } else if hit.document_snapshot {
                            documents
                                .get(&hit.session)
                                .map(|document| {
                                    document
                                        .store
                                        .readable_message(&hit.session, hit.message_id)
                                })
                                .unwrap_or(Ok(false))
                        } else {
                            live.readable_message(&hit.session, hit.message_id)
                        })
                        .and_then(|valid| {
                            if valid {
                                emit(
                                    &sender,
                                    Event::OpenHit {
                                        generation,
                                        hit,
                                        permanent,
                                    },
                                    &stop,
                                )
                            } else {
                                emit(
                                    &sender,
                                    Event::Notice(
                                        "This search result is outdated or its content cannot be read; search again.".into(),
                                    ),
                                    &stop,
                                )
                            }
                        }),
                        ReadCommand::Retain(keys) => {
                            let keys: HashSet<_> = keys.into_iter().collect();
                            documents.retain(|key, _| keys.contains(key));
                            if active.as_ref().is_some_and(|(_, key)| !keys.contains(key)) {
                                active = None;
                            }
                            Ok(())
                        }
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
        })?)
}

fn document_messages(
    store: &Store,
    key: &str,
    cancel: &Cancellation,
) -> Result<Vec<DocumentMessage>> {
    store
        .message_infos(key)?
        .into_iter()
        .map(|info| {
            cancel.check()?;
            let parts = if info.bytes <= 8192 {
                std::iter::once(0..info.bytes).collect()
            } else {
                chunk_ranges(&store.message(info.rowid)?.content)
            };
            Ok(DocumentMessage {
                native_id: info.native_id.clone(),
                attachment_names: info.attachment_names.clone(),
                info,
                parts,
            })
        })
        .collect()
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

fn export_document(store: &Store, key: &str, path: &Path) -> Result<()> {
    let session = store
        .session(key)?
        .context("Session is no longer indexed")?;
    anyhow::ensure!(
        session.available && session.indexed,
        "无法读取 session 内容"
    );
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("Export path has no parent")?)?;
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
    use std::time::Instant;

    const A: &str = "00000000-0000-4000-8000-000000000001";
    const B: &str = "00000000-0000-4000-8000-000000000002";
    const MISSING: &str = "00000000-0000-4000-8000-000000000003";

    struct Fixture {
        directory: TempDir,
        home: PathBuf,
        data: PathBuf,
        settings: Settings,
    }

    impl Fixture {
        fn new(messages: usize, missing: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let home = directory.path().join("source");
            let data = directory.path().join("app");
            fs::create_dir_all(home.join("session-state")).unwrap();
            fs::create_dir(&data).unwrap();
            let settings = Settings {
                sources: vec![home.clone()],
                vscode_sources: Some(Vec::new()),
                codex_sources: Some(Vec::new()),
                ..Default::default()
            };
            persistence::save(&data.join("settings.json"), &settings).unwrap();
            let fixture = Self {
                directory,
                home,
                data,
                settings,
            };
            fixture.write_session(A, messages, "needle original");
            if missing {
                fs::create_dir(fixture.home.join("session-state").join(MISSING)).unwrap();
                let db = rusqlite::Connection::open(fixture.home.join("data.db")).unwrap();
                db.execute_batch(
                    "CREATE TABLE sessions(id TEXT,title TEXT);
                    CREATE TABLE workspaces(session_id TEXT,name TEXT);",
                )
                .unwrap();
                db.execute(
                    "INSERT INTO sessions VALUES(?1,'Unavailable metadata')",
                    [MISSING],
                )
                .unwrap();
            }
            fixture
        }

        fn path(&self, native: &str) -> PathBuf {
            self.home
                .join("session-state")
                .join(native)
                .join("events.jsonl")
        }

        fn write_session(&self, native: &str, count: usize, body: &str) {
            let path = self.path(native);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path.with_file_name("workspace.yaml"),
                format!("client_name: github/cli\nname: Synthetic {native}\n"),
            )
            .unwrap();
            let mut file = fs::File::create(path).unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"session.start","timestamp":"2026-01-01T00:00:00Z",
                "data":{"context":{"cwd":"D:\\synthetic-private-workspace"}}})
            )
            .unwrap();
            for index in 0..count {
                writeln!(file,"{}",serde_json::json!({"id":format!("message-{index}"),"type":"user.message",
                    "timestamp":"2026-01-02T00:00:00Z","data":{"content":format!("{body} {index}")}})).unwrap();
            }
        }

        fn append(&self, body: &str) {
            let mut file = fs::File::options().append(true).open(self.path(A)).unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({"id":"appended","type":"assistant.message",
                "timestamp":"2026-02-01T00:00:00Z","data":{"content":body}})
            )
            .unwrap();
        }

        fn close(self, reader: Runtime) {
            reader.shutdown(self.settings).unwrap();
            drop(reader);
            assert!(!fs::read_dir(&self.data).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("snapshots-")
            }));
            self.directory.close().unwrap();
        }
    }

    fn next(reader: &Runtime, mut predicate: impl FnMut(&Event) -> bool) -> Event {
        let start = Instant::now();
        loop {
            if let Ok(event) = reader
                .events
                .try_recv()
                .or_else(|_| reader.catalog_events.try_recv())
            {
                if let Event::Error(error) | Event::SearchFailed { message: error, .. } = &event {
                    panic!("{error}");
                }
                if predicate(&event) {
                    return event;
                }
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "Runtime event timed out"
            );
            thread::sleep(Duration::from_millis(3));
        }
    }

    fn catalog(reader: &Runtime) -> (Vec<Session>, Vec<scan::ScanProgress>) {
        let mut sessions = HashMap::new();
        let mut progress = HashMap::new();
        next(reader, |event| {
            match event {
                Event::Scan {
                    event: ScanEvent::Session(session),
                    ..
                } => {
                    sessions.insert(session.candidate.key.storage_key(), (**session).clone());
                }
                Event::Scan {
                    event:
                        ScanEvent::Progress {
                            source,
                            progress: value,
                        },
                    ..
                } => {
                    assert!(value.indexed <= value.indexable && value.scanned <= value.discovered);
                    progress.insert(source.clone(), value.clone());
                }
                _ => {}
            }
            matches!(event, Event::CatalogFinished { .. })
        });
        (
            sessions.into_values().collect(),
            progress.into_values().collect(),
        )
    }

    /// A page of a local find.
    fn hits(reader: &Runtime, generation: u64) -> Vec<SearchHit> {
        let event = next(reader, |event| {
            matches!(event,
            Event::Hits { generation: actual, .. } if *actual == generation)
        });
        let Event::Hits { hits, .. } = event else {
            unreachable!()
        };
        hits
    }

    /// Every hit of a global search, collected until it is done, and whether it was capped.
    fn live(reader: &Runtime, generation: u64) -> (Vec<SearchHit>, bool) {
        let mut found = Vec::new();
        let event = next(reader, |event| {
            if let Event::SearchHits {
                generation: actual,
                hits,
            } = event
                && *actual == generation
            {
                found.extend(hits.iter().cloned());
            }
            matches!(event, Event::SearchDone { generation: actual, .. } if *actual == generation)
        });
        let Event::SearchDone { capped, .. } = event else {
            unreachable!()
        };
        (found, capped)
    }

    fn global(reader: &Runtime, generation: u64, text: &str) {
        reader
            .search(
                generation,
                SearchQuery {
                    text: text.into(),
                    ..Default::default()
                },
                SearchCursor::default(),
                Cancellation::default(),
            )
            .unwrap();
    }

    fn opened(reader: &Runtime, generation: u64) -> Vec<DocumentMessage> {
        let event = next(reader, |event| {
            matches!(event,
            Event::Document { generation: actual, .. } | Event::DocumentFailed { generation: actual, .. }
                if *actual == generation)
        });
        let Event::Document { messages, .. } = event else {
            panic!("Document failed to open")
        };
        messages
    }

    #[test]
    fn no_automatic_scans_and_search_reads_the_index_as_last_scanned() {
        let fixture = Fixture::new(1, true);
        let reader = Runtime::new(fixture.data.clone());
        let (sessions, progress) = catalog(&reader);
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            progress
                .iter()
                .map(|progress| progress.indexable)
                .sum::<usize>(),
            1
        );
        assert_eq!(
            progress
                .iter()
                .map(|progress| progress.indexed)
                .sum::<usize>(),
            1
        );
        assert_eq!(
            progress
                .iter()
                .map(|progress| progress.unreadable)
                .sum::<usize>(),
            1
        );
        let unavailable = sessions.iter().find(|session| !session.available).unwrap();
        reader
            .open(1, unavailable.clone(), Cancellation::default())
            .unwrap();
        let event = next(&reader, |event| {
            matches!(event, Event::DocumentFailed { generation: 1, .. })
        });
        assert!(matches!(event, Event::DocumentFailed { message, .. } if !message.is_empty()));
        global(&reader, 1, "later");
        assert!(live(&reader, 1).0.is_empty());
        fixture.append("later needle");
        fixture.write_session(B, 1, "later hidden new session");
        fs::create_dir(fixture.home.join("unrelated")).unwrap();
        thread::sleep(Duration::from_millis(700));
        while let Ok(event) = reader
            .events
            .try_recv()
            .or_else(|_| reader.catalog_events.try_recv())
        {
            assert!(!matches!(
                event,
                Event::Scan { .. } | Event::CatalogStarted { .. } | Event::SearchHits { .. }
            ));
        }
        // A search reads the index as the last scan left it; changed files wait for a refresh.
        global(&reader, 2, "later");
        assert!(live(&reader, 2).0.is_empty());
        reader
            .refresh(
                fixture.settings.sources.clone(),
                Some(Vec::new()),
                Some(Vec::new()),
            )
            .unwrap();
        catalog(&reader);
        global(&reader, 3, "later");
        let (results, capped) = live(&reader, 3);
        assert!(!capped);
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|hit| hit.session.ends_with(A)));
        assert!(results.iter().any(|hit| hit.session.ends_with(B)));
        fixture.close(reader);
    }

    #[test]
    fn global_search_reports_whole_sessions_and_documents_keep_their_versions() {
        let fixture = Fixture::new(600, false);
        let reader = Runtime::new(fixture.data.clone());
        let (sessions, _) = catalog(&reader);
        let session = sessions[0].clone();
        reader
            .open(1, session.clone(), Cancellation::default())
            .unwrap();
        let original = opened(&reader, 1);
        assert_eq!(original.len(), 600);
        global(&reader, 1, "needle");
        let (found, capped) = live(&reader, 1);
        assert!(!capped);
        assert_eq!(found.len(), 600);
        assert!(found.iter().all(|hit| hit.snippet.contains("original")));
        assert!(
            found
                .windows(2)
                .all(|pair| pair[0].ordinal < pair[1].ordinal)
        );
        fixture.append("needle addition");
        fixture.write_session(B, 1, "needle new member");
        reader
            .refresh(
                fixture.settings.sources.clone(),
                Some(Vec::new()),
                Some(Vec::new()),
            )
            .unwrap();
        let (updated, _) = catalog(&reader);
        assert_eq!(updated.len(), 2);
        reader
            .open(2, session.clone(), Cancellation::default())
            .unwrap();
        assert_eq!(opened(&reader, 2).len(), 600);
        let last = original.last().unwrap();
        reader
            .chunk(2, last.info.rowid, 0, last.parts[0].clone())
            .unwrap();
        let event = next(&reader, |event| {
            matches!(event, Event::Chunk { generation: 2, .. })
        });
        assert!(matches!(event, Event::Chunk { text, .. } if text=="needle original 599"));
        let export = fixture.data.join("old-document.md");
        reader
            .export(session.candidate.key.storage_key(), export.clone())
            .unwrap();
        next(
            &reader,
            |event| matches!(event, Event::Notice(message) if message.starts_with("Exported")),
        );
        let markdown = fs::read_to_string(export).unwrap();
        assert!(
            !markdown.contains("addition") && !markdown.contains("synthetic-private-workspace")
        );
        reader
            .search(
                2,
                SearchQuery {
                    text: "addition".into(),
                    session: Some(session.candidate.key.storage_key()),
                    ..Default::default()
                },
                SearchCursor::default(),
                Cancellation::default(),
            )
            .unwrap();
        assert!(hits(&reader, 2).is_empty());
        reader
            .reload(3, session.clone(), Cancellation::default())
            .unwrap();
        assert_eq!(opened(&reader, 3).len(), 601);
        reader
            .search(
                3,
                SearchQuery {
                    text: "addition".into(),
                    session: Some(session.candidate.key.storage_key()),
                    ..Default::default()
                },
                SearchCursor::default(),
                Cancellation::default(),
            )
            .unwrap();
        assert_eq!(hits(&reader, 3).len(), 1);
        fixture.close(reader);
    }

    #[test]
    fn search_starts_before_the_catalog_ends_and_old_hits_report_invalidation() {
        let fixture = Fixture::new(1, true);
        let reader = Runtime::new(fixture.data.clone());
        // Sent while the first catalog run is still going; it does not wait for the run to end.
        global(&reader, 1, "needle");
        let (found, capped) = live(&reader, 1);
        assert!(!capped);
        let terminal = Store::open(&fixture.data.join("index.sqlite"))
            .unwrap()
            .sessions(TimeMode::Latest)
            .unwrap();
        assert_eq!(terminal.len(), 2);
        assert_eq!(
            terminal
                .iter()
                .filter(|session| session.available && session.indexed)
                .count(),
            1
        );
        assert_eq!(
            terminal
                .iter()
                .filter(|session| !session.available && !session.indexed)
                .count(),
            1
        );
        assert_eq!(found.len(), 1);
        let old = found[0].clone();
        fixture.write_session(A, 1, "replacement content");
        reader
            .refresh(
                fixture.settings.sources.clone(),
                Some(Vec::new()),
                Some(Vec::new()),
            )
            .unwrap();
        catalog(&reader);
        global(&reader, 2, "replacement");
        assert_eq!(live(&reader, 2).0.len(), 1);
        reader.validate_hit(1, old, false).unwrap();
        next(
            &reader,
            |event| matches!(event, Event::Notice(message) if message.contains("outdated")),
        );
        fixture.close(reader);
    }

    #[test]
    fn missing_previously_indexed_cli_session_stays_visible_and_does_not_inflate_refresh_targets() {
        let fixture = Fixture::new(1, false);
        let reader = Runtime::new(fixture.data.clone());
        catalog(&reader);
        fs::remove_file(fixture.path(A)).unwrap();
        for _ in 0..2 {
            reader
                .refresh(
                    fixture.settings.sources.clone(),
                    Some(Vec::new()),
                    Some(Vec::new()),
                )
                .unwrap();
            let (sessions, progress) = catalog(&reader);
            assert_eq!(sessions.len(), 1);
            assert!(!sessions[0].available && !sessions[0].indexed);
            assert_eq!(
                progress
                    .iter()
                    .map(|progress| progress.indexable)
                    .sum::<usize>(),
                0
            );
            assert_eq!(
                progress
                    .iter()
                    .map(|progress| progress.indexed)
                    .sum::<usize>(),
                0
            );
            assert_eq!(
                progress
                    .iter()
                    .map(|progress| progress.unreadable)
                    .sum::<usize>(),
                1
            );
        }
        global(&reader, 1, "needle");
        assert!(live(&reader, 1).0.is_empty());
        fixture.close(reader);
    }

    #[test]
    fn cancelled_requests_do_not_publish_results_and_completed_snapshot_is_released_on_cancel() {
        let fixture = Fixture::new(1, false);
        let reader = Runtime::new(fixture.data.clone());
        let (sessions, _) = catalog(&reader);
        let cancelled = Cancellation::default();
        cancelled.cancel();
        reader
            .search(
                1,
                SearchQuery {
                    text: "needle".into(),
                    ..Default::default()
                },
                SearchCursor::default(),
                cancelled,
            )
            .unwrap();
        global(&reader, 2, "needle");
        let mut stale = false;
        next(&reader, |event| {
            if matches!(
                event,
                Event::SearchHits { generation: 1, .. } | Event::SearchDone { generation: 1, .. }
            ) {
                stale = true;
            }
            matches!(event, Event::SearchDone { generation: 2, .. })
        });
        assert!(!stale);
        // A local find keeps its results in a snapshot until it is cancelled.
        let session = sessions.iter().find(|session| session.available).unwrap();
        reader
            .open(1, session.clone(), Cancellation::default())
            .unwrap();
        opened(&reader, 1);
        let cancel = Cancellation::default();
        reader
            .search(
                3,
                SearchQuery {
                    text: "needle".into(),
                    session: Some(session.candidate.key.storage_key()),
                    ..Default::default()
                },
                SearchCursor::default(),
                cancel.clone(),
            )
            .unwrap();
        hits(&reader, 3);
        let snapshot_directory = fs::read_dir(&fixture.data)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("snapshots-")
            })
            .unwrap();
        // The open conversation keeps its own snapshot; only the find snapshot goes away.
        let files = || fs::read_dir(&snapshot_directory).unwrap().count();
        let before = files();
        cancel.cancel();
        let start = Instant::now();
        while files() >= before {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "Cancelled result cache was not released"
            );
            thread::sleep(Duration::from_millis(10));
        }
        fixture.close(reader);
    }
}

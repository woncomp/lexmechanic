use crate::{agents::AgentAdapter, model::*, store::Store};
use anyhow::Result;
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};

pub enum ScanEvent {
    Session(Box<Session>),
    Reclassified(ReclassifiedSession),
    Diagnostic(Diagnostic),
    Progress {
        source: String,
        discovered: usize,
        scanned: usize,
        indexed: usize,
    },
    Finished {
        source: String,
    },
}

pub struct ScanHandle {
    pub cancel: Cancellation,
    pub events: Receiver<ScanEvent>,
    workers: Vec<JoinHandle<()>>,
}

impl Drop for ScanHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                eprintln!("Agent scan worker panicked during shutdown");
            }
        }
    }
}

impl ScanHandle {
    pub fn is_finished(&self) -> bool {
        self.workers.iter().all(JoinHandle::is_finished)
    }
}

pub fn start(adapters: Vec<Arc<dyn AgentAdapter>>, database: PathBuf) -> Result<ScanHandle> {
    let writer = Arc::new(Mutex::new(Store::open(&database)?));
    start_with_writer(adapters, writer)
}

pub type SharedWriter = Arc<Mutex<Store>>;

pub fn start_with_writer(
    adapters: Vec<Arc<dyn AgentAdapter>>,
    writer: SharedWriter,
) -> Result<ScanHandle> {
    let cancel = Cancellation::default();
    let (sender, events) = mpsc::sync_channel(64);
    let mut workers = Vec::new();
    for adapter in adapters {
        let sender = sender.clone();
        let writer = writer.clone();
        let cancel = cancel.clone();
        workers.push(
            thread::Builder::new()
                .name(format!("scan-{}", adapter.source().agent))
                .spawn(move || {
                    let source = adapter.source().id.clone();
                    if let Err(error) = scan_source(adapter.as_ref(), writer, &cancel, &sender)
                        && !cancel.is_cancelled()
                    {
                        let _ = send(
                            &sender,
                            ScanEvent::Diagnostic(Diagnostic {
                                path: adapter.source().root.clone(),
                                offset: None,
                                description: format!("{error:#}"),
                            }),
                            &cancel,
                        );
                    }
                    let _ = send(&sender, ScanEvent::Finished { source }, &cancel);
                })?,
        );
    }
    Ok(ScanHandle {
        cancel,
        events,
        workers,
    })
}

fn send(sender: &SyncSender<ScanEvent>, mut event: ScanEvent, cancel: &Cancellation) -> Result<()> {
    loop {
        cancel.check()?;
        match sender.try_send(event) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Full(returned)) => {
                event = returned;
                thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => anyhow::bail!("Scan receiver closed"),
        }
    }
}

fn scan_source(
    adapter: &dyn AgentAdapter,
    writer: SharedWriter,
    cancel: &Cancellation,
    sender: &SyncSender<ScanEvent>,
) -> Result<()> {
    let Discovery {
        mut candidates,
        diagnostics,
        reclassified,
    } = adapter.discover(cancel)?;
    for replacement in reclassified {
        cancel.check()?;
        let removed = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("Index writer failed"))?
            .reclassify_session(&replacement)?;
        if removed {
            send(sender, ScanEvent::Reclassified(replacement), cancel)?;
        }
    }
    candidates.sort_unstable_by(|a, b| b.modified_ns.cmp(&a.modified_ns).then(a.key.cmp(&b.key)));
    let discovered = candidates.len();
    let present: HashSet<_> = candidates
        .iter()
        .map(|candidate| candidate.key.storage_key())
        .collect();
    let cached = writer
        .lock()
        .map_err(|_| anyhow::anyhow!("Index writer failed"))?
        .sessions(TimeMode::Latest)?;
    for mut session in cached {
        if session.candidate.key.source == adapter.source().id
            && session.candidate.key.agent == adapter.source().agent
            && session.available
            && !present.contains(&session.candidate.key.storage_key())
        {
            let mut store = writer
                .lock()
                .map_err(|_| anyhow::anyhow!("Index writer failed"))?;
            if store
                .session(&session.candidate.key.storage_key())?
                .is_none()
            {
                continue;
            }
            session.available = false;
            store.upsert_session(&session)?;
            drop(store);
            send(sender, ScanEvent::Session(Box::new(session)), cancel)?;
        }
    }
    let mut queue = VecDeque::new();
    let mut scanned = 0;
    let mut indexed = 0;
    for candidate in candidates {
        cancel.check()?;
        match adapter.summarize(&candidate, cancel) {
            Ok(mut session) => {
                let mut store = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Index writer failed"))?;
                let previous = store.session(&candidate.key.storage_key())?;
                if !session.available {
                    if let Some(mut cached) = previous.filter(|session| session.indexed) {
                        cached.name = session.name;
                        cached.workspace = session.workspace;
                        cached.available = false;
                        session = cached;
                    }
                    store.upsert_session(&session)?;
                    drop(store);
                    send(sender, ScanEvent::Session(Box::new(session)), cancel)?;
                    scanned += 1;
                    send(
                        sender,
                        ScanEvent::Progress {
                            source: adapter.source().id.clone(),
                            discovered,
                            scanned,
                            indexed,
                        },
                        cancel,
                    )?;
                    continue;
                }
                if let Some(mut previous) = previous.filter(|previous| {
                    previous.candidate.modified_ns == candidate.modified_ns
                        && previous.candidate.size == candidate.size
                        && previous.candidate.file_identity == candidate.file_identity
                        && previous.indexed
                }) {
                    previous.name = session.name;
                    previous.workspace = session.workspace;
                    previous.available = true;
                    store.upsert_session(&previous)?;
                    drop(store);
                    send(sender, ScanEvent::Session(Box::new(previous)), cancel)?;
                    indexed += 1;
                } else {
                    store.upsert_session(&session)?;
                    let cursor = store.resume_cursor(&session)?;
                    drop(store);
                    session.indexed = false;
                    send(
                        sender,
                        ScanEvent::Session(Box::new(session.clone())),
                        cancel,
                    )?;
                    queue.push_back((session, cursor, 0_u64));
                }
            }
            Err(error) => send(
                sender,
                ScanEvent::Diagnostic(Diagnostic {
                    path: candidate.events_path,
                    offset: None,
                    description: format!("{error:#}"),
                }),
                cancel,
            )?,
        }
        scanned += 1;
        send(
            sender,
            ScanEvent::Progress {
                source: adapter.source().id.clone(),
                discovered,
                scanned,
                indexed,
            },
            cancel,
        )?;
    }
    for diagnostic in diagnostics {
        send(sender, ScanEvent::Diagnostic(diagnostic), cancel)?;
    }
    // Summaries never wait for historical body indexing; large files yield between batches.
    while let Some((mut session, cursor, burst_bytes)) = queue.pop_front() {
        cancel.check()?;
        let batch = match adapter.read_batch(&session.candidate, cursor, 256 * 1024, cancel) {
            Ok(batch) => batch,
            Err(error) => {
                send(
                    sender,
                    ScanEvent::Diagnostic(Diagnostic {
                        path: session.candidate.events_path.clone(),
                        offset: Some(cursor),
                        description: format!("{error:#}"),
                    }),
                    cancel,
                )?;
                continue;
            }
        };
        {
            let mut writer = writer
                .lock()
                .map_err(|_| anyhow::anyhow!("Index writer failed"))?;
            cancel.check()?;
            writer.commit_batch(&mut session, &batch)?;
        }
        for diagnostic in &batch.diagnostics {
            send(sender, ScanEvent::Diagnostic(diagnostic.clone()), cancel)?;
        }
        send(
            sender,
            ScanEvent::Session(Box::new(session.clone())),
            cancel,
        )?;
        if batch.eof {
            indexed += usize::from(!batch.incomplete_tail);
            if batch.incomplete_tail {
                send(
                    sender,
                    ScanEvent::Diagnostic(Diagnostic {
                        path: session.candidate.events_path.clone(),
                        offset: Some(batch.cursor),
                        description: "Incomplete trailing event; waiting for the writer to finish"
                            .into(),
                    }),
                    cancel,
                )?;
            }
        } else if burst_bytes + batch.cursor.saturating_sub(cursor) < 2 * 1024 * 1024 {
            queue.push_front((
                session,
                batch.cursor,
                burst_bytes + batch.cursor.saturating_sub(cursor),
            ));
        } else {
            queue.push_back((session, batch.cursor, 0));
        }
        send(
            sender,
            ScanEvent::Progress {
                source: adapter.source().id.clone(),
                discovered,
                scanned,
                indexed,
            },
            cancel,
        )?;
    }
    Ok(())
}

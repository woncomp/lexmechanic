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
        progress: ScanProgress,
    },
    Finished {
        source: String,
        error: Option<String>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct ScanProgress {
    pub discovered: usize,
    pub scanned: usize,
    pub indexable: usize,
    pub indexed: usize,
    pub unreadable: usize,
    pub failed: usize,
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
                    let error = scan_source(adapter.as_ref(), writer, &cancel, &sender)
                        .err()
                        .map(|error| format!("{error:#}"));
                    if let Some(error) = &error
                        && !cancel.is_cancelled()
                    {
                        let _ = send(
                            &sender,
                            ScanEvent::Diagnostic(Diagnostic {
                                path: adapter.source().root.clone(),
                                offset: None,
                                description: error.clone(),
                            }),
                            &cancel,
                        );
                    }
                    let _ = send(&sender, ScanEvent::Finished { source, error }, &cancel);
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
    let mut progress = ScanProgress {
        discovered: candidates.len(),
        ..Default::default()
    };
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
            store.mark_unreadable(
                &mut session,
                "Session transcript is no longer present in this source".into(),
            )?;
            drop(store);
            send(sender, ScanEvent::Session(Box::new(session)), cancel)?;
            progress.discovered += 1;
            progress.scanned += 1;
            progress.unreadable += 1;
        }
    }
    send(
        sender,
        ScanEvent::Progress {
            source: adapter.source().id.clone(),
            progress: progress.clone(),
        },
        cancel,
    )?;
    let mut queue = VecDeque::new();
    for candidate in candidates {
        cancel.check()?;
        match adapter.summarize(&candidate, cancel) {
            Ok(mut session) => {
                let mut store = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Index writer failed"))?;
                let previous = store.session(&candidate.key.storage_key())?;
                if !session.available {
                    let reason = session
                        .content_error
                        .clone()
                        .unwrap_or_else(|| "Local session transcript is unavailable".into());
                    store.mark_unreadable(&mut session, reason)?;
                    drop(store);
                    send(sender, ScanEvent::Session(Box::new(session)), cancel)?;
                    progress.scanned += 1;
                    progress.unreadable += 1;
                    send(
                        sender,
                        ScanEvent::Progress {
                            source: adapter.source().id.clone(),
                            progress: progress.clone(),
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
                    progress.indexable += 1;
                    progress.indexed += 1;
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
                    progress.indexable += 1;
                }
            }
            Err(error) => {
                let reason = format!("{error:#}");
                let mut store = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Index writer failed"))?;
                let mut session = store
                    .session(&candidate.key.storage_key())?
                    .unwrap_or_else(|| Session::unreadable(candidate.clone(), reason.clone()));
                session.candidate = candidate;
                store.mark_unreadable(&mut session, reason.clone())?;
                drop(store);
                send(
                    sender,
                    ScanEvent::Diagnostic(Diagnostic {
                        path: session.candidate.events_path.clone(),
                        offset: None,
                        description: reason,
                    }),
                    cancel,
                )?;
                send(sender, ScanEvent::Session(Box::new(session)), cancel)?;
                progress.failed += 1;
            }
        }
        progress.scanned += 1;
        send(
            sender,
            ScanEvent::Progress {
                source: adapter.source().id.clone(),
                progress: progress.clone(),
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
        let read = (|| -> Result<ReadBatch> {
            let batch = adapter.read_batch(&session.candidate, cursor, 256 * 1024, cancel)?;
            let mut store = writer
                .lock()
                .map_err(|_| anyhow::anyhow!("Index writer failed"))?;
            cancel.check()?;
            store.commit_batch(&mut session, &batch)?;
            Ok(batch)
        })();
        let batch = match read {
            Ok(batch) => batch,
            Err(error) => {
                cancel.check()?;
                let reason = format!("{error:#}");
                writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Index writer failed"))?
                    .mark_unreadable(&mut session, reason.clone())?;
                send(
                    sender,
                    ScanEvent::Diagnostic(Diagnostic {
                        path: session.candidate.events_path.clone(),
                        offset: Some(cursor),
                        description: reason,
                    }),
                    cancel,
                )?;
                progress.indexable = progress.indexable.saturating_sub(1);
                progress.failed += 1;
                send(sender, ScanEvent::Session(Box::new(session)), cancel)?;
                send(
                    sender,
                    ScanEvent::Progress {
                        source: adapter.source().id.clone(),
                        progress: progress.clone(),
                    },
                    cancel,
                )?;
                continue;
            }
        };
        for diagnostic in &batch.diagnostics {
            send(sender, ScanEvent::Diagnostic(diagnostic.clone()), cancel)?;
        }
        send(
            sender,
            ScanEvent::Session(Box::new(session.clone())),
            cancel,
        )?;
        if batch.eof {
            progress.indexed += 1;
            if batch.incomplete_tail {
                send(
                    sender,
                    ScanEvent::Diagnostic(Diagnostic {
                        path: session.candidate.events_path.clone(),
                        offset: Some(batch.cursor),
                        description: "Incomplete trailing event excluded from this snapshot; refresh to retry"
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
                progress: progress.clone(),
            },
            cancel,
        )?;
    }
    Ok(())
}

/// Rechecks a known member without enumerating its source or changing list membership.
pub fn prepare_known(
    store: &mut Store,
    known: &Session,
    cancel: &Cancellation,
) -> Result<(Session, Vec<Diagnostic>)> {
    cancel.check()?;
    let mut session = known.clone();
    let result = (|| -> Result<Vec<Diagnostic>> {
        let adapter = crate::agents::adapter_for(&known.candidate)?;
        session.candidate = adapter.probe(&known.candidate, cancel)?;
        session.available = true;
        session.content_error = None;
        if let Some(cached) = store.session(&session.candidate.key.storage_key())?
            && cached.candidate == session.candidate
            && cached.available
            && cached.indexed
        {
            session = cached;
            return Ok(Vec::new());
        }
        session.indexed = false;
        store.upsert_session(&session)?;
        let mut cursor = store.resume_cursor(&session)?;
        let mut diagnostics = Vec::new();
        loop {
            cancel.check()?;
            let batch = adapter.read_batch(&session.candidate, cursor, 256 * 1024, cancel)?;
            store.commit_batch(&mut session, &batch)?;
            diagnostics.extend(batch.diagnostics);
            cursor = batch.cursor;
            if batch.eof {
                if batch.incomplete_tail {
                    diagnostics.push(Diagnostic {
                        path: session.candidate.events_path.clone(),
                        offset: Some(cursor),
                        description:
                            "Incomplete trailing event excluded from this snapshot; reload to retry"
                                .into(),
                    });
                }
                break;
            }
        }
        Ok(diagnostics)
    })();
    match result {
        Ok(diagnostics) => Ok((session, diagnostics)),
        Err(error) => {
            cancel.check()?;
            let reason = format!("{error:#}");
            store.mark_unreadable(&mut session, reason.clone())?;
            Ok((
                session,
                vec![Diagnostic {
                    path: known.candidate.events_path.clone(),
                    offset: None,
                    description: reason,
                }],
            ))
        }
    }
}

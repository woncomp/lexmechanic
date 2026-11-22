use agent_chat_reader_core::{
    agents::{AgentAdapter, copilot_cli::CopilotCli},
    model::*,
    scan,
    search::{SearchCursor, SearchQuery},
    snapshot::{DocumentSnapshot, SearchSnapshot},
    store::Store,
};
use serde_json::json;
use std::{fs, io::Write, path::PathBuf};

struct Fixture {
    _directory: tempfile::TempDir,
    home: PathBuf,
    path: PathBuf,
    database: PathBuf,
}

impl Fixture {
    fn new(count: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("copilot");
        let session = home
            .join("session-state")
            .join("00000000-0000-4000-8000-000000000001");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("workspace.yaml"),
            "client_name: github/cli\nname: Snapshot title\n",
        )
        .unwrap();
        let path = session.join("events.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"session.start","timestamp":"2026-01-01T00:00:00Z","data":{}})
        )
        .unwrap();
        for index in 0..count {
            writeln!(file,"{}",json!({"id":format!("message-{index}"),"type":"user.message",
                "timestamp":"2026-01-02T00:00:00Z","data":{"content":format!("needle original {index}")}})).unwrap();
        }
        Self {
            database: directory.path().join("index.sqlite"),
            _directory: directory,
            home,
            path,
        }
    }

    fn summary(&self) -> Session {
        let adapter = CopilotCli::new(&self.home).unwrap();
        let candidate = adapter
            .discover(&Cancellation::default())
            .unwrap()
            .candidates
            .remove(0);
        adapter
            .summarize(&candidate, &Cancellation::default())
            .unwrap()
    }

    fn append(&self, body: &str) {
        let mut file = fs::File::options().append(true).open(&self.path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"id":"appended","type":"assistant.message","timestamp":"2026-02-01T00:00:00Z",
            "data":{"content":body}})
        )
        .unwrap();
    }
}

#[test]
fn operation_boundary_excludes_later_append_and_next_preparation_includes_it() {
    let fixture = Fixture::new(1);
    let summary = fixture.summary();
    fixture.append("later needle");
    let adapter = CopilotCli::new(&fixture.home).unwrap();
    let batch = adapter
        .read_batch(&summary.candidate, 0, usize::MAX, &Cancellation::default())
        .unwrap();
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.cursor, summary.candidate.size);
    let mut store = Store::open(&fixture.database).unwrap();
    let (prepared, _) =
        scan::prepare_known(&mut store, &summary, &Cancellation::default()).unwrap();
    assert_eq!(
        store
            .message_infos(&prepared.candidate.key.storage_key())
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn completed_search_pages_and_unrendered_document_text_survive_active_index_rebuilds() {
    let fixture = Fixture::new(600);
    let mut store = Store::open(&fixture.database).unwrap();
    let (session, _) =
        scan::prepare_known(&mut store, &fixture.summary(), &Cancellation::default()).unwrap();
    let key = session.candidate.key.storage_key();
    let directory = tempfile::tempdir().unwrap();
    let document =
        DocumentSnapshot::capture(&store, &key, directory.path(), &Cancellation::default())
            .unwrap();
    let snapshot = store
        .with_read_snapshot(|source| {
            SearchSnapshot::capture(
                source,
                &SearchQuery {
                    text: "needle".into(),
                    ..Default::default()
                },
                directory.path(),
                &Cancellation::default(),
            )
        })
        .unwrap();
    let (first, next) = snapshot.page(0).unwrap();
    assert_eq!(first.len(), 512);
    let last = document.store.message_infos(&key).unwrap().pop().unwrap();
    store.clear().unwrap();
    fixture.append("needle after snapshot");
    scan::prepare_known(&mut store, &session, &Cancellation::default()).unwrap();
    let (second, next) = snapshot.page(next.unwrap()).unwrap();
    assert_eq!(second.len(), 88);
    assert!(next.is_none());
    assert!(
        second
            .iter()
            .all(|hit| hit.snippet.contains("original") && hit.session_name == "Snapshot title")
    );
    assert_eq!(
        document.store.chunk(last.rowid, 0, last.bytes).unwrap(),
        "needle original 599"
    );
    assert_eq!(document.store.message_infos(&key).unwrap().len(), 600);
    assert_eq!(store.message_infos(&key).unwrap().len(), 601);
    drop(document);
    drop(snapshot);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn missing_source_keeps_metadata_but_cannot_match_new_queries_even_when_it_was_cached() {
    let fixture = Fixture::new(1);
    let mut store = Store::open(&fixture.database).unwrap();
    let (session, _) =
        scan::prepare_known(&mut store, &fixture.summary(), &Cancellation::default()).unwrap();
    let key = session.candidate.key.storage_key();
    fs::remove_file(&fixture.path).unwrap();
    let (unreadable, diagnostics) =
        scan::prepare_known(&mut store, &session, &Cancellation::default()).unwrap();
    assert!(!unreadable.available && !unreadable.indexed);
    assert!(!diagnostics.is_empty());
    assert_eq!(unreadable.name, "Snapshot title");
    assert!(unreadable.content_error.is_some());
    assert!(store.session(&key).unwrap().is_some());
    assert!(store.message_infos(&key).unwrap().is_empty());
    assert!(
        store
            .search_page(
                &SearchQuery {
                    text: "needle".into(),
                    ..Default::default()
                },
                SearchCursor::default(),
                &Cancellation::default()
            )
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn incomplete_tail_finishes_the_readable_prefix_without_waiting_for_more_source_writes() {
    let fixture = Fixture::new(1);
    let mut file = fs::File::options()
        .append(true)
        .open(&fixture.path)
        .unwrap();
    write!(
        file,
        "{{\"type\":\"assistant.message\",\"data\":{{\"content\":\""
    )
    .unwrap();
    drop(file);
    let mut store = Store::open(&fixture.database).unwrap();
    let (prepared, diagnostics) =
        scan::prepare_known(&mut store, &fixture.summary(), &Cancellation::default()).unwrap();
    assert!(prepared.available && prepared.indexed);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.description.contains("Incomplete trailing"))
    );
    assert_eq!(
        store
            .message_infos(&prepared.candidate.key.storage_key())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn query_member_scope_excludes_newly_indexed_sessions_outside_the_fixed_list() {
    let fixture = Fixture::new(1);
    let mut store = Store::open(&fixture.database).unwrap();
    let (session, _) =
        scan::prepare_known(&mut store, &fixture.summary(), &Cancellation::default()).unwrap();
    let query = SearchQuery {
        text: "needle".into(),
        members: Some(Vec::new()),
        ..Default::default()
    };
    assert!(
        store
            .search_page(&query, SearchCursor::default(), &Cancellation::default())
            .unwrap()
            .hits
            .is_empty()
    );
    let query = SearchQuery {
        members: Some(vec![session.candidate.key.storage_key()]),
        ..query
    };
    assert_eq!(
        store
            .search_page(&query, SearchCursor::default(), &Cancellation::default())
            .unwrap()
            .hits
            .len(),
        1
    );
}

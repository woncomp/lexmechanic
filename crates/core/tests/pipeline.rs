use agent_chat_reader_core::{
    agents::{AgentAdapter, copilot_cli::CopilotCli},
    model::*,
    search::{HitKind, SearchCursor, SearchHit, SearchQuery, find_ranges},
    snapshot::SearchSnapshot,
    store::Store,
};
use serde_json::{Value, json};
use std::{fs, io::Write};
use tempfile::TempDir;

struct Fixture {
    _directory: TempDir,
    adapter: CopilotCli,
    database: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("session-state")).unwrap();
        Self {
            adapter: CopilotCli::new(directory.path()).unwrap(),
            database: directory.path().join("index.sqlite"),
            _directory: directory,
        }
    }

    fn session(&self, native: &str, events: &[Value]) -> Candidate {
        let directory = self
            .adapter
            .source()
            .root
            .join("session-state")
            .join(native);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("workspace.yaml"),
            "client_name: github/cli\nname: 'Synthetic session'\n",
        )
        .unwrap();
        let mut file = fs::File::create(directory.join("events.jsonl")).unwrap();
        for event in events {
            writeln!(file, "{event}").unwrap();
        }
        self.adapter
            .discover(&Cancellation::default())
            .unwrap()
            .candidates
            .into_iter()
            .find(|candidate| candidate.key.native == native)
            .unwrap()
    }

    fn index(&self, store: &mut Store, candidate: &Candidate) -> Session {
        let cancel = Cancellation::default();
        let mut session = self.adapter.summarize(candidate, &cancel).unwrap();
        store.upsert_session(&session).unwrap();
        let mut cursor = store.resume_cursor(&session).unwrap();
        loop {
            let batch = self
                .adapter
                .read_batch(candidate, cursor, 256, &cancel)
                .unwrap();
            store.commit_batch(&mut session, &batch).unwrap();
            cursor = batch.cursor;
            if batch.eof {
                break;
            }
        }
        session
    }
}

const A: &str = "00000000-0000-4000-8000-000000000001";
const B: &str = "00000000-0000-4000-8000-000000000002";

fn start() -> Value {
    json!({"type":"session.start","timestamp":"2026-01-01T00:00:00Z",
        "data":{"context":{"cwd":"D:\\synthetic"}}})
}

fn msg(role: &str, time: &str, body: &str) -> Value {
    json!({"type":role,"timestamp":time,"data":{"content":body}})
}

#[test]
fn scanning_uses_mtime_but_table_uses_independent_message_times() {
    let fixture = Fixture::new();
    let a = fixture.session(
        A,
        &[
            start(),
            msg(
                "assistant.message",
                "2026-01-05T00:00:00+08:00",
                "old response",
            ),
        ],
    );
    let b = fixture.session(
        B,
        &[
            start(),
            msg("assistant.message", "2026-01-06T00:00:00Z", "new response"),
        ],
    );
    fs::File::options()
        .write(true)
        .open(&a.events_path)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(200)),
        )
        .unwrap();
    fs::File::options()
        .write(true)
        .open(&b.events_path)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)),
        )
        .unwrap();
    let candidates = fixture
        .adapter
        .discover(&Cancellation::default())
        .unwrap()
        .candidates;
    assert_eq!(candidates[0].key.native, A);
    let mut store = Store::open(&fixture.database).unwrap();
    for candidate in &candidates {
        fixture.index(&mut store, candidate);
    }
    let sessions = store.sessions(TimeMode::Latest).unwrap();
    assert_eq!(sessions[0].candidate.key.native, B);
    assert_eq!(
        sessions[1].first_assistant.value().unwrap().to_rfc3339(),
        "2026-01-04T16:00:00+00:00"
    );
}

#[test]
fn first_assistant_invalid_is_not_replaced_with_later_assistant_or_creation() {
    let fixture = Fixture::new();
    let a = fixture.session(
        A,
        &[
            start(),
            msg("assistant.message", "not-a-time", "first"),
            msg("assistant.message", "2026-01-06T00:00:00Z", "second"),
        ],
    );
    let b = fixture.session(
        B,
        &[
            start(),
            msg("assistant.message", "2026-01-02T00:00:00Z", "valid"),
        ],
    );
    let mut store = Store::open(&fixture.database).unwrap();
    let session = fixture.index(&mut store, &a);
    fixture.index(&mut store, &b);
    assert_eq!(session.first_assistant, MessageTime::Invalid);
    assert_eq!(session.time(TimeMode::FirstAssistant), None);
    assert_eq!(
        store.sessions(TimeMode::FirstAssistant).unwrap()[0]
            .candidate
            .key
            .native,
        B
    );
}

#[test]
fn filtering_preserves_user_text_after_known_wrapper_and_ignores_nested_agents() {
    let fixture = Fixture::new();
    let candidate = fixture.session(
        A,
        &[
            start(),
            msg(
                "user.message",
                "",
                "<system_reminder>internal</system_reminder>\n真实提问",
            ),
            json!({"type":"assistant.message","agentId":"child","data":{"content":"hidden"}}),
            json!({"type":"user.message","data":{"source":"memory","content":"hidden"}}),
            msg(
                "user.message",
                "",
                "<system_reminder>only internal</system_reminder>",
            ),
            json!({"type":"assistant.message","agentId":"","data":{"content":"Visible"}}),
        ],
    );
    let batch = fixture
        .adapter
        .read_batch(&candidate, 0, usize::MAX, &Cancellation::default())
        .unwrap();
    assert_eq!(batch.messages.len(), 2);
    assert_eq!(batch.messages[0].content, "真实提问");
}

#[test]
fn incomplete_tail_does_not_advance_cursor_and_completed_append_is_imported_once() {
    let fixture = Fixture::new();
    let candidate = fixture.session(A, &[start(), msg("user.message", "", "first")]);
    let complete_length = fs::metadata(&candidate.events_path).unwrap().len();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&candidate.events_path)
        .unwrap();
    write!(
        file,
        "{{\"type\":\"assistant.message\",\"data\":{{\"content\":\""
    )
    .unwrap();
    let candidate = fixture
        .adapter
        .probe(&candidate, &Cancellation::default())
        .unwrap();
    let batch = fixture
        .adapter
        .read_batch(&candidate, 0, usize::MAX, &Cancellation::default())
        .unwrap();
    assert!(batch.incomplete_tail);
    assert_eq!(batch.cursor, complete_length);
    writeln!(file, "completed\"}}}}").unwrap();
    let candidate = fixture
        .adapter
        .probe(&candidate, &Cancellation::default())
        .unwrap();
    let mut store = Store::open(&fixture.database).unwrap();
    let session = fixture.index(&mut store, &candidate);
    let again = fixture.index(&mut store, &candidate);
    assert_eq!(
        store
            .message_infos(&again.candidate.key.storage_key())
            .unwrap()
            .len(),
        2
    );
    assert!(session.indexed);
}

#[test]
fn search_is_literal_handles_short_chinese_and_rebuilds_after_replacement() {
    let fixture = Fixture::new();
    let candidate = fixture.session(
        A,
        &[
            start(),
            msg(
                "assistant.message",
                "2026-01-01T01:00:00Z",
                "中文搜索测试 / A_B %path% \"quoted\" ＡＢＣ e\u{301}",
            ),
        ],
    );
    let mut store = Store::open(&fixture.database).unwrap();
    fixture.index(&mut store, &candidate);
    for text in [
        "中",
        "中文",
        "文搜索",
        "a_b",
        "%path%",
        "\"quoted\"",
        "abc",
        "É",
    ] {
        let result = store
            .search_page(
                &SearchQuery {
                    text: text.into(),
                    ..Default::default()
                },
                SearchCursor::default(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(result.hits.len(), 1, "{text}");
    }
    let replaced = fixture.session(A, &[start(), msg("user.message", "", "replacement")]);
    fixture.index(&mut store, &replaced);
    let result = store
        .search_page(
            &SearchQuery {
                text: "搜索".into(),
                ..Default::default()
            },
            SearchCursor::default(),
            &Cancellation::default(),
        )
        .unwrap();
    assert!(result.hits.is_empty());
}

#[test]
fn normalized_search_ranges_map_back_to_whole_original_graphemes() {
    let text = "你e\u{301} ＡＢＣ好";
    let ranges = find_ranges(text, "É");
    assert_eq!(&text[ranges[0].clone()], "e\u{301}");
    let ranges = find_ranges(text, "abc");
    assert_eq!(&text[ranges[0].clone()], "ＡＢＣ");
}

#[test]
fn index_error_reports_gaps_and_keeps_following_valid_messages() {
    let fixture = Fixture::new();
    let candidate = fixture.session(A, &[start()]);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&candidate.events_path)
        .unwrap();
    writeln!(file, "invalid json").unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"user.message","data":{"content":[{"type":"unknown"}]}})
    )
    .unwrap();
    writeln!(file, "{}", msg("user.message", "", "after gap")).unwrap();
    let candidate = fixture
        .adapter
        .probe(&candidate, &Cancellation::default())
        .unwrap();
    let batch = fixture
        .adapter
        .read_batch(&candidate, 0, usize::MAX, &Cancellation::default())
        .unwrap();
    assert_eq!(batch.diagnostics.len(), 2);
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.messages[0].content, "after gap");
}

#[test]
fn document_search_paginates_occurrences_without_losing_or_duplicating_matches() {
    let fixture = Fixture::new();
    let candidate = fixture.session(
        A,
        &[start(), msg("assistant.message", "", &"中文 ".repeat(600))],
    );
    let mut store = Store::open(&fixture.database).unwrap();
    let session = fixture.index(&mut store, &candidate);
    let query = SearchQuery {
        text: "中文".into(),
        session: Some(session.candidate.key.storage_key()),
        ..Default::default()
    };
    let mut cursor = SearchCursor::default();
    let mut offsets = Vec::new();
    loop {
        let page = store
            .search_page(&query, cursor, &Cancellation::default())
            .unwrap();
        offsets.extend(page.hits.into_iter().map(|hit| hit.offset));
        if let Some(next) = page.next {
            cursor = next;
        } else {
            break;
        }
    }
    assert_eq!(offsets.len(), 600);
    assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn source_is_read_only_and_raw_wrapper_remains_available() {
    let fixture = Fixture::new();
    let original = "<system_reminder>internal</system_reminder>\nUser text";
    let candidate = fixture.session(A, &[start(), msg("user.message", "", original)]);
    let bytes = fs::read(&candidate.events_path).unwrap();
    let modified = candidate
        .events_path
        .metadata()
        .unwrap()
        .modified()
        .unwrap();
    let mut store = Store::open(&fixture.database).unwrap();
    let session = fixture.index(&mut store, &candidate);
    let info = store
        .message_infos(&session.candidate.key.storage_key())
        .unwrap()
        .remove(0);
    assert_eq!(store.original_message(info.rowid).unwrap(), original);
    assert_eq!(store.message(info.rowid).unwrap().content, "User text");
    assert_eq!(fs::read(&candidate.events_path).unwrap(), bytes);
    assert_eq!(
        candidate
            .events_path
            .metadata()
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
}

#[test]
fn empty_start_time_and_empty_nested_workspace_use_valid_start_event_fallbacks() {
    let fixture = Fixture::new();
    let candidate = fixture.session(
        A,
        &[json!({"type":"session.start",
        "timestamp":"2026-01-01T00:00:00Z",
        "data":{"startTime":"","context":{"cwd":null},"cwd":"D:\\fallback"}})],
    );
    let summary = fixture
        .adapter
        .summarize(&candidate, &Cancellation::default())
        .unwrap();
    assert_eq!(summary.workspace.as_deref(), Some("D:\\fallback"));
    assert!(summary.created.value().is_some());
}

#[test]
fn replacing_a_file_does_not_reuse_old_search_hit_ids() {
    let fixture = Fixture::new();
    let candidate = fixture.session(A, &[start(), msg("user.message", "", "old")]);
    let mut store = Store::open(&fixture.database).unwrap();
    let session = fixture.index(&mut store, &candidate);
    let old = store
        .message_infos(&session.candidate.key.storage_key())
        .unwrap()[0]
        .rowid;
    let replacement = fixture.session(
        A,
        &[start(), msg("user.message", "", "new, longer content")],
    );
    fixture.index(&mut store, &replacement);
    assert!(store.message(old).is_err());
}

#[test]
fn agents_scan_on_independent_threads_and_results_share_one_time_order() {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    struct Gated {
        adapter: CopilotCli,
        gate: Arc<AtomicBool>,
    }
    impl AgentAdapter for Gated {
        fn source(&self) -> &Source {
            self.adapter.source()
        }
        fn discover(&self, cancel: &Cancellation) -> anyhow::Result<Discovery> {
            self.adapter.discover(cancel)
        }
        fn summarize(
            &self,
            candidate: &Candidate,
            cancel: &Cancellation,
        ) -> anyhow::Result<Session> {
            while !self.gate.load(Ordering::Relaxed) {
                cancel.check()?;
                std::thread::sleep(Duration::from_millis(1));
            }
            self.adapter.summarize(candidate, cancel)
        }
        fn read_batch(
            &self,
            candidate: &Candidate,
            cursor: u64,
            budget: usize,
            cancel: &Cancellation,
        ) -> anyhow::Result<ReadBatch> {
            self.adapter.read_batch(candidate, cursor, budget, cancel)
        }
    }
    let slow = Fixture::new();
    slow.session(
        A,
        &[
            start(),
            msg("assistant.message", "2026-03-01T00:00:00Z", "slow"),
        ],
    );
    let fast = Fixture::new();
    fast.session(
        B,
        &[
            start(),
            msg("assistant.message", "2026-02-01T00:00:00Z", "fast"),
        ],
    );
    let gate = Arc::new(AtomicBool::new(false));
    let scan = agent_chat_reader_core::scan::start(
        vec![
            Arc::new(Gated {
                adapter: slow.adapter.clone(),
                gate: gate.clone(),
            }),
            Arc::new(fast.adapter.clone()),
        ],
        fast.database.clone(),
    )
    .unwrap();
    let mut first = None;
    let mut finished = 0;
    while finished < 2 {
        match scan.events.recv_timeout(Duration::from_secs(5)).unwrap() {
            agent_chat_reader_core::scan::ScanEvent::Session(session) => {
                first.get_or_insert(session.candidate.key.native.clone());
                gate.store(true, Ordering::Relaxed);
            }
            agent_chat_reader_core::scan::ScanEvent::Finished { .. } => finished += 1,
            agent_chat_reader_core::scan::ScanEvent::Diagnostic(error) => {
                panic!("{}", error.description)
            }
            _ => {}
        }
    }
    assert_eq!(first.as_deref(), Some(B));
    let sessions = Store::open(&fast.database)
        .unwrap()
        .sessions(TimeMode::Latest)
        .unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].candidate.key.native, A);
}

#[test]
fn same_size_middle_rewrite_with_new_mtime_invalidates_the_index() {
    let fixture = Fixture::new();
    let body = format!(
        "{} OLD-MARKER {}",
        "prefix ".repeat(100),
        "suffix ".repeat(100)
    );
    let candidate = fixture.session(A, &[start(), msg("user.message", "", &body)]);
    let mut store = Store::open(&fixture.database).unwrap();
    fixture.index(&mut store, &candidate);
    let bytes = fs::read_to_string(&candidate.events_path)
        .unwrap()
        .replace("OLD-MARKER", "NEW-MARKER");
    fs::write(&candidate.events_path, bytes).unwrap();
    let file = fs::File::options()
        .write(true)
        .open(&candidate.events_path)
        .unwrap();
    file.set_times(
        fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2)),
    )
    .unwrap();
    let updated = fixture
        .adapter
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .remove(0);
    assert_eq!(updated.size, candidate.size);
    fixture.index(&mut store, &updated);
    for (text, count) in [("OLD-MARKER", 0), ("NEW-MARKER", 1)] {
        let page = store
            .search_page(
                &SearchQuery {
                    text: text.into(),
                    ..Default::default()
                },
                SearchCursor::default(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(page.hits.len(), count);
    }
}

#[test]
fn keyset_search_visits_every_message_when_all_sort_times_are_null() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.database).unwrap();
    for native in [A, B] {
        let mut events = vec![start()];
        events.extend((0..150).map(|index| msg("user.message", "", &format!("token {index}"))));
        let candidate = fixture.session(native, &events);
        fixture.index(&mut store, &candidate);
    }
    let query = SearchQuery {
        text: "token".into(),
        mode: TimeMode::FirstAssistant,
        ..Default::default()
    };
    let mut cursor = SearchCursor::default();
    let mut ids = Vec::new();
    loop {
        let page = store
            .search_page(&query, cursor, &Cancellation::default())
            .unwrap();
        ids.extend(page.hits.iter().map(|hit| hit.message_id));
        if let Some(next) = page.next {
            cursor = next;
        } else {
            break;
        }
    }
    assert_eq!(ids.len(), 300);
    let distinct: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(distinct.len(), 300);
}

#[test]
fn large_message_search_yields_a_byte_bounded_page_without_losing_remaining_hits() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.database).unwrap();
    let body = format!("findme {}", "x".repeat(1024 * 1024));
    let candidate = fixture.session(
        A,
        &[
            start(),
            msg("assistant.message", "", &body),
            msg("assistant.message", "", &body),
            msg("assistant.message", "", &body),
        ],
    );
    fixture.index(&mut store, &candidate);
    let query = SearchQuery {
        text: "findme".into(),
        ..Default::default()
    };
    let first = store
        .search_page(&query, SearchCursor::default(), &Cancellation::default())
        .unwrap();
    assert_eq!(first.hits.len(), 2);
    let second = store
        .search_page(&query, first.next.unwrap(), &Cancellation::default())
        .unwrap();
    assert_eq!(second.hits.len(), 1);
    assert_ne!(first.hits[1].message_id, second.hits[0].message_id);
}

#[test]
fn title_search_matches_session_names_before_content_and_honors_filters() {
    let fixture = Fixture::new();
    let candidate = fixture.session(
        A,
        &[
            start(),
            msg(
                "user.message",
                "2026-01-01T01:00:00Z",
                "synthetic body text",
            ),
        ],
    );
    let mut store = Store::open(&fixture.database).unwrap();
    let session = fixture.index(&mut store, &candidate);
    let key = candidate.key.storage_key();
    let cancel = Cancellation::default();
    let query = |text: &str| SearchQuery {
        text: text.into(),
        ..Default::default()
    };

    for text in ["SYNTHETIC SESSION", "ic se", "s"] {
        let hits = store.title_hits(&query(text), &cancel).unwrap();
        assert_eq!(hits.len(), 1, "{text}");
        assert_eq!(hits[0].kind, HitKind::Title);
        assert_eq!(hits[0].session, key);
        assert_eq!(hits[0].session_name, session.name);
    }
    assert!(
        store
            .title_hits(&query("absent"), &cancel)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .title_hits(&query("body text"), &cancel)
            .unwrap()
            .is_empty()
    );

    let excluded = [
        SearchQuery {
            role: Some(Role::User),
            ..query("synthetic")
        },
        SearchQuery {
            session: Some(key.clone()),
            ..query("synthetic")
        },
        SearchQuery {
            workspace: Some("nowhere".into()),
            ..query("synthetic")
        },
        SearchQuery {
            members: Some(Vec::new()),
            ..query("synthetic")
        },
        SearchQuery {
            since: Some(chrono::Utc::now() + chrono::Duration::days(1)),
            ..query("synthetic")
        },
    ];
    for query in &excluded {
        assert!(store.title_hits(query, &cancel).unwrap().is_empty());
    }

    // The snapshot lists title hits ahead of the content hits of the same query.
    let directory = tempfile::tempdir().unwrap();
    let snapshot = store
        .with_read_snapshot(|source| {
            SearchSnapshot::capture(source, &query("synthetic"), directory.path(), &cancel)
        })
        .unwrap();
    let (hits, _) = snapshot.page(0).unwrap();
    let kinds: Vec<_> = hits.iter().map(|hit| hit.kind).collect();
    assert_eq!(kinds, [HitKind::Title, HitKind::Message]);
}

#[test]
fn hits_without_a_kind_deserialize_as_message_hits() {
    let hit: SearchHit = serde_json::from_value(json!({
        "session": "s", "message_id": 1, "ordinal": 0, "role": "User",
        "snippet": "x", "offset": 0
    }))
    .unwrap();
    assert_eq!(hit.kind, HitKind::Message);
}

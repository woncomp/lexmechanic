use agent_chat_reader_core::{
    agents::{
        self, AgentAdapter, copilot_app::CopilotApp, copilot_cli::CopilotCli, vscode::VsCode,
    },
    model::*,
    scan::{self, ScanEvent},
    search::{SearchCursor, SearchQuery},
    store::Store,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;

const CLI: &str = "00000000-0000-4000-8000-000000000001";
const APP: &str = "00000000-0000-4000-8000-000000000002";
const CODE: &str = "00000000-0000-4000-8000-000000000003";
const MISSING: &str = "00000000-0000-4000-8000-000000000004";

fn write_jsonl(path: &Path, events: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::File::create(path).unwrap();
    for event in events {
        writeln!(file, "{event}").unwrap();
    }
}

fn append_jsonl(path: &Path, event: Value) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{event}").unwrap();
    file.set_times(
        fs::FileTimes::new().set_modified(std::time::SystemTime::now() + Duration::from_secs(2)),
    )
    .unwrap();
}

struct Fixtures {
    _directory: TempDir,
    copilot: PathBuf,
    code: PathBuf,
    index: PathBuf,
}

impl Fixtures {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let copilot = directory.path().join("copilot");
        let code = directory.path().join("Code").join("User");
        fs::create_dir_all(copilot.join("session-state")).unwrap();
        fs::create_dir_all(
            code.join("workspaceStorage")
                .join("one")
                .join("chatSessions"),
        )
        .unwrap();
        fs::create_dir_all(code.join("globalStorage").join("github.copilot-chat")).unwrap();
        let db = Connection::open(copilot.join("data.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE sessions(id TEXT,title TEXT);
            CREATE TABLE workspaces(session_id TEXT,name TEXT);",
        )
        .unwrap();
        for id in [APP, MISSING] {
            db.execute("INSERT INTO sessions VALUES(?1,'Fallback App title')", [id])
                .unwrap();
        }
        db.execute(
            "INSERT INTO workspaces VALUES(?1,'App conversation name')",
            [APP],
        )
        .unwrap();
        drop(db);
        let db = Connection::open(copilot.join("session-store.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE sessions(id TEXT,cwd TEXT);
            CREATE TABLE turns(session_id TEXT,timestamp TEXT);",
        )
        .unwrap();
        db.execute("INSERT INTO sessions VALUES(?1,'D:\\shared-app')", [APP])
            .unwrap();
        db.execute("INSERT INTO sessions VALUES(?1,'D:\\archived')", [MISSING])
            .unwrap();
        db.execute(
            "INSERT INTO turns VALUES(?1,'2026-03-01 01:00:00')",
            [MISSING],
        )
        .unwrap();
        drop(db);
        for id in [CLI, APP] {
            let path = copilot.join("session-state").join(id).join("events.jsonl");
            write_jsonl(
                &path,
                &[
                    json!({"type":"session.start","timestamp":"2026-01-01T00:00:00Z",
                    "data":{"context":{"cwd":"D:\\event-workspace"}}}),
                    json!({"id":"user-event","type":"user.message","timestamp":"2026-01-02T00:00:00Z",
                    "data":{"content":"User question 中文"}}),
                    json!({"id":"assistant-event","type":"assistant.message","timestamp":"2026-01-03T00:00:00Z",
                    "data":{"content":"Assistant response"}}),
                    json!({"type":"assistant.message","agentId":"child","timestamp":"2026-02-01T00:00:00Z",
                    "data":{"content":"Not visible child"}}),
                ],
            );
            // App membership, not this CLI marker, must determine the App identity.
            fs::write(
                path.with_file_name("workspace.yaml"),
                "client_name: github/cli\nname: CLI name\ncwd: 'D:\\yaml-app'\n",
            )
            .unwrap();
        }
        let db = Connection::open(agents::vscode::session_store(&code)).unwrap();
        db.execute_batch("CREATE TABLE sessions(id TEXT,cwd TEXT,host_type TEXT,agent_name TEXT);")
            .unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,'D:\\vscode-workspace','vscode','GitHub Copilot Chat')",
            [CODE],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,'D:\\other','cli','GitHub Copilot Chat')",
            [CLI],
        )
        .unwrap();
        drop(db);
        let title_db = Connection::open(
            code.join("workspaceStorage")
                .join("one")
                .join("state.vscdb"),
        )
        .unwrap();
        title_db
            .execute_batch("CREATE TABLE ItemTable(key TEXT,value TEXT);")
            .unwrap();
        title_db.execute("INSERT INTO ItemTable VALUES('chat.ChatSessionStore.index',?1)",
            [json!({"entries":{CODE:{"title":"VS Code indexed title","lastMessageDate":1893456000000_i64}}}).to_string()]).unwrap();
        drop(title_db);
        let transcript = code
            .join("workspaceStorage")
            .join("one")
            .join("chatSessions")
            .join(format!("{CODE}.jsonl"));
        write_jsonl(
            &transcript,
            &[
                json!({"kind":0,"v":{"creationDate":1767225600000_i64,"customTitle":"Snapshot title","requests":[]}}),
                json!({"kind":2,"k":["requests"],"v":[{
                    "requestId":"request-a","timestamp":1767312000000_i64,
                    "responseTimestamp":1767398400000_i64,"message":{"text":"VS Code question 中文"},
                    "response":[{"value":"Visible "},{"kind":"toolInvocation","value":"Hidden tool"}]
                },{
                    "requestId":"hidden","hiddenFromTranscript":true,"message":{"text":"Hidden user"},
                    "response":[{"value":"Hidden response"}]
                }]}),
                json!({"kind":2,"k":["requests",0,"response"],"v":[{"value":"answer"}]}),
                json!({"kind":1,"k":["requests","0","message","text"],"v":"Edited VS Code question 中文"}),
            ],
        );
        Self {
            index: directory.path().join("reader.sqlite"),
            _directory: directory,
            copilot,
            code,
        }
    }

    fn transcript(&self) -> PathBuf {
        self.code
            .join("workspaceStorage")
            .join("one")
            .join("chatSessions")
            .join(format!("{CODE}.jsonl"))
    }

    fn index(
        &self,
        adapter: &dyn AgentAdapter,
        candidate: &Candidate,
        store: &mut Store,
    ) -> Session {
        let cancel = Cancellation::default();
        let mut session = adapter.summarize(candidate, &cancel).unwrap();
        store.upsert_session(&session).unwrap();
        let mut cursor = store.resume_cursor(&session).unwrap();
        loop {
            let batch = adapter.read_batch(candidate, cursor, 256, &cancel).unwrap();
            store.commit_batch(&mut session, &batch).unwrap();
            cursor = batch.cursor;
            if batch.eof {
                break;
            }
        }
        session
    }
}

#[test]
fn copilot_app_joins_names_and_workspaces_without_duplicating_cli_sessions() {
    let fixtures = Fixtures::new();
    let cli = CopilotCli::new(&fixtures.copilot).unwrap();
    let app = CopilotApp::new(&fixtures.copilot).unwrap();
    let cancel = Cancellation::default();
    let cli_discovery = cli.discover(&cancel).unwrap();
    assert!(cli_discovery.diagnostics.is_empty());
    assert_eq!(cli_discovery.candidates.len(), 1);
    assert_eq!(cli_discovery.candidates[0].key.native, CLI);
    let discovery = app.discover(&cancel).unwrap();
    assert_eq!(discovery.candidates.len(), 2);
    let candidate = discovery
        .candidates
        .iter()
        .find(|candidate| candidate.key.native == APP)
        .unwrap();
    let mut store = Store::open(&fixtures.index).unwrap();
    let session = fixtures.index(&app, candidate, &mut store);
    assert_eq!(session.name, "App conversation name");
    assert_eq!(session.workspace.as_deref(), Some("D:\\yaml-app"));
    let messages = store.message_infos(&candidate.key.storage_key()).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(
        store.original_message(messages[0].rowid).unwrap(),
        "User question 中文"
    );
    let missing = app
        .summarize(
            discovery
                .candidates
                .iter()
                .find(|candidate| candidate.key.native == MISSING)
                .unwrap(),
            &cancel,
        )
        .unwrap();
    assert!(!missing.available);
    assert_eq!(missing.workspace.as_deref(), Some("D:\\archived"));
    assert!(missing.latest.value().is_some());
    assert_eq!(missing.first_assistant, MessageTime::Missing);
}

#[test]
fn cli_continuation_marker_overrides_origin_and_app_registration() {
    for (native, registered) in [(CLI, false), (APP, true)] {
        for origin in [Some("github/autopilot"), None] {
            let fixtures = Fixtures::new();
            let directory = fixtures.copilot.join("session-state").join(native);
            let metadata = format!(
                "{}last_client_name: github/cli\nname: Continued in CLI\n",
                origin
                    .map(|client| format!("client_name: {client}\n"))
                    .unwrap_or_default(),
            );
            fs::write(directory.join("workspace.yaml"), &metadata).unwrap();
            let cli = CopilotCli::new(&fixtures.copilot).unwrap();
            let app = CopilotApp::new(&fixtures.copilot).unwrap();
            let cancel = Cancellation::default();
            let cli_discovery = cli.discover(&cancel).unwrap();
            let candidate = cli_discovery
                .candidates
                .iter()
                .find(|candidate| candidate.key.native == native)
                .expect("A session last used by CLI must be discovered as CLI");
            assert!(cli_discovery.diagnostics.is_empty());
            assert_eq!(candidate.key.agent, "copilot-cli");
            let app_discovery = app.discover(&cancel).unwrap();
            assert!(app_discovery.diagnostics.is_empty());
            assert!(
                !app_discovery
                    .candidates
                    .iter()
                    .any(|candidate| candidate.key.native == native)
            );
            assert!(
                !app_discovery
                    .reclassified
                    .iter()
                    .any(|replacement| replacement.current.native == native)
            );
            let mut store = Store::open(&fixtures.index).unwrap();
            let summary = fixtures.index(&cli, candidate, &mut store);
            assert_eq!(summary.name, "Continued in CLI");
            assert_eq!(
                store
                    .message_infos(&candidate.key.storage_key())
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(
                fs::read_to_string(directory.join("workspace.yaml")).unwrap(),
                metadata
            );
            if registered {
                assert!(
                    cli_discovery
                        .reclassified
                        .iter()
                        .any(|replacement| replacement.previous.agent == "copilot-app"
                            && replacement.current == candidate.key)
                );
            }
        }
    }
}

#[test]
fn app_origin_without_cli_continuation_is_not_adopted_by_cli() {
    for last_client in [None, Some("github/autopilot")] {
        let fixtures = Fixtures::new();
        for native in [CLI, APP] {
            let metadata = format!(
                "client_name: github/autopilot\n{}name: App origin\n",
                last_client
                    .map(|client| format!("last_client_name: {client}\n"))
                    .unwrap_or_default()
            );
            fs::write(
                fixtures
                    .copilot
                    .join("session-state")
                    .join(native)
                    .join("workspace.yaml"),
                metadata,
            )
            .unwrap();
        }
        let cli = CopilotCli::new(&fixtures.copilot).unwrap();
        let app = CopilotApp::new(&fixtures.copilot).unwrap();
        let cancel = Cancellation::default();
        let cli_discovery = cli.discover(&cancel).unwrap();
        assert!(cli_discovery.candidates.is_empty());
        assert!(cli_discovery.diagnostics.is_empty());
        assert!(cli_discovery.reclassified.is_empty());
        let app_discovery = app.discover(&cancel).unwrap();
        assert!(
            app_discovery
                .candidates
                .iter()
                .any(|candidate| candidate.key.native == APP)
        );
    }
}

#[test]
fn app_to_cli_continuation_reclassifies_cached_messages_without_log_changes_or_duplicates() {
    let fixtures = Fixtures::new();
    let app = CopilotApp::new(&fixtures.copilot).unwrap();
    let original = app
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .into_iter()
        .find(|candidate| candidate.key.native == APP)
        .unwrap();
    let old_messages = {
        let mut store = Store::open(&fixtures.index).unwrap();
        fixtures.index(&app, &original, &mut store);
        store.message_infos(&original.key.storage_key()).unwrap()
    };
    let log_before = fs::read(&original.events_path).unwrap();
    let modified_before = original.events_path.metadata().unwrap().modified().unwrap();
    fs::write(
        original.events_path.with_file_name("workspace.yaml"),
        "client_name: github/autopilot\nlast_client_name: github/cli\nname: Continued in CLI\n",
    )
    .unwrap();
    let cli = CopilotCli::new(&fixtures.copilot).unwrap();
    let current = cli
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .into_iter()
        .find(|candidate| candidate.key.native == APP)
        .unwrap();
    let handle = scan::start(vec![Arc::new(cli), Arc::new(app)], fixtures.index.clone()).unwrap();
    let mut finished = 0;
    let mut replacements = Vec::new();
    while finished < 2 {
        match handle.events.recv_timeout(Duration::from_secs(10)).unwrap() {
            ScanEvent::Finished { .. } => finished += 1,
            ScanEvent::Reclassified(replacement) => replacements.push(replacement),
            ScanEvent::Diagnostic(error) => panic!("{}", error.description),
            _ => {}
        }
    }
    drop(handle);
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0].previous, original.key);
    assert_eq!(replacements[0].current, current.key);
    let store = Store::open(&fixtures.index).unwrap();
    assert!(
        store
            .session(&original.key.storage_key())
            .unwrap()
            .is_none()
    );
    let session = store.session(&current.key.storage_key()).unwrap().unwrap();
    assert_eq!(session.name, "Continued in CLI");
    assert!(session.available && session.indexed);
    let messages = store.message_infos(&current.key.storage_key()).unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.rowid)
            .collect::<Vec<_>>(),
        old_messages
            .iter()
            .map(|message| message.rowid)
            .collect::<Vec<_>>()
    );
    let all = store.sessions(TimeMode::Latest).unwrap();
    assert_eq!(
        all.iter()
            .filter(|session| session.candidate.key.native == APP)
            .count(),
        1
    );
    let hits = store
        .search_page(
            &SearchQuery {
                text: "中文".into(),
                session: Some(current.key.storage_key()),
                ..Default::default()
            },
            SearchCursor::default(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(hits.hits.len(), 1);
    assert_eq!(
        store.original_message(messages[0].rowid).unwrap(),
        "User question 中文"
    );
    assert_eq!(fs::read(&original.events_path).unwrap(), log_before);
    assert_eq!(
        original.events_path.metadata().unwrap().modified().unwrap(),
        modified_before
    );
}

#[test]
fn vscode_replays_snapshot_sets_and_appends_using_only_visible_conversation() {
    let fixtures = Fixtures::new();
    let adapter = VsCode::new(&fixtures.code).unwrap();
    let discovery = adapter.discover(&Cancellation::default()).unwrap();
    assert!(discovery.diagnostics.is_empty());
    assert_eq!(discovery.candidates.len(), 1);
    let candidate = &discovery.candidates[0];
    assert_eq!(candidate.read_mode, ReadMode::Snapshot);
    let mut store = Store::open(&fixtures.index).unwrap();
    let session = fixtures.index(&adapter, candidate, &mut store);
    assert_eq!(session.name, "VS Code indexed title");
    assert_eq!(session.workspace.as_deref(), Some("D:\\vscode-workspace"));
    assert_eq!(
        session.first_assistant.value().unwrap().timestamp_millis(),
        1767398400000
    );
    let infos = store.message_infos(&candidate.key.storage_key()).unwrap();
    assert_eq!(infos.len(), 2);
    assert_eq!(
        store.message(infos[0].rowid).unwrap().content,
        "Edited VS Code question 中文"
    );
    assert_eq!(
        store.message(infos[1].rowid).unwrap().content,
        "Visible answer"
    );
    assert_eq!(
        store.original_message(infos[1].rowid).unwrap(),
        "Visible answer"
    );
    for (query, expected) in [("中文", 1), ("Visible answer", 1), ("Hidden", 0)] {
        let page = store
            .search_page(
                &SearchQuery {
                    text: query.into(),
                    ..Default::default()
                },
                SearchCursor::default(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(page.hits.len(), expected, "{query}");
    }
}

#[test]
fn vscode_appended_deltas_replace_old_indexed_responses_instead_of_duplicating_them() {
    let fixtures = Fixtures::new();
    let adapter = VsCode::new(&fixtures.code).unwrap();
    let candidate = adapter
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .remove(0);
    let mut store = Store::open(&fixtures.index).unwrap();
    fixtures.index(&adapter, &candidate, &mut store);
    let old = store.message_infos(&candidate.key.storage_key()).unwrap()[1].rowid;
    append_jsonl(
        &fixtures.transcript(),
        json!({"kind":1,"k":["requests",0,"response",0,"value"],"v":"Replaced "}),
    );
    append_jsonl(
        &fixtures.transcript(),
        json!({"kind":1,"k":["requests",0,"responseTimestamp"],"v":1767484800000_i64}),
    );
    let changed = adapter
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .remove(0);
    assert!(changed.size > candidate.size);
    let session = fixtures.index(&adapter, &changed, &mut store);
    assert!(session.indexed);
    let infos = store.message_infos(&changed.key.storage_key()).unwrap();
    assert_eq!(infos.len(), 2);
    assert_eq!(
        store.message(infos[1].rowid).unwrap().content,
        "Replaced answer"
    );
    assert!(store.message(old).is_err());
    assert_eq!(
        session.latest.value().unwrap().timestamp_millis(),
        1767484800000
    );
    assert!(
        store
            .search_page(
                &SearchQuery {
                    text: "Visible".into(),
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
fn vscode_incomplete_tail_waits_and_invalid_dependent_delta_is_not_silently_skipped() {
    let fixtures = Fixtures::new();
    let path = fixtures.transcript();
    let complete = path.metadata().unwrap().len();
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    write!(
        file,
        "{{\"kind\":1,\"k\":[\"requests\",0,\"responseTimestamp\"],\"v\":"
    )
    .unwrap();
    drop(file);
    let adapter = VsCode::new(&fixtures.code).unwrap();
    let candidate = adapter
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .remove(0);
    let batch = adapter
        .read_batch(&candidate, 0, 256, &Cancellation::default())
        .unwrap();
    assert!(batch.incomplete_tail);
    assert_eq!(batch.cursor, complete);
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "1767484800000}}").unwrap();
    drop(file);
    let candidate = adapter
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .remove(0);
    assert!(
        !adapter
            .read_batch(&candidate, 0, 256, &Cancellation::default())
            .unwrap()
            .incomplete_tail
    );
    append_jsonl(
        &path,
        json!({"kind":2,"k":["requests",0,"message","text"],"v":"not-an-array"}),
    );
    let candidate = adapter
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .remove(0);
    assert!(
        adapter
            .summarize(&candidate, &Cancellation::default())
            .unwrap_err()
            .to_string()
            .contains("invalid delta")
    );
}

#[test]
fn vscode_duplicate_transcript_uses_newest_copy_and_invalid_first_assistant_stays_invalid() {
    let fixtures = Fixtures::new();
    let second = fixtures
        .code
        .join("workspaceStorage")
        .join("two")
        .join("chatSessions")
        .join(format!("{CODE}.jsonl"));
    write_jsonl(
        &second,
        &[json!({"kind":0,"v":{"customTitle":"New copy","requests":[
            {"message":{"text":"newest"},"responseTimestamp":"bad timestamp","response":[{"value":"first"}]},
            {"responseTimestamp":1767484800000_i64,"response":[{"value":"second"}]}
        ]}})],
    );
    fs::File::options()
        .write(true)
        .open(&second)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(10)),
        )
        .unwrap();
    let adapter = VsCode::new(&fixtures.code).unwrap();
    let discovery = adapter.discover(&Cancellation::default()).unwrap();
    assert_eq!(discovery.candidates.len(), 1);
    assert_eq!(discovery.diagnostics.len(), 1);
    assert_eq!(
        discovery.candidates[0].events_path,
        fs::canonicalize(second).unwrap()
    );
    let mut store = Store::open(&fixtures.index).unwrap();
    let session = fixtures.index(&adapter, &discovery.candidates[0], &mut store);
    assert_eq!(session.name, "New copy");
    assert_eq!(session.first_assistant, MessageTime::Invalid);
    assert!(session.time(TimeMode::FirstAssistant).is_none());
}

#[test]
fn all_three_agents_share_scan_index_and_time_sort_without_writing_source_databases() {
    let fixtures = Fixtures::new();
    let app = CopilotApp::new(&fixtures.copilot).unwrap();
    let cli = CopilotCli::new(&fixtures.copilot).unwrap();
    let mut legacy = app
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .into_iter()
        .find(|candidate| candidate.key.native == APP)
        .unwrap();
    legacy.key.agent = cli.source().agent.clone();
    legacy.key.source = cli.source().id.clone();
    {
        let mut store = Store::open(&fixtures.index).unwrap();
        fixtures.index(&cli, &legacy, &mut store);
    }
    let database_paths = [
        fixtures.copilot.join("data.db"),
        fixtures.copilot.join("session-store.db"),
        agents::vscode::session_store(&fixtures.code),
        fixtures
            .code
            .join("workspaceStorage")
            .join("one")
            .join("state.vscdb"),
    ];
    let before: Vec<_> = database_paths
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();
    let adapters: Vec<Arc<dyn AgentAdapter>> = vec![
        Arc::new(CopilotCli::new(&fixtures.copilot).unwrap()),
        Arc::new(CopilotApp::new(&fixtures.copilot).unwrap()),
        Arc::new(VsCode::new(&fixtures.code).unwrap()),
    ];
    let handle = scan::start(adapters, fixtures.index.clone()).unwrap();
    let mut finished = 0;
    let mut reclassified = 0;
    while finished < 3 {
        match handle.events.recv_timeout(Duration::from_secs(10)).unwrap() {
            ScanEvent::Finished { .. } => finished += 1,
            ScanEvent::Reclassified(_) => reclassified += 1,
            ScanEvent::Diagnostic(error) => panic!("{}", error.description),
            _ => {}
        }
    }
    drop(handle);
    let store = Store::open(&fixtures.index).unwrap();
    let sessions = store.sessions(TimeMode::FirstAssistant).unwrap();
    assert_eq!(sessions.len(), 4);
    assert_eq!(reclassified, 1);
    assert!(store.session(&legacy.key.storage_key()).unwrap().is_none());
    assert_eq!(sessions.iter().filter(|session| session.indexed).count(), 3);
    assert_eq!(sessions.last().unwrap().candidate.key.native, MISSING);
    let page = store
        .search_page(
            &SearchQuery {
                text: "中文".into(),
                ..Default::default()
            },
            SearchCursor::default(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(page.hits.len(), 3);
    for (path, expected) in database_paths.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), expected);
        let connection = Connection::open(path).unwrap();
        let check: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(check, "ok");
    }
}

#[test]
fn app_workspace_and_title_changes_are_visible_without_event_log_modification() {
    let fixtures = Fixtures::new();
    let adapter = CopilotApp::new(&fixtures.copilot).unwrap();
    let first = adapter
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .into_iter()
        .find(|candidate| candidate.key.native == APP)
        .unwrap();
    let mut store = Store::open(&fixtures.index).unwrap();
    let indexed = fixtures.index(&adapter, &first, &mut store);
    let db = Connection::open(fixtures.copilot.join("data.db")).unwrap();
    db.execute(
        "UPDATE workspaces SET name=?1 WHERE session_id=?2",
        params!["Renamed App session", APP],
    )
    .unwrap();
    drop(db);
    let changed = adapter
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .into_iter()
        .find(|candidate| candidate.key.native == APP)
        .unwrap();
    assert_eq!(first, changed);
    let updated = adapter
        .summarize(&changed, &Cancellation::default())
        .unwrap();
    assert_eq!(updated.name, "Renamed App session");
    assert_eq!(indexed.latest, updated.latest);
}

#[test]
fn unreadable_app_reclassification_keeps_metadata_but_excludes_cached_body_from_index() {
    let fixtures = Fixtures::new();
    let app = CopilotApp::new(&fixtures.copilot).unwrap();
    let cli = CopilotCli::new(&fixtures.copilot).unwrap();
    let mut legacy = app
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .into_iter()
        .find(|candidate| candidate.key.native == APP)
        .unwrap();
    let current_key = legacy.key.clone();
    legacy.key.agent = cli.source().agent.clone();
    legacy.key.source = cli.source().id.clone();
    let old_rowid = {
        let mut store = Store::open(&fixtures.index).unwrap();
        fixtures.index(&cli, &legacy, &mut store);
        store.message_infos(&legacy.key.storage_key()).unwrap()[0].rowid
    };
    fs::remove_file(&legacy.events_path).unwrap();
    let scan = scan::start(vec![Arc::new(app)], fixtures.index.clone()).unwrap();
    loop {
        match scan.events.recv_timeout(Duration::from_secs(10)).unwrap() {
            ScanEvent::Finished { .. } => break,
            ScanEvent::Diagnostic(error) => panic!("{}", error.description),
            _ => {}
        }
    }
    drop(scan);
    let store = Store::open(&fixtures.index).unwrap();
    assert!(store.session(&legacy.key.storage_key()).unwrap().is_none());
    let current = store.session(&current_key.storage_key()).unwrap().unwrap();
    assert!(!current.available);
    assert!(!current.indexed);
    assert!(current.content_error.is_some());
    assert_eq!(current.name, "App conversation name");
    assert_eq!(
        store
            .message_infos(&current_key.storage_key())
            .unwrap()
            .len(),
        0
    );
    assert!(store.message(old_rowid).is_err());
    assert_eq!(
        store
            .search_page(
                &SearchQuery {
                    text: "中文".into(),
                    ..Default::default()
                },
                SearchCursor::default(),
                &Cancellation::default()
            )
            .unwrap()
            .hits
            .len(),
        0
    );
}

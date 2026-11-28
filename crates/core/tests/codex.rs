use agent_chat_reader_core::{
    agents::{self, AgentAdapter, codex::Codex},
    model::*,
    scan::{self, ScanEvent},
    search::{SearchCursor, SearchQuery},
    store::Store,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{fs, io::Write, path::Path, sync::Arc, time::Duration};
use tempfile::TempDir;

const USER: &str = "40000000-0000-4000-8000-000000000001";
const PAGED: &str = "40000000-0000-4000-8000-000000000002";
const PAGE: &str = "40000000-0000-4000-8000-0000000000b1";
const ARCHIVED: &str = "40000000-0000-4000-8000-000000000003";
const GUARDIAN: &str = "40000000-0000-4000-8000-000000000004";
const SPAWNED: &str = "40000000-0000-4000-8000-000000000005";
const AGENT_HOST: &str = "40000000-0000-4000-8000-000000000006";
const DB_SPAWNED: &str = "40000000-0000-4000-8000-000000000007";

fn write_lines(path: &Path, lines: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::File::create(path).unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

fn meta(thread: &str, ordinal: u64, originator: &str, source: Value, base: Option<Value>) -> Value {
    let mut payload = json!({"id": thread, "timestamp": "2026-11-20T03:48:02.389Z",
        "cwd": "\\\\?\\d:\\transcript\\codex", "originator": originator, "source": source});
    if let Some(base) = base {
        payload["history_base"] = base;
    }
    json!({"type": "session_meta", "timestamp": "2026-11-20T03:48:02.389Z",
        "ordinal": ordinal, "payload": payload})
}

fn item(ordinal: u64, kind: &str, text: &str) -> Value {
    let part = if kind == "UserMessage" {
        "text"
    } else {
        "Text"
    };
    json!({"type": "event_msg", "timestamp": format!("2026-11-20T03:49:{ordinal:02}.000Z"),
        "ordinal": ordinal,
        "payload": {"type": "item_completed", "item": {"type": kind, "id": format!("i{ordinal}"),
            "content": [{"type": part, "text": text}]}}})
}

fn conversation(thread: &str, originator: &str, source: Value, prompt: &str) -> Vec<Value> {
    vec![
        meta(thread, 0, originator, source, None),
        item(1, "UserMessage", prompt),
        item(2, "AgentMessage", "reply"),
    ]
}

struct Fixtures {
    _directory: TempDir,
    home: std::path::PathBuf,
    index: std::path::PathBuf,
}

impl Fixtures {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("codex");
        let day = home.join("sessions/2026/11/20");
        let cli = || json!("cli");

        write_lines(
            &day.join(format!("rollout-2026-11-20T09-00-00-{USER}.jsonl")),
            &conversation(USER, "codex_cli_rs", cli(), "plain 中文 prompt"),
        );

        // A page continues the main file from the byte where its own share of the history ends.
        let mut main = conversation(PAGED, "codex_cli_rs", cli(), "paged first prompt");
        main.push(item(3, "UserMessage", "aborted prompt"));
        let offset: u64 = main[..3]
            .iter()
            .map(|line| format!("{line}\n").len() as u64)
            .sum();
        write_lines(
            &day.join(format!("rollout-2026-11-20T10-00-00-{PAGED}.jsonl")),
            &main,
        );
        write_lines(
            &day.join(format!("rollout-2026-11-20T11-00-00-{PAGED}_{PAGE}.jsonl")),
            &[
                meta(
                    PAGED,
                    3,
                    "codex_cli_rs",
                    cli(),
                    Some(json!({"thread_id": PAGED, "end_ordinal_exclusive": 3,
                        "end_byte_offset": offset})),
                ),
                item(4, "UserMessage", "paged second prompt"),
                item(5, "AgentMessage", "paged second reply"),
            ],
        );

        // Sessions of other kinds that live in the same folders.
        write_lines(
            &day.join(format!("rollout-2026-11-20T12-00-00-{GUARDIAN}.jsonl")),
            &conversation(
                GUARDIAN,
                "codex_cli_rs",
                json!({"subagent": {"other": "guardian"}}),
                "guardian review",
            ),
        );
        write_lines(
            &day.join(format!("rollout-2026-11-20T12-30-00-{SPAWNED}.jsonl")),
            &conversation(
                SPAWNED,
                "codex_cli_rs",
                json!({"subagent": {"thread_spawn": {"parent_thread_id": USER, "depth": 1}}}),
                "spawned helper",
            ),
        );
        write_lines(
            &day.join(format!("rollout-2026-11-20T13-00-00-{AGENT_HOST}.jsonl")),
            &conversation(
                AGENT_HOST,
                "vscode_agent_host",
                json!("vscode"),
                "agents window",
            ),
        );

        // Codex's own database is authoritative: this rollout looks like an ordinary thread.
        write_lines(
            &day.join(format!("rollout-2026-11-20T13-30-00-{DB_SPAWNED}.jsonl")),
            &conversation(
                DB_SPAWNED,
                "codex_cli_rs",
                cli(),
                "spawned according to the database",
            ),
        );

        // An archived rollout is compressed.
        let archived: String = conversation(
            ARCHIVED,
            "codex_vscode",
            json!("vscode"),
            "archived 中文 prompt",
        )
        .iter()
        .map(|line| format!("{line}\n"))
        .collect();
        let archive = home.join("archived_sessions");
        fs::create_dir_all(&archive).unwrap();
        fs::write(
            archive.join(format!("rollout-2026-09-04T11-56-03-{ARCHIVED}.jsonl.zst")),
            ruzstd::encoding::compress_to_vec(
                archived.as_bytes(),
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        )
        .unwrap();

        // Names: the app's index wins over the state database, and only its newest version counts.
        fs::write(
            home.join("session_index.jsonl"),
            format!(
                "{}\n{}\nnot json\n",
                json!({"id": USER, "thread_name": "Old app name"}),
                json!({"id": USER, "thread_name": "Renamed in app"}),
            ),
        )
        .unwrap();
        let spawned = json!({"subagent": {"thread_spawn": {"parent_thread_id": USER}}}).to_string();
        for (version, rows) in [
            (
                5,
                vec![
                    (USER, "Ignored database name", "cli", ""),
                    (ARCHIVED, "From state db", "vscode", ""),
                    (DB_SPAWNED, "", spawned.as_str(), ""),
                ],
            ),
            (4, vec![(PAGED, "From an old database version", "cli", "")]),
        ] {
            let connection =
                Connection::open(home.join(format!("state_{version}.sqlite"))).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE threads (id TEXT PRIMARY KEY, name TEXT, source TEXT, originator TEXT)",
                )
                .unwrap();
            for (id, name, source, originator) in rows {
                connection
                    .execute(
                        "INSERT INTO threads VALUES (?1, ?2, ?3, ?4)",
                        [id, name, source, originator],
                    )
                    .unwrap();
            }
        }

        Self {
            index: directory.path().join("index.db"),
            home,
            _directory: directory,
        }
    }

    fn discovered(&self) -> (Codex, Vec<Candidate>) {
        let adapter = Codex::new(&self.home).unwrap();
        let mut found = adapter
            .discover(&Cancellation::default())
            .unwrap()
            .candidates;
        found.sort_by(|a, b| a.key.native.cmp(&b.key.native));
        (adapter, found)
    }
}

fn contents(adapter: &Codex, candidate: &Candidate) -> Vec<String> {
    let batch = adapter
        .read_batch(candidate, 0, 1 << 20, &Cancellation::default())
        .unwrap();
    assert!(batch.eof && batch.diagnostics.is_empty());
    batch.messages.into_iter().map(|m| m.content).collect()
}

#[test]
fn lists_only_the_users_own_threads_with_their_names_and_merged_pages() {
    let fixtures = Fixtures::new();
    let (adapter, found) = fixtures.discovered();
    let natives: Vec<_> = found.iter().map(|c| c.key.native.as_str()).collect();
    // Review and spawned threads are not conversations; the Agents window has its own adapter.
    // A thread that Codex's database calls spawned is skipped even if its rollout looks ordinary.
    assert_eq!(natives, [USER, PAGED, ARCHIVED]);
    assert!(found.iter().all(|c| c.key.agent == "codex"));

    let cancel = Cancellation::default();
    let named = |candidate: &Candidate| adapter.summarize(candidate, &cancel).unwrap();
    let user = named(&found[0]);
    assert_eq!(user.name, "Renamed in app");
    assert_eq!(user.workspace.as_deref(), Some("d:\\transcript\\codex"));
    assert_eq!(found[0].read_mode, ReadMode::Append);
    assert_eq!(
        contents(&adapter, &found[0]),
        ["plain 中文 prompt", "reply"]
    );

    // The old database version is ignored, so the first prompt names the thread.
    let paged = named(&found[1]);
    assert_eq!(paged.name, "paged first prompt");
    assert_eq!(found[1].read_mode, ReadMode::Snapshot);
    assert_eq!(
        contents(&adapter, &found[1]),
        [
            "paged first prompt",
            "reply",
            "paged second prompt",
            "paged second reply"
        ]
    );

    let archived = named(&found[2]);
    assert_eq!(archived.name, "From state db");
    assert_eq!(found[2].read_mode, ReadMode::Snapshot);
    assert_eq!(
        contents(&adapter, &found[2]),
        ["archived 中文 prompt", "reply"]
    );
}

#[test]
fn raw_view_and_probe_work_for_plain_paged_and_compressed_threads() {
    let fixtures = Fixtures::new();
    let (adapter, found) = fixtures.discovered();
    for candidate in &found {
        assert_eq!(
            &adapter.probe(candidate, &Cancellation::default()).unwrap(),
            candidate
        );
        let opened = agents::adapter_for(candidate).unwrap();
        let batch = opened
            .read_batch(candidate, 0, 1 << 20, &Cancellation::default())
            .unwrap();
        for message in &batch.messages {
            assert_eq!(
                opened.original_message(candidate, message).unwrap(),
                message.content
            );
        }
    }
    assert_eq!(agents::agent_name("codex"), "Codex");
}

#[test]
fn scan_indexes_the_threads_without_writing_the_codex_files() {
    let fixtures = Fixtures::new();
    let state = fixtures.home.join("state_5.sqlite");
    let before = fs::read(&state).unwrap();
    let handle = scan::start(
        vec![Arc::new(Codex::new(&fixtures.home).unwrap()) as Arc<dyn AgentAdapter>],
        fixtures.index.clone(),
    )
    .unwrap();
    loop {
        if let ScanEvent::Finished { error, .. } =
            handle.events.recv_timeout(Duration::from_secs(10)).unwrap()
        {
            assert!(error.is_none(), "{error:?}");
            break;
        }
    }
    drop(handle);
    let store = Store::open(&fixtures.index).unwrap();
    let sessions = store.sessions(TimeMode::Latest).unwrap();
    assert_eq!(sessions.len(), 3);
    assert!(
        sessions
            .iter()
            .all(|session| session.indexed && session.available)
    );
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
    assert_eq!(page.hits.len(), 2);
    assert_eq!(fs::read(&state).unwrap(), before);
}

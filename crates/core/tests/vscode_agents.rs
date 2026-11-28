use agent_chat_reader_core::{
    agents::{
        self, AgentAdapter,
        copilot_cli::CopilotCli,
        vscode_agents::{Driver, Homes, VsCodeAgents, catalog_path},
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

const COPILOT: &str = "10000000-0000-4000-8000-000000000001";
const COPILOT_SDK: &str = "10000000-0000-4000-8000-0000000000a1";
const CLI_OWNED: &str = "10000000-0000-4000-8000-000000000002";
const CLI_OWNED_SDK: &str = "10000000-0000-4000-8000-0000000000a2";
const DISCOVERED: &str = "10000000-0000-4000-8000-000000000003";
const TOMBSTONED: &str = "10000000-0000-4000-8000-000000000004";
const GONE: &str = "10000000-0000-4000-8000-000000000005";
const STUB: &str = "10000000-0000-4000-8000-000000000006";
const CLAUDE: &str = "20000000-0000-4000-8000-000000000001";
const CLAUDE_SDK: &str = "20000000-0000-4000-8000-0000000000a1";
const CODEX: &str = "30000000-0000-4000-8000-000000000001";
const CODEX_THREAD: &str = "30000000-0000-4000-8000-0000000000a1";
const CODEX_ZST: &str = "30000000-0000-4000-8000-000000000002";
const CODEX_ZST_THREAD: &str = "30000000-0000-4000-8000-0000000000a2";
const CODEX_PAGE: &str = "30000000-0000-4000-8000-0000000000b1";

fn write_lines(path: &Path, lines: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::File::create(path).unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

fn catalog(database: &Path, rows: &[(&str, &str, i64, Value, bool)]) {
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let connection = Connection::open(database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE sessions_v2 (session_uri TEXT PRIMARY KEY, provider TEXT NOT NULL,
                 external INTEGER, modified_time INTEGER NOT NULL DEFAULT 0, payload TEXT);
             CREATE TABLE chats_v2 (chat_uri TEXT PRIMARY KEY, owner_session_uri TEXT NOT NULL,
                 chat_order INTEGER, provider_data TEXT, metadata TEXT NOT NULL,
                 tombstoned INTEGER NOT NULL DEFAULT 0);",
        )
        .unwrap();
    for (uri, provider, external, data, tombstoned) in rows {
        connection
            .execute(
                "INSERT INTO sessions_v2 VALUES (?1, ?2, ?3, 1795000000000, ?4)",
                params![uri, provider, external, data["payload"].to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO chats_v2 VALUES (?1, ?2, 0, ?3, ?4, ?5)",
                params![
                    format!("ahp-chat://default/{uri}"),
                    uri,
                    data["provider_data"].to_string(),
                    json!({"data": {"summary": data["chat_title"]}}).to_string(),
                    tombstoned
                ],
            )
            .unwrap();
    }
}

fn row(
    uri: &str,
    external: i64,
    title: Option<&str>,
    sdk: Option<&str>,
    workspace: &str,
) -> (String, Value) {
    let provider_data = sdk.map_or(json!({}), |sdk| json!({"sdkSessionId": sdk}));
    (
        uri.to_owned(),
        json!({
            "external": external,
            "payload": {"data": {"summary": title, "workingDirectories": [workspace]}},
            "provider_data": provider_data,
            "chat_title": Value::Null,
        }),
    )
}

fn copilot_events(prompt: &str, reply: &str) -> Vec<Value> {
    vec![
        json!({"type": "session.start", "id": "s", "timestamp": "2026-11-17T09:47:42.133Z",
            "data": {"startTime": "2026-11-17T09:47:42.133Z", "context": {"cwd": "W:\\transcript\\cwd"}}}),
        json!({"type": "user.message", "id": "u1", "timestamp": "2026-11-17T09:47:43.000Z",
            "data": {"content": prompt}}),
        json!({"type": "assistant.message", "id": "a1", "timestamp": "2026-11-17T09:47:50.000Z",
            "data": {"content": reply}}),
    ]
}

struct Fixtures {
    _directory: TempDir,
    code: PathBuf,
    homes: Homes,
    index: PathBuf,
}

impl Fixtures {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let code = root.join("Code").join("User");
        let homes = Homes {
            copilot: root.join("copilot"),
            claude: root.join("claude"),
            codex: root.join("codex"),
        };
        fs::create_dir_all(&code).unwrap();

        let rows = [
            // Created in the Agents window, transcript present.
            (
                row(
                    &format!("copilotcli:/{COPILOT}"),
                    0,
                    Some("Agents copilot"),
                    Some(COPILOT_SDK),
                    "file:///d%3A/Work%20space/copilot",
                ),
                "copilotcli",
                false,
            ),
            // Created in the Agents window but run by the Copilot CLI client.
            (
                row(
                    &format!("copilotcli:/{CLI_OWNED}"),
                    0,
                    Some("CLI owned"),
                    Some(CLI_OWNED_SDK),
                    "file:///d%3A/cli",
                ),
                "copilotcli",
                false,
            ),
            // The host merely discovered it on disk.
            (
                row(
                    &format!("copilotcli:/{DISCOVERED}"),
                    1,
                    Some("Discovered"),
                    Some(DISCOVERED),
                    "file:///d%3A/x",
                ),
                "copilotcli",
                false,
            ),
            (
                row(
                    &format!("copilotcli:/{TOMBSTONED}"),
                    0,
                    Some("Deleted"),
                    Some(TOMBSTONED),
                    "file:///d%3A/x",
                ),
                "copilotcli",
                true,
            ),
            // Titled but the transcript is gone, versus an empty stub with nothing to show.
            (
                row(
                    &format!("copilotcli:/{GONE}"),
                    0,
                    Some("Transcript removed"),
                    Some(GONE),
                    "file:///d%3A/gone",
                ),
                "copilotcli",
                false,
            ),
            (
                row(
                    &format!("copilotcli:/{STUB}"),
                    0,
                    None,
                    Some(STUB),
                    "file:///d%3A/stub",
                ),
                "copilotcli",
                false,
            ),
            (
                row(
                    &format!("claude:/{CLAUDE}"),
                    0,
                    Some("Agents claude"),
                    Some(CLAUDE_SDK),
                    "file:///d%3A/claude",
                ),
                "claude",
                false,
            ),
            (
                row(
                    &format!("codex:/{CODEX}"),
                    0,
                    Some("Agents codex"),
                    None,
                    "file:///d%3A/codex",
                ),
                "codex",
                false,
            ),
            (
                row(
                    &format!("codex:/{CODEX_ZST}"),
                    0,
                    Some("Archived codex"),
                    None,
                    "file:///d%3A/codex-old",
                ),
                "codex",
                false,
            ),
        ];
        let rows: Vec<_> = rows
            .iter()
            .map(|((uri, data), provider, tombstoned)| {
                (
                    uri.as_str(),
                    *provider,
                    data["external"].as_i64().unwrap(),
                    data.clone(),
                    *tombstoned,
                )
            })
            .collect();
        catalog(&catalog_path(&code), &rows);

        // Copilot: the host writes `vscode-agent-host` sessions beside the CLI's.
        for (sdk, client, prompt) in [
            (COPILOT_SDK, "vscode-agent-host", "中文 copilot prompt"),
            (CLI_OWNED_SDK, "github/cli", "cli owned prompt"),
        ] {
            let directory = homes.copilot.join("session-state").join(sdk);
            write_lines(
                &directory.join("events.jsonl"),
                &copilot_events(prompt, "copilot reply"),
            );
            fs::write(
                directory.join("workspace.yaml"),
                format!("client_name: {client}\n"),
            )
            .unwrap();
        }

        // Claude: prompts and replies among records the reader must ignore.
        write_lines(
            &homes
                .claude
                .join("projects")
                .join("d--work")
                .join(format!("{CLAUDE_SDK}.jsonl")),
            &[
                json!({"type": "queue-operation", "timestamp": "2026-11-19T07:40:59.034Z"}),
                json!({"type": "user", "uuid": "c-u1", "timestamp": "2026-11-19T07:41:14.763Z",
                    "cwd": "d:\\transcript\\claude",
                    "message": {"role": "user", "content": [{"type": "text", "text": "claude 中文 prompt"}]}}),
                json!({"type": "attachment", "rendered": "<system-reminder>x</system-reminder>"}),
                json!({"type": "assistant", "uuid": "c-a1", "timestamp": "2026-11-19T07:41:43.027Z",
                    "message": {"content": [{"type": "thinking", "thinking": ""}]}}),
                json!({"type": "assistant", "uuid": "c-a2", "timestamp": "2026-11-19T07:41:44.000Z",
                    "message": {"content": [{"type": "tool_use", "name": "Bash"}]}}),
                json!({"type": "user", "uuid": "c-u2", "timestamp": "2026-11-19T07:41:45.000Z",
                    "message": {"content": [{"type": "tool_result", "content": "noise"}]}}),
                json!({"type": "assistant", "uuid": "c-a3", "timestamp": "2026-11-19T07:41:46.000Z",
                    "message": {"content": [{"type": "text", "text": "claude reply"}]}}),
            ],
        );

        // Codex: the host stores the thread id; the rollout is found by it.
        let rollout = |thread: &str, prompt: &str| -> Vec<Value> {
            vec![
                json!({"type": "session_meta", "timestamp": "2026-11-20T03:48:02.389Z", "ordinal": 0,
                    "payload": {"id": thread, "timestamp": "2026-11-20T03:48:02.389Z", "cwd": "d:\\transcript\\codex"}}),
                json!({"type": "response_item", "timestamp": "2026-11-20T03:48:05.000Z", "ordinal": 1,
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "# AGENTS.md injected"}]}}),
                json!({"type": "event_msg", "timestamp": "2026-11-20T03:48:05.524Z", "ordinal": 2,
                    "payload": {"type": "item_completed", "item": {"type": "UserMessage", "id": "u",
                        "content": [{"type": "text", "text": prompt}]}}}),
                json!({"type": "event_msg", "timestamp": "2026-11-20T03:48:30.466Z", "ordinal": 3,
                    "payload": {"type": "item_completed", "item": {"type": "AgentMessage", "id": "a",
                        "phase": "final_answer", "content": [{"type": "Text", "text": "codex reply"}]}}}),
            ]
        };
        let thread_store = |session: &str, thread: &str| {
            let database = root
                .join("Code")
                .join("agentSessionData")
                .join(session)
                .join("session.db");
            fs::create_dir_all(database.parent().unwrap()).unwrap();
            let connection = Connection::open(database).unwrap();
            connection
                .execute_batch("CREATE TABLE session_metadata (key TEXT NOT NULL, value TEXT NOT NULL, turn_id TEXT)")
                .unwrap();
            connection
                .execute(
                    "INSERT INTO session_metadata VALUES ('codex.threadId', ?1, NULL)",
                    [thread],
                )
                .unwrap();
        };
        thread_store(CODEX, CODEX_THREAD);
        thread_store(CODEX_ZST, CODEX_ZST_THREAD);
        write_lines(
            &homes
                .codex
                .join("sessions/2026/11/20")
                .join(format!("rollout-2026-11-20T11-48-02-{CODEX_THREAD}.jsonl")),
            &rollout(CODEX_THREAD, "codex 中文 prompt"),
        );
        write_lines(
            &homes.codex.join("sessions/2026/11/20").join(format!(
                "rollout-2026-11-20T14-02-21-{CODEX_THREAD}_{CODEX_PAGE}.jsonl"
            )),
            &rollout(CODEX_THREAD, "continuation page prompt"),
        );
        let plain: String = rollout(CODEX_ZST_THREAD, "archived 中文 prompt")
            .iter()
            .map(|line| format!("{line}\n"))
            .collect();
        let compressed = ruzstd::encoding::compress_to_vec(
            plain.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let archived = homes.codex.join("archived_sessions");
        fs::create_dir_all(&archived).unwrap();
        fs::write(
            archived.join(format!(
                "rollout-2026-09-04T11-56-03-{CODEX_ZST_THREAD}.jsonl.zst"
            )),
            compressed,
        )
        .unwrap();

        Self {
            index: root.join("index.db"),
            _directory: directory,
            code,
            homes,
        }
    }

    fn adapter(&self, driver: Driver) -> VsCodeAgents {
        VsCodeAgents::new(&self.code, driver, self.homes.clone()).unwrap()
    }

    /// Summaries take their titles from the catalog read by the same adapter's discovery.
    fn discovered(&self, driver: Driver) -> (VsCodeAgents, Vec<Candidate>) {
        let adapter = self.adapter(driver);
        let mut found = adapter
            .discover(&Cancellation::default())
            .unwrap()
            .candidates;
        found.sort_by(|a, b| a.key.native.cmp(&b.key.native));
        (adapter, found)
    }

    fn candidates(&self, driver: Driver) -> Vec<Candidate> {
        self.discovered(driver).1
    }

    fn messages(&self, driver: Driver, candidate: &Candidate) -> Vec<Message> {
        let adapter = self.adapter(driver);
        let batch = adapter
            .read_batch(candidate, 0, 1024 * 1024, &Cancellation::default())
            .unwrap();
        assert!(batch.eof);
        assert!(batch.diagnostics.is_empty());
        batch.messages
    }
}

#[test]
fn copilot_discovers_agents_sessions_and_leaves_cli_owned_ones_alone() {
    let fixtures = Fixtures::new();
    let (adapter, found) = fixtures.discovered(Driver::Copilot);
    let natives: Vec<_> = found.iter().map(|c| c.key.native.as_str()).collect();
    // CLI-owned, host-discovered, tombstoned and untitled-and-missing sessions are not listed.
    assert_eq!(natives, [COPILOT, GONE]);
    let session = adapter
        .summarize(&found[0], &Cancellation::default())
        .unwrap();
    assert_eq!(session.name, "Agents copilot");
    assert_eq!(session.workspace.as_deref(), Some("W:\\transcript\\cwd"));
    assert_eq!(found[0].key.agent, "vscode-agents-copilot");
    assert_eq!(found[0].read_mode, ReadMode::Append);
    let messages = fixtures.messages(Driver::Copilot, &found[0]);
    assert_eq!(
        messages.iter().map(|m| m.role).collect::<Vec<_>>(),
        [Role::User, Role::Assistant]
    );

    // The removed transcript stays visible as an unreadable row, and is never read.
    let gone = adapter
        .summarize(&found[1], &Cancellation::default())
        .unwrap();
    assert!(!gone.available);
    assert_eq!(gone.name, "Transcript removed");
    assert_eq!(
        gone.workspace.as_deref(),
        Some(if cfg!(windows) { "d:\\gone" } else { "d:/gone" })
    );
    assert!(adapter.probe(&found[1], &Cancellation::default()).is_err());

    // The Copilot CLI adapter ignores the Agents-window session, so nothing is listed twice.
    let cli = CopilotCli::new(&fixtures.homes.copilot).unwrap();
    let cli_natives: Vec<_> = cli
        .discover(&Cancellation::default())
        .unwrap()
        .candidates
        .into_iter()
        .map(|candidate| candidate.key.native)
        .collect();
    assert_eq!(cli_natives, [CLI_OWNED_SDK]);
}

#[test]
fn claude_keeps_only_prompts_and_replies_and_resolves_the_sdk_transcript() {
    let fixtures = Fixtures::new();
    let (adapter, found) = fixtures.discovered(Driver::Claude);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.native, CLAUDE);
    let session = adapter
        .summarize(&found[0], &Cancellation::default())
        .unwrap();
    assert_eq!(session.name, "Agents claude");
    assert_eq!(session.workspace.as_deref(), Some("d:\\transcript\\claude"));
    assert!(session.created.value().is_some());
    assert_eq!(
        session.latest.value().unwrap().to_rfc3339(),
        "2026-11-19T07:41:46+00:00"
    );
    let messages = fixtures.messages(Driver::Claude, &found[0]);
    let content: Vec<_> = messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(content, ["claude 中文 prompt", "claude reply"]);
}

#[test]
fn codex_resolves_threads_through_the_host_store_including_archived_compressed_rollouts() {
    let fixtures = Fixtures::new();
    let (adapter, found) = fixtures.discovered(Driver::Codex);
    let natives: Vec<_> = found.iter().map(|c| c.key.native.as_str()).collect();
    assert_eq!(natives, [CODEX, CODEX_ZST]);

    let plain = &found[0];
    assert_eq!(plain.read_mode, ReadMode::Append);
    let report = adapter.discover(&Cancellation::default()).unwrap();
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.description.contains("continuation page")),
        "the unmerged continuation page must be reported"
    );
    let session = adapter.summarize(plain, &Cancellation::default()).unwrap();
    assert_eq!(session.name, "Agents codex");
    assert_eq!(session.workspace.as_deref(), Some("d:\\transcript\\codex"));
    let content: Vec<_> = fixtures
        .messages(Driver::Codex, plain)
        .into_iter()
        .map(|message| message.content)
        .collect();
    // The injected AGENTS.md message and the continuation page are not part of the conversation.
    assert_eq!(content, ["codex 中文 prompt", "codex reply"]);

    let archived = &found[1];
    assert_eq!(archived.read_mode, ReadMode::Snapshot);
    let session = adapter
        .summarize(archived, &Cancellation::default())
        .unwrap();
    assert!(session.available);
    assert_eq!(session.workspace.as_deref(), Some("d:\\transcript\\codex"));
    assert_eq!(
        session.latest.value().unwrap().to_rfc3339(),
        "2026-11-20T03:48:30.466+00:00"
    );
    let batch = adapter
        .read_batch(archived, 0, 1024, &Cancellation::default())
        .unwrap();
    assert_eq!(batch.cursor, archived.size);
    assert!(batch.eof);
    assert_eq!(
        batch
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        ["archived 中文 prompt", "codex reply"]
    );
    // A snapshot replays once; resuming from its end yields nothing more.
    let resumed = adapter
        .read_batch(archived, batch.cursor, 1024, &Cancellation::default())
        .unwrap();
    assert!(resumed.messages.is_empty() && resumed.eof);
}

#[test]
fn raw_view_returns_unfiltered_text_for_every_driver() {
    let fixtures = Fixtures::new();
    for (driver, index, expected) in [
        (Driver::Copilot, 0, "中文 copilot prompt"),
        (Driver::Claude, 0, "claude 中文 prompt"),
        (Driver::Codex, 0, "codex 中文 prompt"),
        (Driver::Codex, 1, "archived 中文 prompt"),
    ] {
        let candidate = fixtures.candidates(driver).remove(index);
        let message = fixtures.messages(driver, &candidate).remove(0);
        let adapter = agents::adapter_for(&candidate).unwrap();
        assert_eq!(
            adapter.original_message(&candidate, &message).unwrap(),
            expected
        );
    }
}

#[test]
fn all_three_drivers_scan_into_one_index_without_writing_the_host_databases() {
    let fixtures = Fixtures::new();
    let catalog_file = catalog_path(&fixtures.code);
    let before = fs::read(&catalog_file).unwrap();
    let adapters: Vec<Arc<dyn AgentAdapter>> = Driver::ALL
        .into_iter()
        .map(|driver| Arc::new(fixtures.adapter(driver)) as Arc<dyn AgentAdapter>)
        .collect();
    let handle = scan::start(adapters, fixtures.index.clone()).unwrap();
    let mut finished = 0;
    while finished < 3 {
        if let ScanEvent::Finished { error, .. } =
            handle.events.recv_timeout(Duration::from_secs(10)).unwrap()
        {
            assert!(error.is_none(), "{error:?}");
            finished += 1;
        }
    }
    drop(handle);
    let store = Store::open(&fixtures.index).unwrap();
    let sessions = store.sessions(TimeMode::Latest).unwrap();
    // copilot + removed copilot + claude + two codex.
    assert_eq!(sessions.len(), 5);
    assert_eq!(sessions.iter().filter(|session| session.indexed).count(), 4);
    assert_eq!(
        sessions.iter().filter(|session| !session.available).count(),
        1
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
    assert_eq!(page.hits.len(), 4);
    assert_eq!(fs::read(&catalog_file).unwrap(), before);
}

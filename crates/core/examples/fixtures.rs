use agent_chat_reader_core::{
    agents::{AgentAdapter, copilot_cli::CopilotCli},
    model::{Cancellation, TimeMode},
    scan::{self, ScanEvent},
    search::{SearchCursor, SearchQuery},
    store::Store,
};
use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use serde_json::json;
use std::{
    fs,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, UNIX_EPOCH},
};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("generate") => {
            let root = PathBuf::from(args.get(1).context("Missing new output directory")?);
            anyhow::ensure!(
                !root.exists(),
                "Use a new fixture directory; existing data is never overwritten"
            );
            let count = args
                .get(2)
                .map(|arg| arg.parse())
                .transpose()?
                .unwrap_or(1000);
            let messages = args
                .get(3)
                .map(|arg| arg.parse())
                .transpose()?
                .unwrap_or(100);
            let body_bytes = args
                .get(4)
                .map(|arg| arg.parse())
                .transpose()?
                .unwrap_or(1024);
            fs::create_dir_all(root.join("session-state"))?;
            let start = Instant::now();
            let mut bytes = 0;
            for session in 0..count {
                let id = uuid::Uuid::from_u128(session as u128 + 1).to_string();
                let directory = root.join("session-state").join(id);
                fs::create_dir(&directory)?;
                fs::write(
                    directory.join("workspace.yaml"),
                    format!("client_name: github/cli\nname: Synthetic session {session:05}\n"),
                )?;
                let file = fs::File::create(directory.join("events.jsonl"))?;
                let mut stream = BufWriter::new(file);
                writeln!(
                    stream,
                    "{}",
                    json!({
                    "type":"session.start", "timestamp":"2026-01-01T00:00:00Z",
                    "data":{"context":{"cwd":format!("D:\\synthetic\\workspace-{}",session % 10)}}})
                )?;
                for message in 0..messages {
                    let timestamp = Utc
                        .timestamp_opt(1_767_225_600 + (session * 1000 + message) as i64, 0)
                        .single()
                        .context("Invalid generated timestamp")?
                        .to_rfc3339();
                    let body = format!(
                        "# Message {message}\n\n中文搜索测试 literal NEEDLE-{session}-{message} 100% a_b\n\n{}",
                        "A synthetic conversation. ".repeat(body_bytes / 26 + 1)
                    );
                    bytes += body.len();
                    writeln!(
                        stream,
                        "{}",
                        json!({"id":format!("message-{message}"),
                        "type":if message % 2 == 0 { "user.message" } else { "assistant.message" },
                        "timestamp":timestamp,"data":{"content":body}})
                    )?;
                    writeln!(
                        stream,
                        "{}",
                        json!({"type":"tool.execution_complete",
                        "timestamp":timestamp,"data":{"output":"tool noise ".repeat(body_bytes / 10 + 1)}})
                    )?;
                }
                stream.flush()?;
                // Deliberately reverse mtime relative to conversation time.
                stream
                    .get_ref()
                    .set_times(fs::FileTimes::new().set_modified(
                        UNIX_EPOCH + Duration::from_secs(1_800_000_000 - session as u64),
                    ))?;
            }
            println!(
                "Generated {count} sessions × {messages} messages; {bytes} body bytes in {:?}",
                start.elapsed()
            );
        }
        Some("scan") => {
            let source = PathBuf::from(args.get(1).context("Missing source directory")?);
            let database = PathBuf::from(args.get(2).context("Missing database path")?);
            let adapter = Arc::new(CopilotCli::new(source)?);
            let cancel = Cancellation::default();
            let start = Instant::now();
            let candidates = adapter.discover(&cancel)?.candidates;
            println!(
                "Discovered {} candidates in {:?}",
                candidates.len(),
                start.elapsed()
            );
            let scan = scan::start(vec![adapter], database.clone())?;
            let mut seen = std::collections::HashSet::new();
            let mut first50 = false;
            let mut errors = 0;
            loop {
                match scan.events.recv()? {
                    ScanEvent::Session(session) => {
                        seen.insert(session.candidate.key.storage_key());
                        if seen.len() >= 50 && !first50 {
                            first50 = true;
                            println!("First 50 summaries available: {:?}", start.elapsed());
                        }
                    }
                    ScanEvent::Diagnostic(error) => {
                        errors += 1;
                        eprintln!("{}: {}", error.path.display(), error.description);
                    }
                    ScanEvent::Finished { .. } => break,
                    _ => {}
                }
            }
            println!("Full scan/index: {:?}; errors: {errors}", start.elapsed());
            let store = Store::open(&database)?;
            for text in ["NEEDLE-123-45", "中文", "文搜索"] {
                let start = Instant::now();
                let page = store.search_page(
                    &SearchQuery {
                        text: text.into(),
                        mode: TimeMode::Latest,
                        ..Default::default()
                    },
                    SearchCursor::default(),
                    &cancel,
                )?;
                println!(
                    "Search {text:?}: first page {:?}, {} hits, {} candidates examined",
                    start.elapsed(),
                    page.hits.len(),
                    page.scanned
                );
            }
            anyhow::ensure!(errors == 0, "Scan reported {errors} errors");
        }
        Some("recent") => {
            let source = PathBuf::from(args.get(1).context("Missing source directory")?);
            let database = PathBuf::from(args.get(2).context("Missing new database path")?);
            anyhow::ensure!(
                !database.exists(),
                "Use a new index for the uncached summary measurement"
            );
            let start = Instant::now();
            let scan = scan::start(vec![Arc::new(CopilotCli::new(source)?)], database)?;
            let mut seen = std::collections::HashSet::new();
            loop {
                match scan.events.recv()? {
                    ScanEvent::Session(session) => {
                        seen.insert(session.candidate.key.storage_key());
                        if seen.len() >= 50 {
                            println!("Uncached first 50 summaries: {:?}", start.elapsed());
                            break;
                        }
                    }
                    ScanEvent::Diagnostic(error) => anyhow::bail!("{}", error.description),
                    ScanEvent::Finished { .. } => break,
                    _ => {}
                }
            }
        }
        Some("search") => {
            let database = PathBuf::from(args.get(1).context("Missing database path")?);
            let store = Store::open(&database)?;
            for text in ["NEEDLE-123-45", "中文", "文搜索"] {
                let mut samples = Vec::new();
                for _ in 0..30 {
                    let start = Instant::now();
                    store.search_page(
                        &SearchQuery {
                            text: text.into(),
                            ..Default::default()
                        },
                        SearchCursor::default(),
                        &Cancellation::default(),
                    )?;
                    samples.push(start.elapsed());
                }
                samples.sort();
                println!(
                    "Search {text:?}: median {:?}, p95 {:?} (30 samples, warm index)",
                    samples[15], samples[28]
                );
            }
        }
        _ => anyhow::bail!(
            "Usage: fixtures generate NEW_DIR [SESSIONS] [MESSAGES] [BODY_BYTES]\n\
             fixtures scan SOURCE_DIR DATABASE\nfixtures recent SOURCE_DIR NEW_DATABASE\nfixtures search DATABASE"
        ),
    }
    Ok(())
}

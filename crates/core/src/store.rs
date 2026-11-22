use crate::{
    model::*,
    search::{
        SearchCursor, SearchHit, SearchPage, SearchPosition, SearchQuery, find_ranges, first_range,
        fts_phrase, normalize, snippet,
    },
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

const SCHEMA_VERSION: i64 = 2;
const PARSER_VERSION: i64 = 1;

pub struct Store {
    connection: Connection,
}

#[derive(Clone, Debug)]
pub struct MessageInfo {
    pub rowid: i64,
    pub ordinal: usize,
    pub role: Role,
    pub time: MessageTime,
    pub bytes: usize,
    pub preview: String,
    pub native_id: String,
    pub attachment_names: Vec<String>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("Cannot open index {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(3))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(
            version <= SCHEMA_VERSION,
            "Index was created by a newer application"
        );
        if version == 0 {
            connection.execute_batch(
                "BEGIN;
                CREATE TABLE sessions(
                    id TEXT PRIMARY KEY, source TEXT NOT NULL, data TEXT NOT NULL,
                    latest INTEGER, first_assistant INTEGER
                );
                CREATE INDEX session_latest ON sessions(latest DESC, id);
                CREATE INDEX session_first ON sessions(first_assistant DESC, id);
                CREATE TABLE messages(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    ordinal INTEGER NOT NULL, event_id TEXT NOT NULL,
                    role TEXT NOT NULL, time TEXT NOT NULL,
                    body TEXT NOT NULL, folded TEXT NOT NULL, attachments TEXT NOT NULL,
                    start INTEGER NOT NULL, end INTEGER NOT NULL,
                    UNIQUE(session, start)
                );
                CREATE INDEX message_session ON messages(session, ordinal);
                CREATE VIRTUAL TABLE message_fts USING fts5(
                    folded, content='messages', content_rowid='id', tokenize='trigram'
                );
                CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO message_fts(rowid,folded) VALUES(new.id,new.folded);
                END;
                CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO message_fts(message_fts,rowid,folded)
                        VALUES('delete',old.id,old.folded);
                END;
                CREATE TABLE checkpoints(
                    session TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                    cursor INTEGER NOT NULL, prefix BLOB NOT NULL, boundary BLOB NOT NULL,
                    parser INTEGER NOT NULL
                );
                PRAGMA user_version=2;
                COMMIT;",
            )?;
        }
        if version == 1 {
            connection.execute_batch(
                "BEGIN;
                DROP TRIGGER messages_ai; DROP TRIGGER messages_ad;
                DROP INDEX message_session;
                ALTER TABLE messages RENAME TO old_messages;
                CREATE TABLE messages(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    ordinal INTEGER NOT NULL, event_id TEXT NOT NULL,
                    role TEXT NOT NULL, time TEXT NOT NULL,
                    body TEXT NOT NULL, folded TEXT NOT NULL, attachments TEXT NOT NULL,
                    start INTEGER NOT NULL, end INTEGER NOT NULL,
                    UNIQUE(session,start)
                );
                INSERT INTO messages SELECT * FROM old_messages;
                DROP TABLE old_messages;
                CREATE INDEX message_session ON messages(session,ordinal);
                CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO message_fts(rowid,folded) VALUES(new.id,new.folded);
                END;
                CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO message_fts(message_fts,rowid,folded)
                        VALUES('delete',old.id,old.folded);
                END;
                PRAGMA user_version=2; COMMIT;",
            )?;
        }
        Ok(Self { connection })
    }

    pub fn upsert_session(&mut self, session: &Session) -> Result<()> {
        let key = session.candidate.key.storage_key();
        if let Some(previous) = self.session(&key)? {
            if &previous == session {
                return Ok(());
            }
            let old = &previous.candidate;
            let new = &session.candidate;
            let replacement = old.file_identity.is_some()
                && new.file_identity.is_some()
                && old.file_identity != new.file_identity;
            let rewritten = new.size <= old.size
                && (new.size != old.size || new.modified_ns != old.modified_ns);
            let snapshot_changed = new.read_mode == ReadMode::Snapshot
                && (new.size != old.size
                    || new.modified_ns != old.modified_ns
                    || new.events_path != old.events_path);
            if replacement || rewritten || snapshot_changed {
                let tx = self.connection.transaction()?;
                tx.execute("DELETE FROM messages WHERE session=?1", [&key])?;
                tx.execute("DELETE FROM checkpoints WHERE session=?1", [&key])?;
                tx.commit()?;
            }
        }
        self.connection.execute(
            "INSERT INTO sessions(id,source,data,latest,first_assistant) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(id) DO UPDATE SET data=excluded.data,
                latest=excluded.latest,first_assistant=excluded.first_assistant",
            params![
                session.candidate.key.storage_key(),
                session.candidate.key.source,
                serde_json::to_string(session)?,
                session
                    .time(TimeMode::Latest)
                    .map(|time| time.timestamp_millis()),
                session
                    .time(TimeMode::FirstAssistant)
                    .map(|time| time.timestamp_millis()),
            ],
        )?;
        Ok(())
    }

    pub fn sessions(&self, mode: TimeMode) -> Result<Vec<Session>> {
        let order = match mode {
            TimeMode::Latest => "latest",
            TimeMode::FirstAssistant => "first_assistant",
        };
        let mut statement = self.connection.prepare(&format!(
            "SELECT data FROM sessions ORDER BY {order} IS NULL,{order} DESC,id"
        ))?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }

    pub fn session(&self, key: &str) -> Result<Option<Session>> {
        self.connection
            .query_row("SELECT data FROM sessions WHERE id=?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .map(|data| Ok(serde_json::from_str(&data)?))
            .transpose()
    }

    pub fn reclassify_session(&mut self, replacement: &ReclassifiedSession) -> Result<bool> {
        let previous = replacement.previous.storage_key();
        let current = replacement.current.storage_key();
        let Some(mut session) = self.session(&previous)? else {
            return Ok(false);
        };
        let exists = self.session(&current)?.is_some();
        let tx = self.connection.transaction()?;
        if !exists {
            session.candidate.key = replacement.current.clone();
            tx.execute(
                "INSERT INTO sessions(id,source,data,latest,first_assistant)
                 SELECT ?2,?3,?4,latest,first_assistant FROM sessions WHERE id=?1",
                params![
                    previous,
                    current,
                    replacement.current.source,
                    serde_json::to_string(&session)?
                ],
            )?;
            tx.execute(
                "UPDATE messages SET session=?2 WHERE session=?1",
                params![previous, current],
            )?;
            tx.execute(
                "UPDATE checkpoints SET session=?2 WHERE session=?1",
                params![previous, current],
            )?;
        }
        tx.execute("DELETE FROM sessions WHERE id=?1", [&previous])?;
        tx.commit()?;
        Ok(true)
    }

    /// Validates both the initial bytes and the committed boundary before resuming.
    pub fn resume_cursor(&mut self, session: &Session) -> Result<u64> {
        let key = session.candidate.key.storage_key();
        let checkpoint = self
            .connection
            .query_row(
                "SELECT cursor,prefix,boundary,parser FROM checkpoints WHERE session=?1",
                [&key],
                |row| {
                    Ok((
                        read_u64(row, 0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        if let Some((cursor, prefix, boundary, version)) = checkpoint
            && version == PARSER_VERSION
            && session.candidate.events_path.metadata()?.len() >= cursor
        {
            let hashes = checkpoint_hashes(&session.candidate.events_path, cursor)?;
            if hashes.0 == prefix && hashes.1 == boundary {
                return Ok(cursor);
            }
        }
        let tx = self.connection.transaction()?;
        tx.execute("DELETE FROM messages WHERE session=?1", [&key])?;
        tx.execute("DELETE FROM checkpoints WHERE session=?1", [&key])?;
        tx.commit()?;
        Ok(0)
    }

    pub fn commit_batch(&mut self, session: &mut Session, batch: &ReadBatch) -> Result<()> {
        let key = session.candidate.key.storage_key();
        if let Some(current) = self.session(&key)? {
            anyhow::ensure!(
                current.candidate.modified_ns == session.candidate.modified_ns
                    && current.candidate.size == session.candidate.size
                    && current.candidate.file_identity == session.candidate.file_identity,
                "Session changed while reading; refresh and retry"
            );
        }
        let committed: Option<i64> = self
            .connection
            .query_row(
                "SELECT cursor FROM checkpoints WHERE session=?1",
                [&key],
                |row| row.get(0),
            )
            .optional()?;
        if committed.is_some_and(|cursor| cursor > i64::try_from(batch.cursor).unwrap_or(i64::MAX))
        {
            if let Some(current) = self.session(&key)? {
                *session = current;
            }
            return Ok(());
        }
        let (prefix, boundary) = checkpoint_hashes(&session.candidate.events_path, batch.cursor)?;
        let tx = self.connection.transaction()?;
        let mut ordinal: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages WHERE session=?1",
            [&key],
            |row| row.get(0),
        )?;
        let first = tx.query_row(
            "SELECT time FROM messages WHERE session=?1 AND role='Assistant' ORDER BY ordinal LIMIT 1",
            [&key], |row| row.get::<_, String>(0)).optional()?;
        let mut first_seen = first.is_some();
        if let Some(time) = first {
            session.first_assistant = serde_json::from_str(&time)?;
        }
        for message in &batch.messages {
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO messages(session,ordinal,event_id,role,time,body,folded,
                    attachments,start,end) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    key,
                    ordinal,
                    message.id,
                    message.role.label(),
                    serde_json::to_string(&message.time)?,
                    message.content,
                    normalize(&message.content),
                    serde_json::to_string(&message.attachments)?,
                    i64::try_from(message.offset)?,
                    i64::try_from(message.end)?
                ],
            )?;
            if inserted > 0 {
                ordinal += 1;
            }
            if !first_seen && message.role == Role::Assistant {
                session.first_assistant = message.time.clone();
                first_seen = true;
            }
        }
        if batch.eof {
            if !first_seen {
                session.first_assistant = MessageTime::Missing;
            }
            let latest = tx
                .query_row(
                    "SELECT time FROM messages WHERE session=?1
                 AND json_type(time,'$.Known') IS NOT NULL ORDER BY ordinal DESC LIMIT 1",
                    [&key],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            session.latest = latest
                .map(|value| serde_json::from_str(&value))
                .transpose()?
                .unwrap_or(MessageTime::Missing);
        }
        session.indexed = batch.eof && !batch.incomplete_tail;
        tx.execute(
            "INSERT INTO checkpoints(session,cursor,prefix,boundary,parser) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(session) DO UPDATE SET cursor=excluded.cursor,
                prefix=excluded.prefix,boundary=excluded.boundary,parser=excluded.parser",
            params![
                key,
                i64::try_from(batch.cursor)?,
                prefix,
                boundary,
                PARSER_VERSION
            ],
        )?;
        tx.execute(
            "UPDATE sessions SET data=?2,latest=?3,first_assistant=?4 WHERE id=?1",
            params![
                key,
                serde_json::to_string(session)?,
                session
                    .time(TimeMode::Latest)
                    .map(|time| time.timestamp_millis()),
                session
                    .time(TimeMode::FirstAssistant)
                    .map(|time| time.timestamp_millis())
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn message_infos(&self, key: &str) -> Result<Vec<MessageInfo>> {
        let mut statement = self.connection.prepare(
            "SELECT id,ordinal,role,time,length(CAST(body AS BLOB)),substr(body,1,100),event_id,attachments
             FROM messages WHERE session=?1 ORDER BY ordinal")?;
        let rows = statement.query_map([key], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                read_usize(row, 1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                read_usize(row, 4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        rows.map(|row| {
            let (rowid, ordinal, role, time, bytes, preview, native_id, attachments) = row?;
            let attachments: Vec<serde_json::Value> = serde_json::from_str(&attachments)?;
            let attachment_names = attachments
                .iter()
                .map(|value| {
                    value
                        .get("displayName")
                        .or_else(|| value.get("name"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Attachment (metadata only)")
                        .to_owned()
                })
                .collect();
            Ok(MessageInfo {
                rowid,
                ordinal,
                role: parse_role(&role)?,
                time: serde_json::from_str(&time)?,
                bytes,
                preview,
                native_id,
                attachment_names,
            })
        })
        .collect()
    }

    pub fn message(&self, rowid: i64) -> Result<Message> {
        let (id, role, time, content, attachments, offset, end) = self.connection.query_row(
            "SELECT event_id,role,time,body,attachments,start,end FROM messages WHERE id=?1",
            [rowid],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    read_u64(row, 5)?,
                    read_u64(row, 6)?,
                ))
            },
        )?;
        Ok(Message {
            id,
            role: parse_role(&role)?,
            time: serde_json::from_str(&time)?,
            content,
            attachments: serde_json::from_str(&attachments)?,
            offset,
            end,
        })
    }

    pub fn chunk(&self, rowid: i64, start: usize, end: usize) -> Result<String> {
        anyhow::ensure!(end >= start, "Invalid message chunk range");
        Ok(self.connection.query_row(
            "SELECT CAST(substr(CAST(body AS BLOB),?2,?3) AS TEXT) FROM messages WHERE id=?1",
            params![
                rowid,
                i64::try_from(start)? + 1,
                i64::try_from(end - start)?
            ],
            |row| row.get(0),
        )?)
    }

    pub fn original_message(&self, rowid: i64) -> Result<String> {
        let session: String = self.connection.query_row(
            "SELECT session FROM messages WHERE id=?1",
            [rowid],
            |row| row.get(0),
        )?;
        let session = self
            .session(&session)?
            .context("Message source is no longer indexed")?;
        crate::agents::adapter_for(&session.candidate)?
            .original_message(&session.candidate, &self.message(rowid)?)
    }

    pub fn search_page(
        &self,
        query: &SearchQuery,
        cursor: SearchCursor,
        cancel: &Cancellation,
    ) -> Result<SearchPage> {
        let normalized = normalize(&query.text);
        if normalized.is_empty() {
            return Ok(SearchPage::default());
        }
        let phrase = fts_phrase(&query.text);
        let order = match query.mode {
            TimeMode::Latest => "s.latest",
            TimeMode::FirstAssistant => "s.first_assistant",
        };
        let interrupted = cancel.clone();
        self.connection
            .progress_handler(1000, Some(move || interrupted.is_cancelled()))?;
        let result = (|| {
            let common = if let Some(phrase) = &phrase {
                self.connection.query_row(
                    "SELECT COUNT(*) FROM (SELECT rowid FROM message_fts WHERE message_fts MATCH ?1 LIMIT 2049)",
                    [phrase], |row| row.get::<_, i64>(0),
                )? > 2048
            } else {
                true
            };
            let order_index = match query.mode {
                TimeMode::Latest => "session_latest",
                TimeMode::FirstAssistant => "session_first",
            };
            let from = if common {
                format!(
                    "FROM sessions s INDEXED BY {order_index}
                    CROSS JOIN messages m INDEXED BY message_session"
                )
            } else {
                "FROM messages m JOIN sessions s ON s.id=m.session
                    JOIN message_fts f ON f.rowid=m.id"
                    .to_owned()
            };
            let matching = if phrase.is_none() {
                "AND ?4 IS NULL"
            } else if common {
                "AND EXISTS(SELECT 1 FROM message_fts WHERE rowid=m.id AND message_fts MATCH ?4)"
            } else {
                "AND message_fts MATCH ?4"
            };
            let sql = format!(
                "WITH candidates AS MATERIALIZED (
                 SELECT m.id,m.session,m.ordinal,m.role,{order} AS sorttime {from}
                 WHERE m.session=s.id AND (?1 IS NULL OR s.id=?1)
                 AND (?2 IS NULL OR m.role=?2)
                 AND (?3 IS NULL OR json_extract(s.data,'$.workspace')=?3)
                 AND (?5 IS NULL OR {order}>=?5)
                 AND (?6=0 OR ({order} IS NULL AND ?7 IS NOT NULL) OR {order}<?7
                     OR ({order} IS ?7 AND (s.id>?8 OR (s.id=?8 AND m.ordinal>=?9))))
                 {matching} ORDER BY {order} DESC,s.id,m.ordinal LIMIT 128)
                 SELECT c.id,c.session,c.ordinal,c.role,m.body,c.sorttime
                 FROM candidates c JOIN messages m ON m.id=c.id"
            );
            let mut statement = self.connection.prepare(&sql)?;
            let mut rows = statement.query(params![
                query.session,
                query.role.map(Role::label),
                query.workspace,
                phrase,
                query.since.map(|time| time.timestamp_millis()),
                cursor.after.is_some(),
                cursor.after.as_ref().and_then(|position| position.time),
                cursor
                    .after
                    .as_ref()
                    .map(|position| position.session.as_str()),
                cursor.after.as_ref().map(|position| position.ordinal),
            ])?;
            let mut page = SearchPage::default();
            let mut last = None;
            let mut processed_bytes = 0_usize;
            while let Some(row) = rows.next()? {
                cancel.check()?;
                page.scanned += 1;
                let body: String = row.get(4)?;
                processed_bytes = processed_bytes.saturating_add(body.len());
                let position = SearchPosition {
                    session: row.get(1)?,
                    ordinal: row.get(2)?,
                    time: row.get(5)?,
                };
                last = Some(SearchPosition {
                    ordinal: position.ordinal + 1,
                    ..position.clone()
                });
                let ranges = if query.session.is_some() {
                    find_ranges(&body, &query.text)
                } else {
                    first_range(&body, &query.text).into_iter().collect()
                };
                let skip = if page.scanned == 1 {
                    cursor.occurrence
                } else {
                    0
                };
                for (occurrence, range) in ranges.into_iter().enumerate().skip(skip) {
                    if page.hits.len() >= 256 {
                        page.next = Some(SearchCursor {
                            offset: cursor.offset + page.scanned - 1,
                            occurrence,
                            after: Some(position),
                        });
                        return Ok(page);
                    }
                    page.hits.push(SearchHit {
                        message_id: row.get(0)?,
                        session: row.get(1)?,
                        ordinal: read_usize(row, 2)?,
                        role: parse_role(&row.get::<_, String>(3)?)?,
                        snippet: snippet(&body, range.start),
                        offset: range.start,
                    });
                }
                if processed_bytes >= 2 * 1024 * 1024 {
                    page.next = Some(SearchCursor {
                        offset: cursor.offset + page.scanned,
                        occurrence: 0,
                        after: last,
                    });
                    return Ok(page);
                }
            }
            if page.scanned == 128 {
                page.next = Some(SearchCursor {
                    offset: cursor.offset + page.scanned,
                    occurrence: 0,
                    after: last,
                });
            }
            Ok(page)
        })();
        self.connection.progress_handler(0, None::<fn() -> bool>)?;
        result
    }

    pub fn clear(&mut self) -> Result<()> {
        self.connection.execute_batch(
            "BEGIN; DELETE FROM checkpoints; DELETE FROM messages;
            DELETE FROM sessions; COMMIT; PRAGMA wal_checkpoint(TRUNCATE); VACUUM;",
        )?;
        Ok(())
    }
}

fn parse_role(value: &str) -> Result<Role> {
    match value {
        "User" => Ok(Role::User),
        "Assistant" => Ok(Role::Assistant),
        _ => anyhow::bail!("Unknown indexed message role"),
    }
}

fn read_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn read_usize(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<usize> {
    let value = row.get::<_, i64>(index)?;
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn checkpoint_hashes(path: &PathBuf, cursor: u64) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut file = File::open(path)?;
    let mut prefix = vec![0; cursor.min(256) as usize];
    file.read_exact(&mut prefix)?;
    file.seek(SeekFrom::Start(cursor.saturating_sub(256)))?;
    let mut boundary = vec![0; cursor.min(256) as usize];
    file.read_exact(&mut boundary)?;
    Ok((
        Sha256::digest(prefix).to_vec(),
        Sha256::digest(boundary).to_vec(),
    ))
}

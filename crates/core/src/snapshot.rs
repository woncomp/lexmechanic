use crate::{
    model::Cancellation,
    search::{HitKind, SearchCursor, SearchHit, SearchQuery},
    store::{Store, title_hits},
};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::{collections::HashMap, path::Path, sync::Arc};
use tempfile::{NamedTempFile, TempPath};

pub struct DocumentSnapshot {
    pub store: Store,
    pub file: Arc<TempPath>,
}

impl DocumentSnapshot {
    pub fn capture(
        source: &Store,
        key: &str,
        directory: &Path,
        cancel: &Cancellation,
    ) -> Result<Self> {
        let file = Arc::new(NamedTempFile::new_in(directory)?.into_temp_path());
        let mut store = Store::open(file.as_ref())?;
        source.with_read_snapshot(|source| source.copy_session_to(key, &mut store, cancel))?;
        Ok(Self { file, store })
    }
}

pub struct SearchSnapshot {
    connection: Connection,
    _file: TempPath,
    pub count: usize,
    pub sessions: usize,
}

impl SearchSnapshot {
    /// The caller pins a consistent read view until the result cache is complete.
    ///
    /// Hits are kept in session-list order, each session's title hit ahead of its message hits,
    /// so the results can be shown grouped by session.
    pub fn capture(
        source: &Store,
        query: &SearchQuery,
        directory: &Path,
        cancel: &Cancellation,
    ) -> Result<Self> {
        let file = NamedTempFile::new_in(directory)?.into_temp_path();
        let mut connection = Connection::open(&file)?;
        connection.execute_batch(
            "CREATE TABLE hits(rank INTEGER NOT NULL,kind INTEGER NOT NULL,             seq INTEGER NOT NULL,session TEXT NOT NULL,data TEXT NOT NULL);",
        )?;
        let tx = connection.transaction()?;
        let sessions = source.sessions(query.mode)?;
        let ranks: HashMap<String, i64> = sessions
            .iter()
            .enumerate()
            .map(|(rank, session)| (session.candidate.key.storage_key(), rank as i64))
            .collect();
        let mut count = 0;
        let mut insert = |hit: &SearchHit| -> Result<()> {
            tx.execute(
                "INSERT INTO hits(rank,kind,seq,session,data) VALUES(?1,?2,?3,?4,?5)",
                params![
                    ranks.get(&hit.session).copied().unwrap_or(i64::MAX),
                    i64::from(hit.kind == HitKind::Message),
                    i64::try_from(count)?,
                    hit.session,
                    serde_json::to_string(hit)?
                ],
            )?;
            count += 1;
            Ok(())
        };
        for hit in title_hits(sessions, query, cancel)? {
            insert(&hit)?;
        }
        let mut cursor = SearchCursor::default();
        let mut metadata = HashMap::new();
        loop {
            cancel.check()?;
            let page = source.search_page(query, cursor, cancel)?;
            for mut hit in page.hits {
                if !metadata.contains_key(&hit.session) {
                    let session = source
                        .session(&hit.session)?
                        .context("Search session disappeared from snapshot")?;
                    metadata.insert(hit.session.clone(), (session.name, session.workspace));
                }
                if let Some((name, workspace)) = metadata.get(&hit.session) {
                    hit.session_name = name.clone();
                    hit.workspace = workspace.clone();
                }
                insert(&hit)?;
            }
            match page.next {
                Some(next) => cursor = next,
                None => break,
            }
        }
        tx.execute_batch(
            "CREATE INDEX hits_order ON hits(rank,kind,seq);             CREATE INDEX hits_session ON hits(session,kind);",
        )?;
        let sessions = tx.query_row("SELECT COUNT(DISTINCT session) FROM hits", [], |row| {
            row.get::<_, i64>(0)
        })?;
        tx.commit()?;
        Ok(Self {
            connection,
            _file: file,
            count,
            sessions: usize::try_from(sessions)?,
        })
    }

    pub fn page(&self, offset: usize) -> Result<(Vec<SearchHit>, Option<usize>)> {
        let mut statement = self
            .connection
            .prepare("SELECT data FROM hits ORDER BY rank,kind,seq LIMIT 512 OFFSET ?1")?;
        let hits: Vec<SearchHit> = statement
            .query_map([i64::try_from(offset)?], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect::<Result<_>>()?;
        let next = offset + hits.len();
        Ok((hits, (next < self.count).then_some(next)))
    }

    /// Message-hit totals over the whole result for the sessions of `hits`, so a session split
    /// across pages still reports its full count.
    pub fn totals(&self, hits: &[SearchHit]) -> Result<HashMap<String, usize>> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT COUNT(*) FROM hits WHERE session=?1 AND kind=1")?;
        let mut totals = HashMap::new();
        for hit in hits {
            if !totals.contains_key(&hit.session) {
                let total = statement.query_row([&hit.session], |row| row.get::<_, i64>(0))?;
                totals.insert(hit.session.clone(), usize::try_from(total)?);
            }
        }
        Ok(totals)
    }
}

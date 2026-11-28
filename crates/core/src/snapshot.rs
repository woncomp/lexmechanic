use crate::{
    model::Cancellation,
    search::{SearchCursor, SearchHit, SearchQuery},
    store::Store,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::{path::Path, sync::Arc};
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
}

impl SearchSnapshot {
    /// The caller pins a consistent read view until the result cache is complete.
    pub fn capture(
        source: &Store,
        query: &SearchQuery,
        directory: &Path,
        cancel: &Cancellation,
    ) -> Result<Self> {
        let file = NamedTempFile::new_in(directory)?.into_temp_path();
        let mut connection = Connection::open(&file)?;
        connection
            .execute_batch("CREATE TABLE hits(position INTEGER PRIMARY KEY,data TEXT NOT NULL);")?;
        let tx = connection.transaction()?;
        let mut cursor = SearchCursor::default();
        let mut count = 0;
        let mut metadata = std::collections::HashMap::new();
        for hit in source.title_hits(query, cancel)? {
            tx.execute(
                "INSERT INTO hits(position,data) VALUES(?1,?2)",
                params![i64::try_from(count)?, serde_json::to_string(&hit)?],
            )?;
            count += 1;
        }
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
                tx.execute(
                    "INSERT INTO hits(position,data) VALUES(?1,?2)",
                    params![i64::try_from(count)?, serde_json::to_string(&hit)?],
                )?;
                count += 1;
            }
            match page.next {
                Some(next) => cursor = next,
                None => break,
            }
        }
        tx.commit()?;
        Ok(Self {
            connection,
            _file: file,
            count,
        })
    }

    pub fn page(&self, offset: usize) -> Result<(Vec<SearchHit>, Option<usize>)> {
        let mut statement = self
            .connection
            .prepare("SELECT data FROM hits WHERE position>=?1 ORDER BY position LIMIT 512")?;
        let hits: Vec<SearchHit> = statement
            .query_map([i64::try_from(offset)?], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect::<Result<_>>()?;
        let next = offset + hits.len();
        Ok((hits, (next < self.count).then_some(next)))
    }
}

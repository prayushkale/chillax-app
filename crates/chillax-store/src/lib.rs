//! SQLite event log. The projection is rebuilt by replay, so the only
//! table is the append-only `events` table.

mod attachments;

pub use attachments::FileAttachmentStore;

use std::path::Path;
use std::str::FromStr;

use async_trait::async_trait;
use chillax_core::{CommandError, Event, EventLog};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("corrupt event {seq}: {source}")]
    Decode {
        seq: i64,
        #[source]
        source: serde_json::Error,
    },
}

impl From<StoreError> for CommandError {
    fn from(err: StoreError) -> Self {
        CommandError::Storage(err.to_string())
    }
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS events (
    seq   INTEGER PRIMARY KEY,
    ts    TEXT NOT NULL,
    kind  TEXT NOT NULL,
    body  TEXT NOT NULL
)";

#[derive(Debug, Clone)]
pub struct SqliteLog {
    pool: SqlitePool,
}

impl SqliteLog {
    /// Open (creating if needed) the database at `path`.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::from_str("sqlite://")?
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    async fn query_since(&self, seq: u64) -> Result<Vec<Event>, StoreError> {
        let rows = sqlx::query("SELECT seq, body FROM events WHERE seq > ? ORDER BY seq")
            .bind(seq as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let seq: i64 = row.get("seq");
                let body: String = row.get("body");
                serde_json::from_str(&body).map_err(|source| StoreError::Decode { seq, source })
            })
            .collect()
    }
}

#[async_trait]
impl EventLog for SqliteLog {
    async fn append(&self, events: &[Event]) -> Result<(), CommandError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::from)?;
        for event in events {
            let body =
                serde_json::to_string(event).map_err(|e| CommandError::Storage(e.to_string()))?;
            sqlx::query("INSERT INTO events (seq, ts, kind, body) VALUES (?, ?, ?, ?)")
                .bind(event.seq as i64)
                .bind(event.ts.to_rfc3339())
                .bind(event.kind.name())
                .bind(body)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::from)?;
        }
        tx.commit().await.map_err(StoreError::from)?;
        Ok(())
    }

    async fn load_all(&self) -> Result<Vec<Event>, CommandError> {
        Ok(self.query_since(0).await?)
    }

    async fn since(&self, seq: u64) -> Result<Vec<Event>, CommandError> {
        Ok(self.query_since(seq).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chillax_core::{Actor, EventKind};

    fn event(seq: u64) -> Event {
        Event {
            seq,
            ts: chrono::Utc::now(),
            actor: Actor::Stakeholder,
            kind: EventKind::ChatMessageSent {
                to: "pat".into(),
                text: format!("hello {seq}"),
            },
        }
    }

    #[tokio::test]
    async fn append_and_replay_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chillax.db");
        let log = SqliteLog::open(&path).await.expect("opens");
        log.append(&[event(1), event(2)]).await.expect("appends");
        log.append(&[event(3)]).await.expect("appends");

        let reopened = SqliteLog::open(&path).await.expect("reopens");
        let all = reopened.load_all().await.expect("loads");
        assert_eq!(
            all,
            vec![
                event_eq(&all[0], 1),
                event_eq(&all[1], 2),
                event_eq(&all[2], 3)
            ]
        );
        assert_eq!(reopened.since(2).await.expect("loads").len(), 1);
    }

    #[tokio::test]
    async fn duplicate_seq_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = SqliteLog::open(&dir.path().join("db"))
            .await
            .expect("opens");
        log.append(&[event(1)]).await.expect("appends");
        assert!(log.append(&[event(1)]).await.is_err());
        assert_eq!(log.load_all().await.expect("loads").len(), 1);
    }

    /// Timestamps round-trip through RFC 3339; compare everything else exactly.
    fn event_eq(loaded: &Event, seq: u64) -> Event {
        Event {
            ts: loaded.ts,
            ..event(seq)
        }
    }
}

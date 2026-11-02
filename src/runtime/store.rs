//! SQLite: the event log, and what must commit with it.
//!
//! One writer connection behind a mutex, so `AUTOINCREMENT` order is commit order
//! and a reader can never see a later `seq` before an earlier one. An event and its
//! effects commit in one transaction. Live readers follow the `watch` tip, which
//! moves only after the commit (the `lattice_events` pattern, with `watch` in place
//! of `Notify` so a reader that's busy can't miss a wakeup).

use super::{GlobalSeq, ThreadId};
use futures::stream::{self, BoxStream, StreamExt};
use plexus_harness::{AgentEvent, RawFrame, RunId};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY, harness TEXT NOT NULL, spec TEXT NOT NULL, cwd TEXT,
    session TEXT, status TEXT NOT NULL, last_turn INTEGER NOT NULL DEFAULT 0,
    started INTEGER NOT NULL DEFAULT 0, parked TEXT, note TEXT, created_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY, thread TEXT NOT NULL, pid INTEGER, identity TEXT,
    live INTEGER NOT NULL, reason TEXT, started_ms INTEGER NOT NULL, ended_ms INTEGER);
CREATE INDEX IF NOT EXISTS idx_runs_thread ON runs(thread);
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, thread TEXT NOT NULL, run TEXT NOT NULL,
    at_ms INTEGER NOT NULL, body TEXT NOT NULL, raw TEXT);
CREATE INDEX IF NOT EXISTS idx_events_thread ON events(thread, seq);
CREATE TABLE IF NOT EXISTS inputs (
    id TEXT PRIMARY KEY, thread TEXT NOT NULL, run TEXT NOT NULL, turn INTEGER NOT NULL,
    tool_use_id TEXT, request TEXT NOT NULL, delivery TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_inputs_thread ON inputs(thread, delivery);
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "store: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self(e.to_string())
    }
}

/// One event in the log: where it sits overall, and which thread and run it's from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    pub seq: GlobalSeq,
    pub thread: ThreadId,
    pub run: RunId,
    pub event: AgentEvent,
}

/// What a reader wants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadFilter {
    /// One thread, or all of them.
    pub thread: Option<ThreadId>,
    /// Leave raw frames out, for a client that doesn't want one on every delta.
    pub without_raw: bool,
}

/// The durable log. Appends commit before `read` can return them, and an appended
/// event comes back with its raw frame byte-identical.
pub trait EventLog: Send + Sync + 'static {
    /// Appends events in one transaction and returns their global sequence.
    fn append(
        &self,
        thread: &ThreadId,
        run: &RunId,
        events: &[AgentEvent],
    ) -> Result<Vec<GlobalSeq>, StoreError>;

    /// One page of what's after `after`, oldest first.
    fn page(
        &self,
        filter: &ReadFilter,
        after: GlobalSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError>;

    /// Everything after `after`, then live events, for as long as it's read.
    /// Dropping the stream affects nothing but the reader.
    fn read(&self, filter: ReadFilter, after: GlobalSeq) -> BoxStream<'static, StoredEvent>;

    /// The newest sequence in the log.
    fn tip(&self) -> GlobalSeq;
}

/// The SQLite log, and the tables that commit with it.
pub struct Store {
    conn: Mutex<Connection>,
    tip: watch::Sender<u64>,
}

/// An open transaction. Events and effects made through this commit together.
pub struct Tx<'a> {
    pub sql: &'a Transaction<'a>,
    last: u64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Arc<Self>, StoreError> {
        let conn = Connection::open(path)?;
        // WAL with NORMAL: a commit survives this process dying, and only an OS
        // crash or power loss can lose the last few. The benchmark measures this.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        let tip: u64 =
            conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
            tip: watch::channel(tip).0,
        }))
    }

    /// Runs `f` in one transaction. Readers hear of it only after it commits.
    pub fn tx<R>(
        &self,
        f: impl FnOnce(&mut Tx<'_>) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let mut conn = self.conn.lock().expect("store poisoned");
        let sql = conn.transaction()?;
        let mut tx = Tx { sql: &sql, last: 0 };
        let out = f(&mut tx)?;
        let last = tx.last;
        sql.commit()?;
        drop(conn);
        if last != 0 {
            self.tip.send_replace(last);
        }
        Ok(out)
    }

    /// A read with no transaction.
    pub fn with<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        f(&self.conn.lock().expect("store poisoned"))
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.tip.subscribe()
    }
}

impl Tx<'_> {
    /// Appends one event. `raw` goes in its own column, exactly as given.
    pub fn append(
        &mut self,
        thread: &ThreadId,
        run: &RunId,
        event: &AgentEvent,
    ) -> Result<GlobalSeq, StoreError> {
        let mut body = event.clone();
        let raw = body.raw.take();
        self.sql
            .prepare_cached(
                "INSERT INTO events (thread, run, at_ms, body, raw) VALUES (?, ?, ?, ?, ?)",
            )?
            .execute(params![
                thread.as_str(),
                run.as_str(),
                event.at_ms as i64,
                serde_json::to_string(&body)?,
                raw.as_ref().map(RawFrame::as_str),
            ])?;
        self.last = self.sql.last_insert_rowid() as u64;
        Ok(GlobalSeq(self.last))
    }
}

fn row_event(without_raw: bool, r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEvent> {
    let body: String = r.get(3)?;
    let raw: Option<String> = r.get(4)?;
    let mut event: AgentEvent = serde_json::from_str(&body).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    event.raw = if without_raw {
        None
    } else {
        raw.map(RawFrame::new)
    };
    Ok(StoredEvent {
        seq: GlobalSeq(r.get::<_, i64>(0)? as u64),
        thread: ThreadId::new(r.get::<_, String>(1)?),
        run: RunId::new(r.get::<_, String>(2)?),
        event,
    })
}

impl Store {
    pub fn latest_run(&self, thread: &ThreadId) -> Result<Option<RunId>, StoreError> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT id FROM runs WHERE thread = ? ORDER BY started_ms DESC, rowid DESC LIMIT 1",
                [thread.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(RunId::new))
        })
    }
}

impl EventLog for Arc<Store> {
    fn append(
        &self,
        thread: &ThreadId,
        run: &RunId,
        events: &[AgentEvent],
    ) -> Result<Vec<GlobalSeq>, StoreError> {
        self.tx(|tx| events.iter().map(|e| tx.append(thread, run, e)).collect())
    }

    fn page(
        &self,
        filter: &ReadFilter,
        after: GlobalSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        self.with(|c| {
            const COLUMNS: &str = "SELECT seq, thread, run, body, raw FROM events";
            let map = |r: &rusqlite::Row<'_>| row_event(filter.without_raw, r);
            let rows = match &filter.thread {
                Some(t) => c
                    .prepare_cached(&format!(
                        "{COLUMNS} WHERE thread = ? AND seq > ? ORDER BY seq LIMIT ?"
                    ))?
                    .query_map(params![t.as_str(), after.0 as i64, limit as i64], map)?
                    .collect::<Result<Vec<_>, _>>()?,
                None => c
                    .prepare_cached(&format!("{COLUMNS} WHERE seq > ? ORDER BY seq LIMIT ?"))?
                    .query_map(params![after.0 as i64, limit as i64], map)?
                    .collect::<Result<Vec<_>, _>>()?,
            };
            Ok(rows)
        })
    }

    fn read(&self, filter: ReadFilter, after: GlobalSeq) -> BoxStream<'static, StoredEvent> {
        const PAGE: usize = 512;
        let store = self.clone();
        let tip = self.subscribe();
        stream::unfold(
            (
                store,
                tip,
                filter,
                after,
                Vec::<StoredEvent>::new().into_iter(),
            ),
            |(store, mut tip, filter, mut after, mut held)| async move {
                loop {
                    if let Some(e) = held.next() {
                        after = e.seq;
                        return Some((e, (store, tip, filter, after, held)));
                    }
                    tip.mark_unchanged();
                    let page = EventLog::page(&store, &filter, after, PAGE).ok()?;
                    if page.is_empty() {
                        tip.changed().await.ok()?;
                        continue;
                    }
                    held = page.into_iter();
                }
            },
        )
        .boxed()
    }

    fn tip(&self) -> GlobalSeq {
        GlobalSeq(*self.subscribe().borrow())
    }
}

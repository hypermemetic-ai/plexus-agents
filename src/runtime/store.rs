//! SQLite: the event log, and what must commit with it.
//!
//! One writer connection behind a mutex, so `AUTOINCREMENT` order is commit order
//! and a reader can never see a later `seq` before an earlier one. An event and its
//! effects commit in one transaction. Live readers follow the `watch` tip, which
//! moves only after the commit (the `lattice_events` pattern, with `watch` in place
//! of `Notify` so a reader that's busy can't miss a wakeup).

use super::{GlobalSeq, ThreadId};
use futures::stream::{self, BoxStream, StreamExt};
use plexus_harness::{AgentEvent, AgentEventKind, RawFrame, RunId};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY, harness TEXT NOT NULL, spec TEXT NOT NULL, cwd TEXT,
    session TEXT, status TEXT NOT NULL, last_turn INTEGER NOT NULL DEFAULT 0,
    started INTEGER NOT NULL DEFAULT 0, parked TEXT, note TEXT, created_ms INTEGER NOT NULL,
    owner TEXT, placement TEXT, title TEXT, last_ms INTEGER);
CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY, thread TEXT NOT NULL, pid INTEGER, identity TEXT,
    live INTEGER NOT NULL, reason TEXT, started_ms INTEGER NOT NULL, ended_ms INTEGER);
CREATE INDEX IF NOT EXISTS idx_runs_thread ON runs(thread);
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, thread TEXT NOT NULL, run TEXT NOT NULL,
    at_ms INTEGER NOT NULL, body TEXT NOT NULL, raw TEXT,
    kind TEXT, ref TEXT, item INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_events_thread ON events(thread, seq);
CREATE TABLE IF NOT EXISTS inputs (
    id TEXT PRIMARY KEY, thread TEXT NOT NULL, run TEXT NOT NULL, turn INTEGER NOT NULL,
    tool_use_id TEXT, request TEXT NOT NULL, delivery TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_inputs_thread ON inputs(thread, delivery);
";

/// What [`migrate`] adds to an older store, and indexes over it: a thread's
/// history is read by seeking these, never by scanning its deltas.
const INDEXES: &str = "
CREATE INDEX IF NOT EXISTS idx_events_items ON events(thread, seq) WHERE item = 1;
CREATE INDEX IF NOT EXISTS idx_events_ref ON events(thread, ref) WHERE ref IS NOT NULL;
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Each thread this transaction appended to: its newest `at_ms`, and the
    /// title its first prompt here gives it. Written to `threads` at commit.
    touched: HashMap<String, (u64, Option<String>)>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Arc<Self>, StoreError> {
        let mut conn = Connection::open(path)?;
        // WAL with NORMAL: a commit survives this process dying, and only an OS
        // crash or power loss can lose the last few. The benchmark measures this.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&mut conn)?;
        conn.execute_batch(INDEXES)?;
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
        let mut tx = Tx {
            sql: &sql,
            last: 0,
            touched: HashMap::new(),
        };
        let out = f(&mut tx)?;
        let last = tx.last;
        for (thread, (at_ms, title)) in std::mem::take(&mut tx.touched) {
            sql.prepare_cached(
                "UPDATE threads SET last_ms = MAX(COALESCE(last_ms, 0), ?1),
                 title = COALESCE(title, ?2) WHERE id = ?3",
            )?
            .execute(params![at_ms as i64, title, thread])?;
        }
        drop(tx);
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
        let index = Indexed::of(event);
        self.sql
            .prepare_cached(
                "INSERT INTO events (thread, run, at_ms, body, raw, kind, ref, item)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )?
            .execute(params![
                thread.as_str(),
                run.as_str(),
                event.at_ms as i64,
                serde_json::to_string(&body)?,
                raw.as_ref().map(RawFrame::as_str),
                index.kind,
                index.reference,
                index.item,
            ])?;
        self.last = self.sql.last_insert_rowid() as u64;
        let touched = self
            .touched
            .entry(thread.as_str().to_string())
            .or_insert((0, None));
        touched.0 = touched.0.max(event.at_ms);
        if touched.1.is_none() {
            touched.1 = title_of(event);
        }
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
        Store::page_settled(self, filter, after, limit, GlobalSeq(0))
    }

    fn read(&self, filter: ReadFilter, after: GlobalSeq) -> BoxStream<'static, StoredEvent> {
        Store::read_settled(self, filter, after, GlobalSeq(0))
    }

    fn tip(&self) -> GlobalSeq {
        GlobalSeq(*self.subscribe().borrow())
    }
}

/// The kinds a settled read leaves out at or before its cutoff: the pieces of
/// messages that have since finished, and frames with no normalized meaning.
const UNSETTLED: &str = "COALESCE(kind, '') IN ('text.delta', 'thinking.delta', 'native')";

impl Store {
    /// [`EventLog::page`], less what's [`UNSETTLED`] at or before `settled`.
    pub fn page_settled(
        self: &Arc<Self>,
        filter: &ReadFilter,
        after: GlobalSeq,
        limit: usize,
        settled: GlobalSeq,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        self.with(|c| {
            const COLUMNS: &str = "SELECT seq, thread, run, body, raw FROM events";
            let map = |r: &rusqlite::Row<'_>| row_event(filter.without_raw, r);
            let cut = settled.0 as i64;
            let rows = match &filter.thread {
                Some(t) => c
                    .prepare_cached(&format!(
                        "{COLUMNS} WHERE thread = ? AND seq > ? AND NOT (seq <= ? AND {UNSETTLED})
                         ORDER BY seq LIMIT ?"
                    ))?
                    .query_map(params![t.as_str(), after.0 as i64, cut, limit as i64], map)?
                    .collect::<Result<Vec<_>, _>>()?,
                None => c
                    .prepare_cached(&format!(
                        "{COLUMNS} WHERE seq > ? AND NOT (seq <= ? AND {UNSETTLED})
                         ORDER BY seq LIMIT ?"
                    ))?
                    .query_map(params![after.0 as i64, cut, limit as i64], map)?
                    .collect::<Result<Vec<_>, _>>()?,
            };
            Ok(rows)
        })
    }

    /// The newest seq of `thread` that ends a message or a turn: deltas at or
    /// before it belong to messages that finished, and a reader catching up can
    /// go without them (`watch {settled}`). Read once, at the start of a read.
    pub fn settled(&self, thread: &ThreadId) -> Result<GlobalSeq, StoreError> {
        self.with(|c| {
            let seq: i64 = c
                .prepare_cached(
                    "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread = ? AND item = 1",
                )?
                .query_row([thread.as_str()], |r| r.get(0))?;
            Ok(GlobalSeq(seq as u64))
        })
    }

    /// [`EventLog::read`], less what's [`UNSETTLED`] at or before `settled`.
    pub fn read_settled(
        self: &Arc<Self>,
        filter: ReadFilter,
        after: GlobalSeq,
        settled: GlobalSeq,
    ) -> BoxStream<'static, StoredEvent> {
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
            move |(store, mut tip, filter, mut after, mut held)| async move {
                loop {
                    if let Some(e) = held.next() {
                        after = e.seq;
                        return Some((e, (store, tip, filter, after, held)));
                    }
                    tip.mark_unchanged();
                    let page = store.page_settled(&filter, after, PAGE, settled).ok()?;
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
}

/// What an event is, in the columns the history reads by: its kind, the call,
/// request or turn it belongs with (`ref`), and whether it's an item of the
/// transcript on its own (`item`). Results and resolutions aren't: they're
/// joined to their call or request by `ref`. A subagent's own steps aren't
/// either; only its start and end are.
pub(crate) struct Indexed {
    pub kind: &'static str,
    pub reference: Option<String>,
    pub item: bool,
}

impl Indexed {
    pub fn of(event: &AgentEvent) -> Self {
        use AgentEventKind as K;
        let root = event.agent.is_none();
        let (kind, reference, item) = match &event.kind {
            K::SessionStarted { .. } => ("session.started", None, false),
            K::TurnStarted { turn, .. } => ("turn.started", Some(format!("turn:{}", turn.0)), root),
            K::UserMessage { .. } => ("user.message", None, root),
            K::TextDelta { .. } => ("text.delta", None, false),
            K::ThinkingDelta { .. } => ("thinking.delta", None, false),
            K::Message { .. } => ("message", None, root),
            K::ToolCall { id, .. } => ("tool.call", Some(format!("tool:{id}")), root),
            K::ToolResult { id, .. } => ("tool.result", Some(format!("tool:{id}")), false),
            K::Tokens { .. } => ("tokens", None, false),
            K::InputRequested { id } => ("input.requested", Some(format!("input:{id}")), root),
            K::InputResolved { id, .. } => ("input.resolved", Some(format!("input:{id}")), false),
            K::InputRerouted { .. } => ("input.rerouted", None, false),
            K::InputOutcomeUnknown { .. } => ("input.outcome_unknown", None, root),
            K::SubagentStarted { .. } => ("subagent.started", None, true),
            K::SubagentProgress { .. } => ("subagent.progress", None, false),
            K::SubagentEnded { .. } => ("subagent.ended", None, true),
            K::BackgroundTaskStarted { .. } => ("task.started", None, root),
            K::BackgroundTaskEnded { .. } => ("task.ended", None, root),
            K::TurnParked { .. } => ("turn.parked", None, root),
            K::TurnTerminal(_) => ("turn.terminal", None, root),
            K::RunEnded { .. } => ("run.ended", None, root),
            K::Native { .. } => ("native", None, false),
        };
        Self {
            kind,
            reference,
            item,
        }
    }
}

/// How long a thread's title may be, in characters.
pub const TITLE_CHARS: usize = 120;

/// The title a prompt gives its thread: the operator's first message, its
/// whitespace collapsed, cut to [`TITLE_CHARS`]. What the runtime says to the
/// agent (`[runtime] ...`) and a subagent's prompt give none.
pub(crate) fn title_of(event: &AgentEvent) -> Option<String> {
    let AgentEventKind::UserMessage { text, .. } = &event.kind else {
        return None;
    };
    if event.agent.is_some() || text.trim_start().starts_with("[runtime]") {
        return None;
    }
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.is_empty() {
        return None;
    }
    if words.chars().count() <= TITLE_CHARS {
        return Some(words);
    }
    let cut: String = words.chars().take(TITLE_CHARS - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// A store from before the history (SUPERCONDUCTOR UI history): each event's
/// `kind`, `ref` and `item` filled in, and each thread's `title` and `last_ms`,
/// once, in one transaction. A store that has them is left alone.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let has = |c: &Connection, table: &str, column: &str| -> Result<bool, StoreError> {
        let mut q = c.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = q
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(names.iter().any(|n| n == column))
    };
    let events = !has(conn, "events", "item")?;
    let threads = !has(conn, "threads", "last_ms")?;
    if !events && !threads {
        return Ok(());
    }
    let tx = conn.transaction()?;
    if events {
        tx.execute_batch(
            "ALTER TABLE events ADD COLUMN kind TEXT;
             ALTER TABLE events ADD COLUMN ref TEXT;
             ALTER TABLE events ADD COLUMN item INTEGER NOT NULL DEFAULT 0;",
        )?;
        let rows = {
            let mut q = tx.prepare("SELECT seq, body FROM events")?;
            let rows = q
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut set = tx.prepare("UPDATE events SET kind = ?, ref = ?, item = ? WHERE seq = ?")?;
        for (seq, body) in rows {
            // A body this build can't read keeps no kind: it's sent as it was.
            if let Ok(event) = serde_json::from_str::<AgentEvent>(&body) {
                let i = Indexed::of(&event);
                set.execute(params![i.kind, i.reference, i.item, seq])?;
            }
        }
    }
    if threads {
        tx.execute_batch(
            "ALTER TABLE threads ADD COLUMN title TEXT;
             ALTER TABLE threads ADD COLUMN last_ms INTEGER;
             UPDATE threads SET last_ms =
                 (SELECT MAX(at_ms) FROM events WHERE events.thread = threads.id);",
        )?;
        let prompts = {
            let mut q = tx.prepare(
                "SELECT thread, body FROM events WHERE kind = 'user.message' ORDER BY seq",
            )?;
            let rows = q
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut titled = std::collections::HashSet::new();
        for (thread, body) in prompts {
            if titled.contains(&thread) {
                continue;
            }
            let Ok(event) = serde_json::from_str::<AgentEvent>(&body) else {
                continue;
            };
            if let Some(title) = title_of(&event) {
                tx.execute(
                    "UPDATE threads SET title = ? WHERE id = ?",
                    params![title, thread],
                )?;
                titled.insert(thread);
            }
        }
    }
    tx.commit()?;
    Ok(())
}

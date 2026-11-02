//! Owns every process, and every pending request.
//!
//! A caller going away changes nothing: a run's events are pumped by a task of the
//! supervisor's, and what a client sees comes from the log.
//!
//! ## An answer is applied once
//!
//! AGENTS-33's `h_crash_kept` ran a tool twice: the service died mid-tool with the
//! answer still stored, and a resume applied it again. So an answer moves through
//! [`Delivery`], and `Delivered` commits before anything can act on it. After a
//! crash a `Delivered` row with no tool result becomes `OutcomeUnknown`, which is
//! shown to a person and never re-applied. The hook side takes its answer file by
//! `rename`, so it can't be applied twice from there either.

use super::store::{EventLog, Store, StoreError, Tx};
use super::{GlobalSeq, ReadFilter, StoredEvent, ThreadId};
use futures::future::{BoxFuture, FutureExt};
use futures::stream::{BoxStream, StreamExt};
use plexus_harness::{
    now_ms, resume_or_restart, retry_answer, AgentEvent, AgentEventKind, Attribution, CancelReason,
    Environment, EventSeq, Harness, InputAnswer, InputError, InputFilter, InputKind, InputOutcome,
    InputQueue, InputRequest, InputRequestId, InputRoute, InputState, InterruptOutcome, ParkAnswer,
    ParkSpec, ParkedCall, ParkedRun, Run, RunEndReason, RunError, RunId, RunSpec, SessionId,
    SessionMode, StopCause, StopMode, StopReport, ThreadDisposition, ToolName, ToolUseId, TurnId,
    TurnStatus, TurnTerminal, UserInput,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Most events committed in one transaction.
const BATCH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// No process. A prompt starts or resumes one.
    Idle,
    /// A process is running.
    Running,
    /// No process: waiting on an answer, or on a person after an unknown outcome.
    Parked,
    /// The conversation can't go on.
    Broken,
}

impl ThreadStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Parked => "parked",
            Self::Broken => "broken",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "parked" => Self::Parked,
            "broken" => Self::Broken,
            _ => Self::Idle,
        }
    }
}

/// Where an answer is on its way to a tool (AGENTS-33).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// No answer yet.
    Pending,
    /// Answered and stored, and nothing has acted on it, so it's safe to apply.
    Answered,
    /// Handed over. From here on it must never be applied again.
    Delivered,
    /// Its tool result came back.
    Applied,
    /// Delivered, then the service died before a result. The tool may have run.
    OutcomeUnknown,
    /// Never used: the agent didn't issue that call again, or a retry took it over.
    Superseded,
    Cancelled,
}

impl Delivery {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Delivered => "delivered",
            Self::Applied => "applied",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Superseded => "superseded",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "answered" => Self::Answered,
            "delivered" => Self::Delivered,
            "applied" => Self::Applied,
            "outcome_unknown" => Self::OutcomeUnknown,
            "superseded" => Self::Superseded,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }
}

/// A request, as the runtime holds it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputRecord {
    pub thread: ThreadId,
    pub run: RunId,
    pub request: InputRequest,
    pub delivery: Delivery,
    pub tool_use_id: Option<ToolUseId>,
    /// An earlier answer to this same call has an unknown outcome. Whoever answers
    /// this should know the tool may already have run.
    pub after_unknown_outcome: bool,
}

/// What a client needs to draw a thread without replaying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThreadView {
    pub id: ThreadId,
    pub harness: String,
    pub status: ThreadStatus,
    pub cwd: Option<String>,
    pub session: Option<SessionId>,
    pub last_turn: TurnId,
    pub runs: u64,
    pub pending: Vec<InputRequestId>,
    pub outcome_unknown: Vec<InputRequestId>,
    /// This thread's newest event: a `since` that picks up from here.
    pub last_seq: GlobalSeq,
}

/// What a restart found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Runs closed out as interrupted.
    pub interrupted: Vec<RunId>,
    /// Processes that outlived the old service, and were killed.
    pub reaped: usize,
    /// Answers whose tool may have run.
    pub outcome_unknown: Vec<InputRequestId>,
    /// Live requests that now go in as a message.
    pub rerouted: Vec<InputRequestId>,
    /// Live requests no answer can reach.
    pub expired: Vec<InputRequestId>,
}

#[derive(Debug)]
pub enum RuntimeError {
    NoThread(ThreadId),
    NoHarness(String),
    /// A prompt on a thread with an undecided request. A prompt would drop the
    /// call, and a late answer with it (AGENTS-33 `h_prompt_undecided`). Answer the
    /// requests, or cancel them with `interrupt`.
    Undecided {
        thread: ThreadId,
        requests: Vec<InputRequestId>,
    },
    Broken(ThreadId),
    /// There's no process, and nothing parked to continue.
    NotRunning(ThreadId),
    Start(String),
    Run(RunError),
    Store(StoreError),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoThread(t) => write!(f, "no thread {t}"),
            Self::NoHarness(h) => write!(f, "no harness {h}"),
            Self::Undecided { thread, requests } => {
                write!(f, "thread {thread} has undecided requests: {requests:?}")
            }
            Self::Broken(t) => write!(f, "thread {t} is broken"),
            Self::NotRunning(t) => write!(f, "thread {t} has no process"),
            Self::Start(s) => write!(f, "start: {s}"),
            Self::Run(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<StoreError> for RuntimeError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<RunError> for RuntimeError {
    fn from(e: RunError) -> Self {
        Self::Run(e)
    }
}

pub struct SupervisorConfig {
    pub db: PathBuf,
    /// Where parking runs keep their files, one directory a thread. In the
    /// environment's filesystem, and outside any `cwd`.
    pub state_root: String,
    pub environment: Arc<dyn Environment>,
    pub harnesses: Vec<Arc<dyn Harness>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadOptions {
    /// Shut the process down on a pending request, where the harness can.
    pub park: bool,
}

struct ThreadRow {
    harness: String,
    spec: RunSpec,
    session: Option<SessionId>,
    status: ThreadStatus,
    last_turn: u64,
    started: bool,
    parked: Option<ParkedRun>,
}

struct Inner {
    store: Arc<Store>,
    env: Arc<dyn Environment>,
    harnesses: HashMap<String, Arc<dyn Harness>>,
    state_root: String,
    live: Mutex<HashMap<ThreadId, Arc<dyn Run>>>,
    /// One at a time per thread, for whatever starts or feeds a process.
    gates: Mutex<HashMap<ThreadId, Arc<tokio::sync::Mutex<()>>>>,
}

/// See the module docs. Cheap to clone.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

fn store_err(e: impl fmt::Display) -> StoreError {
    StoreError(e.to_string())
}

fn cancelled(reason: CancelReason) -> InputState {
    InputState::Cancelled {
        reason,
        cancelled_at_ms: now_ms(),
    }
}

/// The tool and input a request asks about, from whichever frame it came in.
fn call_of(request: &InputRequest, tool_use_id: Option<&ToolUseId>) -> Option<ParkedCall> {
    let (tool, input) = match &request.kind {
        InputKind::ToolApproval { tool, input, .. } => (tool.clone(), input.clone()),
        InputKind::Question { .. } => {
            let raw = &request.raw;
            let input = if raw["input"].is_null() {
                raw["request"]["input"].clone()
            } else {
                raw["input"].clone()
            };
            (ToolName::new("AskUserQuestion"), input)
        }
        InputKind::Other { .. } => return None,
    };
    Some(ParkedCall {
        request: request.id.clone(),
        tool_use_id: tool_use_id?.clone(),
        tool,
        input,
        kind: request.kind.clone(),
        agent: request.agent.clone(),
    })
}

fn tool_use_of(kind: &InputKind) -> Option<ToolUseId> {
    match kind {
        InputKind::ToolApproval { tool_use_id, .. } | InputKind::Question { tool_use_id, .. } => {
            tool_use_id.clone()
        }
        InputKind::Other { .. } => None,
    }
}

impl Tx<'_> {
    fn run_event(
        &mut self,
        thread: &ThreadId,
        run: &RunId,
        who: Attribution,
        kind: AgentEventKind,
    ) -> Result<GlobalSeq, StoreError> {
        let n: i64 = self.sql.query_row(
            "SELECT COUNT(*) FROM events WHERE thread = ? AND run = ?",
            params![thread.as_str(), run.as_str()],
            |r| r.get(0),
        )?;
        let event = AgentEvent {
            seq: EventSeq(n as u64 + 1),
            at_ms: now_ms(),
            in_turn: who.turn,
            agent: who.agent,
            kind,
            raw: None,
        };
        let seq = self.append(thread, run, &event)?;
        self.effects(thread, run, &event, &HashMap::new(), None)?;
        Ok(seq)
    }

    fn set_input(
        &self,
        id: &InputRequestId,
        request: Option<&InputRequest>,
        delivery: Delivery,
    ) -> Result<(), StoreError> {
        match request {
            Some(r) => self.sql.execute(
                "UPDATE inputs SET request = ?, delivery = ? WHERE id = ?",
                params![serde_json::to_string(r)?, delivery.as_str(), id.as_str()],
            )?,
            None => self.sql.execute(
                "UPDATE inputs SET delivery = ? WHERE id = ?",
                params![delivery.as_str(), id.as_str()],
            )?,
        };
        Ok(())
    }

    fn input(
        &self,
        id: &InputRequestId,
    ) -> Result<Option<(InputRequest, Delivery, String, String)>, StoreError> {
        let row = self
            .sql
            .query_row(
                "SELECT request, delivery, thread, run FROM inputs WHERE id = ?",
                [id.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(q, d, t, r)| Ok((serde_json::from_str(&q)?, Delivery::parse(&d), t, r)))
            .transpose()
    }

    fn set_status(&self, thread: &ThreadId, status: ThreadStatus) -> Result<(), StoreError> {
        self.sql.execute(
            "UPDATE threads SET status = ? WHERE id = ?",
            params![status.as_str(), thread.as_str()],
        )?;
        Ok(())
    }

    fn set_parked(&self, thread: &ThreadId, parked: Option<&ParkedRun>) -> Result<(), StoreError> {
        let text = parked.map(serde_json::to_string).transpose()?;
        self.sql.execute(
            "UPDATE threads SET parked = ? WHERE id = ?",
            params![text, thread.as_str()],
        )?;
        Ok(())
    }

    /// An event's effects, in the transaction that appends it.
    fn effects(
        &mut self,
        thread: &ThreadId,
        run: &RunId,
        event: &AgentEvent,
        requests: &HashMap<InputRequestId, InputRequest>,
        parked: Option<&ParkedRun>,
    ) -> Result<(), StoreError> {
        match &event.kind {
            AgentEventKind::SessionStarted { session, .. } => {
                self.sql.execute(
                    "UPDATE threads SET session = ? WHERE id = ?",
                    params![session.as_str(), thread.as_str()],
                )?;
            }
            AgentEventKind::TurnStarted { turn, .. } => {
                self.sql.execute(
                    "UPDATE threads SET last_turn = MAX(last_turn, ?) WHERE id = ?",
                    params![turn.0 as i64, thread.as_str()],
                )?;
            }
            AgentEventKind::InputRequested { id } => {
                if let Some(r) = requests.get(id) {
                    self.sql.execute(
                        "INSERT OR IGNORE INTO inputs (id, thread, run, turn, tool_use_id, request, delivery)
                         VALUES (?, ?, ?, ?, ?, ?, ?)",
                        params![
                            id.as_str(),
                            thread.as_str(),
                            run.as_str(),
                            r.turn.0 as i64,
                            tool_use_of(&r.kind).as_ref().map(ToolUseId::as_str),
                            serde_json::to_string(r)?,
                            Delivery::Pending.as_str(),
                        ],
                    )?;
                }
            }
            AgentEventKind::InputResolved {
                id,
                outcome: InputOutcome::Cancelled(reason),
            } => {
                if let Some((mut r, delivery, ..)) = self.input(id)? {
                    if delivery == Delivery::Pending {
                        r.state = cancelled(*reason);
                        self.set_input(id, Some(&r), Delivery::Cancelled)?;
                    }
                }
            }
            AgentEventKind::ToolResult { id, .. } => {
                // A parked call that got its result isn't parked any more, so a crash
                // from here on leaves an ordinary open turn.
                let parked: Option<String> = self.sql.query_row(
                    "SELECT parked FROM threads WHERE id = ?",
                    [thread.as_str()],
                    |r| r.get(0),
                )?;
                if let Some(text) = parked {
                    let mut p: ParkedRun = serde_json::from_str(&text)?;
                    p.calls.retain(|c| c.tool_use_id != *id);
                    self.set_parked(thread, (!p.calls.is_empty()).then_some(&p))?;
                }
                self.sql.execute(
                    "UPDATE inputs SET delivery = ? WHERE thread = ? AND tool_use_id = ? AND delivery = ?",
                    params![
                        Delivery::Applied.as_str(),
                        thread.as_str(),
                        id.as_str(),
                        Delivery::Delivered.as_str()
                    ],
                )?;
            }
            AgentEventKind::TurnParked { .. } => {
                self.set_parked(thread, parked)?;
            }
            AgentEventKind::TurnTerminal(t) => {
                // The turn is over, so an answer nothing used never will be.
                let route = "\"route\":\"message\"";
                self.sql.execute(
                    "UPDATE inputs SET delivery = ? WHERE thread = ? AND delivery = ?
                     AND (turn = ? OR instr(request, ?) > 0)",
                    params![
                        Delivery::Superseded.as_str(),
                        thread.as_str(),
                        Delivery::Delivered.as_str(),
                        t.turn.0 as i64,
                        route
                    ],
                )?;
                self.set_parked(thread, None)?;
                if t.thread_disposition == ThreadDisposition::Broken {
                    self.set_status(thread, ThreadStatus::Broken)?;
                }
            }
            AgentEventKind::RunEnded { reason, .. } => {
                self.sql.execute(
                    "UPDATE runs SET live = 0, reason = ?, ended_ms = ? WHERE id = ?",
                    params![
                        serde_json::to_string(reason)?,
                        event.at_ms as i64,
                        run.as_str()
                    ],
                )?;
                let status = self.sql.query_row(
                    "SELECT status FROM threads WHERE id = ?",
                    [thread.as_str()],
                    |r| r.get::<_, String>(0),
                )?;
                if ThreadStatus::parse(&status) != ThreadStatus::Broken {
                    let parked = *reason == RunEndReason::Parked;
                    self.set_status(
                        thread,
                        if parked {
                            ThreadStatus::Parked
                        } else {
                            ThreadStatus::Idle
                        },
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Inner {
    fn harness(&self, name: &str) -> Result<Arc<dyn Harness>, RuntimeError> {
        self.harnesses
            .get(name)
            .cloned()
            .ok_or_else(|| RuntimeError::NoHarness(name.to_string()))
    }

    fn gate(&self, thread: &ThreadId) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .lock()
            .expect("gates poisoned")
            .entry(thread.clone())
            .or_default()
            .clone()
    }

    fn live_run(&self, thread: &ThreadId) -> Option<Arc<dyn Run>> {
        self.live
            .lock()
            .expect("live poisoned")
            .get(thread)
            .cloned()
    }

    fn thread(&self, id: &ThreadId) -> Result<ThreadRow, RuntimeError> {
        let row = self.store.with(|c| {
            Ok(c.query_row(
                "SELECT harness, spec, session, status, last_turn, started, parked
                 FROM threads WHERE id = ?",
                [id.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?)
        })?;
        let (harness, spec, session, status, last_turn, started, parked) =
            row.ok_or_else(|| RuntimeError::NoThread(id.clone()))?;
        Ok(ThreadRow {
            harness,
            spec: serde_json::from_str(&spec).map_err(store_err)?,
            session: session.map(SessionId::new),
            status: ThreadStatus::parse(&status),
            last_turn: last_turn as u64,
            started: started != 0,
            parked: parked
                .map(|p| serde_json::from_str(&p))
                .transpose()
                .map_err(store_err)?,
        })
    }

    fn records(&self, thread: &ThreadId) -> Result<Vec<InputRecord>, StoreError> {
        let rows = self.store.with(|c| {
            let mut q = c.prepare_cached(
                "SELECT run, request, delivery, tool_use_id FROM inputs WHERE thread = ? ORDER BY rowid",
            )?;
            let rows = q
                .query_map([thread.as_str()], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let unknown: BTreeSet<&str> = rows
            .iter()
            .filter(|r| Delivery::parse(&r.2) == Delivery::OutcomeUnknown)
            .filter_map(|r| r.3.as_deref())
            .collect();
        rows.iter()
            .map(|(run, request, delivery, tool)| {
                let delivery = Delivery::parse(delivery);
                Ok(InputRecord {
                    thread: thread.clone(),
                    run: RunId::new(run),
                    request: serde_json::from_str(request)?,
                    delivery,
                    tool_use_id: tool.as_deref().map(ToolUseId::new),
                    after_unknown_outcome: delivery != Delivery::OutcomeUnknown
                        && tool.as_deref().is_some_and(|t| unknown.contains(t)),
                })
            })
            .collect()
    }

    fn undecided(&self, thread: &ThreadId) -> Result<Vec<InputRequestId>, StoreError> {
        Ok(self
            .records(thread)?
            .into_iter()
            .filter(|r| r.delivery == Delivery::Pending)
            .map(|r| r.request.id)
            .collect())
    }

    /// Answers delivered and not yet used: a retry can take one by exact match.
    fn retry_pool(
        &self,
        thread: &ThreadId,
    ) -> Result<Vec<(InputRequestId, ParkAnswer)>, StoreError> {
        Ok(self
            .records(thread)?
            .into_iter()
            .filter(|r| r.delivery == Delivery::Delivered)
            .filter_map(|r| {
                let InputState::Answered { answer, .. } = &r.request.state else {
                    return None;
                };
                let call = call_of(&r.request, r.tool_use_id.as_ref())?;
                let answer = answer.clone();
                Some((r.request.id.clone(), ParkAnswer { call, answer }))
            })
            .collect())
    }

    async fn register(
        self: &Arc<Self>,
        thread: &ThreadId,
        run: Arc<dyn Run>,
    ) -> Result<(), RuntimeError> {
        let info = run.info();
        let identity = match info.pid {
            Some(pid) => self.env.identity(pid).await,
            None => None,
        };
        self.store.tx(|tx| {
            tx.sql.execute(
                "INSERT INTO runs (id, thread, pid, identity, live, started_ms) VALUES (?, ?, ?, ?, 1, ?)",
                params![
                    info.run.as_str(),
                    thread.as_str(),
                    info.pid,
                    identity,
                    now_ms() as i64
                ],
            )?;
            tx.sql.execute(
                "UPDATE threads SET started = 1 WHERE id = ?",
                [thread.as_str()],
            )?;
            tx.set_status(thread, ThreadStatus::Running)
        })?;
        self.live
            .lock()
            .expect("live poisoned")
            .insert(thread.clone(), run.clone());
        let inner = self.clone();
        let thread = thread.clone();
        tokio::spawn(async move { inner.pump(thread, run).await });
        Ok(())
    }

    /// Moves a run's events into the log. Runs as long as the process does, whoever
    /// is or isn't watching.
    fn pump(self: Arc<Self>, thread: ThreadId, run: Arc<dyn Run>) -> BoxFuture<'static, ()> {
        async move {
            let mut events = run.events(EventSeq(0));
            while let Some(first) = events.next().await {
                let mut batch = vec![first];
                while batch.len() < BATCH {
                    match events.next().now_or_never() {
                        Some(Some(e)) => batch.push(e),
                        _ => break,
                    }
                }
                if let Err(e) = self.ingest(&thread, &run, batch).await {
                    eprintln!("runtime: thread {thread}: {e}");
                }
            }
        }
        .boxed()
    }

    async fn ingest(
        self: &Arc<Self>,
        thread: &ThreadId,
        run: &Arc<dyn Run>,
        batch: Vec<AgentEvent>,
    ) -> Result<(), RuntimeError> {
        let mut requests = HashMap::new();
        if let Some(q) = run.inputs() {
            for e in &batch {
                if let AgentEventKind::InputRequested { id } = &e.kind {
                    if let Ok(r) = q.get(id).await {
                        requests.insert(id.clone(), r);
                    }
                }
            }
        }
        let parked_now = batch
            .iter()
            .any(|e| matches!(e.kind, AgentEventKind::TurnParked { .. }));
        let parked = if parked_now { run.parked() } else { None };
        let id = run.info().run;
        self.store.tx(|tx| {
            for e in &batch {
                tx.append(thread, &id, e)?;
                tx.effects(thread, &id, e, &requests, parked.as_ref())?;
            }
            Ok(())
        })?;
        // Retries: a live re-request may already have its answer.
        for r in requests.values() {
            if r.route == InputRoute::Live {
                self.take_retry_live(thread, r).await?;
            }
        }
        let ended = batch.iter().find_map(|e| match &e.kind {
            AgentEventKind::RunEnded { reason, .. } => Some(*reason),
            _ => None,
        });
        if let Some(reason) = ended {
            self.live.lock().expect("live poisoned").remove(thread);
            if reason == RunEndReason::Parked {
                let inner = self.clone();
                let thread = thread.clone();
                tokio::spawn(async move {
                    if let Err(e) = inner.after_park(&thread).await {
                        eprintln!("runtime: thread {thread}: {e}");
                    }
                });
            }
        }
        Ok(())
    }

    async fn take_retry_live(
        self: &Arc<Self>,
        thread: &ThreadId,
        r: &InputRequest,
    ) -> Result<(), RuntimeError> {
        let tool_use = tool_use_of(&r.kind);
        let Some(call) = call_of(r, tool_use.as_ref()) else {
            return Ok(());
        };
        let pool = self.retry_pool(thread)?;
        let prior: Vec<ParkAnswer> = pool.iter().map(|(_, a)| a.clone()).collect();
        let Some(answer) = retry_answer(&prior, &call.tool, &call.input) else {
            return Ok(());
        };
        self.use_up(&pool, &call)?;
        let inner = self.clone();
        let id = r.id.clone();
        tokio::spawn(async move {
            if let Err(e) = inner.answer(&id, answer).await {
                eprintln!("runtime: retry of {id}: {e}");
            }
        });
        Ok(())
    }

    /// Spends the pool entry that matched `call`, so an answer applies once.
    fn use_up(
        &self,
        pool: &[(InputRequestId, ParkAnswer)],
        call: &ParkedCall,
    ) -> Result<(), StoreError> {
        let used = pool
            .iter()
            .find(|(_, a)| a.call.tool == call.tool && *a.approved_input() == call.input);
        if let Some((id, _)) = used {
            self.store
                .tx(|tx| tx.set_input(id, None, Delivery::Superseded))?;
        }
        Ok(())
    }

    /// After a park: a call the agent re-issued may match an answer it already has.
    async fn after_park(self: &Arc<Self>, thread: &ThreadId) -> Result<(), RuntimeError> {
        let pool = self.retry_pool(thread)?;
        let prior: Vec<ParkAnswer> = pool.iter().map(|(_, a)| a.clone()).collect();
        for r in self.records(thread)? {
            if r.delivery != Delivery::Pending || r.after_unknown_outcome {
                continue;
            }
            let Some(call) = call_of(&r.request, r.tool_use_id.as_ref()) else {
                continue;
            };
            if let Some(answer) = retry_answer(&prior, &call.tool, &call.input) {
                self.use_up(&pool, &call)?;
                self.record_answer(&r.request.id, answer)
                    .map_err(|e| RuntimeError::Start(format!("retry of {}: {e}", r.request.id)))?;
            }
        }
        self.try_apply(thread).await
    }

    /// First answer wins, in one transaction with its event. `Live` is `Delivered`
    /// at once, because the next step is the process's stdin.
    fn record_answer(
        &self,
        id: &InputRequestId,
        answer: InputAnswer,
    ) -> Result<InputRequest, InputError> {
        let done = self.store.tx(|tx| {
            let Some((mut r, _, thread, run)) = tx.input(id)? else {
                return Ok(Err(InputError::NotFound(id.clone())));
            };
            if !r.state.is_pending() {
                return Ok(Err(InputError::AlreadyResolved {
                    id: id.clone(),
                    state: Box::new(r.state),
                }));
            }
            if !answer.fits(&r.kind) {
                return Ok(Err(InputError::AnswerMismatch {
                    id: id.clone(),
                    detail: format!("{:?} doesn't fit this request's kind", answer.outcome()),
                }));
            }
            r.state = InputState::Answered {
                answer: answer.clone(),
                answered_at_ms: now_ms(),
            };
            let live = r.route == InputRoute::Live;
            let delivery = if live {
                Delivery::Delivered
            } else {
                Delivery::Answered
            };
            tx.set_input(id, Some(&r), delivery)?;
            if !live {
                let who = Attribution {
                    turn: Some(r.turn),
                    agent: r.agent.clone(),
                };
                let outcome = answer.outcome();
                let kind = AgentEventKind::InputResolved {
                    id: id.clone(),
                    outcome,
                };
                tx.run_event(&ThreadId::new(thread), &RunId::new(run), who, kind)?;
            }
            Ok(Ok(r))
        });
        done.map_err(|e| InputError::Delivery {
            id: id.clone(),
            detail: e.to_string(),
        })?
    }

    async fn answer(
        self: &Arc<Self>,
        id: &InputRequestId,
        answer: InputAnswer,
    ) -> Result<InputRequest, InputError> {
        let r = self.record_answer(id, answer.clone())?;
        let thread = self.thread_of(id)?;
        let delivery = |detail: String| InputError::Delivery {
            id: id.clone(),
            detail,
        };
        if r.route == InputRoute::Live {
            let queue = self.live_run(&thread).and_then(|run| run.inputs());
            let result = match queue {
                Some(q) => q.answer(id, answer).await.map(|_| ()),
                None => Err(delivery("the run has gone".into())),
            };
            if let Err(e) = result {
                let state = match &e {
                    InputError::AlreadyResolved { state, .. } => (**state).clone(),
                    _ => cancelled(CancelReason::DeliveryFailed),
                };
                let mut r = r.clone();
                r.state = state;
                self.store
                    .tx(|tx| tx.set_input(id, Some(&r), Delivery::Cancelled))
                    .map_err(|e| delivery(e.to_string()))?;
                return Err(e);
            }
            return Ok(r);
        }
        self.try_apply(&thread)
            .await
            .map_err(|e| delivery(e.to_string()))?;
        Ok(r)
    }

    fn thread_of(&self, id: &InputRequestId) -> Result<ThreadId, InputError> {
        let found = self
            .store
            .with(|c| {
                Ok(c.query_row(
                    "SELECT thread FROM inputs WHERE id = ?",
                    [id.as_str()],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })
            .map_err(|e| InputError::Delivery {
                id: id.clone(),
                detail: e.to_string(),
            })?;
        found
            .map(ThreadId::new)
            .ok_or_else(|| InputError::NotFound(id.clone()))
    }

    /// Once every request is decided, feeds the answers in. `Delivered` commits
    /// before the process starts, so no crash can apply one twice. A spawn that
    /// fails put nothing in reach of a tool, so those answers go back to `Answered`.
    async fn try_apply(self: &Arc<Self>, thread: &ThreadId) -> Result<(), RuntimeError> {
        let gate = self.gate(thread);
        let _held = gate.lock().await;
        if self.live_run(thread).is_some() || !self.undecided(thread)?.is_empty() {
            return Ok(());
        }
        let ready: Vec<InputRecord> = self
            .records(thread)?
            .into_iter()
            .filter(|r| r.delivery == Delivery::Answered)
            .collect();
        if ready.is_empty() {
            return Ok(());
        }
        let row = self.thread(thread)?;
        let harness = self.harness(&row.harness)?;
        let mark = |to: Delivery| {
            self.store.tx(|tx| {
                for r in &ready {
                    tx.set_input(&r.request.id, None, to)?;
                }
                Ok(())
            })
        };
        let answer_of = |r: &InputRecord| match &r.request.state {
            InputState::Answered { answer, .. } => Some(answer.clone()),
            _ => None,
        };
        mark(Delivery::Delivered)?;
        let started = async {
            let park = harness
                .park()
                .ok_or_else(|| RuntimeError::Start("this harness can't park".into()))?;
            if let Some(parked) = row
                .parked
                .as_ref()
                .filter(|_| ready.iter().all(|r| r.request.route == InputRoute::Parked))
            {
                let answers: Vec<ParkAnswer> = ready
                    .iter()
                    .filter_map(|r| {
                        let call = parked
                            .calls
                            .iter()
                            .find(|c| Some(&c.tool_use_id) == r.tool_use_id.as_ref())?;
                        Some(ParkAnswer {
                            call: call.clone(),
                            answer: answer_of(r)?,
                        })
                    })
                    .collect();
                return self.unpark(&harness, thread, parked, &answers).await;
            }
            let text: Vec<String> = ready
                .iter()
                .filter_map(|r| {
                    let call = call_of(&r.request, r.tool_use_id.as_ref())?;
                    Some(park.message(&call, &answer_of(r)?, false).text)
                })
                .collect();
            self.launch(thread, &row, UserInput::text(text.join("\n")))
                .await
                .map(|_| ())
        }
        .await;
        if started.is_err() {
            mark(Delivery::Answered)?;
        }
        started
    }

    async fn unpark(
        self: &Arc<Self>,
        harness: &Arc<dyn Harness>,
        thread: &ThreadId,
        parked: &ParkedRun,
        answers: &[ParkAnswer],
    ) -> Result<(), RuntimeError> {
        let fail = |e: &dyn fmt::Display| RuntimeError::Start(e.to_string());
        let park = harness
            .park()
            .ok_or_else(|| RuntimeError::Start("this harness can't park".into()))?;
        let u = park.plan_unpark(parked, answers).map_err(|e| fail(&e))?;
        let process = self.env.spawn(u.plan).await.map_err(|e| fail(&e))?;
        let run = harness.drive(u.spec, process).await.map_err(|e| fail(&e))?;
        self.register(thread, run).await
    }

    /// Starts a process for `prompt`: the thread's first, or a resume from what was
    /// persisted, in the persisted cwd.
    async fn launch(
        self: &Arc<Self>,
        thread: &ThreadId,
        row: &ThreadRow,
        prompt: UserInput,
    ) -> Result<TurnId, RuntimeError> {
        let fail = |e: &dyn fmt::Display| RuntimeError::Start(e.to_string());
        let harness = self.harness(&row.harness)?;
        let spec = RunSpec {
            turns_before: row.last_turn,
            continues: None,
            ..row.spec.clone()
        };
        if !row.started {
            let plan = harness.plan(&spec).map_err(|e| fail(&e))?;
            let process = self.env.spawn(plan).await.map_err(|e| fail(&e))?;
            let run = harness.drive(spec, process).await.map_err(|e| fail(&e))?;
            self.register(thread, run.clone()).await?;
            return Ok(run.send(prompt).await?);
        }
        let session = row
            .session
            .clone()
            .ok_or_else(|| RuntimeError::Start("no session to resume".into()))?;
        let resumed = resume_or_restart(harness, self.env.as_ref(), &spec, &session, prompt)
            .await
            .map_err(|e| fail(&e))?;
        self.register(thread, resumed.run).await?;
        Ok(resumed.turn)
    }

    /// Closes out what a dead service left. See [`Recovery`].
    async fn recover(self: &Arc<Self>) -> Result<Recovery, RuntimeError> {
        let mut report = Recovery::default();
        let lost = self.store.with(|c| {
            let mut q = c.prepare("SELECT id, thread, pid, identity FROM runs WHERE live = 1")?;
            let rows = q
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<u32>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        for (run, thread, pid, identity) in lost {
            let (run, thread) = (RunId::new(run), ThreadId::new(thread));
            // First: nothing below is true while the old process can still act.
            if let (Some(pid), Some(identity)) = (pid, identity) {
                if self.env.reap(pid, &identity).await {
                    report.reaped += 1;
                }
            }
            self.close_out(&thread, &run, &mut report)?;
        }
        // A crash between `Delivered` and the run's row: a process may have started,
        // and no run says so. The same rule applies: never handed over twice.
        let stranded = self.store.with(|c| {
            let mut q = c.prepare("SELECT id, thread, run FROM inputs WHERE delivery = ?")?;
            let rows = q
                .query_map([Delivery::Delivered.as_str()], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        for (id, thread, run) in stranded {
            let id = InputRequestId::new(id);
            let (thread, run) = (ThreadId::new(thread), RunId::new(run));
            self.store.tx(|tx| {
                tx.set_input(&id, None, Delivery::OutcomeUnknown)?;
                let kind = AgentEventKind::InputOutcomeUnknown { id: id.clone() };
                tx.run_event(&thread, &run, Attribution::default(), kind)?;
                Ok(())
            })?;
            report.outcome_unknown.push(id);
        }
        let threads = self.store.with(|c| {
            let mut q = c.prepare("SELECT DISTINCT thread FROM inputs WHERE delivery = ?")?;
            let rows = q
                .query_map([Delivery::Answered.as_str()], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        for t in threads {
            self.try_apply(&ThreadId::new(t)).await?;
        }
        Ok(report)
    }

    fn close_out(
        &self,
        thread: &ThreadId,
        run: &RunId,
        report: &mut Recovery,
    ) -> Result<(), RuntimeError> {
        let row = self.thread(thread)?;
        let answerable = row.session.is_some()
            && row.status != ThreadStatus::Broken
            && self
                .harness(&row.harness)
                .is_ok_and(|h| h.park().is_some() && h.resume().is_some());
        let records = self.records(thread)?;
        let open = self.open_turns(run)?;
        self.store.tx(|tx| {
            for r in &records {
                let who = Attribution {
                    turn: Some(r.request.turn),
                    agent: r.request.agent.clone(),
                };
                let id = r.request.id.clone();
                match (r.delivery, r.request.route) {
                    (Delivery::Delivered, _) => {
                        tx.set_input(&id, None, Delivery::OutcomeUnknown)?;
                        let kind = AgentEventKind::InputOutcomeUnknown { id: id.clone() };
                        tx.run_event(thread, run, who, kind)?;
                        report.outcome_unknown.push(id);
                    }
                    (Delivery::Pending, InputRoute::Live) if answerable => {
                        let mut q = r.request.clone();
                        q.route = InputRoute::Message;
                        tx.set_input(&id, Some(&q), Delivery::Pending)?;
                        let kind = AgentEventKind::InputRerouted {
                            id: id.clone(),
                            route: InputRoute::Message,
                        };
                        tx.run_event(thread, run, who, kind)?;
                        report.rerouted.push(id);
                    }
                    (Delivery::Pending, InputRoute::Live) => {
                        let mut q = r.request.clone();
                        q.state = cancelled(CancelReason::Expired);
                        q.route = InputRoute::NotResumable;
                        tx.set_input(&id, Some(&q), Delivery::Cancelled)?;
                        let outcome = InputOutcome::Cancelled(CancelReason::Expired);
                        let kind = AgentEventKind::InputResolved {
                            id: id.clone(),
                            outcome,
                        };
                        tx.run_event(thread, run, who, kind)?;
                        report.expired.push(id);
                    }
                    _ => {}
                }
            }
            for turn in &open {
                // A parked turn isn't over: it carries on when it's continued.
                if row.parked.as_ref().is_some_and(|p| p.turn == *turn) {
                    continue;
                }
                let terminal = TurnTerminal {
                    turn: *turn,
                    status: TurnStatus::Interrupted,
                    thread_disposition: ThreadDisposition::Reusable,
                    stopped_by: Some(StopCause::ServiceRestart),
                };
                tx.run_event(
                    thread,
                    run,
                    Attribution::turn(*turn),
                    AgentEventKind::TurnTerminal(terminal),
                )?;
            }
            let reason = RunEndReason::Lost;
            let ended = AgentEventKind::RunEnded {
                run: run.clone(),
                reason,
                exit: None,
            };
            tx.run_event(thread, run, Attribution::default(), ended)?;
            if row.status != ThreadStatus::Broken {
                tx.set_status(
                    thread,
                    if row.parked.is_some() {
                        ThreadStatus::Parked
                    } else {
                        ThreadStatus::Idle
                    },
                )?;
            }
            Ok(())
        })?;
        report.interrupted.push(run.clone());
        Ok(())
    }

    /// Turns a run started and never ended.
    fn open_turns(&self, run: &RunId) -> Result<Vec<TurnId>, StoreError> {
        let bodies = self.store.with(|c| {
            let mut q = c.prepare(
                "SELECT body FROM events WHERE run = ?
                 AND (instr(body, '\"turn.started\"') > 0 OR instr(body, '\"turn.terminal\"') > 0
                      OR instr(body, '\"turn.parked\"') > 0)
                 ORDER BY seq",
            )?;
            let rows = q
                .query_map([run.as_str()], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut open = Vec::new();
        for b in bodies {
            let e: AgentEvent = serde_json::from_str(&b).map_err(store_err)?;
            match e.kind {
                AgentEventKind::TurnStarted { turn, .. } => open.push(turn),
                AgentEventKind::TurnTerminal(t) => open.retain(|x| *x != t.turn),
                AgentEventKind::TurnParked { turn, .. } => open.retain(|x| *x != turn),
                _ => {}
            }
        }
        Ok(open)
    }
}

struct ThreadInputs {
    inner: Arc<Inner>,
    thread: ThreadId,
}

impl InputQueue for ThreadInputs {
    fn list(&self, filter: InputFilter) -> BoxFuture<'static, Vec<InputRequest>> {
        let rows = self.inner.records(&self.thread).unwrap_or_default();
        let out = rows
            .into_iter()
            .map(|r| r.request)
            .filter(|r| filter.accepts(r))
            .collect();
        futures::future::ready(out).boxed()
    }

    fn get(&self, id: &InputRequestId) -> BoxFuture<'static, Result<InputRequest, InputError>> {
        let rows = self.inner.records(&self.thread).unwrap_or_default();
        let r = rows
            .into_iter()
            .map(|r| r.request)
            .find(|r| r.id == *id)
            .ok_or_else(|| InputError::NotFound(id.clone()));
        futures::future::ready(r).boxed()
    }

    fn wait(&self, id: &InputRequestId) -> BoxFuture<'static, Result<InputRequest, InputError>> {
        let (inner, thread, id) = (self.inner.clone(), self.thread.clone(), id.clone());
        async move {
            let mut tip = inner.store.subscribe();
            loop {
                tip.mark_unchanged();
                let r = inner
                    .records(&thread)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|r| r.request.id == id)
                    .ok_or_else(|| InputError::NotFound(id.clone()))?;
                if !r.request.state.is_pending() {
                    return Ok(r.request);
                }
                let _ = tokio::time::timeout(Duration::from_millis(200), tip.changed()).await;
            }
        }
        .boxed()
    }

    fn answer(
        &self,
        id: &InputRequestId,
        answer: InputAnswer,
    ) -> BoxFuture<'static, Result<InputRequest, InputError>> {
        let (inner, id) = (self.inner.clone(), id.clone());
        async move { inner.answer(&id, answer).await }.boxed()
    }
}

impl Supervisor {
    /// Opens the store and closes out whatever a previous service left running.
    pub async fn open(config: SupervisorConfig) -> Result<(Self, Recovery), RuntimeError> {
        let inner = Arc::new(Inner {
            store: Store::open(&config.db)?,
            env: config.environment,
            harnesses: config
                .harnesses
                .into_iter()
                .map(|h| (h.name().to_string(), h))
                .collect(),
            state_root: config.state_root,
            live: Mutex::default(),
            gates: Mutex::default(),
        });
        let report = inner.recover().await?;
        Ok((Self { inner }, report))
    }

    pub fn log(&self) -> Arc<Store> {
        self.inner.store.clone()
    }

    /// Everything after `since`, then live events. Across resumes and threads.
    pub fn read(&self, filter: ReadFilter, since: GlobalSeq) -> BoxStream<'static, StoredEvent> {
        self.inner.store.read(filter, since)
    }

    /// A new thread. Nothing starts until its first prompt.
    pub fn create_thread(
        &self,
        harness: &str,
        spec: RunSpec,
        options: ThreadOptions,
    ) -> Result<ThreadId, RuntimeError> {
        let h = self.inner.harness(harness)?;
        let id = ThreadId::new(uuid::Uuid::new_v4().to_string());
        let mut spec = spec;
        // Known before any frame, so a stop before init can still be resumed.
        if spec.session == SessionMode::Fresh {
            spec.session = SessionMode::Assign {
                id: SessionId::new(uuid::Uuid::new_v4().to_string()),
            };
        }
        if options.park {
            if h.park().is_none() {
                return Err(RuntimeError::Start(format!("{harness} can't park")));
            }
            spec.park = Some(ParkSpec {
                state_dir: format!("{}/{id}", self.inner.state_root),
            });
        }
        let session = match &spec.session {
            SessionMode::Assign { id } | SessionMode::Resume { id } => Some(id.to_string()),
            SessionMode::Fresh => None,
        };
        let text = serde_json::to_string(&spec).map_err(store_err)?;
        self.inner.store.tx(|tx| {
            tx.sql.execute(
                "INSERT INTO threads (id, harness, spec, cwd, session, status, created_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    id.as_str(),
                    harness,
                    text,
                    spec.cwd,
                    session,
                    ThreadStatus::Idle.as_str(),
                    now_ms() as i64
                ],
            )?;
            Ok(())
        })?;
        Ok(id)
    }

    /// Sends a prompt, starting or resuming a process if there's none. Refused while
    /// a request is undecided: see [`RuntimeError::Undecided`].
    pub async fn prompt(
        &self,
        thread: &ThreadId,
        input: UserInput,
    ) -> Result<TurnId, RuntimeError> {
        let gate = self.inner.gate(thread);
        let _held = gate.lock().await;
        let row = self.inner.thread(thread)?;
        if row.status == ThreadStatus::Broken {
            return Err(RuntimeError::Broken(thread.clone()));
        }
        let requests = self.inner.undecided(thread)?;
        if !requests.is_empty() {
            return Err(RuntimeError::Undecided {
                thread: thread.clone(),
                requests,
            });
        }
        if let Some(run) = self.inner.live_run(thread) {
            return Ok(run.send(input).await?);
        }
        // A prompt supersedes a parked call whose outcome is unknown: the agent
        // drops it (AGENTS-33 `h_prompt_undecided`).
        self.inner.store.tx(|tx| tx.set_parked(thread, None))?;
        self.inner.launch(thread, &row, input).await
    }

    /// Resumes a parked thread with no answer, after an unknown outcome. The same
    /// call comes back as a new pending request with `after_unknown_outcome` set, so
    /// a person decides with that in view. Nothing is re-applied.
    pub async fn continue_thread(&self, thread: &ThreadId) -> Result<(), RuntimeError> {
        let gate = self.inner.gate(thread);
        let _held = gate.lock().await;
        let row = self.inner.thread(thread)?;
        let parked = row
            .parked
            .as_ref()
            .ok_or_else(|| RuntimeError::NotRunning(thread.clone()))?;
        if self.inner.live_run(thread).is_some() {
            return Ok(());
        }
        let requests = self.inner.undecided(thread)?;
        if !requests.is_empty() {
            return Err(RuntimeError::Undecided {
                thread: thread.clone(),
                requests,
            });
        }
        let harness = self.inner.harness(&row.harness)?;
        self.inner.unpark(&harness, thread, parked, &[]).await
    }

    /// Stops the turn. On a parked thread there's no process, so this cancels its
    /// requests and ends the turn: the way to clear an undecided thread.
    pub async fn interrupt(&self, thread: &ThreadId) -> Result<InterruptOutcome, RuntimeError> {
        if let Some(run) = self.inner.live_run(thread) {
            return match run.interrupt() {
                Some(i) => Ok(i.interrupt().await?),
                None => Err(RuntimeError::Run(RunError::Io("no interrupt".into()))),
            };
        }
        let row = self.inner.thread(thread)?;
        let records = self.inner.records(thread)?;
        let pending: Vec<_> = records
            .iter()
            .filter(|r| r.delivery == Delivery::Pending)
            .collect();
        if row.parked.is_none() && pending.is_empty() {
            return Ok(InterruptOutcome::Idle);
        }
        let last = self.inner.store.latest_run(thread)?;
        let run = last.ok_or_else(|| RuntimeError::NotRunning(thread.clone()))?;
        self.inner.store.tx(|tx| {
            for r in pending {
                let mut q = r.request.clone();
                q.state = cancelled(CancelReason::Interrupted);
                let id = q.id.clone();
                tx.set_input(&id, Some(&q), Delivery::Cancelled)?;
                let outcome = InputOutcome::Cancelled(CancelReason::Interrupted);
                let kind = AgentEventKind::InputResolved { id, outcome };
                tx.run_event(thread, &run, Attribution::turn(q.turn), kind)?;
            }
            if let Some(parked) = &row.parked {
                let terminal = TurnTerminal {
                    turn: parked.turn,
                    status: TurnStatus::Interrupted,
                    thread_disposition: ThreadDisposition::Reusable,
                    stopped_by: Some(StopCause::Interrupt),
                };
                tx.run_event(
                    thread,
                    &run,
                    Attribution::turn(parked.turn),
                    AgentEventKind::TurnTerminal(terminal),
                )?;
            }
            tx.set_status(thread, ThreadStatus::Idle)
        })?;
        Ok(InterruptOutcome::Stopped)
    }

    pub async fn stop(
        &self,
        thread: &ThreadId,
        mode: StopMode,
    ) -> Result<StopReport, RuntimeError> {
        let run = self
            .inner
            .live_run(thread)
            .ok_or_else(|| RuntimeError::NotRunning(thread.clone()))?;
        Ok(run.stop(mode).await?)
    }

    /// The thread's requests, durable, across every run it's had.
    pub fn inputs(&self, thread: &ThreadId) -> Arc<dyn InputQueue> {
        Arc::new(ThreadInputs {
            inner: self.inner.clone(),
            thread: thread.clone(),
        })
    }

    pub fn records(&self, thread: &ThreadId) -> Result<Vec<InputRecord>, RuntimeError> {
        Ok(self.inner.records(thread)?)
    }

    /// The projection of one thread.
    pub fn thread(&self, id: &ThreadId) -> Result<ThreadView, RuntimeError> {
        let row = self.inner.thread(id)?;
        let records = self.inner.records(id)?;
        let select = |d: Delivery| {
            records
                .iter()
                .filter(|r| r.delivery == d)
                .map(|r| r.request.id.clone())
                .collect()
        };
        let (runs, last_seq) = self.inner.store.with(|c| {
            let runs: i64 = c.query_row(
                "SELECT COUNT(*) FROM runs WHERE thread = ?",
                [id.as_str()],
                |r| r.get(0),
            )?;
            let seq: i64 = c.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread = ?",
                [id.as_str()],
                |r| r.get(0),
            )?;
            Ok((runs as u64, GlobalSeq(seq as u64)))
        })?;
        Ok(ThreadView {
            id: id.clone(),
            harness: row.harness,
            status: row.status,
            cwd: row.spec.cwd,
            session: row.session,
            last_turn: TurnId(row.last_turn),
            runs,
            pending: select(Delivery::Pending),
            outcome_unknown: select(Delivery::OutcomeUnknown),
            last_seq,
        })
    }

    /// Every thread.
    pub fn threads(&self) -> Result<Vec<ThreadView>, RuntimeError> {
        let ids = self.inner.store.with(|c| {
            let mut q = c.prepare("SELECT id FROM threads ORDER BY created_ms")?;
            let rows = q
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        ids.into_iter()
            .map(|i| self.thread(&ThreadId::new(i)))
            .collect()
    }

    /// Whether any process is running on `thread`.
    pub fn is_live(&self, thread: &ThreadId) -> bool {
        self.inner.live_run(thread).is_some()
    }
}

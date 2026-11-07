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
//! shown to a person and never re-applied. An answer reaches a tool through a call
//! the agent makes again, and each answer covers one such call, by exact match.
//!
//! ## Parking
//!
//! A thread that parks has its process killed when the CLI asks for a decision, and
//! only then: the request is the CLI's own `can_use_tool`, so its rules have already
//! said yes or no to anything they cover. The request is durable before the kill,
//! and it waits for as long as it takes: nothing here expires. [`Supervisor::pending`]
//! lists every thread's, oldest first, and answering the last one a thread is waiting
//! on starts it again.
//!
//! Killing the process kills whatever else it was doing. Tools it had allowed run
//! one at a time around a request, so none is in flight then, but a subagent or a
//! background task is lost with it, and the agent has to start that again.
//!
//! ## A grace period first
//!
//! With [`Supervisor::set_park_grace`] above zero, a request first waits with the
//! process alive, `live`: an answer in that time goes straight to the process, as on
//! a thread that doesn't park, with no resume and no runtime message. When the grace
//! runs out with the request still undecided, its row (and every other undecided one
//! of that run) is rerouted `parked`, with an `input.rerouted` event, in the same
//! transaction an answer would have to win, and only then is the process killed.
//! So an answer is either live and delivered, or parked and resumed, never both.
//!
//! ## Permission modes
//!
//! A thread's mode ([`crate::mode`]) is in its spec, so every process it starts
//! takes it. [`Supervisor::set_mode`] changes the spec, and the next prompt to a
//! process started in the old mode restarts it (stop, then resume) when no turn is
//! open; a prompt that would queue behind a running turn goes to it as it is. Each
//! change is in the log as `native` `permission_mode/<mode>`, its raw frame saying
//! who and from what.

use super::store::{EventLog, Store, StoreError, Tx};
use super::{GlobalSeq, ReadFilter, StoredEvent, ThreadId};
use crate::mode::{mode_of, PermissionMode};
use futures::future::{BoxFuture, FutureExt};
use futures::stream::{BoxStream, StreamExt};
use plexus_harness::RawFrame;
use plexus_harness::{
    now_ms, resume_or_restart, retry_answer, same_call, Actor, AgentEvent, AgentEventKind,
    Attribution, CancelReason, Environment, EventSeq, Harness, InputAnswer, InputError,
    InputFilter, InputKind, InputOutcome, InputQueue, InputRequest, InputRequestId, InputRoute,
    InputState, InterruptOutcome, ParkAnswer, ParkedCall, ParkedRun, Run, RunEndReason, RunError,
    RunId, RunSpec, SessionId, SessionMode, StopCause, StopMode, StopReport, ThreadDisposition,
    ToolUseId, TurnId, TurnStatus, TurnTerminal, UserInput,
};
use rusqlite::{params, OptionalExtension};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// Most events committed in one transaction.
const BATCH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadView {
    pub id: ThreadId,
    pub harness: String,
    /// Who started it. `None` where nobody verifies callers.
    pub owner: Option<Actor>,
    /// The name its environment goes by. `None` is the default environment.
    pub placement: Option<String>,
    pub status: ThreadStatus,
    pub cwd: Option<String>,
    pub session: Option<SessionId>,
    pub last_turn: TurnId,
    pub runs: u64,
    pub pending: Vec<InputRequestId>,
    pub outcome_unknown: Vec<InputRequestId>,
    /// This thread's newest event: a `since` that picks up from here.
    pub last_seq: GlobalSeq,
    /// The permission mode its next process starts in.
    pub mode: PermissionMode,
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
    NoPlacement(String),
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
            Self::NoPlacement(p) => write!(f, "no placement {p}"),
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
    /// Where a thread with no placement runs.
    pub environment: Arc<dyn Environment>,
    /// Named places a thread can run, each its own environment.
    pub placements: HashMap<String, Arc<dyn Environment>>,
    pub harnesses: Vec<Arc<dyn Harness>>,
}

#[derive(Debug, Clone, Default)]
pub struct ThreadOptions {
    /// Stop the process when the agent asks for a decision, where the harness can,
    /// rather than holding it open until the answer comes.
    pub park: bool,
    pub owner: Option<Actor>,
    /// One of [`SupervisorConfig::placements`]. `None` is the default environment.
    pub placement: Option<String>,
}

/// Whose a thread is and where it runs. Fixed when it's created, and held in memory,
/// because a grant gate asks on every call and doesn't do I/O.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadHome {
    pub owner: Option<Actor>,
    pub placement: Option<String>,
}

struct ThreadRow {
    harness: String,
    spec: RunSpec,
    session: Option<SessionId>,
    status: ThreadStatus,
    last_turn: u64,
    started: bool,
    parked: Option<ParkedRun>,
    /// What the next prompt has to carry: a stopped turn's answers.
    note: Option<String>,
}

struct Inner {
    store: Arc<Store>,
    env: Arc<dyn Environment>,
    placements: HashMap<String, Arc<dyn Environment>>,
    homes: RwLock<HashMap<ThreadId, ThreadHome>>,
    harnesses: HashMap<String, Arc<dyn Harness>>,
    live: Mutex<HashMap<ThreadId, Arc<dyn Run>>>,
    /// One at a time per thread, for whatever starts or feeds a process.
    gates: Mutex<HashMap<ThreadId, Arc<tokio::sync::Mutex<()>>>>,
    /// How long a request waits with its process alive before a parking thread
    /// parks. Zero parks at once.
    park_grace: Mutex<Duration>,
    /// The mode each live process was started in, or was told to switch to.
    live_modes: Mutex<HashMap<ThreadId, PermissionMode>>,
    /// The folders a thread may start in: see [`crate::folders`].
    roots: RwLock<Vec<PathBuf>>,
    /// Whether this host's owner may choose `bypassPermissions`: `--allow-bypass`.
    allow_bypass: AtomicBool,
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
        self.run_event_raw(thread, run, who, kind, None)
    }

    fn run_event_raw(
        &mut self,
        thread: &ThreadId,
        run: &RunId,
        who: Attribution,
        kind: AgentEventKind,
        raw: Option<RawFrame>,
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
            raw,
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

    /// A harness doesn't know who answered, and the request's row does.
    fn stamp(&self, event: &mut AgentEvent) -> Result<(), StoreError> {
        if let AgentEventKind::InputResolved { id, by, .. } = &mut event.kind {
            if let (None, Some((r, ..))) = (&by, self.input(id)?) {
                if let InputState::Answered { by: who, .. } = r.state {
                    *by = who;
                }
            }
        }
        Ok(())
    }

    fn set_status(&self, thread: &ThreadId, status: ThreadStatus) -> Result<(), StoreError> {
        self.sql.execute(
            "UPDATE threads SET status = ? WHERE id = ?",
            params![status.as_str(), thread.as_str()],
        )?;
        Ok(())
    }

    fn set_spec(&self, thread: &ThreadId, spec: &RunSpec) -> Result<(), StoreError> {
        self.sql.execute(
            "UPDATE threads SET spec = ? WHERE id = ?",
            params![serde_json::to_string(spec)?, thread.as_str()],
        )?;
        Ok(())
    }

    /// `native` `permission_mode/<mode>`, on the thread's latest run. A thread that
    /// has had no run says its mode in `session.started` when it gets one.
    fn mode_event(
        &mut self,
        thread: &ThreadId,
        mode: PermissionMode,
        previous: PermissionMode,
        by: Option<&Actor>,
        via: &str,
    ) -> Result<(), StoreError> {
        let run: Option<String> = self
            .sql
            .query_row(
                "SELECT id FROM runs WHERE thread = ? ORDER BY started_ms DESC, rowid DESC LIMIT 1",
                [thread.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(run) = run else {
            return Ok(());
        };
        let raw = serde_json::json!({
            "type": "permission_mode", "mode": mode, "previous": previous, "by": by, "via": via,
        });
        let kind = AgentEventKind::Native {
            name: format!("permission_mode/{mode}"),
        };
        let raw = Some(RawFrame::new(raw.to_string()));
        self.run_event_raw(thread, &RunId::new(run), Attribution::default(), kind, raw)?;
        Ok(())
    }

    fn set_note(&self, thread: &ThreadId, note: Option<&str>) -> Result<(), StoreError> {
        self.sql.execute(
            "UPDATE threads SET note = ? WHERE id = ?",
            params![note, thread.as_str()],
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
                if parked.is_some() {
                    self.set_parked(thread, parked)?;
                }
            }
            AgentEventKind::InputResolved {
                id,
                outcome: InputOutcome::Cancelled(reason),
                ..
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

    fn env_of(&self, thread: &ThreadId) -> Arc<dyn Environment> {
        let homes = self.homes.read().expect("homes poisoned");
        homes
            .get(thread)
            .and_then(|h| h.placement.as_ref())
            .and_then(|p| self.placements.get(p))
            .unwrap_or(&self.env)
            .clone()
    }

    fn gate(&self, thread: &ThreadId) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .lock()
            .expect("gates poisoned")
            .entry(thread.clone())
            .or_default()
            .clone()
    }

    fn park_grace(&self) -> Duration {
        *self.park_grace.lock().expect("park_grace poisoned")
    }

    /// Whether `thread`'s process was started in a mode other than its spec's.
    fn stale_mode(&self, thread: &ThreadId, spec: &RunSpec) -> bool {
        let live = self.live_modes.lock().expect("live_modes poisoned");
        live.get(thread).is_some_and(|m| *m != mode_of(spec))
    }

    /// Reroutes `run`'s undecided live requests to `parked`, each with its event, and
    /// says which. With `only_if`, nothing moves unless that one is still undecided
    /// and live: its grace is what ran out.
    fn reroute_parked(
        &self,
        thread: &ThreadId,
        run: &RunId,
        only_if: Option<&InputRequestId>,
    ) -> Result<Vec<InputRequestId>, StoreError> {
        self.store.tx(|tx| {
            let ids: Vec<String> = {
                let mut q = tx.sql.prepare(
                    "SELECT id FROM inputs WHERE thread = ? AND run = ? AND delivery = ? ORDER BY rowid",
                )?;
                let rows = q
                    .query_map(
                        params![thread.as_str(), run.as_str(), Delivery::Pending.as_str()],
                        |r| r.get(0),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            let mut live = Vec::new();
            for id in ids {
                let id = InputRequestId::new(id);
                if let Some((r, ..)) = tx.input(&id)? {
                    if r.route == InputRoute::Live && ParkedCall::of(&r).is_some() {
                        live.push(r);
                    }
                }
            }
            if only_if.is_some_and(|w| !live.iter().any(|r| r.id == *w)) {
                return Ok(Vec::new());
            }
            let mut moved = Vec::new();
            for mut r in live {
                r.route = InputRoute::Parked;
                tx.set_input(&r.id, Some(&r), Delivery::Pending)?;
                let who = Attribution {
                    turn: Some(r.turn),
                    agent: r.agent.clone(),
                };
                let kind = AgentEventKind::InputRerouted {
                    id: r.id.clone(),
                    route: InputRoute::Parked,
                };
                tx.run_event(thread, run, who, kind)?;
                moved.push(r.id);
            }
            Ok(moved)
        })
    }

    /// `id`'s grace ran out: if it's still undecided on the live `run`, park.
    async fn park_late(
        self: &Arc<Self>,
        thread: &ThreadId,
        run: &Arc<dyn Run>,
        id: &InputRequestId,
    ) -> Result<(), RuntimeError> {
        let run_id = run.info().run;
        let current = self
            .live_run(thread)
            .is_some_and(|r| r.info().run == run_id);
        if !current || run.parked().is_some() {
            return Ok(());
        }
        let moved = self.reroute_parked(thread, &run_id, Some(id))?;
        if !moved.is_empty() {
            run.park().await?;
        }
        Ok(())
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
                "SELECT harness, spec, session, status, last_turn, started, parked, note
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
                        r.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?)
        })?;
        let (harness, spec, session, status, last_turn, started, parked, note) =
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
            note,
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
        let parsed = rows
            .into_iter()
            .map(|(run, request, delivery, tool)| {
                let request: InputRequest = serde_json::from_str(&request)?;
                Ok((run, request, Delivery::parse(&delivery), tool))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        // A retry has a new id, so it's the same call if the input does the same.
        let unknown: Vec<ParkedCall> = parsed
            .iter()
            .filter(|r| r.2 == Delivery::OutcomeUnknown)
            .filter_map(|r| ParkedCall::of(&r.1))
            .collect();
        let same = |request: &InputRequest| {
            ParkedCall::of(request).is_some_and(|c| {
                unknown
                    .iter()
                    .any(|u| u.tool == c.tool && same_call(&c.tool, &u.input, &c.input))
            })
        };
        Ok(parsed
            .iter()
            .map(|(run, request, delivery, tool)| InputRecord {
                thread: thread.clone(),
                run: RunId::new(run),
                request: request.clone(),
                delivery: *delivery,
                tool_use_id: tool.as_deref().map(ToolUseId::new),
                after_unknown_outcome: *delivery != Delivery::OutcomeUnknown && same(request),
            })
            .collect())
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
    fn retry_pool(&self, thread: &ThreadId) -> Result<Vec<Spendable>, StoreError> {
        Ok(self
            .records(thread)?
            .into_iter()
            .filter(|r| r.delivery == Delivery::Delivered)
            .filter_map(|r| {
                let InputState::Answered { answer, by, .. } = &r.request.state else {
                    return None;
                };
                let call = ParkedCall::of(&r.request)?;
                Some(Spendable {
                    request: r.request.id.clone(),
                    answer: ParkAnswer {
                        call,
                        answer: answer.clone(),
                    },
                    by: by.clone(),
                })
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
            Some(pid) => self.env_of(thread).identity(pid).await,
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
        // Under the thread's gate, so this is the spec the process was planned from.
        let mode = mode_of(&self.thread(thread)?.spec);
        self.live_modes
            .lock()
            .expect("live_modes poisoned")
            .insert(thread.clone(), mode);
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
        let asked = batch
            .iter()
            .any(|e| matches!(e.kind, AgentEventKind::InputRequested { .. }));
        let mut requests = HashMap::new();
        let mut retries = Vec::new();
        let mut park_now = false;
        let mut waiting = Vec::new();
        let grace = self.park_grace();
        if let (true, Some(q)) = (asked, run.inputs()) {
            let parks = self.thread(thread)?.spec.park;
            // A request that comes while the process is going parks with it.
            let at_once = grace.is_zero() || run.parked().is_some();
            let mut pool = self.retry_pool(thread)?;
            for e in &batch {
                let AgentEventKind::InputRequested { id } = &e.kind else {
                    continue;
                };
                let Ok(mut r) = q.get(id).await else {
                    continue;
                };
                if let Some(taken) = take_retry(&mut pool, &r) {
                    retries.push((r.clone(), taken));
                } else if parks && ParkedCall::of(&r).is_some() {
                    if at_once {
                        // In the row before anyone can see it, so an answer that
                        // beats the kill waits for the resume.
                        r.route = InputRoute::Parked;
                        park_now = true;
                    } else {
                        // Live for now: see "A grace period first".
                        waiting.push(id.clone());
                    }
                }
                requests.insert(id.clone(), r);
            }
        }
        let mut batch = batch;
        let parked_now = batch
            .iter()
            .any(|e| matches!(e.kind, AgentEventKind::TurnParked { .. }));
        let parked = if parked_now || asked {
            run.parked()
        } else {
            None
        };
        let id = run.info().run;
        self.store.tx(|tx| {
            for e in &mut batch {
                tx.stamp(e)?;
                tx.append(thread, &id, e)?;
                tx.effects(thread, &id, e, &requests, parked.as_ref())?;
            }
            Ok(())
        })?;
        for (r, taken) in retries {
            self.answer_retry(thread, &r, taken)?;
        }
        if park_now {
            run.park().await?;
        }
        for id in waiting {
            let (inner, thread, run) = (self.clone(), thread.clone(), run.clone());
            tokio::spawn(async move {
                tokio::time::sleep(grace).await;
                if let Err(e) = inner.park_late(&thread, &run, &id).await {
                    eprintln!("runtime: thread {thread}: parking after the grace: {e}");
                }
            });
        }
        let ended = batch.iter().find_map(|e| match &e.kind {
            AgentEventKind::RunEnded { reason, .. } => Some(*reason),
            _ => None,
        });
        if let Some(reason) = ended {
            self.live.lock().expect("live poisoned").remove(thread);
            self.live_modes
                .lock()
                .expect("live_modes poisoned")
                .remove(thread);
            if reason == RunEndReason::Parked {
                let inner = self.clone();
                let thread = thread.clone();
                tokio::spawn(async move {
                    if let Err(e) = inner.after_park(&thread, &id).await {
                        eprintln!("runtime: thread {thread}: {e}");
                    }
                });
            }
        }
        Ok(())
    }

    /// Answers a retry from the answer it matched. That answer is spent first, so it
    /// covers one call. If `r` is a parked call's retry, it takes that call's place.
    fn answer_retry(
        self: &Arc<Self>,
        thread: &ThreadId,
        r: &InputRequest,
        taken: Taken,
    ) -> Result<(), RuntimeError> {
        let row = self.thread(thread)?;
        self.store.tx(|tx| {
            tx.set_input(&taken.prior, None, Delivery::Superseded)?;
            if let (Some(mut p), Some(call)) = (row.parked, ParkedCall::of(r)) {
                for c in p.calls.iter_mut().filter(|c| c.request == taken.prior) {
                    *c = call.clone();
                }
                tx.set_parked(thread, Some(&p))?;
            }
            Ok(())
        })?;
        let inner = self.clone();
        let id = r.id.clone();
        tokio::spawn(async move {
            let by = Answering {
                by: taken.by,
                on: None,
            };
            if let Err(e) = inner.answer(&id, taken.answer, by).await {
                eprintln!("runtime: retry of {id}: {e}");
            }
        });
        Ok(())
    }

    /// After a park: an answer may have come while the process was going, and a
    /// request may have come live after the reroute and before the kill.
    async fn after_park(
        self: &Arc<Self>,
        thread: &ThreadId,
        run: &RunId,
    ) -> Result<(), RuntimeError> {
        self.reroute_parked(thread, run, None)?;
        self.try_apply(thread).await
    }

    /// First answer wins, in one transaction with its event. `Live` is `Delivered`
    /// at once, because the next step is the process's stdin.
    fn record_answer(
        &self,
        id: &InputRequestId,
        answer: InputAnswer,
        answering: &Answering,
    ) -> Result<InputRequest, InputError> {
        let done = self.store.tx(|tx| {
            let row = tx.input(id)?;
            let on = answering.on.as_ref();
            // Someone else's request is one this caller can't name.
            let Some((mut r, _, thread, run)) =
                row.filter(|(.., thread, _)| on.is_none_or(|t| t.as_str() == thread))
            else {
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
                by: answering.by.clone(),
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
                    by: answering.by.clone(),
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
        answering: Answering,
    ) -> Result<InputRequest, InputError> {
        let r = self.record_answer(id, answer.clone(), &answering)?;
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
        let park = harness
            .park()
            .ok_or_else(|| RuntimeError::Start("this harness can't park".into()))?;
        let text = |r: &InputRecord| {
            let call = ParkedCall::of(&r.request)?;
            Some(park.message(&call, &answer_of(r)?, false).text)
        };
        let stops = |r: &InputRecord| {
            matches!(
                answer_of(r),
                Some(InputAnswer::Deny {
                    stop_turn: true,
                    ..
                })
            )
        };
        if ready.iter().any(stops) {
            let said: Vec<String> = ready.iter().filter_map(text).collect();
            return self.stop_turn(thread, &row, &ready, &said.join("\n"));
        }
        mark(Delivery::Delivered)?;
        let started = async {
            if let Some(parked) = row
                .parked
                .as_ref()
                .filter(|_| ready.iter().all(|r| r.request.route == InputRoute::Parked))
            {
                let answers: Vec<ParkAnswer> = ready
                    .iter()
                    .filter_map(|r| {
                        Some(ParkAnswer {
                            call: ParkedCall::of(&r.request)?,
                            answer: answer_of(r)?,
                        })
                    })
                    .collect();
                return self.unpark(&harness, thread, parked, &answers).await;
            }
            let said: Vec<String> = ready.iter().filter_map(text).collect();
            self.launch(thread, &row, UserInput::text(said.join("\n")))
                .await
                .map(|_| ())
        }
        .await;
        if started.is_err() {
            mark(Delivery::Answered)?;
        }
        started
    }

    /// A deny that also stops the turn, on a thread with no process: nothing needs
    /// starting to end it. The agent hears why with the next prompt.
    fn stop_turn(
        &self,
        thread: &ThreadId,
        row: &ThreadRow,
        ready: &[InputRecord],
        note: &str,
    ) -> Result<(), RuntimeError> {
        let run = self.store.latest_run(thread)?;
        let run = run.ok_or_else(|| RuntimeError::NotRunning(thread.clone()))?;
        self.store.tx(|tx| {
            for r in ready {
                tx.set_input(&r.request.id, None, Delivery::Superseded)?;
            }
            tx.set_note(thread, Some(note))?;
            if let Some(parked) = &row.parked {
                let terminal = TurnTerminal {
                    turn: parked.turn,
                    status: TurnStatus::Interrupted,
                    thread_disposition: ThreadDisposition::Reusable,
                    stopped_by: Some(StopCause::DenyAndStop),
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
        Ok(())
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
        // The thread's mode and rules now, not the parked process's.
        let mut parked = parked.clone();
        let row = self.thread(thread)?;
        parked.spec.env = row.spec.env;
        parked.spec.tools = row.spec.tools;
        let u = park.plan_unpark(&parked, answers).map_err(|e| fail(&e))?;
        let env = self.env_of(thread);
        let process = env.spawn(u.plan).await.map_err(|e| fail(&e))?;
        let run = harness.drive(u.spec, process).await.map_err(|e| fail(&e))?;
        self.register(thread, run.clone()).await?;
        run.send(u.input).await?;
        Ok(())
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
            let env = self.env_of(thread);
            let process = env.spawn(plan).await.map_err(|e| fail(&e))?;
            let run = harness.drive(spec, process).await.map_err(|e| fail(&e))?;
            self.register(thread, run.clone()).await?;
            return Ok(run.send(prompt).await?);
        }
        let session = row
            .session
            .clone()
            .ok_or_else(|| RuntimeError::Start("no session to resume".into()))?;
        let env = self.env_of(thread);
        let resumed = resume_or_restart(harness, env.as_ref(), &spec, &session, prompt)
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
                if self.env_of(&thread).reap(pid, &identity).await {
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
                            by: None,
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
    /// Waits for the pump to take `run` out of the live map, after its process
    /// ended: until then a new process for the thread would be taken out with it.
    async fn wait_gone(&self, thread: &ThreadId, run: &RunId) -> Result<(), RuntimeError> {
        let gone = tokio::time::timeout(Duration::from_secs(30), async {
            while self.live_run(thread).is_some_and(|r| r.info().run == *run) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        gone.map_err(|_| RuntimeError::Start("the old process didn't end".into()))
    }

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

/// An answer delivered and not yet used, and who gave it.
struct Spendable {
    request: InputRequestId,
    answer: ParkAnswer,
    by: Option<Actor>,
}

/// An answer a retry took over. It's still the answer of whoever gave it.
struct Taken {
    prior: InputRequestId,
    answer: InputAnswer,
    by: Option<Actor>,
}

/// Who an answer is from, and the thread it was addressed to, if one was named.
#[derive(Default)]
struct Answering {
    by: Option<Actor>,
    on: Option<ThreadId>,
}

/// The answer in `pool` that `r` retries, by exact match, taken out of the pool.
fn take_retry(pool: &mut Vec<Spendable>, r: &InputRequest) -> Option<Taken> {
    let call = ParkedCall::of(r)?;
    let at = pool.iter().position(|s| {
        retry_answer(std::slice::from_ref(&s.answer), &call.tool, &call.input).is_some()
    })?;
    let s = pool.remove(at);
    let answer = retry_answer(&[s.answer], &call.tool, &call.input)?;
    Some(Taken {
        prior: s.request,
        answer,
        by: s.by,
    })
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
        async move { inner.answer(&id, answer, Answering::default()).await }.boxed()
    }
}

impl Supervisor {
    /// Opens the store and closes out whatever a previous service left running.
    pub async fn open(config: SupervisorConfig) -> Result<(Self, Recovery), RuntimeError> {
        let store = Store::open(&config.db)?;
        let homes = store.with(|c| {
            let mut q = c.prepare("SELECT id, owner, placement FROM threads")?;
            let rows = q
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let homes = homes
            .into_iter()
            .map(|(id, owner, placement)| {
                let owner = owner
                    .map(|o| serde_json::from_str(&o))
                    .transpose()
                    .map_err(store_err)?;
                Ok((ThreadId::new(id), ThreadHome { owner, placement }))
            })
            .collect::<Result<HashMap<_, _>, StoreError>>()?;
        let inner = Arc::new(Inner {
            store,
            env: config.environment,
            placements: config.placements,
            homes: RwLock::new(homes),
            harnesses: config
                .harnesses
                .into_iter()
                .map(|h| (h.name().to_string(), h))
                .collect(),
            live: Mutex::default(),
            gates: Mutex::default(),
            park_grace: Mutex::new(Duration::ZERO),
            live_modes: Mutex::default(),
            roots: RwLock::new(crate::folders::default_roots()),
            allow_bypass: AtomicBool::new(false),
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
        if let Some(p) = options.placement.as_ref() {
            if !self.inner.placements.contains_key(p) {
                return Err(RuntimeError::NoPlacement(p.clone()));
            }
        }
        let id = ThreadId::new(uuid::Uuid::new_v4().to_string());
        let mut spec = spec;
        // Known before any frame, so a stop before init can still be resumed.
        if spec.session == SessionMode::Fresh {
            spec.session = SessionMode::Assign {
                id: SessionId::new(uuid::Uuid::new_v4().to_string()),
            };
        }
        if options.park && h.park().is_none() {
            return Err(RuntimeError::Start(format!("{harness} can't park")));
        }
        spec.park = options.park;
        let session = match &spec.session {
            SessionMode::Assign { id } | SessionMode::Resume { id } => Some(id.to_string()),
            SessionMode::Fresh => None,
        };
        let text = serde_json::to_string(&spec).map_err(store_err)?;
        let owner = options.owner.as_ref();
        let owner = owner
            .map(serde_json::to_string)
            .transpose()
            .map_err(store_err)?;
        self.inner.store.tx(|tx| {
            tx.sql.execute(
                "INSERT INTO threads
                 (id, harness, spec, cwd, session, status, created_ms, owner, placement)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    id.as_str(),
                    harness,
                    text,
                    spec.cwd,
                    session,
                    ThreadStatus::Idle.as_str(),
                    now_ms() as i64,
                    owner,
                    options.placement
                ],
            )?;
            Ok(())
        })?;
        self.inner.homes.write().expect("homes poisoned").insert(
            id.clone(),
            ThreadHome {
                owner: options.owner,
                placement: options.placement,
            },
        );
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
            let idle = self.inner.open_turns(&run.info().run)?.is_empty();
            if !(idle && self.inner.stale_mode(thread, &row.spec)) {
                return Ok(run.send(input).await?);
            }
            // Started in another mode, and between turns: start again in this one.
            run.stop(StopMode::Graceful).await?;
            self.inner.wait_gone(thread, &run.info().run).await?;
        }
        let row = self.inner.thread(thread)?;
        // A prompt supersedes a parked call whose outcome is unknown: the agent
        // drops it (AGENTS-33 `h_prompt_undecided`).
        let input = match &row.note {
            Some(note) => UserInput {
                text: format!("{note}\n\n{}", input.text),
                by: input.by,
            },
            None => input,
        };
        self.inner.store.tx(|tx| tx.set_parked(thread, None))?;
        let turn = self.inner.launch(thread, &row, input).await?;
        self.inner.store.tx(|tx| tx.set_note(thread, None))?;
        Ok(turn)
    }

    /// Resumes a parked thread with no answer, after an unknown outcome. The agent is
    /// asked to make the call again, and it comes back as a new pending request with
    /// `after_unknown_outcome` set, so a person decides with that in view. Nothing is
    /// re-applied.
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
                let kind = AgentEventKind::InputResolved {
                    id,
                    outcome,
                    by: None,
                };
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

    /// Every undecided request on every thread, oldest first. Durable, so a restart
    /// changes nothing, and nothing here expires. Each comes from the request's row,
    /// so what's listed is what can be answered.
    pub fn pending(&self) -> Result<Vec<InputRecord>, RuntimeError> {
        let rows = self.inner.store.with(|c| {
            let mut q =
                c.prepare("SELECT id, thread FROM inputs WHERE delivery = ? ORDER BY rowid")?;
            let rows = q
                .query_map([Delivery::Pending.as_str()], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut held: HashMap<String, Vec<InputRecord>> = HashMap::new();
        let mut out = Vec::with_capacity(rows.len());
        for (id, thread) in rows {
            if !held.contains_key(&thread) {
                let records = self.inner.records(&ThreadId::new(&thread))?;
                held.insert(thread.clone(), records);
            }
            let found = held[&thread].iter().find(|r| r.request.id.as_str() == id);
            out.extend(found.cloned());
        }
        Ok(out)
    }

    /// Answers a request, whichever thread it's on. Once a parked thread has every
    /// answer it's waiting on, this starts it.
    pub async fn answer(
        &self,
        id: &InputRequestId,
        answer: InputAnswer,
    ) -> Result<InputRequest, InputError> {
        self.inner.answer(id, answer, Answering::default()).await
    }

    /// Answers a request on `thread`, as `actor`. A request on any other thread is
    /// [`InputError::NotFound`], so being allowed one thread reaches no other.
    pub async fn answer_as(
        &self,
        thread: &ThreadId,
        id: &InputRequestId,
        answer: InputAnswer,
        actor: Actor,
    ) -> Result<InputRequest, InputError> {
        let answering = Answering {
            by: Some(actor),
            on: Some(thread.clone()),
        };
        self.inner.answer(id, answer, answering).await
    }

    /// Whose `id` is and where it runs. `None` for a thread that doesn't exist.
    pub fn home(&self, id: &ThreadId) -> Option<ThreadHome> {
        let homes = self.inner.homes.read().expect("homes poisoned");
        homes.get(id).cloned()
    }

    /// Whether `name` is a place a thread can run.
    pub fn has_placement(&self, name: &str) -> bool {
        self.inner.placements.contains_key(name)
    }

    /// The places a thread can run, in order.
    pub fn placements(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.placements.keys().cloned().collect();
        names.sort();
        names
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
        let home = self.home(id).unwrap_or_default();
        let mode = mode_of(&row.spec);
        Ok(ThreadView {
            id: id.clone(),
            harness: row.harness,
            owner: home.owner,
            placement: home.placement,
            status: row.status,
            cwd: row.spec.cwd,
            session: row.session,
            last_turn: TurnId(row.last_turn),
            runs,
            pending: select(Delivery::Pending),
            outcome_unknown: select(Delivery::OutcomeUnknown),
            last_seq,
            mode,
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

    /// How long a request on a parking thread waits with its process alive before
    /// the thread parks. Zero, the default, parks at once.
    pub fn set_park_grace(&self, grace: Duration) {
        *self.inner.park_grace.lock().expect("park_grace poisoned") = grace;
    }

    /// Whether a host's owner may put threads in `bypassPermissions` here
    /// (`plexus-agents host --allow-bypass`). Off unless set.
    pub fn set_allow_bypass(&self, allow: bool) {
        self.inner.allow_bypass.store(allow, AtomicOrdering::SeqCst);
    }

    /// See [`Supervisor::set_allow_bypass`].
    pub fn allows_bypass(&self) -> bool {
        self.inner.allow_bypass.load(AtomicOrdering::SeqCst)
    }

    /// The folders threads may start in, each resolved; ones that don't resolve to
    /// a directory are dropped. Empty is the user's home.
    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        let roots = if roots.is_empty() {
            crate::folders::default_roots()
        } else {
            roots
        };
        *self.inner.roots.write().expect("roots poisoned") = roots;
    }

    /// The roots, resolved.
    pub fn roots(&self) -> Vec<PathBuf> {
        let roots = self.inner.roots.read().expect("roots poisoned").clone();
        crate::folders::resolve_roots(&roots)
    }

    /// The working directories of the newest threads, newest first, each once.
    pub fn recent_cwds(&self, limit: usize) -> Result<Vec<String>, RuntimeError> {
        let rows = self.inner.store.with(|c| {
            let mut q = c.prepare(
                "SELECT cwd FROM threads WHERE cwd IS NOT NULL
                 GROUP BY cwd ORDER BY MAX(created_ms) DESC LIMIT ?",
            )?;
            let rows = q
                .query_map([limit as i64], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }

    /// See [`crate::folders::list`]: the roots, their projects, and recent folders.
    pub fn folders(
        &self,
        under: Option<&std::path::Path>,
    ) -> Result<Vec<crate::folders::Folder>, String> {
        let recent = self
            .recent_cwds(crate::folders::MAX_RECENT * 2)
            .map_err(|e| e.to_string())?;
        crate::folders::list(&self.roots(), &recent, under)
    }

    /// Sets `thread`'s permission mode for its next process: see the module docs.
    /// Whoever calls this has decided `by` may choose `mode`, `bypassPermissions`
    /// included ([`crate::mode::may_bypass`]).
    pub async fn set_mode(
        &self,
        thread: &ThreadId,
        mode: PermissionMode,
        by: Option<Actor>,
    ) -> Result<ThreadView, RuntimeError> {
        let gate = self.inner.gate(thread);
        let _held = gate.lock().await;
        let row = self.inner.thread(thread)?;
        let previous = mode_of(&row.spec);
        if previous != mode {
            let mut spec = row.spec;
            crate::mode::set_mode(&mut spec, mode);
            self.inner.store.tx(|tx| {
                tx.set_spec(thread, &spec)?;
                tx.mode_event(thread, mode, previous, by.as_ref(), "set_mode")
            })?;
            log_mode(thread, mode, previous, by.as_ref());
        }
        drop(_held);
        self.thread(thread)
    }

    /// [`Supervisor::answer_as`], allowing and applying the request's own rule
    /// suggestions at `apply` ([`crate::permissions`]). The rules join the thread's,
    /// so the processes after this one have them too; a live process gets them with
    /// the answer. `may_bypass` is whether this caller is one who may put the thread
    /// in `bypassPermissions` ([`crate::mode::may_bypass`]), which a suggestion could
    /// ask for; it also takes this host's [`Supervisor::set_allow_bypass`].
    pub async fn answer_applying(
        &self,
        thread: &ThreadId,
        id: &InputRequestId,
        answer: InputAnswer,
        actor: Actor,
        apply: &[u32],
        may_bypass: bool,
    ) -> Result<InputRequest, InputError> {
        if apply.is_empty() {
            return self.answer_as(thread, id, answer, actor).await;
        }
        let mismatch = |detail: String| InputError::AnswerMismatch {
            id: id.clone(),
            detail,
        };
        if !matches!(answer, InputAnswer::Allow { .. }) {
            return Err(mismatch("only an allow applies suggestions".into()));
        }
        let delivery = |e: &dyn fmt::Display| InputError::Delivery {
            id: id.clone(),
            detail: e.to_string(),
        };
        let records = self.inner.records(thread).map_err(|e| delivery(&e))?;
        let record = records
            .into_iter()
            .find(|r| r.request.id == *id)
            .ok_or_else(|| InputError::NotFound(id.clone()))?;
        if !record.request.state.is_pending() {
            return Err(InputError::AlreadyResolved {
                id: id.clone(),
                state: Box::new(record.request.state),
            });
        }
        let updates = crate::permissions::pick(&record.request, apply).map_err(mismatch)?;
        let rules = crate::permissions::thread_rules(&updates);
        if rules.mode.is_some_and(PermissionMode::is_bypass) {
            if !may_bypass {
                return Err(mismatch(crate::mode::bypass_refused()));
            }
            if !self.allows_bypass() {
                return Err(mismatch(crate::mode::bypass_off()));
            }
        }
        let row = self.inner.thread(thread).map_err(|e| delivery(&e))?;
        let previous = mode_of(&row.spec);
        let mut spec = row.spec;
        for r in rules.allow {
            if !spec.tools.preapproved.contains(&r) {
                spec.tools.preapproved.push(r);
            }
        }
        for r in rules.deny {
            if !spec.tools.denied.contains(&r) {
                spec.tools.denied.push(r);
            }
        }
        let mode = rules.mode.unwrap_or(previous);
        crate::mode::set_mode(&mut spec, mode);
        self.inner
            .store
            .tx(|tx| {
                tx.set_spec(thread, &spec)?;
                if mode != previous {
                    tx.mode_event(thread, mode, previous, Some(&actor), "suggestion")?;
                }
                Ok(())
            })
            .map_err(|e| delivery(&e))?;
        if mode != previous {
            log_mode(thread, mode, previous, Some(&actor));
        }
        let live = record.request.route == InputRoute::Live;
        if live {
            crate::permissions::stash(id, updates);
            if rules.mode.is_some() {
                // The process switches with the answer, so it needn't start again.
                let mut g = self.inner.live_modes.lock().expect("live_modes poisoned");
                if let Some(m) = g.get_mut(thread) {
                    *m = mode;
                }
            }
        }
        let answered = self.answer_as(thread, id, answer, actor).await;
        if live {
            // Delivered or not, it's no one's now.
            crate::permissions::take(id);
        }
        answered
    }
}

/// Every mode change goes to the service's log; a bypass says so loudly.
fn log_mode(thread: &ThreadId, mode: PermissionMode, previous: PermissionMode, by: Option<&Actor>) {
    let who = by.map_or("nobody verified", |a| a.id.as_str());
    if mode.is_bypass() {
        eprintln!("runtime: thread {thread}: permission mode {previous} -> {mode} by {who}: PERMISSION CHECKS BYPASSED");
    } else {
        eprintln!("runtime: thread {thread}: permission mode {previous} -> {mode} by {who}");
    }
}

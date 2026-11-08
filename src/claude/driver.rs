//! Owns one `claude` process.
//!
//! A stop is classified by what this driver did, never by the exit code or the
//! last frame: a clean interrupt then close exits 1, SIGINT exits 0, TERM mid-tool
//! exits 143 with no `result`, and interrupt's `result` has no reason of its own.

use super::{frames, ClaudeOptions, SUPPORTED_CLI_VERSIONS};
use futures::future::{BoxFuture, FutureExt};
use futures::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use futures::lock::Mutex as AsyncMutex;
use futures::stream::{BoxStream, StreamExt};
use plexus_harness::{
    now_ms, AgentEvent, AgentEventKind, Attribution, BoxAsyncRead, BoxAsyncWrite, CancelReason,
    CompletedCall, DriveError, EnvName, EnvValue, EventLog, EventSeq, ExitInfo, FailureKind,
    InputAnswer, InputFilter, InputKind, InputOutcome, InputQueue, InputRequest, InputRequestId,
    InputResponder, InputRoute, Interrupt, InterruptOutcome, McpTransport, MemoryInputQueue,
    ModelName, ParkedCall, ParkedRun, Process, ProcessSignal, RawFrame, ResolvedSecret, Run,
    RunEndReason, RunError, RunId, RunInfo, RunSpec, SecretResolver, SessionId, SessionMode,
    Signal, StopCause, StopMode, StopReport, SubagentId, TaskId, ThreadDisposition, ToolName,
    ToolUseId, TurnFailure, TurnId, TurnOrigin, TurnStatus, TurnTerminal, UserInput,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, watch};

const MCP_ACK: Duration = Duration::from_secs(30);
const FORCE_WAIT: Duration = Duration::from_secs(10);
/// Most stderr kept for reading a failure out of it.
const STDERR_TAIL: usize = 8192;

struct OpenTurn {
    id: TurnId,
    stop: Option<StopCause>,
    origin: TurnOrigin,
    /// The CLI's `system/init` for this turn has come. Each turn gets one.
    began: bool,
}

/// A task the CLI announced with `task_started`.
struct Task {
    /// `task_type: local_agent`. Everything else stays a task.
    subagent: bool,
    /// The turn that started it. Its events stay there, even after that turn ends.
    turn: Option<TurnId>,
    /// The subagent that owns it, for a task that isn't one.
    owner: Option<String>,
}

/// One line of stdout: the parsed frame, and the bytes as they came.
struct Frame {
    v: Value,
    raw: RawFrame,
}

#[derive(Default)]
struct State {
    next_turn: u64,
    /// In order: the CLI queues prompts sent during a turn. The front is running.
    turns: VecDeque<OpenTurn>,
    tasks: HashMap<String, Task>,
    /// Tasks that have started and not ended.
    live_tasks: HashSet<String>,
    /// Which subagent an Agent `tool_use` started.
    agent_of_tool: HashMap<String, String>,
    /// Which turn a `tool_use` was made in.
    turn_of_tool: HashMap<String, TurnId>,
    /// The message each call came in, while it has no result.
    open_calls: HashMap<String, (String, CompletedCall)>,
    /// Calls that have a result, and their message.
    finished: Vec<(String, CompletedCall)>,
    parked: Option<ParkedRun>,
    /// A turn carried on from a park, waiting for the input that restarts it.
    feed: Option<TurnId>,
    session: Option<SessionId>,
    session_reported: bool,
    /// The run itself is being stopped.
    stopping: Option<StopCause>,
    refused: bool,
    input_closed: bool,
    exit: Option<ExitInfo>,
    native_of: HashMap<InputRequestId, String>,
    by_native: HashMap<String, InputRequestId>,
    awaiting: HashMap<String, oneshot::Sender<Value>>,
}

struct Shared {
    run: RunId,
    pid: Option<u32>,
    options: ClaudeOptions,
    log: EventLog,
    state: Mutex<State>,
    writer: AsyncMutex<Option<BoxAsyncWrite>>,
    signal: Arc<dyn ProcessSignal>,
    stderr: Mutex<String>,
    ended: watch::Sender<bool>,
    /// How many tasks are live, for an interrupt with no turn to wait on.
    live: watch::Sender<usize>,
    spec: RunSpec,
    next_control: AtomicU64,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("driver state poisoned")
    }

    async fn write(&self, frame: &Value) -> Result<(), RunError> {
        let mut g = self.writer.lock().await;
        let w = g.as_mut().ok_or(RunError::InputClosed)?;
        let line = format!("{frame}\n");
        let io = |e: std::io::Error| match e.kind() {
            std::io::ErrorKind::BrokenPipe => RunError::Ended,
            _ => RunError::Io(e.to_string()),
        };
        w.write_all(line.as_bytes()).await.map_err(io)?;
        w.flush().await.map_err(io)
    }

    fn control_id(&self, what: &str) -> String {
        format!(
            "{what}-{}",
            self.next_control.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// Emits an event of the root agent, in the turn now running.
    fn emit(&self, kind: AgentEventKind, raw: Option<RawFrame>) {
        let turn = self.state().turns.front().map(|t| t.id);
        let who = Attribution { turn, agent: None };
        self.log.append_as(who, kind, raw);
    }

    fn emit_as(&self, who: Attribution, kind: AgentEventKind, raw: Option<RawFrame>) {
        self.log.append_as(who, kind, raw);
    }

    /// Whose `agent` is: its own turn, or the one running if it's unknown.
    fn who_of(&self, agent: Option<&str>) -> Attribution {
        let g = self.state();
        let turn = agent
            .and_then(|a| g.tasks.get(a))
            .and_then(|t| t.turn)
            .or_else(|| g.turns.front().map(|t| t.id));
        Attribution {
            turn,
            agent: agent.map(SubagentId::new),
        }
    }

    /// Who a message frame belongs to. `agent_id` names a subagent directly, and
    /// `parent_tool_use_id` is the Agent call that started it.
    fn who(&self, f: &Value) -> Attribution {
        let agent = f["agent_id"].as_str().map(str::to_string).or_else(|| {
            let tool = f["parent_tool_use_id"].as_str()?;
            self.state().agent_of_tool.get(tool).cloned()
        });
        self.who_of(agent.as_deref())
    }

    fn terminal(
        &self,
        turn: TurnId,
        status: TurnStatus,
        thread_disposition: ThreadDisposition,
        stopped_by: Option<StopCause>,
        raw: Option<RawFrame>,
    ) {
        let t = TurnTerminal {
            turn,
            status,
            thread_disposition,
            stopped_by,
        };
        self.emit_as(
            Attribution::turn(turn),
            AgentEventKind::TurnTerminal(t),
            raw,
        );
    }

    /// Opens a turn nobody asked for, and says so, so a terminal is never without
    /// a start.
    fn open_unasked(&self, origin: TurnOrigin) -> OpenTurn {
        let turn = {
            let mut g = self.state();
            g.next_turn += 1;
            OpenTurn {
                id: TurnId(g.next_turn),
                stop: g.stopping,
                origin,
                began: true,
            }
        };
        let (id, who) = (turn.id, Attribution::turn(turn.id));
        self.emit_as(who, AgentEventKind::TurnStarted { turn: id, origin }, None);
        turn
    }

    /// The turn a `result` ends. A `result` with an `origin` ends only a turn the
    /// agent started itself, and one without ends only a turn that was asked for,
    /// so a background turn can't end a prompt's. With no such turn open, the frame
    /// is a resume failing before reading stdin, or a self-started turn whose init
    /// was taken for a queued prompt's.
    fn take_turn(&self, by_itself: bool) -> OpenTurn {
        let found = {
            let mut g = self.state();
            let at = g
                .turns
                .iter()
                .position(|t| (t.origin == TurnOrigin::TaskNotification) == by_itself);
            match at {
                Some(i) => g.turns.remove(i),
                None => {
                    if by_itself {
                        if let Some(t) = g.turns.front_mut() {
                            t.began = false;
                        }
                    }
                    None
                }
            }
        };
        found.unwrap_or_else(|| {
            self.open_unasked(if by_itself {
                TurnOrigin::TaskNotification
            } else {
                TurnOrigin::Resume
            })
        })
    }

    fn cancel_pending(&self, queue: &MemoryInputQueue, reason: CancelReason) {
        for r in queue.cancel_all(reason) {
            let outcome = InputOutcome::Cancelled(reason);
            let who = Attribution {
                turn: Some(r.turn),
                agent: r.agent,
            };
            self.emit_as(
                who,
                AgentEventKind::InputResolved {
                    id: r.id,
                    outcome,
                    by: None,
                },
                None,
            );
        }
    }

    async fn wait_ended(&self, timeout: Duration) -> bool {
        let mut rx = self.ended.subscribe();
        let done = tokio::time::timeout(timeout, rx.wait_for(|e| *e))
            .await
            .is_ok();
        done
    }

    async fn close_stdin(&self) -> Result<(), RunError> {
        self.state().input_closed = true;
        if let Some(mut w) = self.writer.lock().await.take() {
            // The peer may be gone already; closing is still done.
            let _ = w.close().await;
        }
        Ok(())
    }

    async fn force(&self, cause: StopCause) -> Result<(), RunError> {
        {
            let mut g = self.state();
            g.stopping.get_or_insert(cause);
            for t in g.turns.iter_mut() {
                t.stop.get_or_insert(cause);
            }
        }
        self.signal
            .signal(Signal::KillTree)
            .await
            .map_err(|e| RunError::Io(e.to_string()))?;
        self.wait_ended(FORCE_WAIT).await;
        Ok(())
    }

    fn report(&self, cause: StopCause, forced: bool) -> StopReport {
        StopReport {
            cause,
            exit: self.state().exit,
            forced,
        }
    }

    /// Fails the open turn and kills the run: this CLI can't be driven safely.
    async fn refuse(&self, failure: FailureKind, detail: String, raw: RawFrame) {
        let turn = self.take_turn(false);
        self.state().refused = true;
        let status = TurnStatus::Failed(TurnFailure { failure, detail });
        self.terminal(turn.id, status, ThreadDisposition::Broken, None, Some(raw));
        let _ = self.signal.signal(Signal::KillTree).await;
    }
}

/// Carries an answer to the CLI.
struct Responder(Arc<Shared>);

impl InputResponder for Responder {
    fn deliver(
        &self,
        request: InputRequest,
        answer: InputAnswer,
    ) -> BoxFuture<'static, Result<(), String>> {
        let shared = self.0.clone();
        async move {
            // A parked run has nothing to write to. The answer stays on the
            // request, for `Park::plan_unpark`.
            if shared.state().parked.is_some() {
                return Ok(());
            }
            let native = shared.state().native_of.get(&request.id).cloned();
            let native = native.ok_or("the request has no native id")?;
            if matches!(
                answer,
                InputAnswer::Deny {
                    stop_turn: true,
                    ..
                }
            ) {
                if let Some(t) = shared.state().turns.front_mut() {
                    t.stop.get_or_insert(StopCause::DenyAndStop);
                }
            }
            let updates = crate::permissions::take(&request.id);
            let frame =
                frames::control_response(&native, &request.raw["request"], &answer, updates);
            shared.write(&frame).await.map_err(|e| e.to_string())?;
            let outcome = answer.outcome();
            let who = Attribution {
                turn: Some(request.turn),
                agent: request.agent,
            };
            shared.emit_as(
                who,
                AgentEventKind::InputResolved {
                    id: request.id,
                    outcome,
                    by: None,
                },
                None,
            );
            Ok(())
        }
        .boxed()
    }
}

async fn drain_stderr(shared: Arc<Shared>, mut stderr: BoxAsyncRead) {
    let mut buf = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let mut g = shared.stderr.lock().expect("stderr poisoned");
        g.push_str(&String::from_utf8_lossy(&buf[..n]));
        if g.len() > STDERR_TAIL {
            let cut = g.len() - STDERR_TAIL;
            let cut = (cut..g.len()).find(|i| g.is_char_boundary(*i)).unwrap_or(0);
            g.drain(..cut);
        }
    }
}

fn on_init(shared: &Shared, frame: Frame) -> Option<(FailureKind, String, RawFrame)> {
    let Frame { v: f, raw } = frame;
    let version = f["claude_code_version"].as_str().map(str::to_string);
    if let Some(v) = &version {
        if !SUPPORTED_CLI_VERSIONS.contains(&v.as_str()) {
            let detail =
                format!("claude {v} is not supported; supported: {SUPPORTED_CLI_VERSIONS:?}");
            return Some((FailureKind::UnsupportedVersion, detail, raw));
        }
    }
    let mode = f["permissionMode"].as_str().map(str::to_string);
    // Only a thread put in bypass may run in it: the CLI must not get there alone.
    let asked = crate::mode::mode_of(&shared.spec);
    if mode.as_deref() == Some("bypassPermissions") && !asked.is_bypass() {
        let detail = "the CLI reports bypassed permissions; refusing to drive it".to_string();
        return Some((FailureKind::Protocol, detail, raw));
    }
    let id = SessionId::new(f["session_id"].as_str().unwrap_or(""));
    let (first, by_itself) = {
        let mut g = shared.state();
        g.session = Some(id.clone());
        let first = !std::mem::replace(&mut g.session_reported, true);
        if let Some(t) = g.turns.iter_mut().find(|t| !t.began) {
            t.began = true;
        }
        (first, g.turns.is_empty() && !first)
    };
    if by_itself {
        // An init nothing asked for: a background task woke the agent up.
        let turn = shared.open_unasked(TurnOrigin::TaskNotification);
        shared.state().turns.push_front(turn);
    }
    if first {
        let kind = AgentEventKind::SessionStarted {
            session: id,
            model: f["model"].as_str().map(ModelName::new),
            harness_version: version,
            permission_mode: mode,
        };
        shared.emit(kind, Some(raw));
    } else {
        shared.emit(
            AgentEventKind::Native {
                name: "system/init".into(),
            },
            Some(raw),
        );
    }
    None
}

fn on_stream_event(shared: &Shared, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let delta = &f["event"]["delta"];
    let text = |k: &str| delta[k].as_str().unwrap_or("").to_string();
    let kind = match delta["type"].as_str() {
        Some("text_delta") => AgentEventKind::TextDelta { text: text("text") },
        Some("thinking_delta") => AgentEventKind::ThinkingDelta {
            text: text("thinking"),
        },
        _ => {
            let name = format!(
                "stream_event/{}",
                f["event"]["type"].as_str().unwrap_or("?")
            );
            AgentEventKind::Native { name }
        }
    };
    shared.emit_as(shared.who(&f), kind, Some(raw));
}

fn on_assistant(shared: &Shared, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let who = shared.who(&f);
    let blocks = f["message"]["content"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut said = false;
    for b in blocks {
        said = true;
        let kind = match b["type"].as_str() {
            Some("text") => AgentEventKind::Message {
                text: b["text"].as_str().unwrap_or("").to_string(),
            },
            Some("tool_use") => {
                let id = ToolUseId::new(b["id"].as_str().unwrap_or(""));
                let name = ToolName::new(b["name"].as_str().unwrap_or(""));
                let mut g = shared.state();
                if let Some(turn) = who.turn {
                    g.turn_of_tool.insert(id.to_string(), turn);
                }
                let message = f["message"]["id"].as_str().unwrap_or("").to_string();
                let call = CompletedCall {
                    tool: name.clone(),
                    input: b["input"].clone(),
                    is_error: false,
                    output: String::new(),
                };
                g.open_calls.insert(id.to_string(), (message, call));
                drop(g);
                AgentEventKind::ToolCall {
                    id,
                    name,
                    input: b["input"].clone(),
                }
            }
            _ => AgentEventKind::Native {
                name: format!("assistant/{}", b["type"].as_str().unwrap_or("?")),
            },
        };
        shared.emit_as(who.clone(), kind, Some(raw.clone()));
    }
    if !said {
        let kind = AgentEventKind::Native {
            name: "assistant".into(),
        };
        shared.emit_as(who, kind, Some(raw));
    }
}

fn on_user(shared: &Shared, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let who = shared.who(&f);
    let results = frames::tool_results(&f);
    if results.is_empty() {
        let kind = AgentEventKind::Native {
            name: "user".into(),
        };
        shared.emit_as(who, kind, Some(raw));
        return;
    }
    for (id, is_error, output) in results {
        {
            let mut g = shared.state();
            if let Some((message, mut call)) = g.open_calls.remove(&id) {
                call.is_error = is_error;
                call.output = match &output {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                g.finished.push((message, call));
            }
        }
        let kind = AgentEventKind::ToolResult {
            id: ToolUseId::new(id),
            is_error,
            output,
        };
        shared.emit_as(who.clone(), kind, Some(raw.clone()));
    }
}

/// `system/task_started`. Only `task_type: local_agent` is a subagent, and an id
/// seen before is that subagent starting again.
fn on_task_started(shared: &Shared, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let id = f["task_id"].as_str().unwrap_or("").to_string();
    let subagent = f["task_type"] == "local_agent";
    let tool_use = f["tool_use_id"].as_str();
    let parent = f["parent_task_id"].as_str().map(str::to_string);
    let (resumed, who) = {
        let mut g = shared.state();
        let turn = tool_use
            .and_then(|t| g.turn_of_tool.get(t).copied())
            .or_else(|| parent.as_ref().and_then(|p| g.tasks.get(p)?.turn))
            .or_else(|| g.turns.front().map(|t| t.id));
        let resumed = g
            .tasks
            .insert(
                id.clone(),
                Task {
                    subagent,
                    turn,
                    owner: parent.clone(),
                },
            )
            .is_some();
        if let (true, Some(tool)) = (subagent, tool_use) {
            g.agent_of_tool.insert(tool.to_string(), id.clone());
        }
        g.live_tasks.insert(id.clone());
        let count = g.live_tasks.len();
        drop(g);
        shared.live.send_replace(count);
        let owner = if subagent { Some(&id) } else { parent.as_ref() };
        let agent = owner.cloned().map(SubagentId::new);
        (resumed, Attribution { turn, agent })
    };
    let text = |k: &str| f[k].as_str().unwrap_or("").to_string();
    let tool_use = tool_use.map(ToolUseId::new);
    let kind = if subagent {
        AgentEventKind::SubagentStarted {
            id: SubagentId::new(&id),
            parent: parent.map(SubagentId::new),
            tool_use,
            agent_type: f["subagent_type"].as_str().map(str::to_string),
            description: text("description"),
            prompt: f["prompt"].as_str().map(str::to_string),
            background: f["is_backgrounded"].as_bool().unwrap_or(false),
            depth: f["spawn_depth"].as_u64().unwrap_or(1) as u32,
            resumed,
        }
    } else {
        AgentEventKind::BackgroundTaskStarted {
            id: TaskId::new(&id),
            owner: parent.map(SubagentId::new),
            tool_use,
            task_type: text("task_type"),
            description: text("description"),
        }
    };
    shared.emit_as(who, kind, Some(raw));
}

/// Whose a task frame is: the subagent itself, or the owner of a task that isn't one.
fn who_of_task(shared: &Shared, f: &Value) -> (Attribution, bool) {
    let id = f["task_id"].as_str().unwrap_or("");
    let (subagent, owner) = {
        let g = shared.state();
        match g.tasks.get(id) {
            Some(t) if t.subagent => (true, Some(id.to_string())),
            Some(t) => (false, t.owner.clone()),
            None => (false, None),
        }
    };
    let mut who = shared.who_of(owner.as_deref());
    if owner.is_none() {
        let turn = shared.state().tasks.get(id).and_then(|t| t.turn);
        who.turn = turn.or(who.turn);
    }
    (who, subagent)
}

fn on_task_progress(shared: &Shared, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let (who, subagent) = who_of_task(shared, &f);
    let kind = if subagent {
        AgentEventKind::SubagentProgress {
            id: SubagentId::new(f["task_id"].as_str().unwrap_or("")),
            description: f["description"].as_str().unwrap_or("").to_string(),
            last_tool: f["last_tool_name"].as_str().map(ToolName::new),
            usage: frames::subagent_usage(&f),
        }
    } else {
        AgentEventKind::Native {
            name: "system/task_progress".into(),
        }
    };
    shared.emit_as(who, kind, Some(raw));
}

/// `system/task_notification`: ended for now. The same id can start again.
fn on_task_notification(shared: &Shared, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let (who, subagent) = who_of_task(shared, &f);
    let id = f["task_id"].as_str().unwrap_or("");
    let count = {
        let mut g = shared.state();
        g.live_tasks.remove(id);
        g.live_tasks.len()
    };
    shared.live.send_replace(count);
    let status = frames::subagent_status(&f);
    let summary = f["summary"].as_str().map(str::to_string);
    let kind = if subagent {
        AgentEventKind::SubagentEnded {
            id: SubagentId::new(id),
            status,
            summary,
            usage: frames::subagent_usage(&f),
        }
    } else {
        AgentEventKind::BackgroundTaskEnded {
            id: TaskId::new(id),
            status,
            summary,
        }
    };
    shared.emit_as(who, kind, Some(raw));
}

fn on_control_request(shared: &Shared, queue: &MemoryInputQueue, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let kind: InputKind = frames::input_kind(&f["request"]);
    let native = f["request_id"].as_str().unwrap_or("").to_string();
    let who = shared.who_of(f["request"]["agent_id"].as_str());
    let turn = who.turn.unwrap_or(TurnId(0));
    let r = queue.enqueue_as(turn, who.agent.clone(), InputRoute::Live, kind, f);
    {
        let mut g = shared.state();
        g.native_of.insert(r.id.clone(), native.clone());
        g.by_native.insert(native, r.id.clone());
        // Raced the kill: it's pending all the same, and comes back with the rest.
        if let (Some(p), Some(call)) = (g.parked.as_mut(), ParkedCall::of(&r)) {
            p.calls.push(call);
        }
    }
    shared.emit_as(who, AgentEventKind::InputRequested { id: r.id }, Some(raw));
}

fn on_cancel(shared: &Shared, queue: &MemoryInputQueue, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let native = f["request_id"].as_str().unwrap_or("").to_string();
    let id = shared.state().by_native.get(&native).cloned();
    let cancelled = id.and_then(|id| queue.cancel(&id, CancelReason::WithdrawnByAgent));
    match cancelled {
        Some(r) => {
            let outcome = InputOutcome::Cancelled(CancelReason::WithdrawnByAgent);
            let who = Attribution {
                turn: Some(r.turn),
                agent: r.agent,
            };
            let kind = AgentEventKind::InputResolved {
                id: r.id,
                outcome,
                by: None,
            };
            shared.emit_as(who, kind, Some(raw));
        }
        // Our interrupt usually got there first, and a late cancel moves nothing.
        None => shared.emit(
            AgentEventKind::Native {
                name: "control_cancel_request".into(),
            },
            Some(raw),
        ),
    }
}

fn on_control_response(shared: &Shared, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let id = f["response"]["request_id"].as_str().unwrap_or("");
    if let Some(tx) = shared.state().awaiting.remove(id) {
        let _ = tx.send(f.clone());
    }
    shared.emit(
        AgentEventKind::Native {
            name: "control_response".into(),
        },
        Some(raw),
    );
}

fn on_result(shared: &Shared, frame: Frame) {
    let Frame { v: f, raw } = frame;
    let turn = shared.take_turn(!f["origin"].is_null());
    if let Some(usage) = frames::usage(&f) {
        shared.emit_as(
            Attribution::turn(turn.id),
            AgentEventKind::Tokens {
                turn: turn.id,
                usage,
            },
            None,
        );
    }
    let (status, disposition, stopped_by) = classify(&f, turn.stop);
    shared.terminal(turn.id, status, disposition, stopped_by, Some(raw));
}

/// Reads a result in light of what the driver did. A clean result means the turn
/// beat the stop, so it completed; any other result after a stop is that stop.
fn classify(
    result: &Value,
    stop: Option<StopCause>,
) -> (TurnStatus, ThreadDisposition, Option<StopCause>) {
    if result["is_error"] != Value::Bool(true) {
        return (TurnStatus::Completed, ThreadDisposition::Reusable, None);
    }
    let (failure, detail) = frames::failure_of(result);
    let broken = matches!(
        failure,
        FailureKind::SessionNotFound | FailureKind::NotLoggedIn
    );
    if !broken {
        if let Some(cause) = stop {
            return (
                TurnStatus::Interrupted,
                ThreadDisposition::Reusable,
                Some(cause),
            );
        }
    }
    let disposition = if broken {
        ThreadDisposition::Broken
    } else {
        ThreadDisposition::Reusable
    };
    (
        TurnStatus::Failed(TurnFailure { failure, detail }),
        disposition,
        None,
    )
}

async fn pump(
    shared: Arc<Shared>,
    queue: Arc<MemoryInputQueue>,
    stdout: BoxAsyncRead,
    stderr: tokio::task::JoinHandle<()>,
    wait: BoxFuture<'static, ExitInfo>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let raw = RawFrame::new(String::from_utf8_lossy(&line).trim_end().to_string());
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            shared.emit(
                AgentEventKind::Native {
                    name: "unparsed".into(),
                },
                Some(raw),
            );
            continue;
        };
        let ty = v["type"].as_str().unwrap_or("").to_string();
        let subtype = v["subtype"].as_str().map(str::to_string);
        let f = Frame { v, raw };
        match (ty.as_str(), subtype.as_deref()) {
            ("system", Some("init")) => {
                if let Some((failure, detail, raw)) = on_init(&shared, f) {
                    shared.refuse(failure, detail, raw).await;
                }
            }
            ("system", Some("task_started")) => on_task_started(&shared, f),
            ("system", Some("task_progress")) => on_task_progress(&shared, f),
            ("system", Some("task_notification")) => on_task_notification(&shared, f),
            ("system", Some("task_updated")) => {
                let (who, _) = who_of_task(&shared, &f.v);
                let name = "system/task_updated".to_string();
                shared.emit_as(who, AgentEventKind::Native { name }, Some(f.raw));
            }
            ("stream_event", _) => on_stream_event(&shared, f),
            ("assistant", _) => on_assistant(&shared, f),
            ("user", _) => on_user(&shared, f),
            ("control_request", _) => on_control_request(&shared, &queue, f),
            ("control_cancel_request", _) => on_cancel(&shared, &queue, f),
            ("control_response", _) => on_control_response(&shared, f),
            // The kill is on its way, and the parked turn isn't over.
            ("result", _) if shared.state().parked.is_some() => {
                let name = "result".to_string();
                shared.emit(AgentEventKind::Native { name }, Some(f.raw));
            }
            ("result", _) => on_result(&shared, f),
            (other, subtype) => {
                let name = match subtype {
                    Some(s) => format!("{other}/{s}"),
                    None => other.to_string(),
                };
                shared.emit(AgentEventKind::Native { name }, Some(f.raw));
            }
        }
    }

    let exit = wait.await;
    let _ = tokio::time::timeout(Duration::from_secs(1), stderr).await;
    finish(&shared, &queue, exit);
}

fn finish(shared: &Shared, queue: &MemoryInputQueue, exit: ExitInfo) {
    let parked = shared.state().parked.is_some();
    if !parked {
        shared.cancel_pending(queue, CancelReason::RunEnded);
    }
    let (open, stopping, refused, closed) = {
        let mut g = shared.state();
        g.exit = Some(exit);
        g.awaiting.clear();
        (
            std::mem::take(&mut g.turns),
            g.stopping,
            g.refused,
            g.input_closed,
        )
    };
    let stderr = shared.stderr.lock().expect("stderr poisoned").clone();
    let in_use = stderr.contains("already in use");

    let mut open = open;
    if open.is_empty() && in_use {
        // The CLI refused the id before reading anything, so no prompt got through.
        open.push_back(shared.take_turn(false));
    }
    for t in open {
        let cause = t.stop.or(stopping);
        if in_use {
            let failure = TurnFailure {
                failure: FailureKind::SessionInUse,
                detail: stderr.trim().to_string(),
            };
            shared.terminal(
                t.id,
                TurnStatus::Failed(failure),
                ThreadDisposition::Broken,
                None,
                None,
            );
        } else if let Some(cause) = cause {
            shared.terminal(
                t.id,
                TurnStatus::Interrupted,
                ThreadDisposition::Reusable,
                Some(cause),
                None,
            );
        } else {
            // Not ours. The transcript survives this, so the thread does too.
            let detail = format!("exited {exit:?} with no result; stderr: {}", stderr.trim());
            let failure = TurnFailure {
                failure: FailureKind::ProcessExited,
                detail,
            };
            shared.terminal(
                t.id,
                TurnStatus::Failed(failure),
                ThreadDisposition::Reusable,
                None,
                None,
            );
        }
    }
    let reason = match (refused, stopping, closed) {
        (true, _, _) => RunEndReason::Refused,
        (_, Some(cause), _) => RunEndReason::Stopped(cause),
        (_, None, _) if parked => RunEndReason::Parked,
        (_, None, true) => RunEndReason::Closed,
        (_, None, false) => RunEndReason::ProcessExited,
    };
    shared.emit(
        AgentEventKind::RunEnded {
            run: shared.run.clone(),
            reason,
            exit: Some(exit),
        },
        None,
    );
    shared.log.close();
    let _ = shared.ended.send(true);
}

async fn attach_mcp(
    shared: &Shared,
    spec: &RunSpec,
    secrets: &dyn SecretResolver,
) -> Result<(), DriveError> {
    if spec.mcp.is_empty() {
        return Ok(());
    }
    async fn resolved<K: ToString>(
        pairs: impl Iterator<Item = (K, &EnvValue)>,
        secrets: &dyn SecretResolver,
    ) -> Result<Vec<(String, ResolvedSecret)>, DriveError> {
        let mut out = Vec::new();
        for (k, v) in pairs {
            let value = secrets
                .resolve(v)
                .await
                .map_err(|e| DriveError::Setup(e.to_string()))?;
            out.push((k.to_string(), value));
        }
        Ok(out)
    }
    let mut servers = Vec::new();
    for server in &spec.mcp {
        let values = match &server.transport {
            McpTransport::Stdio { env, .. } => {
                resolved(
                    env.iter().map(|(k, v): (&EnvName, _)| (k.clone(), v)),
                    secrets,
                )
                .await?
            }
            McpTransport::Http { headers, .. } => {
                resolved(headers.iter().map(|(k, v)| (k.clone(), v)), secrets).await?
            }
        };
        servers.push((server, frames::mcp_config(&server.transport, &values)));
    }
    let id = shared.control_id("mcp");
    let (tx, rx) = oneshot::channel();
    shared.state().awaiting.insert(id.clone(), tx);
    shared
        .write(&frames::mcp_set_servers(&id, &servers))
        .await
        .map_err(|e| DriveError::Io(e.to_string()))?;
    let ack = tokio::time::timeout(MCP_ACK, rx)
        .await
        .map_err(|_| DriveError::Setup("no ack for mcp_set_servers".into()))?
        .map_err(|_| DriveError::Setup("the CLI went away during mcp_set_servers".into()))?;
    let errors = &ack["response"]["response"]["errors"];
    if ack["response"]["subtype"] != "success" || errors.as_object().is_some_and(|e| !e.is_empty())
    {
        return Err(DriveError::Setup(format!("mcp_set_servers: {errors}")));
    }
    Ok(())
}

pub async fn drive(
    options: ClaudeOptions,
    spec: RunSpec,
    process: Process,
) -> Result<Arc<dyn Run>, DriveError> {
    let Process {
        pid,
        stdin,
        stdout,
        stderr,
        signal,
        wait,
        secrets,
    } = process;
    let run = RunId::new(uuid::Uuid::new_v4().to_string());
    let session = match &spec.session {
        SessionMode::Fresh => None,
        SessionMode::Assign { id } | SessionMode::Resume { id } => Some(id.clone()),
    };
    let mut state = State {
        session,
        next_turn: spec.turns_before,
        ..State::default()
    };
    // An unparked run's input carries on the parked turn, so that turn opens here.
    if let Some(turn) = spec.continues {
        state.next_turn = turn.0;
        state.feed = Some(turn);
        state.turns.push_back(OpenTurn {
            id: turn,
            stop: None,
            origin: TurnOrigin::Resume,
            began: false,
        });
    }
    let shared = Arc::new(Shared {
        spec: spec.clone(),
        run: run.clone(),
        pid,
        options,
        log: EventLog::new(),
        state: Mutex::new(state),
        writer: AsyncMutex::new(Some(stdin)),
        signal,
        stderr: Mutex::new(String::new()),
        ended: watch::channel(false).0,
        live: watch::channel(0).0,
        next_control: AtomicU64::new(1),
    });
    let queue = Arc::new(MemoryInputQueue::new(
        run,
        Arc::new(Responder(shared.clone())),
    ));
    if let Some(turn) = spec.continues {
        let origin = TurnOrigin::Resume;
        shared.emit_as(
            Attribution::turn(turn),
            AgentEventKind::TurnStarted { turn, origin },
            None,
        );
    }
    let errors = tokio::spawn(drain_stderr(shared.clone(), stderr));
    tokio::spawn(pump(shared.clone(), queue.clone(), stdout, errors, wait));
    attach_mcp(&shared, &spec, secrets.as_ref()).await?;
    Ok(Arc::new(ClaudeRun { shared, queue }))
}

struct ClaudeRun {
    shared: Arc<Shared>,
    queue: Arc<MemoryInputQueue>,
}

impl ClaudeRun {
    /// Marks the oldest open turn as stopped by `cause`, and cancels what's
    /// pending first, so no answer slips in behind the stop. `None` if idle.
    fn begin_stop(&self, cause: StopCause) -> Option<TurnId> {
        let turn = {
            let mut g = self.shared.state();
            let t = g.turns.front_mut()?;
            t.stop.get_or_insert(cause);
            t.id
        };
        self.shared
            .cancel_pending(&self.queue, CancelReason::Interrupted);
        Some(turn)
    }
}

async fn turn_ended(mut events: BoxStream<'static, AgentEvent>, turn: TurnId) {
    while let Some(e) = events.next().await {
        if matches!(&e.kind, AgentEventKind::TurnTerminal(t) if t.turn == turn) {
            return;
        }
    }
}

impl Interrupt for ClaudeRun {
    fn interrupt(&self) -> BoxFuture<'static, Result<InterruptOutcome, RunError>> {
        let shared = self.shared.clone();
        // Subscribed before the frame goes out, so a fast terminal can't be missed.
        let events = shared.log.subscribe(EventSeq(0));
        let turn = self.begin_stop(StopCause::Interrupt);
        let background = turn.is_none() && !shared.state().live_tasks.is_empty();
        if background {
            shared.cancel_pending(&self.queue, CancelReason::Interrupted);
        }
        async move {
            let timeout = shared.options.interrupt_timeout;
            let id = shared.control_id("interrupt");
            let Some(turn) = turn else {
                if !background {
                    return Ok(InterruptOutcome::Idle);
                }
                // The CLI stops its tasks and sends no `result`: there's no turn to
                // end, so what's waited on is the tasks.
                let mut live = shared.live.subscribe();
                shared.write(&frames::interrupt(&id)).await?;
                let stopped = tokio::time::timeout(timeout, live.wait_for(|n| *n == 0))
                    .await
                    .is_ok_and(|r| r.is_ok());
                if stopped {
                    return Ok(InterruptOutcome::BackgroundStopped);
                }
                shared.force(StopCause::Force).await?;
                return Ok(InterruptOutcome::Forced);
            };
            shared.write(&frames::interrupt(&id)).await?;
            if tokio::time::timeout(timeout, turn_ended(events, turn))
                .await
                .is_ok()
            {
                return Ok(InterruptOutcome::Stopped);
            }
            shared.force(StopCause::Force).await?;
            Ok(InterruptOutcome::Forced)
        }
        .boxed()
    }
}

impl Run for ClaudeRun {
    fn info(&self) -> RunInfo {
        RunInfo {
            run: self.shared.run.clone(),
            pid: self.shared.pid,
            session: self.shared.state().session.clone(),
        }
    }

    fn events(&self, since: EventSeq) -> BoxStream<'static, AgentEvent> {
        self.shared.log.subscribe(since)
    }

    fn send(&self, input: UserInput) -> BoxFuture<'static, Result<TurnId, RunError>> {
        let shared = self.shared.clone();
        async move {
            let carried = shared.state().feed.take();
            if let Some(turn) = carried {
                let who = Attribution::turn(turn);
                let (text, by) = (input.text.clone(), input.by.clone());
                shared.emit_as(who, AgentEventKind::UserMessage { text, by }, None);
                shared.write(&frames::user(&input.text)).await?;
                return Ok(turn);
            }
            let turn = {
                let mut g = shared.state();
                if g.exit.is_some() {
                    return Err(RunError::Ended);
                }
                g.next_turn += 1;
                let id = TurnId(g.next_turn);
                g.turns.push_back(OpenTurn {
                    id,
                    stop: None,
                    origin: TurnOrigin::Prompt,
                    began: false,
                });
                id
            };
            let who = Attribution::turn(turn);
            let origin = TurnOrigin::Prompt;
            shared.emit_as(
                who.clone(),
                AgentEventKind::TurnStarted { turn, origin },
                None,
            );
            let (text, by) = (input.text.clone(), input.by.clone());
            shared.emit_as(who, AgentEventKind::UserMessage { text, by }, None);
            shared.write(&frames::user(&input.text)).await?;
            Ok(turn)
        }
        .boxed()
    }

    fn interrupt(&self) -> Option<&dyn Interrupt> {
        Some(self)
    }

    fn parked(&self) -> Option<ParkedRun> {
        self.shared.state().parked.clone()
    }

    /// Kills the process. Closing stdin won't do: the CLI fails what's pending, and
    /// the model carries on as though it had been refused.
    fn park(&self) -> BoxFuture<'static, Result<(), RunError>> {
        let (shared, queue) = (self.shared.clone(), self.queue.clone());
        async move {
            if shared.state().parked.is_some() {
                return Ok(());
            }
            let pending = queue.list(InputFilter::default()).await;
            let calls: Vec<ParkedCall> = pending
                .iter()
                .filter(|r| r.state.is_pending())
                .filter_map(ParkedCall::of)
                .collect();
            let front = shared.state().turns.pop_front();
            let turn = front.unwrap_or_else(|| shared.open_unasked(TurnOrigin::Resume));
            let ids = calls.iter().map(|c| c.tool_use_id.clone()).collect();
            let (session, completed) = {
                let g = shared.state();
                let pending_in: HashSet<&str> = calls
                    .iter()
                    .filter_map(|c| g.open_calls.get(c.tool_use_id.as_str()))
                    .map(|(message, _)| message.as_str())
                    .collect();
                let completed = g
                    .finished
                    .iter()
                    .filter(|(message, _)| pending_in.contains(message.as_str()))
                    .map(|(_, call)| call.clone())
                    .collect();
                (g.session.clone(), completed)
            };
            // Set before the event, so whoever reads `turn.parked` can read this.
            shared.state().parked = Some(ParkedRun {
                harness: "claude".into(),
                session: session.unwrap_or_else(|| SessionId::new("")),
                cwd: shared.spec.cwd.clone(),
                turn: turn.id,
                spec: shared.spec.clone(),
                calls,
                completed,
                parked_at_ms: now_ms(),
            });
            shared.emit_as(
                Attribution::turn(turn.id),
                AgentEventKind::TurnParked {
                    turn: turn.id,
                    calls: ids,
                },
                None,
            );
            shared
                .signal
                .signal(Signal::KillTree)
                .await
                .map_err(|e| RunError::Io(e.to_string()))
        }
        .boxed()
    }

    fn inputs(&self) -> Option<Arc<dyn InputQueue>> {
        Some(self.queue.clone())
    }

    fn close_input(&self) -> BoxFuture<'static, Result<(), RunError>> {
        let shared = self.shared.clone();
        async move { shared.close_stdin().await }.boxed()
    }

    fn stop(&self, mode: StopMode) -> BoxFuture<'static, Result<StopReport, RunError>> {
        let shared = self.shared.clone();
        let cause = match mode {
            StopMode::Graceful => StopCause::Stop,
            StopMode::Force => StopCause::Force,
        };
        shared.state().stopping.get_or_insert(cause);
        let open = self.begin_stop(cause);
        async move {
            if mode == StopMode::Force {
                shared.force(cause).await?;
                return Ok(shared.report(cause, true));
            }
            let background = !shared.state().live_tasks.is_empty();
            if open.is_some() || background {
                let id = shared.control_id("interrupt");
                // Losing this race to the process exiting is the same as stopping.
                let _ = shared.write(&frames::interrupt(&id)).await;
            }
            shared.close_stdin().await?;
            if shared.wait_ended(shared.options.stop_timeout).await {
                return Ok(shared.report(cause, false));
            }
            shared.force(StopCause::Force).await?;
            Ok(shared.report(cause, true))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_interrupts_error_result_is_interrupted_not_failed() {
        let r = json!({"is_error": true, "subtype": "error_during_execution", "errors": ["[ede_diagnostic]"]});
        let (status, disposition, by) = classify(&r, Some(StopCause::Interrupt));
        assert_eq!(status, TurnStatus::Interrupted);
        assert_eq!(disposition, ThreadDisposition::Reusable);
        assert_eq!(by, Some(StopCause::Interrupt));
    }

    #[test]
    fn the_same_frame_with_no_stop_is_a_failure() {
        let r = json!({"is_error": true, "errors": ["[ede_diagnostic]"]});
        let (status, _, by) = classify(&r, None);
        assert!(matches!(status, TurnStatus::Failed(_)));
        assert_eq!(by, None);
    }

    #[test]
    fn a_turn_that_beat_the_interrupt_completed() {
        let (status, _, by) = classify(&json!({"is_error": false}), Some(StopCause::Interrupt));
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(by, None);
    }

    #[test]
    fn a_missing_conversation_is_broken_even_during_a_stop() {
        let r = json!({"is_error": true, "errors": ["No conversation found with session ID: x"]});
        let (status, disposition, _) = classify(&r, Some(StopCause::Stop));
        let TurnStatus::Failed(f) = status else {
            panic!()
        };
        assert_eq!(f.failure, FailureKind::SessionNotFound);
        assert_eq!(disposition, ThreadDisposition::Broken);
    }
}

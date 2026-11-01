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
    AgentEvent, AgentEventKind, BoxAsyncRead, BoxAsyncWrite, CancelReason, DriveError, EnvName,
    EnvValue, EventLog, EventSeq, ExitInfo, FailureKind, InputAnswer, InputKind, InputOutcome,
    InputQueue, InputRequest, InputRequestId, InputResponder, Interrupt, InterruptOutcome,
    McpTransport, MemoryInputQueue, ModelName, Process, ProcessSignal, ResolvedSecret, Run,
    RunEndReason, RunError, RunId, RunInfo, RunSpec, SecretResolver, SessionId, SessionMode,
    Signal, StopCause, StopMode, StopReport, ThreadDisposition, ToolName, ToolUseId, TurnFailure,
    TurnId, TurnStatus, TurnTerminal, UserInput,
};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
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
}

#[derive(Default)]
struct State {
    next_turn: u64,
    /// In order: the CLI queues prompts sent during a turn.
    turns: VecDeque<OpenTurn>,
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

    fn emit(&self, kind: AgentEventKind, raw: Option<Value>) {
        self.log.append(kind, raw);
    }

    fn terminal(
        &self,
        turn: TurnId,
        status: TurnStatus,
        thread_disposition: ThreadDisposition,
        stopped_by: Option<StopCause>,
        raw: Option<Value>,
    ) {
        let t = TurnTerminal {
            turn,
            status,
            thread_disposition,
            stopped_by,
        };
        self.emit(AgentEventKind::TurnTerminal(t), raw);
    }

    /// The oldest open turn, or a turn for a frame no prompt asked for: a resume
    /// that fails reports it before reading any stdin.
    fn take_turn(&self) -> OpenTurn {
        let mut g = self.state();
        g.turns.pop_front().unwrap_or_else(|| {
            g.next_turn += 1;
            OpenTurn {
                id: TurnId(g.next_turn),
                stop: g.stopping,
            }
        })
    }

    fn cancel_pending(&self, queue: &MemoryInputQueue, reason: CancelReason) {
        for r in queue.cancel_all(reason) {
            let outcome = InputOutcome::Cancelled(reason);
            self.emit(AgentEventKind::InputResolved { id: r.id, outcome }, None);
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
    async fn refuse(&self, failure: FailureKind, detail: String, raw: Value) {
        let turn = self.take_turn();
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
            let frame = frames::control_response(&native, &request.raw["request"], &answer);
            shared.write(&frame).await.map_err(|e| e.to_string())?;
            let outcome = answer.outcome();
            shared.emit(
                AgentEventKind::InputResolved {
                    id: request.id,
                    outcome,
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

fn on_init(shared: &Shared, f: Value) -> Option<(FailureKind, String, Value)> {
    let version = f["claude_code_version"].as_str().map(str::to_string);
    if let Some(v) = &version {
        if !SUPPORTED_CLI_VERSIONS.contains(&v.as_str()) {
            let detail =
                format!("claude {v} is not supported; supported: {SUPPORTED_CLI_VERSIONS:?}");
            return Some((FailureKind::UnsupportedVersion, detail, f));
        }
    }
    let mode = f["permissionMode"].as_str().map(str::to_string);
    if mode.as_deref() == Some("bypassPermissions") {
        let detail = "the CLI reports bypassed permissions; refusing to drive it".to_string();
        return Some((FailureKind::Protocol, detail, f));
    }
    let id = SessionId::new(f["session_id"].as_str().unwrap_or(""));
    let first = {
        let mut g = shared.state();
        g.session = Some(id.clone());
        !std::mem::replace(&mut g.session_reported, true)
    };
    if first {
        let kind = AgentEventKind::SessionStarted {
            session: id,
            model: f["model"].as_str().map(ModelName::new),
            harness_version: version,
            permission_mode: mode,
        };
        shared.emit(kind, Some(f));
    } else {
        shared.emit(
            AgentEventKind::Native {
                name: "system/init".into(),
            },
            Some(f),
        );
    }
    None
}

fn on_stream_event(shared: &Shared, f: Value) {
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
    shared.emit(kind, Some(f));
}

fn on_assistant(shared: &Shared, f: Value) {
    let blocks = f["message"]["content"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut said = false;
    for b in blocks {
        said = true;
        match b["type"].as_str() {
            Some("text") => {
                let text = b["text"].as_str().unwrap_or("").to_string();
                shared.emit(AgentEventKind::Message { text }, Some(f.clone()));
            }
            Some("tool_use") => {
                let kind = AgentEventKind::ToolCall {
                    id: ToolUseId::new(b["id"].as_str().unwrap_or("")),
                    name: ToolName::new(b["name"].as_str().unwrap_or("")),
                    input: b["input"].clone(),
                };
                shared.emit(kind, Some(f.clone()));
            }
            _ => {
                let name = format!("assistant/{}", b["type"].as_str().unwrap_or("?"));
                shared.emit(AgentEventKind::Native { name }, Some(f.clone()));
            }
        }
    }
    if !said {
        shared.emit(
            AgentEventKind::Native {
                name: "assistant".into(),
            },
            Some(f),
        );
    }
}

fn on_user(shared: &Shared, f: Value) {
    let results = frames::tool_results(&f);
    if results.is_empty() {
        shared.emit(
            AgentEventKind::Native {
                name: "user".into(),
            },
            Some(f),
        );
        return;
    }
    for (id, is_error, output) in results {
        let kind = AgentEventKind::ToolResult {
            id: ToolUseId::new(id),
            is_error,
            output,
        };
        shared.emit(kind, Some(f.clone()));
    }
}

fn on_control_request(shared: &Shared, queue: &MemoryInputQueue, f: Value) {
    let kind: InputKind = frames::input_kind(&f["request"]);
    let native = f["request_id"].as_str().unwrap_or("").to_string();
    let turn = shared.state().turns.front().map_or(TurnId(0), |t| t.id);
    let r = queue.enqueue(turn, kind, f.clone());
    {
        let mut g = shared.state();
        g.native_of.insert(r.id.clone(), native.clone());
        g.by_native.insert(native, r.id.clone());
    }
    shared.emit(AgentEventKind::InputRequested { id: r.id }, Some(f));
}

fn on_cancel(shared: &Shared, queue: &MemoryInputQueue, f: Value) {
    let native = f["request_id"].as_str().unwrap_or("").to_string();
    let id = shared.state().by_native.get(&native).cloned();
    let cancelled = id.and_then(|id| queue.cancel(&id, CancelReason::WithdrawnByAgent));
    match cancelled {
        Some(r) => {
            let outcome = InputOutcome::Cancelled(CancelReason::WithdrawnByAgent);
            shared.emit(AgentEventKind::InputResolved { id: r.id, outcome }, Some(f));
        }
        // Our interrupt usually got there first, and a late cancel moves nothing.
        None => shared.emit(
            AgentEventKind::Native {
                name: "control_cancel_request".into(),
            },
            Some(f),
        ),
    }
}

fn on_control_response(shared: &Shared, f: Value) {
    let id = f["response"]["request_id"].as_str().unwrap_or("");
    if let Some(tx) = shared.state().awaiting.remove(id) {
        let _ = tx.send(f.clone());
    }
    shared.emit(
        AgentEventKind::Native {
            name: "control_response".into(),
        },
        Some(f),
    );
}

fn on_result(shared: &Shared, f: Value) {
    let turn = shared.take_turn();
    if let Some(usage) = frames::usage(&f) {
        shared.emit(
            AgentEventKind::Tokens {
                turn: turn.id,
                usage,
            },
            None,
        );
    }
    let (status, disposition, stopped_by) = classify(&f, turn.stop);
    shared.terminal(turn.id, status, disposition, stopped_by, Some(f));
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
        let Ok(f) = serde_json::from_slice::<Value>(&line) else {
            let raw = Value::String(String::from_utf8_lossy(&line).trim_end().to_string());
            shared.emit(
                AgentEventKind::Native {
                    name: "unparsed".into(),
                },
                Some(raw),
            );
            continue;
        };
        let ty = f["type"].as_str().unwrap_or("").to_string();
        match ty.as_str() {
            "system" if f["subtype"] == "init" => {
                if let Some((failure, detail, raw)) = on_init(&shared, f) {
                    shared.refuse(failure, detail, raw).await;
                }
            }
            "stream_event" => on_stream_event(&shared, f),
            "assistant" => on_assistant(&shared, f),
            "user" => on_user(&shared, f),
            "control_request" => on_control_request(&shared, &queue, f),
            "control_cancel_request" => on_cancel(&shared, &queue, f),
            "control_response" => on_control_response(&shared, f),
            "result" => on_result(&shared, f),
            other => {
                let name = match f["subtype"].as_str() {
                    Some(s) => format!("{other}/{s}"),
                    None => other.to_string(),
                };
                shared.emit(AgentEventKind::Native { name }, Some(f));
            }
        }
    }

    let exit = wait.await;
    let _ = tokio::time::timeout(Duration::from_secs(1), stderr).await;
    finish(&shared, &queue, exit);
}

fn finish(shared: &Shared, queue: &MemoryInputQueue, exit: ExitInfo) {
    shared.cancel_pending(queue, CancelReason::RunEnded);
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
        open.push_back(shared.take_turn());
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
    let shared = Arc::new(Shared {
        run: run.clone(),
        pid,
        options,
        log: EventLog::new(),
        state: Mutex::new(State {
            session,
            ..State::default()
        }),
        writer: AsyncMutex::new(Some(stdin)),
        signal,
        stderr: Mutex::new(String::new()),
        ended: watch::channel(false).0,
        next_control: AtomicU64::new(1),
    });
    let queue = Arc::new(MemoryInputQueue::new(
        run,
        Arc::new(Responder(shared.clone())),
    ));
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
        async move {
            let Some(turn) = turn else {
                return Ok(InterruptOutcome::Idle);
            };
            let id = shared.control_id("interrupt");
            shared.write(&frames::interrupt(&id)).await?;
            let timeout = shared.options.interrupt_timeout;
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
            let turn = {
                let mut g = shared.state();
                if g.exit.is_some() {
                    return Err(RunError::Ended);
                }
                g.next_turn += 1;
                let id = TurnId(g.next_turn);
                g.turns.push_back(OpenTurn { id, stop: None });
                id
            };
            shared.emit(AgentEventKind::TurnStarted { turn }, None);
            shared.write(&frames::user(&input.text)).await?;
            Ok(turn)
        }
        .boxed()
    }

    fn interrupt(&self) -> Option<&dyn Interrupt> {
        Some(self)
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
            if open.is_some() {
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

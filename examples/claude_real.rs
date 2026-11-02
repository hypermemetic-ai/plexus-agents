//! Real Claude runs through `LocalEnvironment` (AGENTS-10). Spends tokens.
//!
//! ```text
//! AGENTS_REAL=spend-tokens claude_real --raw DIR --cwd DIR [--only a,b] [--model haiku]
//! ```
//! Uses the logged-in account, never an API key, and refuses to start with one
//! set. Never bypasses permissions: a tool runs by an answered approval, or by one
//! narrow rule. `--raw` is unscrubbed and stays outside any repo; run
//! `scripts/scrub-fixtures` to make a fixture of it.
//!
//! Every scenario returns named claims from what it observed. A false claim is a
//! FAIL and the exit code says so.

use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use plexus_agents::runner::launch;
use plexus_agents::{ClaudeHarness, ClaudeOptions, LocalEnvironment, RecordingEnvironment};
use plexus_harness::{
    resume_or_restart, AgentEvent, AgentEventKind, CancelReason, EnvName, EnvValue, EventSeq,
    FailureKind, Harness, InputAnswer, InputError, InputFilter, InputKind, InputOutcome,
    InputRequest, InputState, InterruptOutcome, McpServer, McpTransport, ModelName, Run, RunSpec,
    SecretRef, SessionId, SessionMode, StopCause, StopMode, ThreadDisposition, ToolName,
    ToolPolicy, TurnOrigin, TurnStatus, TurnTerminal, UserInput,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(90);
const TICK_TOOL: &str =
    "i=0; while [ $i -lt 60 ]; do echo $i >> ticks; sleep 1; i=$((i+1)); done\n";
const TICK_RULE: &str = "Bash(sh slow-tool.sh)";
const MCP_TOKEN_VAR: &str = "AGENTS10_DEMO_MCP_TOKEN";
const MCP_TOKEN: &str = "agents10-demo-token-not-a-credential";

struct Ctx {
    model: String,
    cwd: PathBuf,
    env: RecordingEnvironment<LocalEnvironment>,
    claims: Map<String, Value>,
    facts: Map<String, Value>,
}

impl Ctx {
    fn claim(&mut self, name: &str, ok: bool) {
        println!("    {} {name}", if ok { "ok  " } else { "FAIL" });
        self.claims.insert(name.to_string(), json!(ok));
    }

    fn fact(&mut self, name: &str, v: impl Into<Value>) {
        self.facts.insert(name.to_string(), v.into());
    }

    fn spec(&self) -> RunSpec {
        // The minimum a real run takes: USER reaches the Keychain login, PATH is
        // for the tools. HOME isn't needed: the CLI finds it from the account.
        RunSpec {
            cwd: Some(self.cwd.to_string_lossy().to_string()),
            model: Some(ModelName::new(self.model.clone())),
            tools: tools(&["Bash"], &[]),
            ..RunSpec::default()
        }
        .with_host_env(["USER", "PATH"])
    }

    fn file(&self, name: &str) -> PathBuf {
        self.cwd.join(name)
    }

    fn ticks(&self) -> usize {
        std::fs::read_to_string(self.file("ticks")).map_or(0, |s| s.lines().count())
    }

    async fn start(&self, spec: RunSpec) -> Live {
        self.start_with(&ClaudeHarness::default(), spec).await
    }

    async fn start_with(&self, harness: &ClaudeHarness, spec: RunSpec) -> Live {
        let run = launch(harness, &self.env, spec).await.expect("launch");
        Live::new(run)
    }
}

fn tools(enabled: &[&str], preapproved: &[&str]) -> ToolPolicy {
    ToolPolicy {
        enabled: Some(enabled.iter().map(|t| ToolName::new(*t)).collect()),
        preapproved: preapproved.iter().map(|s| s.to_string()).collect(),
    }
}

/// A run and a cursor over its events.
struct Live {
    run: Arc<dyn Run>,
    cursor: EventSeq,
}

impl Live {
    fn new(run: Arc<dyn Run>) -> Self {
        Self {
            run,
            cursor: EventSeq(0),
        }
    }

    /// Reads on until `want` passes, returning what it picked.
    async fn until<T>(&mut self, what: &str, want: impl Fn(&AgentEvent) -> Option<T>) -> T {
        let mut events = self.run.events(self.cursor);
        let found = tokio::time::timeout(WAIT, async {
            while let Some(e) = events.next().await {
                self.cursor = e.seq;
                if let Some(v) = want(&e) {
                    return Some(v);
                }
            }
            None
        })
        .await;
        match found {
            Ok(Some(v)) => v,
            Ok(None) => panic!("stream ended waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }

    async fn terminal(&mut self) -> TurnTerminal {
        self.until("turn.terminal", |e| match &e.kind {
            AgentEventKind::TurnTerminal(t) => Some(t.clone()),
            _ => None,
        })
        .await
    }

    async fn send(&self, text: &str) {
        self.run.send(UserInput::text(text)).await.expect("send");
    }

    async fn turn(&mut self, text: &str) -> TurnTerminal {
        self.send(text).await;
        self.terminal().await
    }

    async fn requested(&mut self) -> plexus_harness::InputRequestId {
        self.until("input.requested", |e| match &e.kind {
            AgentEventKind::InputRequested { id } => Some(id.clone()),
            _ => None,
        })
        .await
    }

    fn text_since(&self, from: EventSeq) -> BoxFuture<'static, String> {
        // Everything already logged after `from`; the log is closed or we'd block.
        let mut events = self.run.events(from);
        async move {
            let mut text = String::new();
            while let Ok(Some(e)) =
                tokio::time::timeout(Duration::from_millis(50), events.next()).await
            {
                if let AgentEventKind::Message { text: t } = e.kind {
                    text.push_str(&t);
                }
            }
            text
        }
        .boxed()
    }

    async fn close(&mut self) {
        let _ = self.run.close_input().await;
        self.until("run.ended", |e| {
            matches!(e.kind, AgentEventKind::RunEnded { .. }).then_some(())
        })
        .await;
    }

    fn pid(&self) -> u32 {
        self.run.info().pid.expect("a local run has a pid")
    }
}

fn is_completed(t: &TurnTerminal) -> bool {
    t.status == TurnStatus::Completed
}

fn start_time(pid: u32) -> String {
    let out = Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Every pid below `root`.
fn tree(root: u32) -> Vec<u32> {
    let out = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .expect("ps");
    let rows: Vec<(u32, u32)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect();
    let (mut found, mut todo) = (vec![], vec![root]);
    while let Some(p) = todo.pop() {
        for (pid, ppid) in &rows {
            if *ppid == p && !found.contains(pid) {
                found.push(*pid);
                todo.push(*pid);
            }
        }
    }
    found
}

fn alive(pid: u32) -> bool {
    let out = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output();
    out.is_ok_and(|o| {
        let s = String::from_utf8_lossy(&o.stdout);
        let s = s.trim();
        !s.is_empty() && !s.starts_with('Z')
    })
}

async fn streaming_second_turn(c: &mut Ctx) {
    let mut l = c
        .start(RunSpec {
            tools: tools(&[], &[]),
            ..c.spec()
        })
        .await;
    let pid = l.pid();
    let t = l
        .turn("Remember the word heron. Count from 1 to 40, one number per line.")
        .await;
    c.claim("turn 1 completed", is_completed(&t));
    let events: Vec<AgentEvent> = {
        let mut s = l.run.events(EventSeq(0));
        let mut v = vec![];
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(50), s.next()).await {
            v.push(e);
        }
        v
    };
    let stamps: Vec<u64> = events
        .iter()
        .filter(|e| matches!(e.kind, AgentEventKind::TextDelta { .. }))
        .map(|e| e.at_ms)
        .collect();
    let mut distinct = stamps.clone();
    distinct.dedup();
    c.fact("text_deltas", stamps.len());
    c.fact("distinct_delta_ms", distinct.len());
    c.fact(
        "delta_span_ms",
        stamps.last().unwrap_or(&0) - stamps.first().unwrap_or(&0),
    );
    c.claim("partial text arrived as many deltas", stamps.len() >= 3);
    c.claim("deltas carry distinct timestamps", distinct.len() >= 3);
    let mode = events.iter().find_map(|e| match &e.kind {
        AgentEventKind::SessionStarted {
            permission_mode, ..
        } => permission_mode.clone(),
        _ => None,
    });
    c.fact("init_permission_mode", mode.clone().unwrap_or_default());
    c.claim(
        "init reports permission mode default",
        mode.as_deref() == Some("default"),
    );

    let before = start_time(pid);
    let from = l.cursor;
    let t = l
        .turn("What word did I ask you to remember? Reply with only that word.")
        .await;
    let text = l.text_since(from).await;
    c.claim("turn 2 completed", is_completed(&t));
    c.claim("turn 2 knows turn 1", text.to_lowercase().contains("heron"));
    c.claim(
        "same pid, same process start",
        l.pid() == pid && start_time(pid) == before && !before.is_empty(),
    );
    c.fact("pid", pid);
    l.close().await;
}

async fn approval(
    c: &mut Ctx,
    marker: &str,
    answer: InputAnswer,
) -> (Live, InputRequest, TurnTerminal) {
    let mut l = c.start(c.spec()).await;
    l.send(&format!("Run this exact Bash command: touch {marker}"))
        .await;
    let id = l.requested().await;
    // A separate task with nothing but the queue and the id, as another client would be.
    let q = l.run.inputs().expect("queue");
    let client = {
        let (q, id) = (q.clone(), id.clone());
        tokio::spawn(async move {
            let request = q.get(&id).await.expect("found by id");
            let listed = q.list(InputFilter::pending()).await;
            let answered = q.answer(&id, answer).await;
            (request, listed, answered)
        })
    };
    let (request, listed, answered) = client.await.expect("client task");
    c.claim(
        "the request is in the queue, found by id from another task",
        listed.iter().any(|r| r.id == id),
    );
    c.claim("the answer was accepted", answered.is_ok());
    let t = l.terminal().await;
    (l, request, t)
}

async fn approve_allow(c: &mut Ctx) {
    let (mut l, request, t) = approval(
        c,
        "allowed-marker",
        InputAnswer::Allow {
            updated_input: None,
        },
    )
    .await;
    c.claim(
        "it's a Bash tool approval",
        matches!(
        &request.kind, InputKind::ToolApproval { tool, .. } if tool.as_str() == "Bash"),
    );
    c.claim("the tool ran", c.file("allowed-marker").exists());
    c.claim("the turn completed", is_completed(&t));
    let q = l.run.inputs().expect("queue");
    let after = q.get(&request.id).await.expect("still queryable");
    c.claim(
        "stays queryable, Answered",
        matches!(after.state, InputState::Answered { .. }),
    );
    let second = q
        .answer(
            &request.id,
            InputAnswer::Allow {
                updated_input: None,
            },
        )
        .await;
    c.claim(
        "a second answer gets AlreadyResolved",
        matches!(second, Err(InputError::AlreadyResolved { .. })),
    );
    let id = request.id.clone();
    let all: Vec<AgentEvent> = {
        let mut s = l.run.events(EventSeq(0));
        let mut v = vec![];
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(50), s.next()).await {
            v.push(e);
        }
        v
    };
    c.claim("input.resolved says allowed", all.iter().any(|e| matches!(
        &e.kind, AgentEventKind::InputResolved { id: i, outcome: InputOutcome::Allowed } if *i == id)));
    l.close().await;
}

async fn approve_edited(c: &mut Ctx) {
    let edited = json!({"command": "touch edited-marker"});
    let answer = InputAnswer::Allow {
        updated_input: Some(edited),
    };
    let (mut l, _, t) = approval(c, "original-marker", answer).await;
    c.claim("the edited command ran", c.file("edited-marker").exists());
    c.claim("the original did not", !c.file("original-marker").exists());
    c.claim("the turn completed", is_completed(&t));
    l.close().await;
}

async fn deny(c: &mut Ctx) {
    let answer = InputAnswer::Deny {
        message: "operator said no".into(),
        stop_turn: false,
    };
    let (mut l, _, t) = approval(c, "denied-marker", answer).await;
    c.claim("the tool did not run", !c.file("denied-marker").exists());
    c.claim("the turn went on and completed", is_completed(&t));
    let mut s = l.run.events(EventSeq(0));
    let mut message = false;
    while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(50), s.next()).await {
        if let AgentEventKind::ToolResult {
            is_error: true,
            output,
            ..
        } = &e.kind
        {
            message |= output.to_string().contains("operator said no");
        }
    }
    c.claim("the agent read the message", message);
    l.close().await;
}

async fn deny_stop_turn(c: &mut Ctx) {
    let answer = InputAnswer::Deny {
        message: "stop here".into(),
        stop_turn: true,
    };
    let (mut l, _, t) = approval(c, "stop-marker", answer).await;
    c.claim("the tool did not run", !c.file("stop-marker").exists());
    c.claim(
        "the terminal is Interrupted",
        t.status == TurnStatus::Interrupted,
    );
    c.claim(
        "it says a denial stopped it",
        t.stopped_by == Some(StopCause::DenyAndStop),
    );
    c.claim(
        "the thread is reusable",
        t.thread_disposition == ThreadDisposition::Reusable,
    );
    let t = l.turn("Reply with only: ok").await;
    c.claim("the next turn completes", is_completed(&t));
    l.close().await;
}

async fn question(c: &mut Ctx) {
    let spec = RunSpec {
        tools: tools(&["AskUserQuestion"], &[]),
        ..c.spec()
    };
    let mut l = c.start(spec).await;
    l.send("Use the AskUserQuestion tool to ask which color I prefer, with options red and blue. Then reply with only my pick.").await;
    let id = l.requested().await;
    let q = l.run.inputs().expect("queue");
    let r = q.get(&id).await.expect("found");
    let Some(asked) = (match &r.kind {
        InputKind::Question { questions, .. } => questions.first().cloned(),
        _ => None,
    }) else {
        c.claim("it's a Question", false);
        l.close().await;
        return;
    };
    c.claim("it's a Question with options", asked.options.len() >= 2);
    let pick = asked
        .options
        .iter()
        .find(|o| o.label.to_lowercase().contains("blue"))
        .map_or("blue", |o| &o.label);
    let answers = BTreeMap::from([(asked.question.clone(), pick.to_string())]);
    q.answer(&id, InputAnswer::Answers { answers })
        .await
        .expect("answer");
    let from = l.cursor;
    let t = l.terminal().await;
    let text = l.text_since(from).await;
    c.claim("the turn completed", is_completed(&t));
    c.claim(
        "the agent got the answer",
        text.to_lowercase().contains("blue"),
    );
    l.close().await;
}

async fn interrupt_continue(c: &mut Ctx) {
    let mut l = c
        .start(RunSpec {
            tools: tools(&[], &[]),
            ..c.spec()
        })
        .await;
    let pid = l.pid();
    let before = start_time(pid);
    let t = l
        .turn("Remember the word walrus. Reply with only: ok")
        .await;
    c.claim("turn 1 completed", is_completed(&t));
    l.send("Remember the word quince too. Then count from 1 to 400, one number per line, with a one-sentence remark on each.").await;
    l.until("text", |e| {
        matches!(e.kind, AgentEventKind::TextDelta { .. }).then_some(())
    })
    .await;
    let outcome = l
        .run
        .interrupt()
        .expect("interrupt")
        .interrupt()
        .await
        .expect("interrupt");
    c.claim(
        "the interrupt stopped the turn",
        outcome == InterruptOutcome::Stopped,
    );
    let t = l.terminal().await;
    c.claim(
        "the terminal is Interrupted, by the driver",
        t.status == TurnStatus::Interrupted && t.stopped_by == Some(StopCause::Interrupt),
    );
    c.claim(
        "the thread is reusable",
        t.thread_disposition == ThreadDisposition::Reusable,
    );
    let idle = l
        .run
        .interrupt()
        .expect("interrupt")
        .interrupt()
        .await
        .expect("interrupt");
    c.claim(
        "nothing to interrupt when idle",
        idle == InterruptOutcome::Idle,
    );
    let from = l.cursor;
    let t = l
        .turn("List every word I asked you to remember, comma separated.")
        .await;
    let text = l.text_since(from).await.to_lowercase();
    c.claim(
        "the same process carries on",
        is_completed(&t) && start_time(pid) == before,
    );
    c.claim(
        "it knows both words",
        text.contains("walrus") && text.contains("quince"),
    );
    l.close().await;
}

async fn pending_interrupt(c: &mut Ctx) {
    let mut l = c.start(c.spec()).await;
    let t = l
        .turn("Remember the word lantern. Reply with only: ok")
        .await;
    c.claim("turn 1 completed", is_completed(&t));
    l.send("Run this exact Bash command: touch pending-marker")
        .await;
    let id = l.requested().await;
    let q = l.run.inputs().expect("queue");
    let waiter = tokio::spawn({
        let (q, id) = (q.clone(), id.clone());
        async move { q.wait(&id).await }
    });
    let outcome = l
        .run
        .interrupt()
        .expect("interrupt")
        .interrupt()
        .await
        .expect("interrupt");
    c.claim(
        "the interrupt stopped the turn",
        outcome == InterruptOutcome::Stopped,
    );
    let got = waiter.await.expect("waiter").expect("wait");
    c.claim(
        "the approval is Cancelled, Interrupted, and a waiter was told",
        matches!(
            got.state,
            InputState::Cancelled {
                reason: CancelReason::Interrupted,
                ..
            }
        ),
    );
    let late = q
        .answer(
            &id,
            InputAnswer::Allow {
                updated_input: None,
            },
        )
        .await;
    c.claim(
        "a late answer is refused",
        matches!(late, Err(InputError::AlreadyResolved { .. })),
    );
    let t = l.terminal().await;
    c.claim(
        "the terminal is Interrupted",
        t.status == TurnStatus::Interrupted,
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    c.claim("the tool never ran", !c.file("pending-marker").exists());
    let t = l.turn("Reply with only: ok").await;
    c.claim("the same process takes another turn", is_completed(&t));
    l.close().await;
    c.claim(
        "the tool still never ran",
        !c.file("pending-marker").exists(),
    );
}

async fn resume(c: &mut Ctx) {
    let mut l = c
        .start(RunSpec {
            tools: tools(&[], &[]),
            ..c.spec()
        })
        .await;
    let t = l
        .turn("Remember the word marble. Reply with only: ok")
        .await;
    c.claim("turn 1 completed", is_completed(&t));
    let session = l.run.info().session.expect("session");
    let first_pid = l.pid();
    l.close().await;
    let harness: Arc<dyn Harness> = Arc::new(ClaudeHarness::default());
    let spec = RunSpec {
        tools: tools(&[], &[]),
        ..c.spec()
    };
    let prompt = UserInput::text("What word did I ask you to remember? Reply with only that word.");
    let r = resume_or_restart(harness, &c.env, &spec, &session, prompt)
        .await
        .expect("resume");
    c.claim("it resumed rather than restarted", !r.restarted);
    let mut l = Live::new(r.run);
    c.claim("a new process", l.pid() != first_pid);
    let t = l.terminal().await;
    let text = l.text_since(EventSeq(0)).await;
    c.claim("the resumed turn completed", is_completed(&t));
    c.claim("it knows the word", text.to_lowercase().contains("marble"));
    l.close().await;
}

async fn force_stop(c: &mut Ctx) {
    std::fs::write(c.file("slow-tool.sh"), TICK_TOOL).expect("tool");
    let spec = RunSpec {
        tools: tools(&["Bash"], &[TICK_RULE]),
        ..c.spec()
    };
    let mut l = c.start(spec.clone()).await;
    let pid = l.pid();
    let t = l
        .turn("Remember the word cobalt. Reply with only: ok")
        .await;
    c.claim("turn 1 completed", is_completed(&t));
    let session = l.run.info().session.expect("session");
    l.send("Run this exact Bash command and then reply done: sh slow-tool.sh")
        .await;
    l.until("tool.call", |e| {
        matches!(e.kind, AgentEventKind::ToolCall { .. }).then_some(())
    })
    .await;
    for _ in 0..200 {
        if c.ticks() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pids = tree(pid);
    c.fact("tool_tree", pids.len());
    c.claim(
        "the tool was ticking, with processes below claude",
        c.ticks() >= 2 && !pids.is_empty(),
    );
    let report = l.run.stop(StopMode::Force).await.expect("stop");
    c.claim("the stop reports forced", report.forced);
    let t = l.terminal().await;
    c.claim(
        "the terminal says Interrupted by Force",
        t.status == TurnStatus::Interrupted && t.stopped_by == Some(StopCause::Force),
    );
    let ticks = c.ticks();
    tokio::time::sleep(Duration::from_secs(3)).await;
    c.fact("ticks_grew_after_stop", c.ticks() - ticks);
    c.claim("no tick after the stop", c.ticks() == ticks);
    let left: Vec<u32> = pids.iter().copied().filter(|p| alive(*p)).collect();
    c.fact("left_running", json!(left));
    c.claim(
        "no tool process left running",
        left.is_empty() && !alive(pid),
    );
    let harness: Arc<dyn Harness> = Arc::new(ClaudeHarness::default());
    let prompt = UserInput::text("What word did I ask you to remember? Reply with only that word.");
    let r = resume_or_restart(harness, &c.env, &spec, &session, prompt)
        .await
        .expect("resume");
    let mut l = Live::new(r.run);
    let t = l.terminal().await;
    let text = l.text_since(EventSeq(0)).await;
    c.claim(
        "the thread resumes after a force stop",
        is_completed(&t) && !r.restarted,
    );
    c.claim("it knows the word", text.to_lowercase().contains("cobalt"));
    l.close().await;
}

async fn stop_before_init(c: &mut Ctx) {
    stopped_early(c, StopMode::Force, true).await;
}

async fn stop_before_init_graceful(c: &mut Ctx) {
    // AGENTS-3 c4e: a control interrupt this early can leave a transcript, so this
    // branch must recover by whichever road the CLI left open.
    stopped_early(c, StopMode::Graceful, false).await;
}

async fn stopped_early(c: &mut Ctx, mode: StopMode, must_restart: bool) {
    let id = SessionId::new(uuid::Uuid::new_v4().to_string());
    let spec = RunSpec {
        session: SessionMode::Assign { id: id.clone() },
        tools: tools(&[], &[]),
        ..c.spec()
    };
    let mut l = c.start(spec).await;
    l.send("Remember the word thistle. Reply with only: ok")
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let report = l.run.stop(mode).await.expect("stop");
    c.fact("stop_exit", json!(report.exit));
    let t = l.terminal().await;
    c.claim(
        "the terminal says the driver stopped it",
        t.stopped_by.is_some() && t.status == TurnStatus::Interrupted,
    );
    let all: Vec<AgentEvent> = l.run.events(EventSeq(0)).collect().await;
    let init = all
        .iter()
        .any(|e| matches!(e.kind, AgentEventKind::SessionStarted { .. }));
    c.fact("init_seen", init);
    if must_restart {
        c.claim("it stopped before system/init", !init);
    }
    c.claim(
        "the thread reads reusable",
        t.thread_disposition == ThreadDisposition::Reusable,
    );

    let harness: Arc<dyn Harness> = Arc::new(ClaudeHarness::default());
    let base = RunSpec {
        tools: tools(&[], &[]),
        ..c.spec()
    };
    let r = resume_or_restart(
        harness,
        &c.env,
        &base,
        &id,
        UserInput::text("Reply with only: ok"),
    )
    .await
    .expect("recover");
    c.fact("restarted", r.restarted);
    if must_restart {
        c.claim("--resume failed, so the same id restarted", r.restarted);
    }
    let mut l = Live::new(r.run);
    let t = l.terminal().await;
    c.claim("the thread takes a turn: not bricked", is_completed(&t));
    c.claim(
        "under the same session id",
        l.run.info().session.as_ref() == Some(&id),
    );
    if must_restart {
        let from = l.cursor;
        let t = l
            .turn("Did I ask you to remember a word? Reply yes or no only.")
            .await;
        let text = l.text_since(from).await.to_lowercase();
        c.claim(
            "the stopped prompt is lost",
            is_completed(&t) && text.contains("no"),
        );
    }
    l.close().await;
}

async fn already_in_use(c: &mut Ctx) {
    let id = SessionId::new(uuid::Uuid::new_v4().to_string());
    let spec = RunSpec {
        session: SessionMode::Assign { id: id.clone() },
        tools: tools(&[], &[]),
        ..c.spec()
    };
    let mut l = c.start(spec.clone()).await;
    let t = l.turn("Reply with only: ok").await;
    c.claim("the first run completed", is_completed(&t));
    l.close().await;
    let mut l = c.start(spec).await;
    let t = l.terminal().await;
    c.claim(
        "reusing the id says SessionInUse, Broken",
        matches!(
            &t.status, TurnStatus::Failed(f) if f.failure == FailureKind::SessionInUse
        ) && t.thread_disposition == ThreadDisposition::Broken,
    );
    l.close().await;
}

async fn queued_prompts(c: &mut Ctx) {
    let mut l = c
        .start(RunSpec {
            tools: tools(&[], &[]),
            ..c.spec()
        })
        .await;
    l.send("Count from 1 to 30, one number per line.").await;
    l.until("text", |e| {
        matches!(e.kind, AgentEventKind::TextDelta { .. }).then_some(())
    })
    .await;
    l.send("Reply with only: second").await;
    let a = l.terminal().await;
    let from = l.cursor;
    let b = l.terminal().await;
    let text = l.text_since(from).await;
    c.fact("second_text", text.chars().take(80).collect::<String>());
    c.claim(
        "each queued prompt gets its own terminal, in order",
        a.turn.0 == 1 && b.turn.0 == 2,
    );
    c.claim("both completed", is_completed(&a) && is_completed(&b));
    l.close().await;
}

async fn mcp(c: &mut Ctx) {
    // SAFETY: set before any spawn in this scenario, read by the resolver only.
    unsafe { std::env::set_var(MCP_TOKEN_VAR, MCP_TOKEN) };
    let server = c.file("server.py");
    let seen = c.file("seen.json");
    std::fs::write(
        &server,
        MCP_SERVER.replace("SEEN_PATH", &seen.to_string_lossy()),
    )
    .expect("server");
    let mut env = BTreeMap::new();
    env.insert(
        EnvName::new("DEMO_TOKEN"),
        EnvValue::secret(SecretRef::env(MCP_TOKEN_VAR)),
    );
    let spec = RunSpec {
        tools: tools(&[], &["mcp__demo__reveal"]),
        mcp: vec![McpServer {
            name: "demo".into(),
            transport: McpTransport::Stdio {
                command: "/usr/bin/python3".into(),
                args: vec!["-I".into(), server.to_string_lossy().to_string()],
                env,
            },
        }],
        ..c.spec()
    };
    let mut l = c.start(spec).await;
    let argv = Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &l.pid().to_string()])
        .output()
        .expect("ps");
    c.claim(
        "the token isn't on argv",
        !String::from_utf8_lossy(&argv.stdout).contains(MCP_TOKEN),
    );
    let t = l
        .turn("Call the tool mcp__demo__reveal, then reply done.")
        .await;
    c.claim("the turn completed", is_completed(&t));
    let reached = std::fs::read_to_string(&seen).unwrap_or_default();
    c.claim(
        "the server got the token through the control channel",
        reached.contains(&format!("\"token_len\": {}", MCP_TOKEN.len())),
    );
    l.close().await;
}

const MCP_SERVER: &str = r#"
import json, os, sys
for line in sys.stdin:
    m = json.loads(line)
    mid = m.get("id")
    if m.get("method") == "initialize":
        r = {"protocolVersion": m["params"].get("protocolVersion", "2024-11-05"), "capabilities": {"tools": {}}, "serverInfo": {"name": "demo", "version": "1"}}
    elif m.get("method") == "tools/list":
        r = {"tools": [{"name": "reveal", "description": "says hi", "inputSchema": {"type": "object", "properties": {}}}]}
    elif m.get("method") == "tools/call":
        open("SEEN_PATH", "w").write(json.dumps({"token_len": len(os.environ.get("DEMO_TOKEN", ""))}))
        r = {"content": [{"type": "text", "text": "hi"}]}
    elif mid is None:
        continue
    else:
        r = {}
    print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": r}), flush=True)
"#;

async fn minimum_env(c: &mut Ctx) {
    // Only USER: this is the floor for a claude.ai login on macOS.
    let mut spec = c.spec();
    spec.env.clear();
    let spec = spec.with_host_env(["USER"]);
    let mut l = c
        .start(RunSpec {
            tools: tools(&[], &[]),
            ..spec
        })
        .await;
    let t = l.turn("Reply with only: ok").await;
    c.claim("USER alone reaches the login", is_completed(&t));
    l.close().await;
}

async fn config_dir(c: &mut Ctx) {
    let harness = ClaudeHarness::new(ClaudeOptions {
        config_dir: Some(c.file("config").to_string_lossy().to_string()),
        ..ClaudeOptions::default()
    });
    let mut l = c
        .start_with(
            &harness,
            RunSpec {
                tools: tools(&[], &[]),
                ..c.spec()
            },
        )
        .await;
    let t = l.turn("Reply with only: ok").await;
    c.fact("status", format!("{:?}", t.status));
    c.claim(
        "a config dir loses the login, and the terminal says so",
        matches!(
            &t.status, TurnStatus::Failed(f) if f.failure == FailureKind::NotLoggedIn
        ) && t.thread_disposition == ThreadDisposition::Broken,
    );
    l.close().await;
}

/// Reads a run to the end, allowing what `allow` passes and denying the rest, and
/// closing input once `done` is true of what's been seen.
async fn drain(
    l: &mut Live,
    allow: impl Fn(&InputRequest) -> bool,
    done: impl Fn(&[AgentEvent]) -> bool,
) -> Vec<AgentEvent> {
    let mut events = l.run.events(EventSeq(0));
    let mut all = Vec::new();
    let mut closed = false;
    while let Ok(Some(e)) = tokio::time::timeout(WAIT, events.next()).await {
        if let AgentEventKind::InputRequested { id } = &e.kind {
            let q = l.run.inputs().expect("queue");
            let request = q.get(id).await.expect("request");
            let answer = if allow(&request) {
                InputAnswer::Allow {
                    updated_input: None,
                }
            } else {
                InputAnswer::Deny {
                    message: "not this one".into(),
                    stop_turn: false,
                }
            };
            let _ = q.answer(id, answer).await;
        }
        all.push(e);
        if !closed && done(&all) {
            closed = true;
            let _ = l.run.close_input().await;
        }
    }
    all
}

fn subagent_tools(c: &Ctx) -> RunSpec {
    RunSpec {
        tools: tools(&["Agent", "Bash"], &[]),
        ..c.spec()
    }
}

async fn subagent_approval(c: &mut Ctx) {
    let mut l = c.start(subagent_tools(c)).await;
    l.send(
        "Use your Agent tool once with run_in_background false, subagent_type general-purpose, \
         description 'marker'. Prompt it: 'Run this exact Bash command, then reply done: \
         touch subagent.marker'. Then reply with only the word finished.",
    )
    .await;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let all = drain(
        &mut l,
        move |r| {
            seen.lock().unwrap().push(r.clone());
            matches!(&r.kind, InputKind::ToolApproval { tool, .. }
                if tool.as_str() == "Bash" || tool.as_str() == "Agent")
        },
        |all| {
            all.iter()
                .any(|e| matches!(e.kind, AgentEventKind::TurnTerminal(_)))
        },
    )
    .await;
    let started: Vec<_> = all
        .iter()
        .filter_map(|e| match &e.kind {
            AgentEventKind::SubagentStarted { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let requests = requests.lock().unwrap().clone();
    let theirs: Vec<_> = requests.iter().filter(|r| r.agent.is_some()).collect();
    c.claim("a subagent started", !started.is_empty());
    c.claim(
        "the subagent's approval names it",
        theirs
            .iter()
            .any(|r| started.contains(r.agent.as_ref().unwrap())),
    );
    c.claim("the tool ran", c.file("subagent.marker").exists());
    c.claim(
        "the input.requested event names it too",
        all.iter()
            .any(|e| matches!(e.kind, AgentEventKind::InputRequested { .. }) && e.agent.is_some()),
    );
    c.fact("requests", requests.len());
}

async fn background_self_started(c: &mut Ctx) {
    let mut l = c.start(subagent_tools(c)).await;
    l.send(
        "Use your Agent tool once with run_in_background true, subagent_type general-purpose, \
         description 'bg'. Prompt it: 'Reply with only the word pong'. Do not wait for it. \
         Reply with only the word launched.",
    )
    .await;
    let all = drain(
        &mut l,
        |r| matches!(&r.kind, InputKind::ToolApproval { tool, .. } if tool.as_str() == "Agent"),
        |all| {
            let self_started = all.iter().find_map(|e| match e.kind {
                AgentEventKind::TurnStarted {
                    turn,
                    origin: TurnOrigin::TaskNotification,
                } => Some(turn),
                _ => None,
            });
            self_started.is_some_and(|t| {
                all.iter()
                    .any(|e| matches!(&e.kind, AgentEventKind::TurnTerminal(x) if x.turn == t))
            })
        },
    )
    .await;
    let mut started = vec![];
    let mut phantom = false;
    let mut ended = vec![];
    for e in &all {
        match &e.kind {
            AgentEventKind::TurnStarted { turn, origin } => started.push((*turn, *origin)),
            AgentEventKind::TurnTerminal(t) => {
                phantom |= !started.iter().any(|(s, _)| *s == t.turn);
                ended.push(t.turn);
            }
            _ => {}
        }
    }
    c.claim("no terminal lacks a start", !phantom);
    c.claim(
        "the self-started turn says so",
        started
            .iter()
            .any(|(_, o)| *o == TurnOrigin::TaskNotification),
    );
    c.claim("the prompt's turn ended once", {
        let first = started[0].0;
        ended.iter().filter(|t| **t == first).count() == 1
    });
    c.claim(
        "the subagent's frames are its own",
        all.iter()
            .filter(|e| {
                e.raw
                    .as_ref()
                    .is_some_and(|r| r.as_str().contains("\"agent_id\""))
            })
            .all(|e| e.agent.is_some()),
    );
    c.fact("turns", started.len());
}

macro_rules! scenarios {
    ($($name:literal => $f:ident),+ $(,)?) => {
        const NAMES: &[&str] = &[$($name),+];
        async fn dispatch(name: &str, c: &mut Ctx) {
            match name { $($name => $f(c).await,)+ _ => unreachable!() }
        }
    };
}

scenarios! {
    "streaming-second-turn" => streaming_second_turn,
    "approve-allow" => approve_allow,
    "approve-edited-input" => approve_edited,
    "deny" => deny,
    "deny-stop-turn" => deny_stop_turn,
    "question" => question,
    "interrupt-continue" => interrupt_continue,
    "interrupt-pending-approval" => pending_interrupt,
    "resume" => resume,
    "force-stop-ticking-tool" => force_stop,
    "stop-before-init" => stop_before_init,
    "stop-before-init-graceful" => stop_before_init_graceful,
    "session-already-in-use" => already_in_use,
    "queued-prompts" => queued_prompts,
    "mcp-over-control-channel" => mcp,
    "minimum-env" => minimum_env,
    "config-dir-loses-login" => config_dir,
    "subagent-approval" => subagent_approval,
    "background-self-started" => background_self_started,
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn cli_version() -> String {
    let out = Command::new("claude")
        .arg("--version")
        .output()
        .expect("claude");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({}))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(
        std::env::var("AGENTS_REAL").as_deref(),
        Ok("spend-tokens"),
        "spends tokens: set AGENTS_REAL=spend-tokens"
    );
    assert!(
        !std::env::vars().any(|(k, _)| k.starts_with("ANTHROPIC_")),
        "an API key route is set; these runs use the login"
    );
    let raw = PathBuf::from(arg(&args, "--raw").expect("--raw DIR"));
    let cwd = PathBuf::from(arg(&args, "--cwd").expect("--cwd DIR"));
    let model = arg(&args, "--model").unwrap_or_else(|| "haiku".into());
    let only = arg(&args, "--only");
    std::fs::create_dir_all(&raw).expect("raw dir");
    let version = cli_version();
    let meta_path = raw.join("run.json");
    let mut meta = read_json(&meta_path);
    meta["cli_version"] = json!(version);
    meta["model"] = json!(model);
    meta["env_given"] = json!(["USER", "PATH"]);
    let mut failed = 0;
    for name in NAMES {
        if let Some(only) = &only {
            if !only.split(',').any(|o| name.contains(o)) {
                continue;
            }
        }
        println!("== {name}");
        let dir = raw.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let run_cwd = cwd.join(name);
        std::fs::create_dir_all(&run_cwd).expect("cwd");
        let run_cwd = run_cwd.canonicalize().expect("cwd");
        let mut c = Ctx {
            model: model.clone(),
            cwd: run_cwd,
            env: RecordingEnvironment::new(LocalEnvironment, &dir).expect("record"),
            claims: Map::new(),
            facts: Map::new(),
        };
        let done = tokio::time::timeout(Duration::from_secs(240), dispatch(name, &mut c)).await;
        if done.is_err() {
            c.claim("the scenario finished in time", false);
        }
        let bad = c.claims.values().filter(|v| **v != json!(true)).count();
        failed += bad;
        let result = json!({"cli_version": version, "claims": c.claims, "facts": c.facts});
        std::fs::write(
            dir.join("result.json"),
            serde_json::to_string_pretty(&result).expect("json"),
        )
        .expect("result");
        meta["cases"][name] = result;
        std::fs::write(
            &meta_path,
            serde_json::to_string_pretty(&meta).expect("json"),
        )
        .expect("meta");
    }
    println!("{failed} claims failed");
    std::process::exit(i32::from(failed > 0));
}

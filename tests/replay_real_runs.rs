//! AGENTS-10: the driver, replaying its own recorded real runs. Recordings come
//! from `examples/claude_real.rs` on the logged-in account, scrubbed.

use futures::future::BoxFuture;
use futures::StreamExt;
use plexus_agents::runner::launch;
use plexus_agents::{ClaudeHarness, ClaudeOptions, SUPPORTED_CLI_VERSIONS};
use plexus_harness::conformance::replay::{
    fixture_meta, ExitTrigger, ReplayEnvironment, ReplayScript,
};
use plexus_harness::{
    resume_or_restart, AgentEvent, AgentEventKind, CancelReason, Environment, EventSeq, ExitInfo,
    FailureKind, Harness, InputAnswer, InputError, InputFilter, InputOutcome, InputState,
    InterruptOutcome, LaunchPlan, Process, RunEndReason, RunSpec, SessionId, Signal, SpawnError,
    StopCause, StopMode, ThreadDisposition, TurnStatus, TurnTerminal, UserInput,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn set() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/recorded/agents-10-2026-11-01")
}

fn exit_of(dir: &Path, phase: &str) -> ExitInfo {
    let text = std::fs::read_to_string(dir.join(format!("{phase}.exit"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn script(case: &str, phase: &str, trigger: ExitTrigger) -> ReplayScript {
    let dir = set().join(case);
    ReplayScript::load(&dir, phase, exit_of(&dir, phase), trigger, 0).unwrap()
}

/// Each spawn plays the next recording.
struct Sequence {
    steps: Vec<ReplayEnvironment>,
    next: AtomicUsize,
}

impl Sequence {
    fn new(scripts: Vec<ReplayScript>) -> Self {
        let steps = scripts.into_iter().map(ReplayEnvironment::new).collect();
        Self {
            steps,
            next: AtomicUsize::new(0),
        }
    }
}

impl Environment for Sequence {
    fn name(&self) -> &str {
        "sequence"
    }
    fn spawn(&self, plan: LaunchPlan) -> BoxFuture<'static, Result<Process, SpawnError>> {
        self.steps[self.next.fetch_add(1, Ordering::SeqCst)].spawn(plan)
    }
}

fn terminal_of(e: &AgentEvent) -> Option<TurnTerminal> {
    match &e.kind {
        AgentEventKind::TurnTerminal(t) => Some(t.clone()),
        _ => None,
    }
}

async fn all(run: &Arc<dyn plexus_harness::Run>) -> Vec<AgentEvent> {
    run.events(EventSeq(0)).collect().await
}

async fn next_terminal(
    run: &Arc<dyn plexus_harness::Run>,
    since: EventSeq,
) -> (TurnTerminal, EventSeq) {
    let mut s = run.events(since);
    while let Some(e) = s.next().await {
        if let Some(t) = terminal_of(&e) {
            return (t, e.seq);
        }
    }
    panic!("no terminal");
}

#[test]
fn every_recorded_claim_held_on_a_pinned_cli() {
    let meta = fixture_meta(&set()).unwrap();
    let version = meta.cli_version.split_whitespace().next().unwrap();
    assert!(SUPPORTED_CLI_VERSIONS.contains(&version), "{meta:?}");
    assert_eq!(meta.model, "haiku");
    let text = std::fs::read_to_string(set().join("run.json")).unwrap();
    let run: Value = serde_json::from_str(&text).unwrap();
    let cases = run["cases"].as_object().unwrap();
    assert_eq!(cases.len(), 17);
    for (name, case) in cases {
        let claims = case["claims"].as_object().unwrap();
        assert!(!claims.is_empty(), "{name}");
        assert!(
            claims.values().all(|v| *v == Value::Bool(true)),
            "{name}: {claims:?}"
        );
    }
    assert_eq!(
        cases["streaming-second-turn"]["facts"]["init_permission_mode"],
        "default"
    );
}

#[test]
fn every_recorded_process_ran_in_permission_mode_default() {
    let mut inits = 0;
    for case in std::fs::read_dir(set())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
    {
        for f in std::fs::read_dir(&case).unwrap().map(|e| e.unwrap().path()) {
            if f.extension().is_none_or(|e| e != "stdout") {
                continue;
            }
            for line in std::fs::read_to_string(&f).unwrap().lines() {
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if v["type"] == "system" && v["subtype"] == "init" {
                    assert_eq!(v["permissionMode"], "default", "{}", f.display());
                    inits += 1;
                }
            }
        }
    }
    assert!(inits >= 17, "{inits}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_before_init_is_not_a_brick() {
    // Recorded: a kill before system/init, `--resume` failing on stdout, then the
    // same id starting again.
    let env = Sequence::new(vec![
        script("stop-before-init", "2-claude", ExitTrigger::AfterLastFrame),
        script("stop-before-init", "3-claude", ExitTrigger::OnInputClose),
    ]);
    let id = SessionId::new("6b5fcdac-47b3-49a8-a9a7-22fe168aa02e");
    let harness: Arc<dyn Harness> = Arc::new(ClaudeHarness::default());
    let r = resume_or_restart(
        harness,
        &env,
        &RunSpec::default(),
        &id,
        UserInput::text("hi"),
    )
    .await
    .unwrap();
    assert!(r.restarted, "a missing conversation restarts the same id");
    let (t, _) = next_terminal(&r.run, EventSeq(0)).await;
    assert_eq!(t.status, TurnStatus::Completed);
    r.run.close_input().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_failed_resume_is_read_as_a_missing_session_not_an_interrupt() {
    let env = ReplayEnvironment::new(script(
        "stop-before-init",
        "2-claude",
        ExitTrigger::AfterLastFrame,
    ));
    let spec = RunSpec::default();
    let run = launch(&ClaudeHarness::default(), &env, spec).await.unwrap();
    let (t, _) = next_terminal(&run, EventSeq(0)).await;
    let TurnStatus::Failed(f) = t.status else {
        panic!("{t:?}")
    };
    assert_eq!(f.failure, FailureKind::SessionNotFound);
    assert_eq!(t.thread_disposition, ThreadDisposition::Broken);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_that_works_does_not_restart() {
    let env = Sequence::new(vec![script(
        "resume",
        "2-claude",
        ExitTrigger::OnInputClose,
    )]);
    let id = SessionId::new("any");
    let harness: Arc<dyn Harness> = Arc::new(ClaudeHarness::default());
    let r = resume_or_restart(
        harness,
        &env,
        &RunSpec::default(),
        &id,
        UserInput::text("hi"),
    )
    .await
    .unwrap();
    assert!(!r.restarted);
    r.run.close_input().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_allow_resolves_the_request_and_the_turn() {
    let env = ReplayEnvironment::new(script(
        "approve-allow",
        "1-claude",
        ExitTrigger::OnInputClose,
    ));
    let lines = env.stdin.clone();
    let run = launch(&ClaudeHarness::default(), &env, RunSpec::default())
        .await
        .unwrap();
    run.send(UserInput::text("touch")).await.unwrap();
    let q = run.inputs().unwrap();
    let id = loop {
        if let Some(r) = q.list(InputFilter::pending()).await.into_iter().next() {
            break r.id;
        }
        tokio::task::yield_now().await;
    };
    q.answer(
        &id,
        InputAnswer::Allow {
            updated_input: None,
        },
    )
    .await
    .unwrap();
    let (t, _) = next_terminal(&run, EventSeq(0)).await;
    assert_eq!(t.status, TurnStatus::Completed);
    let answer = lines
        .lock()
        .unwrap()
        .iter()
        .find(|v| v["type"] == "control_response")
        .cloned()
        .unwrap();
    assert_eq!(answer["response"]["response"]["behavior"], "allow");
    assert!(answer["response"]["response"]["updatedInput"]["command"].is_string());
    run.close_input().await.unwrap();
    assert!(all(&run).await.iter().any(|e| matches!(
        &e.kind,
        AgentEventKind::InputResolved {
            outcome: InputOutcome::Allowed,
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_interrupt_cancels_the_pending_approval() {
    let env = ReplayEnvironment::new(script(
        "interrupt-pending-approval",
        "1-claude",
        ExitTrigger::OnInputClose,
    ));
    let run = launch(&ClaudeHarness::default(), &env, RunSpec::default())
        .await
        .unwrap();
    run.send(UserInput::text("one")).await.unwrap();
    let (first, at) = next_terminal(&run, EventSeq(0)).await;
    assert_eq!(first.status, TurnStatus::Completed);
    run.send(UserInput::text("two")).await.unwrap();
    let q = run.inputs().unwrap();
    let id = loop {
        if let Some(r) = q.list(InputFilter::pending()).await.into_iter().next() {
            break r.id;
        }
        tokio::task::yield_now().await;
    };
    run.interrupt().unwrap().interrupt().await.unwrap();
    let r = q.get(&id).await.unwrap();
    assert!(matches!(
        r.state,
        InputState::Cancelled {
            reason: CancelReason::Interrupted,
            ..
        }
    ));
    let late = q
        .answer(
            &id,
            InputAnswer::Allow {
                updated_input: None,
            },
        )
        .await;
    assert!(matches!(late, Err(InputError::AlreadyResolved { .. })));
    let (t, _) = next_terminal(&run, at).await;
    assert_eq!(t.status, TurnStatus::Interrupted);
    run.close_input().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_force_stop_kills_the_tree_and_says_force() {
    let env = ReplayEnvironment::new(script(
        "force-stop-ticking-tool",
        "1-claude",
        ExitTrigger::OnInputClose,
    ));
    let signals = env.signals.clone();
    let run = launch(&ClaudeHarness::default(), &env, RunSpec::default())
        .await
        .unwrap();
    run.send(UserInput::text("one")).await.unwrap();
    let (first, at) = next_terminal(&run, EventSeq(0)).await;
    assert_eq!(first.status, TurnStatus::Completed);
    run.send(UserInput::text("two")).await.unwrap();
    let report = run.stop(StopMode::Force).await.unwrap();
    assert!(report.forced);
    assert_eq!(*signals.lock().unwrap(), vec![Signal::KillTree]);
    let (t, _) = next_terminal(&run, at).await;
    assert_eq!(t.status, TurnStatus::Interrupted);
    assert_eq!(t.stopped_by, Some(StopCause::Force));
    assert_eq!(
        report.exit,
        Some(ExitInfo {
            code: None,
            signal: Some(9)
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_config_dir_run_says_not_logged_in() {
    let env = ReplayEnvironment::new(script(
        "config-dir-loses-login",
        "1-claude",
        ExitTrigger::OnInputClose,
    ));
    let run = launch(&ClaudeHarness::default(), &env, RunSpec::default())
        .await
        .unwrap();
    run.send(UserInput::text("hi")).await.unwrap();
    let (t, _) = next_terminal(&run, EventSeq(0)).await;
    let TurnStatus::Failed(f) = t.status else {
        panic!("{t:?}")
    };
    assert_eq!(f.failure, FailureKind::NotLoggedIn);
    assert_eq!(t.thread_disposition, ThreadDisposition::Broken);
    run.close_input().await.unwrap();
}

/// Frames of a real turn cut off before its `result`, so nothing ever ends it.
fn hung_turn() -> ReplayScript {
    let mut s = script(
        "streaming-second-turn",
        "1-claude",
        ExitTrigger::OnInputClose,
    );
    let cut = s
        .stdout
        .iter()
        .position(|l| l.contains("\"type\": \"result\""))
        .unwrap();
    s.stdout.truncate(cut);
    s.exit = ExitInfo {
        code: None,
        signal: Some(9),
    };
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_that_times_out_becomes_a_force_stop() {
    let env = ReplayEnvironment::new(hung_turn());
    let signals = env.signals.clone();
    let harness = ClaudeHarness::new(ClaudeOptions {
        interrupt_timeout: Duration::from_millis(100),
        ..ClaudeOptions::default()
    });
    let run = launch(&harness, &env, RunSpec::default()).await.unwrap();
    run.send(UserInput::text("hi")).await.unwrap();
    let outcome = run.interrupt().unwrap().interrupt().await.unwrap();
    assert_eq!(outcome, InterruptOutcome::Forced);
    assert_eq!(*signals.lock().unwrap(), vec![Signal::KillTree]);
    let (t, _) = next_terminal(&run, EventSeq(0)).await;
    assert_eq!(t.status, TurnStatus::Interrupted);
    assert_eq!(t.stopped_by, Some(StopCause::Interrupt));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graceful_stop_that_times_out_is_forced() {
    let script = ReplayScript {
        trigger: ExitTrigger::OnSignal,
        ..hung_turn()
    };
    let env = ReplayEnvironment::new(script);
    let signals = env.signals.clone();
    let harness = ClaudeHarness::new(ClaudeOptions {
        stop_timeout: Duration::from_millis(100),
        ..ClaudeOptions::default()
    });
    let run = launch(&harness, &env, RunSpec::default()).await.unwrap();
    let report = run.stop(StopMode::Graceful).await.unwrap();
    assert!(report.forced, "{report:?}");
    assert_eq!(report.cause, StopCause::Stop);
    assert_eq!(*signals.lock().unwrap(), vec![Signal::KillTree]);
}

fn with_init_field(field: &str, value: &str) -> ReplayScript {
    let mut s = script(
        "streaming-second-turn",
        "1-claude",
        ExitTrigger::OnInputClose,
    );
    for line in &mut s.stdout {
        let mut v: Value = serde_json::from_str(line).unwrap();
        if v["type"] == "system" && v["subtype"] == "init" {
            v[field] = Value::String(value.to_string());
            *line = v.to_string();
        }
    }
    s
}

async fn refused(script: ReplayScript) -> (TurnTerminal, Vec<Signal>) {
    let env = ReplayEnvironment::new(script);
    let signals = env.signals.clone();
    let run = launch(&ClaudeHarness::default(), &env, RunSpec::default())
        .await
        .unwrap();
    run.send(UserInput::text("hi")).await.unwrap();
    let (t, _) = next_terminal(&run, EventSeq(0)).await;
    let ended = all(&run).await;
    assert!(ended.iter().any(|e| matches!(
        e.kind,
        AgentEventKind::RunEnded {
            reason: RunEndReason::Refused,
            ..
        }
    )));
    let sent = signals.lock().unwrap().clone();
    (t, sent)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_version_outside_the_pin_is_refused_from_init() {
    let (t, signals) = refused(with_init_field("claude_code_version", "9.9.9")).await;
    let TurnStatus::Failed(f) = t.status else {
        panic!("{t:?}")
    };
    assert_eq!(f.failure, FailureKind::UnsupportedVersion);
    assert!(
        f.detail.contains("9.9.9") && f.detail.contains("2.1.316"),
        "{}",
        f.detail
    );
    assert_eq!(t.thread_disposition, ThreadDisposition::Broken);
    assert_eq!(signals, vec![Signal::KillTree]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cli_reporting_bypassed_permissions_is_refused() {
    let (t, signals) = refused(with_init_field("permissionMode", "bypassPermissions")).await;
    assert!(matches!(t.status, TurnStatus::Failed(_)), "{t:?}");
    assert_eq!(signals, vec![Signal::KillTree]);
}

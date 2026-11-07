//! Permission modes per thread, Claude's rule suggestions applied, and the grace a
//! request gets before its thread parks. Replays of real runs (CLI 2.1.316), as
//! `tests/runtime.rs` uses them, with what the driver wrote and the plans it made
//! kept for checking.

use futures::future::BoxFuture;
use futures::StreamExt;
use plexus_agents::mode::{self, PermissionMode};
use plexus_agents::runner::launch;
use plexus_agents::runtime::{
    Delivery, EventLog, GlobalSeq, ReadFilter, StoredEvent, Supervisor, SupervisorConfig, ThreadId,
    ThreadOptions, ThreadStatus,
};
use plexus_agents::ClaudeHarness;
use plexus_harness::conformance::replay::{ExitTrigger, ReplayEnvironment, ReplayScript};
use plexus_harness::{
    Actor, AgentEventKind, Environment, EventSeq, FailureKind, Harness, InputAnswer, InputError,
    InputRoute, LaunchPlan, ModelName, Park, ParkedRun, Process, Resume, RunSpec, SessionId,
    Signal, SpawnError, ToolName, TurnId, TurnStatus, UserInput,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SET_9: &str = "agents-9-2026-11-01";
const SET_10: &str = "agents-10-2026-11-01";
const SET_37: &str = "agents-37-can-use-tool-2026-11-01";

fn set(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/recorded")
        .join(name)
}

fn script(dir: PathBuf, killed: bool) -> ReplayScript {
    let exit = ReplayScript::exit_from_rc(0);
    let trigger = if killed {
        ExitTrigger::OnSignal
    } else {
        ExitTrigger::OnInputClose
    };
    ReplayScript::load(&dir, "1-claude", exit, trigger, 0).unwrap()
}

/// The recorded run that asks once and, answered live, finishes its turn.
fn live() -> ReplayScript {
    script(set(SET_37).join("live-without-park/svc1"), false)
}

/// A script with every line passed through `edit`.
fn edited(mut s: ReplayScript, edit: impl Fn(&str) -> String) -> ReplayScript {
    s.stdout = s.stdout.iter().map(|l| edit(l)).collect();
    s
}

/// What each spawned replay was planned with, wrote, and was sent.
#[derive(Default)]
struct Probe {
    plans: Mutex<Vec<LaunchPlan>>,
    stdin: Vec<Arc<Mutex<Vec<Value>>>>,
    signals: Vec<Arc<Mutex<Vec<Signal>>>>,
}

struct Sequence {
    steps: Vec<ReplayEnvironment>,
    next: Arc<AtomicUsize>,
    probe: Arc<Probe>,
}

impl Environment for Sequence {
    fn name(&self) -> &str {
        "sequence"
    }
    fn spawn(&self, plan: LaunchPlan) -> BoxFuture<'static, Result<Process, SpawnError>> {
        self.probe.plans.lock().unwrap().push(plan.clone());
        let at = self.next.fetch_add(1, Ordering::SeqCst);
        match self.steps.get(at) {
            Some(step) => step.spawn(plan),
            None => Box::pin(async { Err(SpawnError::Io("no more replays".into())) }),
        }
    }
}

struct Fixture {
    supervisor: Supervisor,
    spawned: Arc<AtomicUsize>,
    probe: Arc<Probe>,
    db: PathBuf,
    _dir: tempfile::TempDir,
}

async fn open(scripts: Vec<ReplayScript>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let steps: Vec<ReplayEnvironment> = scripts.into_iter().map(ReplayEnvironment::new).collect();
    let probe = Arc::new(Probe {
        plans: Mutex::default(),
        stdin: steps.iter().map(|s| s.stdin.clone()).collect(),
        signals: steps.iter().map(|s| s.signals.clone()).collect(),
    });
    let spawned = Arc::new(AtomicUsize::new(0));
    let env = Sequence {
        steps,
        next: spawned.clone(),
        probe: probe.clone(),
    };
    let db = dir.path().join("runtime.db");
    let (supervisor, _) = Supervisor::open(SupervisorConfig {
        db: db.clone(),
        environment: Arc::new(env),
        placements: Default::default(),
        harnesses: vec![Arc::new(ClaudeHarness::default())],
    })
    .await
    .unwrap();
    Fixture {
        supervisor,
        spawned,
        probe,
        db,
        _dir: dir,
    }
}

fn spec() -> RunSpec {
    RunSpec {
        cwd: Some("/recorded/cwd".into()),
        model: Some(ModelName::new("haiku")),
        tools: plexus_harness::ToolPolicy {
            enabled: Some(vec![ToolName::new("Bash")]),
            preapproved: vec![],
            denied: vec![],
        },
        ..RunSpec::default()
    }
}

fn thread_in(f: &Fixture, park: bool, spec: RunSpec) -> ThreadId {
    let options = ThreadOptions {
        park,
        ..ThreadOptions::default()
    };
    f.supervisor.create_thread("claude", spec, options).unwrap()
}

fn ada() -> Actor {
    Actor {
        id: "idp:ada".into(),
        name: Some("ada".into()),
    }
}

async fn until(what: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !what() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("timed out");
}

fn allow() -> InputAnswer {
    InputAnswer::Allow {
        updated_input: None,
    }
}

fn log(f: &Fixture, t: &ThreadId) -> Vec<StoredEvent> {
    let filter = ReadFilter {
        thread: Some(t.clone()),
        without_raw: false,
    };
    f.supervisor
        .log()
        .page(&filter, GlobalSeq(0), 100_000)
        .unwrap()
}

fn terminals(f: &Fixture, t: &ThreadId) -> usize {
    log(f, t)
        .iter()
        .filter(|e| matches!(e.event.kind, AgentEventKind::TurnTerminal(_)))
        .count()
}

fn argv_has(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2).any(|w| w[0] == flag && w[1] == value)
}

/// The thread's spec, as stored.
fn stored_spec(f: &Fixture, t: &ThreadId) -> RunSpec {
    let c = rusqlite::Connection::open(&f.db).unwrap();
    let text: String = c
        .query_row("SELECT spec FROM threads WHERE id = ?", [t.as_str()], |r| {
            r.get(0)
        })
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn every_plan_takes_the_threads_mode_as_a_flag_and_not_as_env() {
    let h = ClaudeHarness::default();
    let p = h.plan(&spec()).unwrap();
    assert!(argv_has(&p.argv, "--permission-mode", "default"));
    assert!(!p.argv.iter().any(|a| a.contains("dangerously")));
    for m in [PermissionMode::AcceptEdits, PermissionMode::Plan] {
        let p = h.plan(&mode::with_mode(spec(), m)).unwrap();
        assert!(argv_has(&p.argv, "--permission-mode", m.as_str()), "{m}");
        assert!(!p.argv.iter().any(|a| a.contains("dangerously")), "{m}");
        assert!(p.env.keys().all(|k| k.as_str() != mode::ENV_KEY), "{m}");
    }
    let bypass = mode::with_mode(spec(), PermissionMode::BypassPermissions);
    let p = h.plan(&bypass).unwrap();
    assert!(argv_has(&p.argv, "--permission-mode", "bypassPermissions"));
    assert!(p
        .argv
        .contains(&"--allow-dangerously-skip-permissions".to_string()));
    assert!(p.env.keys().all(|k| k.as_str() != mode::ENV_KEY));

    // A resume and an unpark are planned from the same spec, so they keep it.
    let planned = mode::with_mode(spec(), PermissionMode::Plan);
    let resumed = h.plan_resume(&planned, &SessionId::new("s")).unwrap();
    assert!(argv_has(&resumed.argv, "--permission-mode", "plan"));
    let parked = ParkedRun {
        harness: "claude".into(),
        session: SessionId::new("s"),
        cwd: None,
        turn: TurnId(1),
        spec: RunSpec {
            park: true,
            ..planned
        },
        calls: vec![],
        completed: vec![],
        parked_at_ms: 0,
    };
    let unparked = h.plan_unpark(&parked, &[]).unwrap();
    assert!(argv_has(&unparked.plan.argv, "--permission-mode", "plan"));
}

/// The CLI saying it bypasses permissions is refused unless the thread asked for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_cli_in_bypass_is_refused_unless_the_thread_asked_for_it() {
    let bypassing = || {
        let s = script(set(SET_10).join("minimum-env"), false);
        edited(s, |l| {
            l.replace(
                "\"permissionMode\": \"default\"",
                "\"permissionMode\": \"bypassPermissions\"",
            )
        })
    };
    for (asked, refused) in [
        (PermissionMode::Default, true),
        (PermissionMode::BypassPermissions, false),
    ] {
        let env = ReplayEnvironment::new(bypassing());
        let run = launch(
            &ClaudeHarness::default(),
            &env,
            mode::with_mode(spec(), asked),
        )
        .await
        .unwrap();
        run.send(UserInput::text("the recorded prompt"))
            .await
            .unwrap();
        let mut events = run.events(EventSeq(0));
        let status = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(e) = events.next().await {
                if let AgentEventKind::TurnTerminal(t) = e.kind {
                    return t.status;
                }
            }
            panic!("no terminal");
        })
        .await
        .unwrap();
        let protocol =
            matches!(&status, TurnStatus::Failed(f) if f.failure == FailureKind::Protocol);
        assert_eq!(protocol, refused, "{asked}: {status:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mode_set_on_a_thread_with_no_run_needs_no_event() {
    let f = open(vec![]).await;
    let t = thread_in(&f, true, spec());
    assert_eq!(
        f.supervisor.thread(&t).unwrap().mode,
        PermissionMode::Default
    );
    let v = f
        .supervisor
        .set_mode(&t, PermissionMode::AcceptEdits, Some(ada()))
        .await
        .unwrap();
    assert_eq!(v.mode, PermissionMode::AcceptEdits);
    assert_eq!(
        mode::mode_of(&stored_spec(&f, &t)),
        PermissionMode::AcceptEdits
    );
    assert!(log(&f, &t).is_empty());
    let listed = f.supervisor.threads().unwrap();
    assert_eq!(listed[0].mode, PermissionMode::AcceptEdits);
}

/// Between turns, the next prompt restarts the process in the new mode; the change
/// is in the log, with who made it.
#[tokio::test(flavor = "multi_thread")]
async fn a_mode_set_between_turns_restarts_the_process_in_it_at_the_next_prompt() {
    let f = open(vec![live(), live()]).await;
    let t = thread_in(&f, false, spec());
    f.supervisor
        .prompt(&t, UserInput::text("the recorded prompt"))
        .await
        .unwrap();
    until(|| !f.supervisor.thread(&t).unwrap().pending.is_empty()).await;
    let id = f.supervisor.thread(&t).unwrap().pending[0].clone();
    f.supervisor.answer(&id, allow()).await.unwrap();
    until(|| terminals(&f, &t) == 1).await;

    let v = f
        .supervisor
        .set_mode(&t, PermissionMode::Plan, Some(ada()))
        .await
        .unwrap();
    assert_eq!(v.mode, PermissionMode::Plan);
    assert!(f.supervisor.is_live(&t), "set_mode kills nothing");
    let changes: Vec<_> = log(&f, &t)
        .into_iter()
        .filter_map(|e| match &e.event.kind {
            AgentEventKind::Native { name } if name.starts_with("permission_mode/") => {
                Some((name.clone(), e.event.raw.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].0, "permission_mode/plan");
    let raw: Value = serde_json::from_str(changes[0].1.as_ref().unwrap().as_str()).unwrap();
    assert_eq!(raw["mode"], "plan");
    assert_eq!(raw["previous"], "default");
    assert_eq!(raw["by"]["id"], "idp:ada");

    // Setting it again is no change, and no event.
    f.supervisor
        .set_mode(&t, PermissionMode::Plan, Some(ada()))
        .await
        .unwrap();

    f.supervisor
        .prompt(&t, UserInput::text("again"))
        .await
        .unwrap();
    assert_eq!(f.spawned.load(Ordering::SeqCst), 2, "started again");
    let plans = f.probe.plans.lock().unwrap().clone();
    assert!(argv_has(&plans[0].argv, "--permission-mode", "default"));
    assert!(argv_has(&plans[1].argv, "--permission-mode", "plan"));
    assert!(plans[1].argv.contains(&"--resume".to_string()));
    let n = log(&f, &t)
        .iter()
        .filter(|e| matches!(&e.event.kind, AgentEventKind::Native { name } if name.starts_with("permission_mode/")))
        .count();
    assert_eq!(n, 1);
}

/// AGENTS-37's live run on a thread that parks, answered inside the grace: the
/// answer goes to the process that asked, which is never killed, and applies the
/// request's own suggestion.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_inside_the_grace_goes_live_and_applies_its_suggestion() {
    let f = open(vec![live()]).await;
    f.supervisor.set_park_grace(Duration::from_secs(60));
    let t = thread_in(&f, true, spec());
    f.supervisor
        .prompt(&t, UserInput::text("the recorded prompt"))
        .await
        .unwrap();
    until(|| !f.supervisor.thread(&t).unwrap().pending.is_empty()).await;
    let record = f.supervisor.pending().unwrap()[0].clone();
    assert_eq!(record.request.route, InputRoute::Live);
    let suggested = plexus_agents::permissions::suggestions(&record.request);
    assert_eq!(suggested.len(), 1, "kept on the request");
    assert_eq!(suggested[0]["destination"], "localSettings");

    let answered = f
        .supervisor
        .answer_applying(&t, &record.request.id, allow(), ada(), &[0], false)
        .await
        .unwrap();
    assert!(!answered.state.is_pending());
    until(|| terminals(&f, &t) == 1).await;

    assert_eq!(f.spawned.load(Ordering::SeqCst), 1, "no resume");
    assert!(
        f.probe.signals[0].lock().unwrap().is_empty(),
        "nothing killed"
    );
    assert!(f.supervisor.is_live(&t));
    let events = log(&f, &t);
    assert!(!events
        .iter()
        .any(|e| matches!(e.event.kind, AgentEventKind::TurnParked { .. })));
    assert_eq!(
        f.supervisor.records(&t).unwrap()[0].delivery,
        Delivery::Applied
    );
    // The control protocol's answer: allow, with the suggestion for the session.
    let stdin = f.probe.stdin[0].lock().unwrap().clone();
    let reply = stdin
        .iter()
        .find(|l| l["type"] == "control_response")
        .expect("answered on stdin");
    let body = &reply["response"]["response"];
    assert_eq!(body["behavior"], "allow");
    let updates = body["updatedPermissions"].as_array().unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["type"], "addRules");
    assert_eq!(updates[0]["destination"], "session");
    assert_eq!(updates[0]["rules"][0]["ruleContent"], "sh effect.sh live");
    // And the thread keeps the rule for its next process.
    let kept = stored_spec(&f, &t).tools.preapproved;
    assert_eq!(kept, vec!["Bash(sh effect.sh live)".to_string()]);
    let next = ClaudeHarness::default().plan(&stored_spec(&f, &t)).unwrap();
    assert!(argv_has(
        &next.argv,
        "--allowedTools",
        "Bash(sh effect.sh live)"
    ));
}

/// Applying suggestions is an allow's, of ones the request made, and a bypass is
/// for whoever may choose one.
#[tokio::test(flavor = "multi_thread")]
async fn applying_a_suggestion_is_refused_where_it_doesnt_fit() {
    // The same run, its one suggestion now a switch to bypass.
    let s = edited(live(), |l| {
        if !l.contains("permission_suggestions") {
            return l.to_string();
        }
        let mut v: Value = serde_json::from_str(l).unwrap();
        v["request"]["permission_suggestions"] = serde_json::json!([
            {"type": "setMode", "mode": "bypassPermissions", "destination": "session"},
            {"type": "setMode", "mode": "acceptEdits", "destination": "session"}]);
        v.to_string()
    });
    let f = open(vec![s]).await;
    let t = thread_in(&f, false, spec());
    f.supervisor
        .prompt(&t, UserInput::text("the recorded prompt"))
        .await
        .unwrap();
    until(|| !f.supervisor.thread(&t).unwrap().pending.is_empty()).await;
    let id = f.supervisor.thread(&t).unwrap().pending[0].clone();
    let deny = InputAnswer::Deny {
        message: "no".into(),
        stop_turn: false,
    };
    for (answer, apply, may) in [
        (allow(), vec![0], false),
        (allow(), vec![7], true),
        (deny, vec![1], true),
    ] {
        let r = f
            .supervisor
            .answer_applying(&t, &id, answer, ada(), &apply, may)
            .await;
        assert!(matches!(r, Err(InputError::AnswerMismatch { .. })), "{r:?}");
        assert_eq!(f.supervisor.thread(&t).unwrap().pending, vec![id.clone()]);
        assert_eq!(
            f.supervisor.thread(&t).unwrap().mode,
            PermissionMode::Default
        );
    }
    // A mode it may choose goes to the live process with the answer, and to the
    // thread, and the process isn't restarted for it.
    f.supervisor
        .answer_applying(&t, &id, allow(), ada(), &[1], false)
        .await
        .unwrap();
    until(|| terminals(&f, &t) == 1).await;
    assert_eq!(
        f.supervisor.thread(&t).unwrap().mode,
        PermissionMode::AcceptEdits
    );
    let changes = log(&f, &t)
        .iter()
        .filter(|e| matches!(&e.event.kind, AgentEventKind::Native { name } if name == "permission_mode/acceptEdits"))
        .count();
    assert_eq!(changes, 1);
    f.supervisor.prompt(&t, UserInput::text("again")).await.ok();
    assert_eq!(
        f.spawned.load(Ordering::SeqCst),
        1,
        "switched live, not restarted"
    );
}

/// AGENTS-9's parked run with a short grace: nobody answers, so after the grace the
/// request is rerouted, the process killed, and the answer resumes it as before.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_still_undecided_when_the_grace_runs_out_parks() {
    let dir = set(SET_9).join("restart-parked");
    let f = open(vec![
        script(dir.join("svc1"), true),
        script(dir.join("svc2"), false),
    ])
    .await;
    f.supervisor.set_park_grace(Duration::from_millis(1500));
    let t = thread_in(&f, true, spec());
    f.supervisor
        .prompt(&t, UserInput::text("the recorded prompt"))
        .await
        .unwrap();
    until(|| !f.supervisor.thread(&t).unwrap().pending.is_empty()).await;
    let first = f.supervisor.pending().unwrap()[0].clone();
    assert_eq!(first.request.route, InputRoute::Live, "alive at first");
    assert!(f.supervisor.is_live(&t));

    until(|| f.supervisor.thread(&t).unwrap().status == ThreadStatus::Parked).await;
    assert!(f.probe.signals[0]
        .lock()
        .unwrap()
        .contains(&Signal::KillTree));
    let rerouted: Vec<_> = log(&f, &t)
        .into_iter()
        .filter_map(|e| match e.event.kind {
            AgentEventKind::InputRerouted { id, route } => Some((id, route)),
            _ => None,
        })
        .collect();
    assert_eq!(
        rerouted,
        vec![(first.request.id.clone(), InputRoute::Parked)]
    );
    let now = f.supervisor.pending().unwrap()[0].clone();
    assert_eq!(now.request.route, InputRoute::Parked);

    f.supervisor
        .answer(&first.request.id, allow())
        .await
        .unwrap();
    until(|| terminals(&f, &t) == 1).await;
    assert_eq!(f.spawned.load(Ordering::SeqCst), 2, "resumed");
}

#[tokio::test(flavor = "multi_thread")]
async fn folders_are_the_roots_projects_and_recent_threads_inside_them() {
    let f = open(vec![]).await;
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().canonicalize().unwrap();
    for d in ["repo/.git", "crate", "plain"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(root.join("crate/Cargo.toml"), "").unwrap();
    let outside = tempfile::tempdir().unwrap();
    f.supervisor.set_roots(vec![root.clone()]);
    assert_eq!(f.supervisor.roots(), vec![root.clone()]);
    for cwd in [root.join("plain"), outside.path().to_path_buf()] {
        let s = RunSpec {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..spec()
        };
        thread_in(&f, true, s);
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    let recent = f.supervisor.recent_cwds(10).unwrap();
    assert_eq!(recent[0], outside.path().to_string_lossy());
    let got = f.supervisor.folders(None).unwrap();
    let names: Vec<_> = got
        .iter()
        .map(|g| (g.name.as_str(), g.recent, g.git))
        .collect();
    let root_name = root.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        names,
        vec![
            ("plain", true, false),
            (root_name, false, false),
            ("crate", false, false),
            ("repo", false, true)
        ]
    );
    assert!(f.supervisor.folders(Some(outside.path())).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thread_from_before_modes_reads_as_default() {
    let f = open(vec![]).await;
    let t = thread_in(&f, true, spec());
    // As an older service stored it: no mode anywhere in the spec.
    assert!(!stored_spec(&f, &t)
        .env
        .keys()
        .any(|k| k.as_str() == mode::ENV_KEY));
    assert_eq!(
        f.supervisor.thread(&t).unwrap().mode,
        PermissionMode::Default
    );
}

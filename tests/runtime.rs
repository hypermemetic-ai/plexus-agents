//! AGENTS-9: the runtime over replays of real runs. Everything here came from the
//! CLI: `scripts/runtime-real` for AGENTS-9's set, `claude_real` for AGENTS-37's.

use futures::future::BoxFuture;
use futures::StreamExt;
use plexus_agents::runtime::{
    Delivery, EventLog, GlobalSeq, ReadFilter, RuntimeError, Store, Supervisor, SupervisorConfig,
    ThreadId, ThreadOptions, ThreadStatus,
};
use plexus_agents::ClaudeHarness;
use plexus_harness::conformance::replay::{ExitTrigger, ReplayEnvironment, ReplayScript};
use plexus_harness::{
    AgentEvent, AgentEventKind, Environment, EventSeq, InputAnswer, InputFilter, InputRoute,
    LaunchPlan, ModelName, ParkAnswer, Process, RawFrame, RunId, RunSpec, SpawnError, ToolName,
    UserInput,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SET_9: &str = "agents-9-2026-11-01";
const SET_37: &str = "agents-37-2026-11-01";

fn set(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/recorded")
        .join(name)
}

fn phase(dir: PathBuf, name: &str, unprompted: bool) -> ReplayScript {
    let exit = ReplayScript::exit_from_rc(0);
    let mut s = ReplayScript::load(&dir, name, exit, ExitTrigger::OnInputClose, 0).unwrap();
    s.unprompted = unprompted;
    s
}

struct Sequence {
    steps: Vec<ReplayEnvironment>,
    next: Arc<AtomicUsize>,
}

impl Environment for Sequence {
    fn name(&self) -> &str {
        "sequence"
    }
    fn spawn(&self, plan: LaunchPlan) -> BoxFuture<'static, Result<Process, SpawnError>> {
        self.steps[self.next.fetch_add(1, Ordering::SeqCst)].spawn(plan)
    }
}

struct Fixture {
    supervisor: Supervisor,
    spawned: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}

async fn open(scripts: Vec<ReplayScript>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let spawned = Arc::new(AtomicUsize::new(0));
    let env = Sequence {
        steps: scripts.into_iter().map(ReplayEnvironment::new).collect(),
        next: spawned.clone(),
    };
    let (supervisor, recovery) = Supervisor::open(SupervisorConfig {
        db: dir.path().join("runtime.db"),
        state_root: dir.path().join("state").to_string_lossy().to_string(),
        environment: Arc::new(env),
        harnesses: vec![Arc::new(ClaudeHarness::default())],
    })
    .await
    .unwrap();
    assert!(recovery.interrupted.is_empty());
    Fixture {
        supervisor,
        spawned,
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

fn thread(f: &Fixture, park: bool) -> ThreadId {
    f.supervisor
        .create_thread("claude", spec(), ThreadOptions { park })
        .unwrap()
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

fn parked_replays() -> Vec<ReplayScript> {
    let dir = set(SET_9).join("restart-parked");
    vec![
        phase(dir.join("svc1"), "1-claude", false),
        phase(dir.join("svc2"), "1-claude", true),
    ]
}

async fn parked(f: &Fixture) -> ThreadId {
    let t = thread(f, true);
    f.supervisor
        .prompt(&t, UserInput::text("the recorded prompt"))
        .await
        .unwrap();
    until(|| f.supervisor.thread(&t).unwrap().status == ThreadStatus::Parked).await;
    t
}

fn terminals(f: &Fixture, t: &ThreadId) -> usize {
    let filter = ReadFilter {
        thread: Some(t.clone()),
        without_raw: true,
    };
    let log = f.supervisor.log();
    let page = log.page(&filter, GlobalSeq(0), 100_000).unwrap();
    page.iter()
        .filter(|e| matches!(e.event.kind, AgentEventKind::TurnTerminal(_)))
        .count()
}

#[test]
fn every_real_runtime_run_held_its_claims() {
    let path = set(SET_9).join("run.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cases = v["cases"].as_object().unwrap();
    assert_eq!(cases.len(), 4);
    for (name, case) in cases {
        let claims = case["claims"].as_object().unwrap();
        assert!(claims.len() >= 4, "{name}");
        for (claim, ok) in claims {
            assert_eq!(ok, true, "{name}: {claim}");
        }
    }
    assert!(v["cli_version"].as_str().unwrap().starts_with("2.1.316"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_is_refused_while_a_request_is_undecided() {
    let f = open(parked_replays()).await;
    let t = parked(&f).await;
    let pending = f.supervisor.thread(&t).unwrap().pending;
    assert_eq!(pending.len(), 1);
    let refused = f
        .supervisor
        .prompt(&t, UserInput::text("something else"))
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, RuntimeError::Undecided { requests, .. } if *requests == pending),
        "{refused:?}"
    );
    // Refused and nothing else: no process, and the request is still answerable.
    assert_eq!(f.spawned.load(Ordering::SeqCst), 1);
    assert_eq!(f.supervisor.thread(&t).unwrap().pending, pending);

    // The way out that isn't an answer: cancel it, and the thread takes prompts.
    f.supervisor.interrupt(&t).await.unwrap();
    assert!(f.supervisor.thread(&t).unwrap().pending.is_empty());
    assert_eq!(f.supervisor.thread(&t).unwrap().status, ThreadStatus::Idle);
    assert_eq!(terminals(&f, &t), 1);
    assert_eq!(
        f.supervisor.records(&t).unwrap()[0].delivery,
        Delivery::Cancelled
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_replays_from_since_across_a_resume() {
    let f = open(parked_replays()).await;
    let t = parked(&f).await;
    let cursor = f.supervisor.thread(&t).unwrap().last_seq;
    let pending = f.supervisor.thread(&t).unwrap().pending;

    // Someone who was connected up to the park reads from there, across the resume.
    let filter = ReadFilter {
        thread: Some(t.clone()),
        without_raw: false,
    };
    let reader = f.supervisor.read(filter.clone(), cursor);
    f.supervisor
        .inputs(&t)
        .answer(&pending[0], allow())
        .await
        .unwrap();
    until(|| terminals(&f, &t) == 1).await;
    let tip = f.supervisor.thread(&t).unwrap().last_seq;
    let mut after = Vec::new();
    let mut reader = reader;
    while after
        .last()
        .map(|e: &plexus_agents::runtime::StoredEvent| e.seq)
        != Some(tip)
    {
        after.push(reader.next().await.unwrap());
    }
    assert_eq!(after.first().unwrap().seq, GlobalSeq(cursor.0 + 1));
    assert_eq!(after.last().unwrap().seq, tip);
    let runs: std::collections::BTreeSet<_> = after.iter().map(|e| e.run.clone()).collect();
    let view = f.supervisor.thread(&t).unwrap();
    assert_eq!(view.runs, 2, "the park and the resume");
    assert!(runs.len() <= 2);
    let (first, resumed) = (&after[0], after.last().unwrap());
    assert_ne!(first.run, resumed.run, "since crossed into the resumed run");
    for w in after.windows(2) {
        assert_eq!(w[1].seq.0, w[0].seq.0 + 1);
    }

    // The same cursor reads the same events again.
    let log = f.supervisor.log();
    let again = log.page(&filter, cursor, 100_000).unwrap();
    assert_eq!(again, after);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_going_away_stops_nothing_and_the_next_has_no_gaps() {
    let f = open(parked_replays()).await;
    let t = thread(&f, true);
    let filter = ReadFilter {
        thread: Some(t.clone()),
        without_raw: true,
    };
    let mut watcher = f.supervisor.read(filter.clone(), GlobalSeq(0));
    f.supervisor
        .prompt(&t, UserInput::text("the recorded prompt"))
        .await
        .unwrap();
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(watcher.next().await.unwrap().seq);
    }
    drop(watcher);

    // Nobody is watching. The run parks, and the request is answered.
    until(|| f.supervisor.thread(&t).unwrap().status == ThreadStatus::Parked).await;
    let pending = f.supervisor.thread(&t).unwrap().pending;
    f.supervisor
        .inputs(&t)
        .answer(&pending[0], allow())
        .await
        .unwrap();
    until(|| terminals(&f, &t) == 1).await;

    let tip = f.supervisor.thread(&t).unwrap().last_seq;
    let mut next = f.supervisor.read(filter, *seen.last().unwrap());
    while *seen.last().unwrap() != tip {
        seen.push(next.next().await.unwrap().seq);
    }
    let log = f.supervisor.log();
    let every: Vec<_> = log
        .page(
            &ReadFilter {
                thread: Some(t),
                without_raw: true,
            },
            GlobalSeq(0),
            100_000,
        )
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seen, every);
}

#[test]
fn raw_payloads_come_back_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("log.db")).unwrap();
    let (t, r) = (ThreadId::new("t"), RunId::new("r"));
    // Things a JSON round trip would change: key order, spacing, 1.0, escapes.
    let raws = [
        r#"{"b":1.0, "a":  2}"#,
        "{\"e\":\"\\u00e9 é\", \"n\":1e2,\t\"dup\":1,\"dup\":2}",
        "not json at all\u{2028}",
    ];
    let events: Vec<AgentEvent> = raws
        .iter()
        .enumerate()
        .map(|(i, raw)| AgentEvent {
            seq: EventSeq(i as u64 + 1),
            at_ms: 1,
            in_turn: None,
            agent: None,
            kind: AgentEventKind::TextDelta { text: "x".into() },
            raw: Some(RawFrame::new(*raw)),
        })
        .collect();
    store.append(&t, &r, &events).unwrap();
    let page = store
        .page(&ReadFilter::default(), GlobalSeq(0), 10)
        .unwrap();
    for (e, raw) in page.iter().zip(raws) {
        assert_eq!(
            e.event.raw.as_ref().unwrap().as_str().as_bytes(),
            raw.as_bytes()
        );
    }
    // Reopened from disk, where it counts.
    drop(store);
    let store = Store::open(&dir.path().join("log.db")).unwrap();
    let page = store
        .page(&ReadFilter::default(), GlobalSeq(0), 10)
        .unwrap();
    assert_eq!(page.len(), 3);
    assert_eq!(page[0].event.raw.as_ref().unwrap().as_str(), raws[0]);
    // A reader can leave them out.
    let filter = ReadFilter {
        thread: None,
        without_raw: true,
    };
    assert!(store.page(&filter, GlobalSeq(0), 10).unwrap()[0]
        .event
        .raw
        .is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn real_frames_come_back_byte_identical_through_a_run() {
    let f = open(parked_replays()).await;
    let t = parked(&f).await;
    let text =
        std::fs::read_to_string(set(SET_9).join("restart-parked/svc1/1-claude.stdout")).unwrap();
    let frames: std::collections::HashSet<&str> = text.lines().collect();
    let filter = ReadFilter {
        thread: Some(t),
        without_raw: false,
    };
    let page = f
        .supervisor
        .log()
        .page(&filter, GlobalSeq(0), 100_000)
        .unwrap();
    let raws: Vec<_> = page.iter().filter_map(|e| e.event.raw.as_ref()).collect();
    assert!(raws.len() > 5);
    for raw in raws {
        assert!(
            frames.contains(raw.as_str()),
            "{}",
            &raw.as_str()[..120.min(raw.as_str().len())]
        );
    }
}

/// Answers to a batch are all applied together, and only once all are in.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_waits_for_every_answer() {
    let dir = set(SET_37).join("park-batch");
    let f = open(vec![
        phase(dir.clone(), "1-claude", false),
        phase(dir, "2-claude", true),
    ])
    .await;
    let t = parked(&f).await;
    let pending = f.supervisor.thread(&t).unwrap().pending;
    assert!(pending.len() >= 2, "{pending:?}");
    let q = f.supervisor.inputs(&t);
    for (n, id) in pending.iter().enumerate() {
        assert_eq!(f.spawned.load(Ordering::SeqCst), 1, "still waiting at {n}");
        q.answer(id, allow()).await.unwrap();
    }
    assert_eq!(f.spawned.load(Ordering::SeqCst), 2);
    let again = q.answer(&pending[0], allow()).await;
    assert!(again.is_err(), "the first answer stands");
    let listed = q.list(InputFilter::default()).await;
    assert!(listed.iter().all(|r| r.route == InputRoute::Parked));
}

/// The real retry: the live request became a message, the agent called the tool
/// again, and the same answer took the new call because its input matched.
#[tokio::test(flavor = "multi_thread")]
async fn a_rerouted_answer_covers_an_exact_retry_and_nothing_else() {
    let dir = set(SET_9).join("live-pending-killed");
    let path = dir.join("svc2/1-claude.stdout");
    let frames = std::fs::read_to_string(path).unwrap();
    let asked = frames
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["request"]["subtype"] == "can_use_tool")
        .expect("the retry asked");
    let input = &asked["request"]["input"];
    let call = plexus_harness::ParkedCall {
        request: plexus_harness::InputRequestId::new("r"),
        tool_use_id: plexus_harness::ToolUseId::new("old"),
        tool: ToolName::new("Bash"),
        input: input.clone(),
        kind: plexus_harness::InputKind::Other { name: "x".into() },
        agent: None,
    };
    let prior = [ParkAnswer {
        call,
        answer: allow(),
    }];
    let bash = ToolName::new("Bash");
    assert!(plexus_harness::retry_answer(&prior, &bash, input).is_some());
    let mut changed = input.clone();
    changed["command"] = "sh something-else.sh".into();
    assert!(plexus_harness::retry_answer(&prior, &bash, &changed).is_none());
}

/// The narrowest window: `Delivered` committed, and the process's run row didn't.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_before_the_run_row_still_never_reapplies() {
    let f = open(parked_replays()).await;
    let t = parked(&f).await;
    f.supervisor
        .log()
        .tx(|tx| {
            tx.sql
                .execute("UPDATE inputs SET delivery = 'delivered'", [])?;
            Ok(())
        })
        .unwrap();
    let db = f._dir.path().join("runtime.db");
    let (again, recovery) = Supervisor::open(SupervisorConfig {
        db,
        state_root: "/unused".into(),
        environment: Arc::new(Sequence {
            steps: vec![],
            next: Arc::default(),
        }),
        harnesses: vec![Arc::new(ClaudeHarness::default())],
    })
    .await
    .unwrap();
    assert_eq!(recovery.outcome_unknown.len(), 1);
    let view = again.thread(&t).unwrap();
    assert_eq!(view.status, ThreadStatus::Parked);
    assert_eq!(view.outcome_unknown, recovery.outcome_unknown);
}

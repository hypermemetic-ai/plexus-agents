//! AGENTS-36: every frame of AGENTS-34's real recordings, replayed through the
//! driver, is attributed. The recordings came from real CLI 2.1.316; nothing here
//! is written by hand.
//!
//! Each case is played as it was run: `claude.stdin` says what to send and when.

use futures::StreamExt;
use plexus_agents::runner::launch;
use plexus_agents::ClaudeHarness;
use plexus_harness::conformance::replay::{ExitTrigger, ReplayEnvironment, ReplayScript};
use plexus_harness::{
    AgentEvent, AgentEventKind, EventSeq, InputAnswer, InputFilter, InterruptOutcome, ModelName,
    Run, RunSpec, SubagentId, TurnId, TurnOrigin, TurnStatus, UserInput,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const QUIET: Duration = Duration::from_millis(120);

fn set() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/recorded/agents-34-2026-11-01")
}

/// A case's stdin lines, as parsed JSON.
fn stdin_of(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("claude.stdin"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

struct Played {
    events: Vec<AgentEvent>,
    outcomes: Vec<InterruptOutcome>,
    run: Arc<dyn Run>,
}

/// Waits until the replay has nothing more to say without being told something.
async fn settle(seen: &Arc<Mutex<usize>>) {
    loop {
        let before = *seen.lock().unwrap();
        tokio::time::sleep(QUIET).await;
        if *seen.lock().unwrap() == before {
            return;
        }
    }
}

async fn play(case: &str) -> Played {
    let dir = set().join(case);
    let exit = ReplayScript::exit_from_rc(0);
    let script = ReplayScript::load(&dir, "claude", exit, ExitTrigger::OnInputClose, 0).unwrap();
    let env = ReplayEnvironment::new(script);
    let spec = RunSpec {
        model: Some(ModelName::new("haiku")),
        ..RunSpec::default()
    };
    let run = launch(&ClaudeHarness::default(), &env, spec).await.unwrap();

    // Answers exactly as many requests as the recording answered, in order, and
    // holds the rest, as a held approval's case needs.
    let stdin = stdin_of(&dir);
    let mut answers: Vec<Value> = stdin
        .iter()
        .filter(|l| l["type"] == "control_response")
        .map(|l| l["response"]["response"].clone())
        .collect();
    answers.reverse();
    let seen = Arc::new(Mutex::new(0usize));
    let reader = {
        let (run, seen) = (run.clone(), seen.clone());
        tokio::spawn(async move {
            let mut events = run.events(EventSeq(0));
            let mut all = Vec::new();
            while let Some(e) = events.next().await {
                if let AgentEventKind::InputRequested { id } = &e.kind {
                    if let Some(recorded) = answers.pop() {
                        let q = run.inputs().unwrap();
                        let a = if recorded["behavior"] == "allow" {
                            InputAnswer::Allow {
                                updated_input: None,
                            }
                        } else {
                            InputAnswer::Deny {
                                message: "denied in the recording".into(),
                                stop_turn: false,
                            }
                        };
                        let _ = q.answer(id, a).await;
                    }
                }
                *seen.lock().unwrap() += 1;
                all.push(e);
            }
            all
        })
    };

    let mut outcomes = Vec::new();
    for line in stdin {
        settle(&seen).await;
        if line["type"] == "user" {
            let text = line["message"]["content"][0]["text"].as_str().unwrap();
            run.send(UserInput::text(text)).await.unwrap();
        } else if line["type"] == "control_request" && line["request"]["subtype"] == "interrupt" {
            let i = run.interrupt().unwrap();
            outcomes.push(i.interrupt().await.unwrap());
        }
    }
    settle(&seen).await;
    run.close_input().await.unwrap();
    let events = tokio::time::timeout(Duration::from_secs(20), reader)
        .await
        .unwrap_or_else(|_| panic!("{case}: the replay didn't end"))
        .unwrap();
    Played {
        events,
        outcomes,
        run,
    }
}

/// Which subagent a frame says it belongs to, read from the frame alone.
fn claimed(f: &Value, tasks: &HashMap<String, Option<String>>) -> Option<String> {
    if let Some(a) = f["agent_id"].as_str() {
        return Some(a.to_string());
    }
    if let Some(a) = f["request"]["agent_id"].as_str() {
        return Some(a.to_string());
    }
    if f["type"] == "system" {
        if let Some(t) = f["task_id"].as_str() {
            return tasks.get(t).cloned().flatten();
        }
    }
    None
}

#[derive(Default)]
struct Tally {
    frames: usize,
    unattributed: Vec<String>,
}

fn audit(case: &str, events: &[AgentEvent], tally: &mut Tally) {
    // task_id to the subagent it counts as: itself, or the owner of a plain task.
    let mut tasks: HashMap<String, Option<String>> = HashMap::new();
    let mut seen_frames = HashSet::new();
    for e in events {
        let Some(raw) = &e.raw else { continue };
        let f = raw.json().expect("recorded frames are JSON");
        if seen_frames.insert(raw.as_str().to_string()) {
            tally.frames += 1;
        }
        if f["subtype"] == "task_started" {
            let owner = if f["task_type"] == "local_agent" {
                f["task_id"].as_str()
            } else {
                f["parent_task_id"].as_str()
            };
            tasks.insert(
                f["task_id"].as_str().unwrap().to_string(),
                owner.map(str::to_string),
            );
        }
        let wrong = |why: &str| format!("{case} seq {}: {why}: {:?}", e.seq, e.kind);
        let want = claimed(&f, &tasks).map(SubagentId::new);
        if e.agent != want {
            tally.unattributed.push(wrong("wrong agent"));
        }
        if !f["parent_tool_use_id"].is_null() && e.agent.is_none() {
            tally
                .unattributed
                .push(wrong("a subagent's frame reads as the root's"));
        }
        let turn_scoped = matches!(f["type"].as_str(), Some("assistant" | "user" | "result"))
            || f["type"] == "stream_event"
            || f["type"] == "control_request";
        if turn_scoped && e.in_turn.is_none() {
            tally.unattributed.push(wrong("no turn"));
        }
    }
}

fn cases() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(set())
        .unwrap()
        .map(|d| d.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with('s'))
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn every_frame_of_every_recording_is_attributed() {
    let mut tally = Tally::default();
    let mut cases_played = 0;
    for case in cases() {
        let dir = set().join(&case);
        let stdout = std::fs::read_to_string(dir.join("claude.stdout")).unwrap();
        let frames = stdout.lines().filter(|l| !l.trim().is_empty()).count();
        let played = play(&case).await;
        audit(&case, &played.events, &mut tally);
        let distinct: HashSet<&str> = played
            .events
            .iter()
            .filter_map(|e| e.raw.as_ref().map(|r| r.as_str()))
            .collect();
        // Identical frames collapse here, so this can only undercount.
        let unique: HashSet<String> = stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap().to_string())
            .collect();
        assert_eq!(distinct.len(), unique.len(), "{case}: {frames} frames");
        cases_played += 1;
    }
    println!(
        "{cases_played} cases, {} frames, {} unattributed",
        tally.frames,
        tally.unattributed.len()
    );
    assert_eq!(cases_played, 20);
    assert!(tally.frames > 1000, "{}", tally.frames);
    assert!(
        tally.unattributed.is_empty(),
        "{:#?}",
        &tally.unattributed[..tally.unattributed.len().min(20)]
    );
}

fn terminals_have_starts(events: &[AgentEvent]) {
    let mut started = Vec::new();
    for e in events {
        match &e.kind {
            AgentEventKind::TurnStarted { turn, .. } => started.push(*turn),
            AgentEventKind::TurnTerminal(t) => assert!(started.contains(&t.turn), "phantom"),
            _ => {}
        }
    }
}

fn origin_of(events: &[AgentEvent], turn: TurnId) -> TurnOrigin {
    events
        .iter()
        .find_map(|e| match e.kind {
            AgentEventKind::TurnStarted { turn: t, origin } if t == turn => Some(origin),
            _ => None,
        })
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_self_started_turn_is_not_a_phantom_and_ends_no_other_turn() {
    for case in ["s2b-parallel-default", "s6a-background-connected"] {
        let p = play(case).await;
        terminals_have_starts(&p.events);
        let terminals: Vec<_> = p
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                AgentEventKind::TurnTerminal(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(terminals.len(), 3, "{case}: 3 results for 1 prompt");
        assert_eq!(origin_of(&p.events, terminals[0].turn), TurnOrigin::Prompt);
        for t in &terminals[1..] {
            assert_eq!(
                origin_of(&p.events, t.turn),
                TurnOrigin::TaskNotification,
                "{case}"
            );
            assert_eq!(t.status, TurnStatus::Completed);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_frames_stay_in_the_turn_that_started_it() {
    let p = play("s2b-parallel-default").await;
    let spawned = p
        .events
        .iter()
        .find_map(|e| match &e.kind {
            AgentEventKind::SubagentStarted { .. } => e.in_turn,
            _ => None,
        })
        .unwrap();
    let theirs: Vec<_> = p.events.iter().filter(|e| e.agent.is_some()).collect();
    assert!(theirs.len() > 8);
    assert!(theirs.iter().all(|e| e.in_turn == Some(spawned)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_approval_names_it() {
    for case in ["s4a-approval-foreground", "s4b-approval-background"] {
        let p = play(case).await;
        let q = p.run.inputs().unwrap();
        let all = q.list(InputFilter::default()).await;
        assert!(!all.is_empty(), "{case}");
        let started: Vec<SubagentId> = p
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                AgentEventKind::SubagentStarted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        for r in all {
            assert!(
                r.agent.as_ref().is_some_and(|a| started.contains(a)),
                "{case}: {:?}",
                r.agent
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nested_approval_names_the_inner_subagent() {
    let p = play("s3-nested").await;
    let q = p.run.inputs().unwrap();
    let all = q.list(InputFilter::default()).await;
    let depth_two: Vec<SubagentId> = p
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            AgentEventKind::SubagentStarted { id, depth: 2, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert!(all
        .iter()
        .any(|r| r.agent.as_ref().is_some_and(|a| depth_two.contains(a))));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_local_agent_tasks_are_subagents_and_a_task_can_restart() {
    let p = play("s6a-background-connected").await;
    let plain = p
        .events
        .iter()
        .any(|e| matches!(&e.kind, AgentEventKind::BackgroundTaskStarted { .. }));
    assert!(plain, "the local_bash task stays a task");
    let restarts = p
        .events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                AgentEventKind::SubagentStarted { resumed: true, .. }
            )
        })
        .count();
    assert_eq!(restarts, 1, "trap 3: the same task_id started again");
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_reaches_background_tasks_between_turns() {
    for case in ["s5b-interrupt-default", "s5c-interrupt-background"] {
        let p = play(case).await;
        assert_eq!(
            p.outcomes,
            vec![InterruptOutcome::BackgroundStopped],
            "{case}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cost_is_reported_as_cumulative() {
    let p = play("s2b-parallel-default").await;
    let costs: Vec<f64> = p
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            AgentEventKind::Tokens { usage, .. } => usage.cumulative_cost_usd,
            _ => None,
        })
        .collect();
    assert_eq!(costs.len(), 3);
    assert!(costs.windows(2).all(|w| w[1] >= w[0]), "{costs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_is_in_the_stream_in_its_own_turn() {
    let p = play("s5b-interrupt-default").await;
    let prompts: Vec<_> = p
        .events
        .iter()
        .filter(|e| matches!(e.kind, AgentEventKind::UserMessage { .. }))
        .collect();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].in_turn, Some(TurnId(1)));
    assert_eq!(prompts[1].in_turn, Some(TurnId(2)));
}

/// The driver's own real runs, replayed, keep their claims.
#[test]
fn the_real_driver_runs_held_their_claims() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/recorded/agents-36-2026-11-01/run.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(v["cases"].as_object().unwrap().len(), 2);
    for (name, case) in v["cases"].as_object().unwrap() {
        for (claim, ok) in case["claims"].as_object().unwrap() {
            assert_eq!(ok, true, "{name}: {claim}");
        }
    }
}

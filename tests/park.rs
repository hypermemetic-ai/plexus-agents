//! AGENTS-37: parking through the driver, replayed from real runs of
//! `scripts/runtime-real` on CLI 2.1.316, and the park's plan.

use futures::StreamExt;
use plexus_agents::runner::{launch, pending};
use plexus_agents::{ClaudeHarness, ClaudeOptions};
use plexus_harness::conformance::replay::{ExitTrigger, ReplayEnvironment, ReplayScript};
use plexus_harness::{
    AgentEventKind, Environment, EventSeq, Harness, InputAnswer, InputKind, ModelName, Need, Park,
    ParkAnswer, ParkedRun, PlanError, Run, RunEndReason, RunSpec, Signal, ToolName, TurnOrigin,
    UserInput,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const SET: &str = "agents-37-can-use-tool-2026-11-01";

fn set(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/recorded")
        .join(name)
}

fn spec() -> RunSpec {
    RunSpec {
        cwd: Some("/parked/here".into()),
        model: Some(ModelName::new("haiku")),
        park: true,
        ..RunSpec::default()
    }
}

fn script(case: &str, phase: &str, trigger: ExitTrigger) -> ReplayScript {
    let exit = ReplayScript::exit_from_rc(0);
    ReplayScript::load(&set(SET).join(case).join("svc1"), phase, exit, trigger, 0).unwrap()
}

/// Runs a recorded first process up to its request, then parks it as the runtime
/// would. The recording ends in the kill, so only a signal ends the process.
async fn parked_in(case: &str) -> (Arc<dyn Run>, ReplayEnvironment, ParkedRun) {
    let env = ReplayEnvironment::new(script(case, "1-claude", ExitTrigger::OnSignal));
    let run = launch(&ClaudeHarness::default(), &env, spec())
        .await
        .unwrap();
    run.send(UserInput::text("the recorded prompt"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while pending(run.as_ref()).await.is_empty() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the recording asks");
    run.park().await.unwrap();
    let all: Vec<_> = run.events(EventSeq(0)).collect().await;
    let last = all.last().unwrap();
    assert!(
        matches!(
            last.kind,
            AgentEventKind::RunEnded {
                reason: RunEndReason::Parked,
                ..
            }
        ),
        "{:?}",
        last.kind
    );
    assert!(
        !all.iter()
            .any(|e| matches!(e.kind, AgentEventKind::TurnTerminal(_))),
        "a park isn't the end of a turn"
    );
    let parked = run.parked().expect("parked");
    (run, env, parked)
}

fn claims_hold(path: PathBuf, cases: usize) {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let got = v["cases"].as_object().unwrap();
    assert_eq!(got.len(), cases);
    for (name, case) in got {
        let claims = case["claims"].as_object().unwrap();
        assert!(!claims.is_empty(), "{name}");
        for (claim, ok) in claims {
            assert_eq!(ok, true, "{name}: {claim}");
        }
    }
    assert!(v["cli_version"].as_str().unwrap().starts_with("2.1.316"));
}

#[test]
fn every_real_park_run_held_its_claims() {
    claims_hold(set(SET).join("run.json"), 13);
}

/// What was learned about hooks against rules, kept though the hook has gone.
#[test]
fn the_hook_recordings_still_hold_their_claims() {
    claims_hold(set("agents-37-2026-11-01").join("run.json"), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_run_keeps_its_requests_and_needs_no_process() {
    let (run, env, parked) = parked_in("park-allow").await;
    assert_eq!(parked.calls.len(), 1);
    assert_eq!(parked.cwd.as_deref(), Some("/parked/here"));
    let q = run.inputs().unwrap();
    let r = q.get(&parked.calls[0].request).await.unwrap();
    assert!(
        r.state.is_pending(),
        "a park doesn't cancel what it waits on"
    );
    // Killed: closing stdin makes the CLI fail the request, and the model carries on.
    assert!(env.signals.lock().unwrap().contains(&Signal::KillTree));
}

#[tokio::test(flavor = "multi_thread")]
async fn parking_twice_is_parking_once() {
    let (run, env, parked) = parked_in("park-allow").await;
    run.park().await.unwrap();
    assert_eq!(run.parked().unwrap().parked_at_ms, parked.parked_at_ms);
    assert_eq!(env.signals.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_parks_as_a_question() {
    let (_, _, parked) = parked_in("park-question").await;
    assert!(matches!(parked.calls[0].kind, InputKind::Question { .. }));
    assert_eq!(parked.calls[0].tool, ToolName::new("AskUserQuestion"));
    assert!(parked.calls[0].input["questions"].is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_request_parks_with_the_subagent_named() {
    let (_, _, parked) = parked_in("park-subagent").await;
    assert!(parked.calls[0].agent.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpark_uses_the_parked_cwd_whatever_the_spec_says() {
    let (_, _, mut parked) = parked_in("park-allow").await;
    parked.spec.cwd = Some("/somewhere/else".into());
    let h = ClaudeHarness::default();
    let u = h.plan_unpark(&parked, &[]).unwrap();
    assert_eq!(u.plan.cwd.as_deref(), Some("/parked/here"));
    assert!(u.plan.needs.contains(&Need::ExistingDirectory {
        path: "/parked/here".into()
    }));
    let at = u.plan.argv.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(u.plan.argv[at + 1], parked.session.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_goes_in_as_text_and_leaves_nothing_on_disk() {
    let (_, _, parked) = parked_in("park-allow").await;
    let a = ParkAnswer {
        call: parked.calls[0].clone(),
        answer: InputAnswer::Allow {
            updated_input: None,
        },
    };
    let u = ClaudeHarness::default().plan_unpark(&parked, &[a]).unwrap();
    assert!(u.plan.files.is_empty());
    assert!(
        u.input.text.contains(&parked.calls[0].input.to_string()),
        "the retry can't match unless it's given the input: {}",
        u.input.text
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_for_a_call_that_isnt_pending_is_refused() {
    let (_, _, parked) = parked_in("park-allow").await;
    let mut call = parked.calls[0].clone();
    call.tool_use_id = plexus_harness::ToolUseId::new("toolu_other");
    let a = ParkAnswer {
        call,
        answer: InputAnswer::Allow {
            updated_input: None,
        },
    };
    assert!(matches!(
        ClaudeHarness::default().plan_unpark(&parked, &[a]),
        Err(PlanError::Conflict { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_of_the_wrong_kind_is_refused() {
    let (_, _, parked) = parked_in("park-allow").await;
    let a = ParkAnswer {
        call: parked.calls[0].clone(),
        answer: InputAnswer::Answers {
            answers: Default::default(),
        },
    };
    assert!(matches!(
        ClaudeHarness::default().plan_unpark(&parked, &[a]),
        Err(PlanError::Conflict { .. })
    ));
}

/// The bug this replaced: a hook sees a call before the rules do.
#[test]
fn parking_adds_nothing_to_the_cli_so_its_rules_stay_in_charge() {
    let parks = ClaudeHarness::default().plan(&spec()).unwrap();
    let plain = RunSpec {
        park: false,
        ..spec()
    };
    let not = ClaudeHarness::default().plan(&plain).unwrap();
    assert_eq!(parks.argv, not.argv);
    assert_eq!(parks.needs, not.needs);
    assert!(parks.files.is_empty());
    let sources = parks
        .argv
        .iter()
        .position(|a| a == "--setting-sources")
        .unwrap();
    assert_eq!(parks.argv[sources + 1], "", "the user's settings stay out");
    assert!(parks
        .argv
        .windows(2)
        .any(|w| w == ["--permission-mode", "default"]));
}

#[test]
fn rules_reach_the_cli_as_flags() {
    let mut s = spec();
    s.tools.denied = vec!["Bash(rm *)".into()];
    s.tools.preapproved = vec!["Bash(git:*)".into()];
    s.tools.enabled = Some(vec![ToolName::new("Bash")]);
    let p = ClaudeHarness::default().plan(&s).unwrap();
    assert!(p
        .argv
        .windows(2)
        .any(|w| w == ["--disallowedTools", "Bash(rm *)"]));
    assert!(p
        .argv
        .windows(2)
        .any(|w| w == ["--allowedTools", "Bash(git:*)"]));
}

#[test]
fn a_permissions_block_goes_inline_and_is_no_file() {
    let h = ClaudeHarness::new(ClaudeOptions {
        permissions: Some(json!({"ask": ["Bash(sh ask.sh)"]})),
        ..ClaudeOptions::default()
    });
    let p = h.plan(&spec()).unwrap();
    let at = p.argv.iter().position(|a| a == "--settings").unwrap();
    let v: Value = serde_json::from_str(&p.argv[at + 1]).unwrap();
    assert_eq!(v["permissions"]["ask"][0], "Bash(sh ask.sh)");
    assert!(p.files.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_run_carries_on_its_turn_with_what_its_fed() {
    let (_, _, parked) = parked_in("park-allow").await;
    let h = ClaudeHarness::default();
    let a = ParkAnswer {
        call: parked.calls[0].clone(),
        answer: InputAnswer::Allow {
            updated_input: None,
        },
    };
    let u = h.plan_unpark(&parked, &[a]).unwrap();
    let text = u.input.text.clone();
    let env = ReplayEnvironment::new(script("park-allow", "2-claude", ExitTrigger::OnInputClose));
    let process = env.spawn(u.plan).await.unwrap();
    let run = h.drive(u.spec, process).await.unwrap();
    let turn = run.send(u.input).await.unwrap();
    assert_eq!(turn, parked.turn, "the same turn, and no new one");
    // A second send is an ordinary prompt: only the first carries the turn on.
    let mut events = run.events(EventSeq(0));
    let mut origin = None;
    let mut terminal = None;
    let mut starts = 0;
    while let Some(e) = events.next().await {
        match e.kind {
            AgentEventKind::TurnStarted { turn, origin: o } => {
                starts += 1;
                origin = Some((turn, o));
            }
            AgentEventKind::InputRequested { id } => {
                let q = run.inputs().unwrap();
                let allow = InputAnswer::Allow {
                    updated_input: None,
                };
                q.answer(&id, allow).await.unwrap();
            }
            AgentEventKind::TurnTerminal(t) => {
                terminal = Some(t);
                run.close_input().await.unwrap();
            }
            _ => {}
        }
    }
    assert_eq!(starts, 1);
    assert_eq!(origin, Some((parked.turn, TurnOrigin::Resume)));
    assert_eq!(terminal.unwrap().turn, parked.turn);
    let stdin = env.stdin.lock().unwrap();
    assert_eq!(stdin[0]["message"]["content"][0]["text"], text.as_str());
}

#[test]
fn a_run_not_started_to_park_cant_be_unparked() {
    let parked = ParkedRun {
        harness: "claude".into(),
        session: plexus_harness::SessionId::new("s"),
        cwd: None,
        turn: plexus_harness::TurnId(1),
        spec: RunSpec::default(),
        calls: vec![],
        completed: vec![],
        parked_at_ms: 0,
    };
    assert!(matches!(
        ClaudeHarness::default().plan_unpark(&parked, &[]),
        Err(PlanError::Conflict { .. })
    ));
}

#[test]
fn the_harness_reports_it_can_park() {
    let c = ClaudeHarness::default().capabilities();
    assert_eq!(c.park.value, Some(true));
}

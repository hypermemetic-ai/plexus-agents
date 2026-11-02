//! AGENTS-37: replays of the driver's own real park runs, and the park's plan.
//! The recordings came from `examples/claude_real.rs` on real CLI 2.1.316.

use futures::StreamExt;
use plexus_agents::runner::launch;
use plexus_agents::ClaudeHarness;
use plexus_harness::conformance::replay::{ExitTrigger, ReplayEnvironment, ReplayScript};
use plexus_harness::{
    AgentEventKind, Environment, EventSeq, Harness, InputAnswer, InputKind, InputRoute, ModelName,
    Need, Park, ParkAnswer, ParkSpec, ParkedRun, PlanError, Run, RunEndReason, RunSpec, Signal,
    ToolName, TurnOrigin, UserInput,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn set() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/recorded/agents-37-2026-11-01")
}

fn spec() -> RunSpec {
    RunSpec {
        cwd: Some("/parked/here".into()),
        model: Some(ModelName::new("haiku")),
        park: Some(ParkSpec {
            state_dir: "/state/one".into(),
        }),
        ..RunSpec::default()
    }
}

fn script(case: &str, phase: &str, trigger: ExitTrigger) -> ReplayScript {
    let exit = ReplayScript::exit_from_rc(0);
    ReplayScript::load(&set().join(case), phase, exit, trigger, 0).unwrap()
}

async fn parked_in(case: &str) -> (Arc<dyn Run>, ReplayEnvironment, ParkedRun) {
    let env = ReplayEnvironment::new(script(case, "1-claude", ExitTrigger::OnInputClose));
    let run = launch(&ClaudeHarness::default(), &env, spec())
        .await
        .unwrap();
    run.send(UserInput::text("the recorded prompt"))
        .await
        .unwrap();
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
    let parked = run.parked().expect("parked");
    (run, env, parked)
}

#[test]
fn every_real_park_run_held_its_claims() {
    let path = set().join("run.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cases = v["cases"].as_object().unwrap();
    assert_eq!(cases.len(), 13);
    for (name, case) in cases {
        for (claim, ok) in case["claims"].as_object().unwrap() {
            assert_eq!(ok, true, "{name}: {claim}");
        }
    }
    assert!(v["cli_version"].as_str().unwrap().starts_with("2.1.316"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_run_keeps_its_requests_and_needs_no_process() {
    let (run, env, parked) = parked_in("park-allow").await;
    assert_eq!(parked.calls.len(), 1);
    assert_eq!(parked.cwd.as_deref(), Some("/parked/here"));
    assert_eq!(parked.reported.as_ref(), Some(&parked.calls[0].tool_use_id));
    let q = run.inputs().unwrap();
    let r = q.get(&parked.calls[0].request).await.unwrap();
    assert!(
        r.state.is_pending(),
        "a park doesn't cancel what it waits on"
    );
    assert_eq!(r.route, InputRoute::Parked);
    // The process went by itself, so nothing was signalled.
    assert!(!env.signals.lock().unwrap().contains(&Signal::KillTree));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_parks_with_every_call_though_result_names_one() {
    let (_, _, parked) = parked_in("park-batch").await;
    assert_eq!(parked.calls.len(), 2);
    assert!(parked.reported.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_parks_as_a_question() {
    let (_, _, parked) = parked_in("park-question").await;
    assert!(matches!(parked.calls[0].kind, InputKind::Question { .. }));
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
async fn an_answer_becomes_one_file_outside_the_cwd_and_nothing_else_changes() {
    let (_, _, parked) = parked_in("park-allow").await;
    let h = ClaudeHarness::default();
    let bare = h.plan_unpark(&parked, &[]).unwrap();
    let a = ParkAnswer {
        call: parked.calls[0].clone(),
        answer: InputAnswer::Allow {
            updated_input: None,
        },
    };
    let u = h.plan_unpark(&parked, &[a]).unwrap();
    assert_eq!(u.plan.files.len(), bare.plan.files.len() + 1);
    let file = u.plan.files.last().unwrap();
    assert!(
        file.path.starts_with("/state/one/answers/"),
        "{}",
        file.path
    );
    assert!(!u
        .plan
        .files
        .iter()
        .any(|f| f.path.contains(".claude") || f.path.starts_with("/parked/here")));
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

#[test]
fn a_state_dir_inside_the_cwd_is_refused() {
    // Answers there would be one tool call away from the agent.
    let spec = RunSpec {
        park: Some(ParkSpec {
            state_dir: "/parked/here/.state".into(),
        }),
        ..spec()
    };
    assert!(matches!(
        ClaudeHarness::default().plan(&spec),
        Err(PlanError::Conflict { .. })
    ));
}

#[test]
fn the_hook_reaches_the_cli_through_this_runs_settings_file_alone() {
    let p = ClaudeHarness::default().plan(&spec()).unwrap();
    let at = p.argv.iter().position(|a| a == "--settings").unwrap();
    assert_eq!(p.argv[at + 1], "/state/one/settings.json");
    let sources = p
        .argv
        .iter()
        .position(|a| a == "--setting-sources")
        .unwrap();
    assert_eq!(p.argv[sources + 1], "", "the user's settings stay out");
    assert!(p
        .argv
        .windows(2)
        .any(|w| w == ["--permission-mode", "default"]));
    assert!(p.files.iter().all(|f| f.path.starts_with("/state/one/")));
}

#[test]
fn a_run_that_doesnt_ask_to_park_gets_no_hook() {
    let spec = RunSpec {
        park: None,
        ..spec()
    };
    let p = ClaudeHarness::default().plan(&spec).unwrap();
    assert!(!p.argv.iter().any(|a| a == "--settings"));
    assert!(p.files.is_empty());
}

#[test]
fn rules_reach_the_cli_as_flags() {
    let mut s = spec();
    s.tools.denied = vec!["Bash(rm *)".into()];
    s.tools.enabled = Some(vec![ToolName::new("Bash")]);
    let p = ClaudeHarness::default().plan(&s).unwrap();
    assert!(p
        .argv
        .windows(2)
        .any(|w| w == ["--disallowedTools", "Bash(rm *)"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_run_opens_its_turn_with_no_prompt() {
    let (_, _, parked) = parked_in("park-allow").await;
    let h = ClaudeHarness::default();
    let u = h.plan_unpark(&parked, &[]).unwrap();
    let mut s = script("park-allow", "2-claude", ExitTrigger::OnInputClose);
    s.unprompted = true;
    let env = ReplayEnvironment::new(s);
    let process = env.spawn(u.plan).await.unwrap();
    let run = h.drive(u.spec, process).await.unwrap();
    let mut events = run.events(EventSeq(0));
    let mut origin = None;
    let mut terminal = None;
    while let Some(e) = events.next().await {
        match e.kind {
            AgentEventKind::TurnStarted { turn, origin: o } => origin = Some((turn, o)),
            AgentEventKind::TurnTerminal(t) => {
                terminal = Some(t);
                run.close_input().await.unwrap();
            }
            _ => {}
        }
    }
    assert_eq!(origin, Some((parked.turn, TurnOrigin::Resume)));
    assert_eq!(terminal.unwrap().turn, parked.turn);
    let stdin = env.stdin.lock().unwrap();
    assert!(stdin.is_empty(), "a resume sends no prompt: {stdin:?}");
}

#[test]
fn the_harness_reports_it_can_park() {
    let c = ClaudeHarness::default().capabilities();
    assert_eq!(c.park.value, Some(true));
}

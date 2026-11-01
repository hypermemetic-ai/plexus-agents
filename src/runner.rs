//! Drives any `Harness` with no service: the body of the example runner.

use futures::StreamExt;
use plexus_harness::{
    AgentEvent, AgentEventKind, Environment, EventSeq, Harness, InputAnswer, InputError,
    InputFilter, InputKind, InputRequest, Run, RunError, RunSpec, SpawnError, TurnTerminal,
    UserInput,
};
use std::fmt;
use std::sync::Arc;

/// Plan, spawn and drive.
pub async fn launch(
    harness: &dyn Harness,
    env: &dyn Environment,
    spec: RunSpec,
) -> Result<Arc<dyn Run>, LaunchError> {
    let plan = harness
        .plan(&spec)
        .map_err(|e| LaunchError(e.to_string()))?;
    let process = env.spawn(plan).await.map_err(LaunchError::from)?;
    harness
        .drive(spec, process)
        .await
        .map_err(|e| LaunchError(e.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchError(pub String);

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LaunchError {}

impl From<SpawnError> for LaunchError {
    fn from(e: SpawnError) -> Self {
        Self(e.to_string())
    }
}

/// What the script does when the agent asks for input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    Allow,
    Deny,
}

fn scripted(policy: ApprovalPolicy, request: &InputRequest) -> InputAnswer {
    match (policy, &request.kind) {
        (ApprovalPolicy::Allow, InputKind::ToolApproval { .. }) => InputAnswer::Allow {
            updated_input: None,
        },
        (_, InputKind::Other { name }) => InputAnswer::Refuse {
            reason: format!("no client handles {name}"),
        },
        _ => InputAnswer::Deny {
            message: "denied by the example runner".into(),
            stop_turn: false,
        },
    }
}

/// Sends each prompt, answers input by `policy`, and calls `on_event` per event.
/// Closes input after the last turn and returns each turn's terminal.
pub async fn run_script(
    run: Arc<dyn Run>,
    prompts: Vec<String>,
    policy: ApprovalPolicy,
    mut on_event: impl FnMut(&AgentEvent),
) -> Result<Vec<TurnTerminal>, RunError> {
    let mut events = run.events(EventSeq(0));
    let mut terminals = Vec::new();
    for prompt in prompts {
        run.send(UserInput::text(prompt)).await?;
        loop {
            let Some(e) = events.next().await else {
                return Err(RunError::Ended);
            };
            on_event(&e);
            match &e.kind {
                AgentEventKind::InputRequested { id } => {
                    let Some(q) = run.inputs() else { continue };
                    let Ok(r) = q.get(id).await else { continue };
                    match q.answer(id, scripted(policy, &r)).await {
                        Ok(_) | Err(InputError::AlreadyResolved { .. }) => {}
                        Err(err) => return Err(RunError::Io(err.to_string())),
                    }
                }
                AgentEventKind::TurnTerminal(t) => {
                    terminals.push(t.clone());
                    break;
                }
                _ => {}
            }
        }
    }
    run.close_input().await?;
    while let Some(e) = events.next().await {
        on_event(&e);
    }
    Ok(terminals)
}

/// Every request still pending on a run.
pub async fn pending(run: &dyn Run) -> Vec<InputRequest> {
    match run.inputs() {
        Some(q) => q.list(InputFilter::pending()).await,
        None => vec![],
    }
}

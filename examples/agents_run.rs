//! Drives any harness from the command line, with no service.
//!
//! ```text
//! agents_run --harness wire --prompt hi --env PATH -- /bin/sh script.sh
//! agents_run --harness claude --model haiku --cwd /tmp/x --prompt hi --prompt again
//!            [--deny] [--env NAME]... [--record DIR] [--session-id UUID] [--resume UUID]
//! ```
//! Prints one JSON event per line. A claude run names `USER`, or `PATH` for tools,
//! with `--env`: nothing else reaches it.

use plexus_agents::runner::{launch, run_script, ApprovalPolicy};
use plexus_agents::{LocalEnvironment, RecordingEnvironment};
use plexus_harness::conformance::wire::WireHarness;
use plexus_harness::{Environment, Harness, ModelName, RunSpec, SessionId, SessionMode, ToolName};
use std::sync::Arc;

#[derive(Default)]
struct Args {
    harness: String,
    prompts: Vec<String>,
    env: Vec<String>,
    deny: bool,
    record: Option<String>,
    spec: RunSpec,
    command: Vec<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--harness" => a.harness = value()?,
            "--prompt" => a.prompts.push(value()?),
            "--env" => a.env.push(value()?),
            "--cwd" => a.spec.cwd = Some(value()?),
            "--model" => a.spec.model = Some(ModelName::new(value()?)),
            "--record" => a.record = Some(value()?),
            "--deny" => a.deny = true,
            "--session-id" => {
                a.spec.session = SessionMode::Assign {
                    id: SessionId::new(value()?),
                }
            }
            "--resume" => {
                a.spec.session = SessionMode::Resume {
                    id: SessionId::new(value()?),
                }
            }
            "--tool" => a
                .spec
                .tools
                .enabled
                .get_or_insert_with(Vec::new)
                .push(ToolName::new(value()?)),
            "--allow-rule" => a.spec.tools.preapproved.push(value()?),
            "--" => {
                a.command = it.by_ref().collect();
                break;
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() {
    let a = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let harness: Arc<dyn Harness> = match a.harness.as_str() {
        "wire" => Arc::new(WireHarness::new(a.command)),
        other => {
            eprintln!("unknown harness {other}");
            std::process::exit(2);
        }
    };
    let env: Box<dyn Environment> = match a.record {
        Some(dir) => {
            Box::new(RecordingEnvironment::new(LocalEnvironment, dir).expect("record dir"))
        }
        None => Box::new(LocalEnvironment),
    };
    let names: Vec<&str> = a.env.iter().map(String::as_str).collect();
    let spec = a.spec.with_host_env(names);
    let run = match launch(harness.as_ref(), env.as_ref(), spec).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let policy = if a.deny {
        ApprovalPolicy::Deny
    } else {
        ApprovalPolicy::Allow
    };
    let out = run_script(run, a.prompts, policy, |e| {
        println!("{}", serde_json::to_string(e).expect("events serialize"));
    })
    .await;
    if let Err(e) = out {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

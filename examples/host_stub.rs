//! A stand-in host for `tests/host_wire.rs`: `plexus-agents host` as a process a test
//! can kill, with its runs replayed from a real one rather than real Claude. It
//! opens no port: it dials the service and serves its `agents` on that socket.
//!
//!     host_stub --server URL --name NAME --db FILE [--token-file F] [--key F]
//!               [--pins DIR] [--margin-secs N] [--fixture DIR] [--root DIR]
//!               [--allow-bypass]
//!
//! `--fixture` replays another parked run laid out as restart-parked is
//! (`svc1/`, `svc2/`): tests/ui_review_transcript.rs's scripted run.
//!
//! The pairing code and link go to stdout, as `plexus-agents host` prints them.

use futures::future::BoxFuture;
use plexus_agents::host::{self, HostConfig};
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::hub;
use plexus_agents::ClaudeHarness;
use plexus_harness::conformance::replay::{ExitTrigger, ReplayEnvironment, ReplayScript};
use plexus_harness::{Environment, Harness, LaunchPlan, Process, SpawnError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn phase(dir: PathBuf, killed: bool) -> ReplayScript {
    let trigger = if killed {
        ExitTrigger::OnSignal
    } else {
        ExitTrigger::OnInputClose
    };
    let exit = ReplayScript::exit_from_rc(0);
    ReplayScript::load(&dir, "1-claude", exit, trigger, 0).unwrap()
}

/// A real run that parks, and the resume that takes the answer.
struct Sequence(Vec<ReplayEnvironment>, AtomicUsize);

impl Environment for Sequence {
    fn name(&self) -> &str {
        "sequence"
    }
    fn spawn(&self, plan: LaunchPlan) -> BoxFuture<'static, Result<Process, SpawnError>> {
        self.0[self.1.fetch_add(1, Ordering::SeqCst)].spawn(plan)
    }
}

fn parked_run(fixture: Option<PathBuf>) -> Sequence {
    let dir = fixture.unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/recorded/agents-9-2026-11-01/restart-parked")
    });
    let steps = [
        phase(dir.join("svc1"), true),
        phase(dir.join("svc2"), false),
    ];
    Sequence(
        steps.into_iter().map(ReplayEnvironment::new).collect(),
        AtomicUsize::new(0),
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        let at = args.iter().position(|a| a == flag);
        at.map(|i| args[i + 1].clone())
    };
    let need = |flag: &str| value(flag).ok_or(anyhow::anyhow!("{flag} is required"));
    let mut config = HostConfig::new(&need("--server")?, &need("--name")?);
    config.token_file = value("--token-file").map(Into::into);
    config.key = value("--key").map(Into::into);
    config.pins = value("--pins").map(Into::into);
    config.backoff_max = Duration::from_millis(500);
    if let Some(m) = value("--margin-secs") {
        config.margin = Duration::from_secs(m.parse()?);
    }
    let env: Arc<dyn Environment> = Arc::new(parked_run(value("--fixture").map(Into::into)));
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: need("--db")?.into(),
        environment: env.clone(),
        placements: HashMap::from([("local".to_string(), env)]),
        harnesses: harnesses.clone(),
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(root) = value("--root") {
        runtime.set_roots(vec![root.into()]);
    }
    runtime.set_allow_bypass(args.iter().any(|a| a == "--allow-bypass"));
    host::run(hub(runtime, harnesses, vec![host::server_grant()]), config).await
}

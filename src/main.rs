//! `plexus-agents`: the `agents` service.
//!
//! ```text
//! plexus-agents --db FILE --issuer URL --audience AUD
//!               [--port 4470] [--bind 127.0.0.1] [--placement NAME]...
//!               [--grant PRINCIPAL=ROLE@SCOPE]... [--record DIR]
//! ```
//!
//! `--placement` names a place a thread can run; `local` if none is given. `--grant`
//! gives a role: `user@local` lets a principal start runs on `local`, `admin@*` lets
//! it do everything. A caller with no grant can see only its own threads, and has
//! none. There is no way to run with auth off.

use anyhow::{anyhow, Context};
use plexus_agents::auth::Grant;
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{self, TokenIssuer};
use plexus_agents::{ClaudeHarness, LocalEnvironment, RecordingEnvironment};
use plexus_harness::{Environment, Harness};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut db, mut issuer, mut audience, mut record) = (None, None, None, None);
    let (mut port, mut bind) = (4470u16, IpAddr::from([127, 0, 0, 1]));
    let (mut placements, mut grants) = (Vec::new(), Vec::<Grant>::new());
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => db = Some(value()?),
            "--issuer" => issuer = Some(value()?),
            "--audience" => audience = Some(value()?),
            "--record" => record = Some(value()?),
            "--port" => port = value()?.parse().context("--port")?,
            "--bind" => bind = value()?.parse().context("--bind")?,
            "--placement" => placements.push(value()?),
            "--grant" => grants.push(value()?.parse().map_err(|e: String| anyhow!(e))?),
            other => return Err(anyhow!("no flag {other}")),
        }
    }
    if placements.is_empty() {
        placements.push("local".to_string());
    }
    let tokens = TokenIssuer {
        issuer: issuer.context("--issuer is required: there's no running without auth")?,
        audience: audience.context("--audience is required: tokens must be for this service")?,
    };
    let environment: Arc<dyn Environment> = match record {
        Some(dir) => Arc::new(RecordingEnvironment::new(LocalEnvironment::new(), dir)?),
        None => Arc::new(LocalEnvironment::new()),
    };
    let places: HashMap<String, Arc<dyn Environment>> = placements
        .into_iter()
        .map(|p| (p, environment.clone()))
        .collect();
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, recovery) = Supervisor::open(SupervisorConfig {
        db: db.context("--db is required")?.into(),
        environment,
        placements: places,
        harnesses: harnesses.clone(),
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;
    eprintln!("plexus-agents: on {bind}:{port}, recovered {recovery:?}");
    let hub = service::hub(runtime, harnesses, grants);
    service::serve(hub, tokens.validator()?, bind, port).await
}

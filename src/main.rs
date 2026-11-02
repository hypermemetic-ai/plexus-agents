//! `plexus-agents`: the `agents` service.
//!
//! ```text
//! plexus-agents --db FILE [--issuer URL --audience AUD]
//!               [--port 4470] [--bind 127.0.0.1] [--placement NAME]...
//!               [--grant PRINCIPAL=ROLE@SCOPE]... [--record DIR]
//!               [--key-audience URL]... [--service-key FILE] [--peer NAME=URL]...
//! ```
//!
//! `--placement` names a place a thread can run; `local` if none is given. `--grant`
//! gives a role: `user@local` lets a principal start runs on `local`, `admin@*` lets
//! it do everything. A caller with no grant can see only its own threads, and has
//! none. There is no way to run with auth off.
//!
//! `--key-audience` admits key proofs naming that URL, and proofs naming this
//! service's own `ws://127.0.0.1:<port>` whenever one is given. A peer needs a
//! `--grant key:<hex>=admin@*` here before a proof gets it anything. `--peer` names a
//! service reachable at `agents.on.<name>`, dialed with `--service-key`, a 32-byte
//! Ed25519 seed in a file of mode 0600. `--print-key FILE` prints that key's principal
//! and exits, so a script can write the peer's `--grant`.

use anyhow::{anyhow, Context};
use plexus_agents::auth::Grant;
use plexus_agents::peer::{self, Peer, PeerConfig};
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{self, SignIn, TokenIssuer};
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
    let (mut key_audiences, mut service_key, mut peers) = (Vec::new(), None, Vec::new());
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
            "--key-audience" => key_audiences.push(value()?),
            "--service-key" => service_key = Some(value()?),
            "--peer" => peers.push(value()?),
            "--print-key" => {
                println!(
                    "{}",
                    peer::principal_of_file(value()?.as_ref()).map_err(|e| anyhow!(e))?
                );
                return Ok(());
            }
            other => return Err(anyhow!("no flag {other}")),
        }
    }
    if placements.is_empty() {
        placements.push("local".to_string());
    }
    let tokens = match (issuer, audience) {
        (Some(issuer), Some(audience)) => Some(TokenIssuer { issuer, audience }),
        (None, None) => None,
        _ => return Err(anyhow!("--issuer and --audience go together")),
    };
    if !key_audiences.is_empty() {
        key_audiences.push(format!("ws://{bind}:{port}"));
    }
    let sign_in = SignIn {
        tokens,
        key_audiences,
    };
    let validator = sign_in.validator()?;
    let peers = peers
        .iter()
        .map(|spec| {
            let key = service_key.as_ref().context("--peer needs --service-key")?;
            let config = PeerConfig::parse(spec, key).map_err(|e| anyhow!(e))?;
            Peer::new(config).map_err(|e| anyhow!(e))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
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
    let hub = service::hub_with_peers(runtime, harnesses, grants, peers);
    service::serve(hub, validator, bind, port).await
}

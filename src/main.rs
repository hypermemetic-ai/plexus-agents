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
//!
//! Hosts behind NAT dial in (SUPERCONDUCTOR-15) and are reached at
//! `agents.on.<host>`; `hosts.pair`, `hosts.unpair` and `hosts.list` manage them, in
//! the same `--db`. `--public-url` is the URL they dial: a host's key proof must
//! name it, and each call out is stamped with it.
//!
//! ```text
//! plexus-agents host --server URL --name NAME --db FILE
//!                    [--token-file FILE] [--key FILE [--pair]] [--placement NAME]...
//! ```
//!
//! runs a host: dials the service, attaches as `NAME`, and serves this machine's own
//! `agents` to the calls that come back on that socket. It opens no port. See
//! `plexus_agents::host`.

use anyhow::{anyhow, Context};
use plexus_agents::auth::Grant;
use plexus_agents::host::{self, HostConfig};
use plexus_agents::hosts::Hosts;
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
    if std::env::args().nth(1).as_deref() == Some("host") {
        return host().await;
    }
    let mut public_url = None;
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
            "--public-url" => public_url = Some(value()?),
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
    if let Some(url) = &public_url {
        key_audiences.push(url.clone());
    }
    if !key_audiences.is_empty() {
        key_audiences.push(format!("ws://{bind}:{port}"));
    }
    let public_url = public_url.unwrap_or_else(|| {
        key_audiences
            .first()
            .cloned()
            .unwrap_or(format!("ws://{bind}:{port}"))
    });
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
    let db: std::path::PathBuf = db.context("--db is required")?.into();
    let hosts = Hosts::open(&db, public_url)?;
    let (runtime, recovery) = Supervisor::open(SupervisorConfig {
        db,
        environment,
        placements: places,
        harnesses: harnesses.clone(),
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;
    eprintln!("plexus-agents: on {bind}:{port}, recovered {recovery:?}");
    let hub = service::hub_with_hosts(runtime, harnesses, grants, peers, Some(hosts.clone()));
    service::serve_with_hosts(hub, validator, bind, port, Some(hosts)).await
}

/// `plexus-agents host`: see the module docs.
async fn host() -> anyhow::Result<()> {
    let (mut server, mut name, mut db, mut record) = (None, None, None, None);
    let (mut token_file, mut key, mut pair) = (None, None, false);
    let mut placements = Vec::new();
    let mut it = std::env::args().skip(2);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--server" => server = Some(value()?),
            "--name" => name = Some(value()?),
            "--db" => db = Some(value()?),
            "--record" => record = Some(value()?),
            "--token-file" => token_file = Some(value()?.into()),
            "--key" => key = Some(value()?.into()),
            "--pair" => pair = true,
            "--placement" => placements.push(value()?),
            other => return Err(anyhow!("no flag {other}")),
        }
    }
    if placements.is_empty() {
        placements.push("local".to_string());
    }
    let mut config = HostConfig::new(
        &server.context("--server is required")?,
        &name.context("--name is required")?,
    );
    config.token_file = token_file;
    config.key = key;
    config.pair = pair;
    let environment: Arc<dyn Environment> = match record {
        Some(dir) => Arc::new(RecordingEnvironment::new(LocalEnvironment::new(), dir)?),
        None => Arc::new(LocalEnvironment::new()),
    };
    let places = placements
        .into_iter()
        .map(|p| (p, environment.clone()))
        .collect();
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: db.context("--db is required")?.into(),
        environment,
        placements: places,
        harnesses: harnesses.clone(),
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;
    let hub = service::hub(runtime, harnesses, vec![host::server_grant()]);
    host::run(hub, config).await
}

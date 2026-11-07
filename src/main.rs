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
//! `agents.on.<host>`, given `--server-key FILE`: this service's Ed25519 identity
//! (`key_s`, a 32-byte seed, mode 0600), which hosts pin and every call to a host is
//! signed with (SUPERCONDUCTOR-18). `--generate-server-key` makes that file if it's
//! missing: for development. Without `--server-key` there are no hosts. A host
//! pairs by a code both sides show; `hosts.pairing`, `hosts.confirm`,
//! `hosts.unpair` and `hosts.list` are a signed-in user's, in the same `--db`.
//! `--public-url` is the URL hosts dial: a host's key proof must name it.
//! `--no-local` serves no `local` placement when none is named: a hosted service
//! runs nothing itself. `--trusted-proxy CIDR` names a proxy whose
//! `X-Forwarded-For` is believed, for the per-address limit on pairing requests.
//!
//! ```text
//! plexus-agents host --server URL --name NAME --db FILE
//!                    [--token-file FILE] [--key FILE] [--pins DIR] [--app URL]
//!                    [--placement NAME]...
//! ```
//!
//! runs a host: dials the service, attaches as `NAME`, and serves this machine's own
//! `agents` to the calls that come back on that socket. It opens no port. With
//! `--token-file` it signs in as that token's user. Otherwise it signs in with its
//! key (`--key`, by default `~/.plexus/keys/NAME.key`, made if missing), and an
//! unpaired key prints a code and a link to confirm in the browser. Server keys are
//! pinned in `--pins` (default `~/.plexus/hosts`). See `plexus_agents::host`.

use anyhow::{anyhow, Context};
use plexus_agents::auth::Grant;
use plexus_agents::host::{self, HostConfig};
use plexus_agents::hosts::Hosts;
use plexus_agents::pairing::{self, ServerKey};
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
    let (mut server_key, mut generate_server_key, mut no_local) = (None, false, false);
    let mut trusted_proxies = Vec::new();
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
            "--server-key" => server_key = Some(value()?),
            "--generate-server-key" => generate_server_key = true,
            "--no-local" => no_local = true,
            "--trusted-proxy" => trusted_proxies.push(value()?),
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
    if placements.is_empty() && !no_local {
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
    let hosts = match &server_key {
        Some(file) => {
            let key = ServerKey::load(file.as_ref(), generate_server_key)?;
            eprintln!("plexus-agents: hosts pin this service as {}", key.public());
            Some(Hosts::open(&db, public_url, key)?)
        }
        None => {
            eprintln!("plexus-agents: no --server-key, so no hosts dial in");
            None
        }
    };
    let (runtime, recovery) = Supervisor::open(SupervisorConfig {
        db,
        environment,
        placements: places,
        harnesses: harnesses.clone(),
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;
    eprintln!("plexus-agents: on {bind}:{port}, recovered {recovery:?}");
    let hub = service::hub_with_hosts(runtime, harnesses, grants, peers, hosts.clone());
    let serving = service::Serving {
        hosts,
        trusted_proxies,
        ..service::Serving::new(bind, port)
    };
    service::serve_with(hub, validator, serving).await
}

/// `plexus-agents host`: see the module docs.
async fn host() -> anyhow::Result<()> {
    let (mut server, mut name, mut db, mut record) = (None, None, None, None);
    let (mut token_file, mut key, mut pins, mut app) = (None, None, None, None);
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
            "--pins" => pins = Some(value()?.into()),
            "--app" => app = Some(value()?),
            "--placement" => placements.push(value()?),
            other => return Err(anyhow!("no flag {other}")),
        }
    }
    if placements.is_empty() {
        placements.push("local".to_string());
    }
    let name = name.context("--name is required")?;
    let mut config = HostConfig::new(&server.context("--server is required")?, &name);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let plexus = || home.clone().map(|h| h.join(".plexus"));
    if token_file.is_none() {
        let key = match key {
            Some(k) => k,
            None => plexus()
                .context("no HOME: give --key")?
                .join(format!("keys/{name}.key")),
        };
        if !key.exists() {
            pairing::write_new_seed(&key)?;
            eprintln!("plexus-agents host: made a key at {}", key.display());
        }
        config.key = Some(key);
    }
    config.token_file = token_file;
    config.pins = pins.or_else(|| plexus().map(|p| p.join("hosts")));
    config.app = app;
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

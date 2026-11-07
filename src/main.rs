//! `plexus-agents`: the `agents` service, and the superconductor (SUPERCONDUCTOR-11):
//! plexus-ui linked in the same process, its shell and bundle on the same port.
//!
//! ```text
//! plexus-agents --db FILE [--port 4470] [--bind 127.0.0.1] [--public-url URL]
//!               [--ui-db FILE] [--ui-signer HEX]... [--allow RULE]...
//!               [--placement NAME]... [--no-local] [--grant PRINCIPAL=ROLE@SCOPE]...
//!               [--admin PRINCIPAL]... [--record DIR]
//!               [--workos-client CLIENT [--workos-api-key-file FILE]] [--identity SPEC]
//!               [--issuer URL --audience AUD] [--key-audience URL]...
//!               [--server-key FILE [--generate-server-key]] [--trusted-proxy CIDR]...
//!               [--sessions-file FILE] [--service-key FILE] [--peer NAME=URL]...
//! ```
//!
//! One port serves everything (SUPERCONDUCTOR-19, -11): an upgrade on any path (`/`,
//! `/ws`) is the Plexus socket, carrying `agents`, `hosts`, `ui` and `auth`;
//! `/login*` is plexus-login's sign-in; and `GET /`, `/app/…` is plexus-ui's shell,
//! serving the live bundle (the repository's `ui/`, pushed with `plexus-ui push`;
//! deploy/push-ui.sh). A browser's upgrade must come from this service's own origin;
//! a client sending no `Origin` may connect.
//!
//! **The UI.** `--ui-db` is plexus-ui's store (default `ui.db` beside `--db`): the
//! pushed versions and the live pointer. `--ui-signer` (64 hex, repeatable) names a
//! key whose signed bundles a push takes; with none, every push is refused. A push
//! is `admin`'s, or `deployer`'s (`--grant key:<hex>=deployer@*`); every signed-in
//! person reads the UI.
//!
//! **Signing in.** Key sign-in (`key:<hex>`) is always on: key proofs naming this
//! service's sockets (`ws://127.0.0.1:<port>`, `ws://localhost:<port>`, and the
//! public URL's `wss://host` and `wss://host/ws`) or a `--key-audience`. WorkOS,
//! GitHub only (SUPERCONDUCTOR-25: no emailed code, no SSO), is on given
//! `--workos-client` and its API key, from `--workos-api-key-file` or
//! `AGENTS_WORKOS_API_KEY`; without a key there's no WorkOS button. `--identity`
//! takes a whole plexus-login identity spec instead. `--allow` names who gets a
//! session: `github:<numeric id>` (a WorkOS sign-in's linked GitHub account) or
//! `principal:<principal>`, repeatable; with any rule, WorkOS bearer tokens are
//! refused at the socket (key proofs and sessions are not). WorkOS on a public
//! (non-loopback) URL with no rule refuses to start.
//! `--issuer`/`--audience` admit plexus-idp tokens as before. `--admin` makes a
//! principal `admin` everywhere (`key:<hex>` where there's no WorkOS).
//! `auth.*` (`login`, `whoami`, `providers`, …) is mounted, so `synapse _login`
//! works against the service.
//!
//! `--public-url` is where people and hosts reach this service: `https://host` or
//! `wss://host/ws`, the same origin either way. `--trusted-proxy` names the proxy
//! in front (a CIDR, or `none`): its `X-Forwarded-For` is believed.
//!
//! `--placement` names a place a thread can run; `local` if none is given, and none
//! with `--no-local`: a hosted service runs nothing itself. `--grant` gives a role:
//! `user@local` lets a principal start runs on `local`, `admin@*` lets it do
//! everything. A caller with no grant can see only its own threads, and has none.
//! There is no way to run with auth off.
//!
//! A peer needs a `--grant key:<hex>=admin@*` here before a proof gets it anything.
//! `--peer` names a service reachable at `agents.on.<name>`, dialed with
//! `--service-key`, a 32-byte Ed25519 seed in a file of mode 0600. `--print-key
//! FILE` prints that key's principal and exits, so a script can write the peer's
//! `--grant`.
//!
//! Hosts behind NAT dial in (SUPERCONDUCTOR-15) and are reached at
//! `agents.on.<host>`, given `--server-key FILE`: this service's Ed25519 identity
//! (`key_s`, a 32-byte seed, mode 0600, the same format as `--service-key`), which
//! hosts pin and every call to a host is signed with (SUPERCONDUCTOR-18).
//! `--generate-server-key` makes that file if it's missing: for development.
//! Without `--server-key` there are no hosts. A host pairs by a code both sides
//! show; `hosts.pairing`, `hosts.confirm`, `hosts.unpair` and `hosts.list` are a
//! signed-in user's, in the same `--db`.
//!
//! Every flag can come from the environment instead: `AGENTS_DB`, `AGENTS_PORT`,
//! `AGENTS_BIND`, `AGENTS_PUBLIC_URL`, `AGENTS_SERVER_KEY`, `AGENTS_NO_LOCAL=1`,
//! `AGENTS_WORKOS_CLIENT`, `AGENTS_WORKOS_API_KEY_FILE`, `AGENTS_IDENTITY`,
//! `AGENTS_SESSIONS_FILE`, `AGENTS_UI_DB`, and the lists `AGENTS_ADMINS`,
//! `AGENTS_TRUSTED_PROXIES`, `AGENTS_PLACEMENTS`, `AGENTS_KEY_AUDIENCES`,
//! `AGENTS_ALLOW`, `AGENTS_UI_SIGNERS` (comma-separated). A flag wins.
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
use plexus_agents::front::{self, SignInConfig};
use plexus_agents::host::{self, HostConfig};
use plexus_agents::hosts::Hosts;
use plexus_agents::pairing::{self, ServerKey};
use plexus_agents::peer::{self, Peer, PeerConfig};
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{self, TokenIssuer};
use plexus_agents::{ClaudeHarness, LocalEnvironment, RecordingEnvironment};
use plexus_harness::{Environment, Harness};
use plexus_ui_service::{Signers, UiActivation, UiStore, DEFAULT_CHANNEL, UI_DB_FILE};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_list(name: &str) -> Vec<String> {
    let list = env(name).unwrap_or_default();
    let items = list.split(',').map(str::trim).filter(|s| !s.is_empty());
    items.map(str::to_string).collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("host") {
        return host().await;
    }
    let mut public_url = env("AGENTS_PUBLIC_URL");
    let (mut db, mut issuer, mut audience, mut record) = (env("AGENTS_DB"), None, None, None);
    let mut port = env("AGENTS_PORT")
        .map_or(Ok(4470u16), |p| p.parse())
        .context("AGENTS_PORT")?;
    let mut bind: IpAddr = env("AGENTS_BIND")
        .map_or(Ok(IpAddr::from([127, 0, 0, 1])), |b| b.parse())
        .context("AGENTS_BIND")?;
    let (mut placements, mut grants) = (env_list("AGENTS_PLACEMENTS"), Vec::<Grant>::new());
    let (mut key_audiences, mut service_key, mut peers) =
        (env_list("AGENTS_KEY_AUDIENCES"), None, Vec::new());
    let mut server_key = env("AGENTS_SERVER_KEY");
    let mut generate_server_key = false;
    let mut no_local = env("AGENTS_NO_LOCAL").is_some_and(|v| v != "0" && v != "false");
    let mut trusted_proxies = env_list("AGENTS_TRUSTED_PROXIES");
    let mut admins = env_list("AGENTS_ADMINS");
    let mut workos_client = env("AGENTS_WORKOS_CLIENT");
    let mut workos_key_file = env("AGENTS_WORKOS_API_KEY_FILE").map(std::path::PathBuf::from);
    let mut identity = env("AGENTS_IDENTITY");
    let mut sessions_file = env("AGENTS_SESSIONS_FILE").map(std::path::PathBuf::from);
    let mut ui_db = env("AGENTS_UI_DB").map(std::path::PathBuf::from);
    let mut ui_signers = env_list("AGENTS_UI_SIGNERS");
    let mut allow = env_list("AGENTS_ALLOW");
    // A list flag given replaces the environment's list.
    let mut replaced = std::collections::HashSet::new();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(anyhow!("{flag} needs a value"));
        let mut list = |l: &mut Vec<String>, v: String, name: &'static str| {
            if replaced.insert(name) {
                l.clear();
            }
            l.push(v);
        };
        match flag.as_str() {
            "--db" => db = Some(value()?),
            "--issuer" => issuer = Some(value()?),
            "--audience" => audience = Some(value()?),
            "--record" => record = Some(value()?),
            "--port" => port = value()?.parse().context("--port")?,
            "--bind" => bind = value()?.parse().context("--bind")?,
            "--placement" => list(&mut placements, value()?, "placement"),
            "--grant" => grants.push(value()?.parse().map_err(|e: String| anyhow!(e))?),
            "--admin" => list(&mut admins, value()?, "admin"),
            "--key-audience" => list(&mut key_audiences, value()?, "key-audience"),
            "--service-key" => service_key = Some(value()?),
            "--peer" => peers.push(value()?),
            "--public-url" => public_url = Some(value()?),
            "--server-key" => server_key = Some(value()?),
            "--generate-server-key" => generate_server_key = true,
            "--no-local" => no_local = true,
            "--trusted-proxy" => list(&mut trusted_proxies, value()?, "trusted-proxy"),
            "--workos-client" => workos_client = Some(value()?),
            "--workos-api-key-file" => workos_key_file = Some(value()?.into()),
            "--identity" => identity = Some(value()?),
            "--sessions-file" => sessions_file = Some(value()?.into()),
            "--ui-db" => ui_db = Some(value()?.into()),
            "--ui-signer" => list(&mut ui_signers, value()?, "ui-signer"),
            "--allow" => list(&mut allow, value()?, "allow"),
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
    let idp = match (issuer, audience) {
        (Some(issuer), Some(audience)) => Some(TokenIssuer { issuer, audience }),
        (None, None) => None,
        _ => return Err(anyhow!("--issuer and --audience go together")),
    };
    for admin in &admins {
        grants.push(
            format!("{admin}=admin@*")
                .parse()
                .map_err(|e: String| anyhow!("--admin: {e}"))?,
        );
    }
    let origin = public_url.as_deref().map(front::page_origin).transpose()?;
    // What a host's calls out are stamped with: the socket it dials.
    let socket = origin
        .as_deref()
        .map_or(format!("ws://{bind}:{port}"), front::socket_url);
    let db: std::path::PathBuf = db.context("--db is required")?.into();
    let sign_in = SignInConfig {
        public_url: origin,
        workos_client,
        workos_key_file,
        identity,
        idp,
        key_audiences,
        admins,
        trusted_proxies: trusted_proxies.clone(),
        sessions_file: Some(sessions_file.unwrap_or_else(|| db.with_extension("sessions"))),
        loopback: bind.is_loopback(),
        allow,
    };
    let login = front::sign_in(&sign_in, port)?;
    eprintln!(
        "plexus-agents: sign-in by key{}{}",
        if login.workos {
            " and WorkOS (GitHub)"
        } else {
            " (no WorkOS)"
        },
        if login.allow.is_empty() {
            String::new()
        } else {
            format!(", allowing {}", login.allow)
        }
    );
    // SUPERCONDUCTOR-11: plexus-ui's store and the keys whose pushes it takes.
    let ui_db = ui_db.unwrap_or_else(|| default_ui_db(&db));
    let signers = Signers::parse(&ui_signers).map_err(|e| anyhow!("--ui-signer: {e}"))?;
    let store = UiStore::open_channel(&ui_db, DEFAULT_CHANNEL)
        .map_err(|e| anyhow!("the ui store {} (--ui-db): {e}", ui_db.display()))?;
    let store = Arc::new(store);
    eprintln!(
        "plexus-agents: ui store {}, {} signer(s)",
        ui_db.display(),
        ui_signers.len()
    );
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
    let hosts = match &server_key {
        Some(file) => {
            let key = ServerKey::load(file.as_ref(), generate_server_key)?;
            eprintln!("plexus-agents: hosts pin this service as {}", key.public());
            Some(Hosts::open(&db, socket, key)?)
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
    let backend = login.backend();
    let ui = UiActivation::new(store.clone(), signers);
    let hub = service::hub_with_ui(
        runtime,
        harnesses,
        grants,
        peers,
        hosts.clone(),
        backend,
        Some(ui),
    );
    let serving = service::Serving {
        hosts,
        trusted_proxies,
        ui: Some(store),
        ..service::Serving::new(bind, port)
    };
    front::serve_public(hub, login, serving).await
}

/// plexus-ui's store beside `db`: `ui.db` in its directory (`agents-ui.db` when the
/// agents file is itself named `ui.db`).
fn default_ui_db(db: &std::path::Path) -> std::path::PathBuf {
    match db.with_file_name(UI_DB_FILE) {
        same if same == db => db.with_file_name(format!("agents-{UI_DB_FILE}")),
        ui => ui,
    }
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

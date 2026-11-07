//! The `agents` service: an activation over the [`Supervisor`], addressed by thread.
//!
//! Every method declares its [`Caller`] through `#[from_auth]`, and the activation is
//! `typed_params`, so a key that names no parameter is refused at dispatch: there is
//! no way to send an actor. Thread methods declare `target = "thread"`, and `start`
//! declares `target = "placement"`, so the gate decides each before it's routed. The
//! exceptions are `public` on purpose: `harnesses` and `claudecode.describe` say what
//! this service can run, take no `Caller`, and are what anon may call.
//!
//! `on` is [`crate::on`]: the same surface addressed by where it runs.
//!
//! `claudecode` is a [`HarnessActivation`], AGENTS-30's adapter over `Arc<dyn Harness>`.
//! It describes a harness and starts nothing: runs start through `agents.start`.

use crate::auth::{self, Access, AgentsRequest, Caller, Grant};
use crate::hosts::{Hosts, HostsActivation};
use crate::on::OnRouter;
use crate::peer::Peer;
use crate::runtime::{
    GlobalSeq, InputRecord, ReadFilter, RuntimeError, StoredEvent, Supervisor, ThreadId,
    ThreadOptions, ThreadView,
};
use anyhow::Context;
use futures::stream::{self, Stream, StreamExt};
use plexus_rpc::auth_core::adapters::{Carriers, KeyProofAdapter, OidcAdapter};
use plexus_rpc::auth_core::grants::Gate;
use plexus_rpc::auth_core::identity::AuthenticatorChain;
use plexus_rpc::auth_core::oidc::{Audience, OidcConfig, OidcSessionValidator, OidcValidator};
use plexus_rpc::auth_core::IssuerUrl;
use plexus_rpc::core::plexus::{DynamicHub, SessionValidator};
use plexus_harness::{
    Capabilities, Harness, InputAnswer, InputRequest, InputRequestId, InterruptOutcome, ModelName,
    RunSpec, StopMode, StopReport, ToolName, ToolPolicy, TurnId, UserInput,
};
use plexus_rpc::transport::TransportServer;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::Arc;

/// The harness every thread runs on, until AGENTS-29 brings another.
const CLAUDE: &str = "claude";

macro_rules! text_param {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub String);
    };
}

text_param!(
    /// A place a thread can run, from config.
    Placement
);
text_param!(
    /// What to say to the agent.
    Prompt
);
text_param!(
    /// The directory a run works in.
    WorkDir
);

/// A harness, as a client reads it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HarnessInfo {
    pub name: String,
    pub capabilities: Capabilities,
}

/// A place `on` can reach.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlacementInfo {
    pub name: String,
    /// Whether it's another service, not a placement of this one.
    pub peer: bool,
}

/// Everything `agents` yields.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentsEvent {
    Started {
        thread: ThreadId,
        turn: TurnId,
    },
    Threads {
        threads: Vec<ThreadView>,
    },
    Harnesses {
        harnesses: Vec<HarnessInfo>,
    },
    Placements {
        placements: Vec<PlacementInfo>,
    },
    Event {
        event: Box<StoredEvent>,
    },
    Sent {
        thread: ThreadId,
        turn: TurnId,
    },
    Interrupted {
        thread: ThreadId,
        outcome: InterruptOutcome,
    },
    Stopped {
        thread: ThreadId,
        report: StopReport,
    },
    Resumed {
        thread: ThreadId,
    },
    Pending {
        requests: Vec<InputRecord>,
    },
    Answered {
        thread: ThreadId,
        request: Box<InputRequest>,
    },
    /// The sign-in behind a held-open `watch` ended.
    Ended,
    Failed {
        message: String,
    },
}

/// SUPERCONDUCTOR-24: Plexus 0.8's typed client yields a call that failed on
/// the client's side as the stream's last event: `Failed`, as the server would.
impl plexus_rpc::client::FromClientError for AgentsEvent {
    fn from_client_error(error: plexus_rpc::client::ClientError) -> Self {
        failed(error)
    }
}

fn failed(e: impl std::fmt::Display) -> AgentsEvent {
    AgentsEvent::Failed {
        message: e.to_string(),
    }
}

fn one(event: AgentsEvent) -> stream::Iter<std::vec::IntoIter<AgentsEvent>> {
    stream::iter(vec![event])
}

fn outcome(r: Result<AgentsEvent, RuntimeError>) -> stream::Iter<std::vec::IntoIter<AgentsEvent>> {
    one(r.unwrap_or_else(failed))
}

/// Serves one harness from its `Harness` impl. Written once, for every harness.
#[derive(Clone)]
pub struct HarnessActivation {
    harness: Arc<dyn Harness>,
    ns: String,
}

impl HarnessActivation {
    pub fn new(ns: &str, harness: Arc<dyn Harness>) -> Self {
        Self {
            harness,
            ns: ns.to_string(),
        }
    }

    fn ns(&self) -> &str {
        &self.ns
    }
}

/// A coding-agent harness, served from its Harness impl.
#[plexus_rpc::macros::activation(
    trait = "HarnessWire",
    namespace = "harness",
    namespace_fn = "ns",
    version = "0.1.0",
    auth = "mixed",
    request = AgentsRequest
)]
impl HarnessActivation {
    /// What this harness can do.
    #[plexus_rpc::macros::method(public)]
    pub async fn describe(&self) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        one(AgentsEvent::Harnesses {
            harnesses: vec![HarnessInfo {
                name: self.harness.name().to_string(),
                capabilities: self.harness.capabilities(),
            }],
        })
    }
}

/// The `agents` activation. The runtime holds the state; this adds none.
#[derive(Clone)]
pub struct AgentsActivation {
    pub(crate) runtime: Supervisor,
    harnesses: Vec<Arc<dyn Harness>>,
    access: Access,
    pub(crate) gate: Gate,
    pub(crate) peers: Vec<Arc<Peer>>,
    pub(crate) hosts: Option<Arc<Hosts>>,
}

impl AgentsActivation {
    pub fn new(
        runtime: Supervisor,
        harnesses: Vec<Arc<dyn Harness>>,
        gate: Gate,
        peers: Vec<Arc<Peer>>,
    ) -> Self {
        Self {
            runtime,
            harnesses,
            access: Access,
            gate,
            peers,
            hosts: None,
        }
    }

    /// With the hosts that dial in, reachable at `on.<host>`.
    pub fn with_hosts(mut self, hosts: Arc<Hosts>) -> Self {
        self.hosts = Some(hosts);
        self
    }
}

/// Coding agents as durable threads, addressed by thread.
#[plexus_rpc::macros::activation(
    version = "0.1.0",
    auth = "mixed",
    request = AgentsRequest,
    typed_params
)]
impl AgentsActivation {
    /// Starts a thread on a placement, and its first turn.
    #[plexus_rpc::macros::method(target = "placement")]
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** where it runs */
        placement: Placement,
        /** the directory it works in */
        cwd: WorkDir,
        /** the first prompt */
        prompt: Prompt,
        /** the model, or the harness's default */
        model: Option<ModelName>,
        /** tools it may use; only Bash if omitted */
        tools: Option<Vec<ToolName>>,
        /** stop the process while a request waits; default true */
        park: Option<bool>,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let spec = RunSpec {
            cwd: Some(cwd.0),
            model,
            tools: ToolPolicy {
                enabled: Some(tools.unwrap_or_else(|| vec![ToolName::new("Bash")])),
                preapproved: vec![],
                denied: vec![],
            },
            ..RunSpec::default()
        }
        .with_host_env(["USER", "PATH"]);
        let actor = caller.actor().clone();
        let options = ThreadOptions {
            park: park.unwrap_or(true),
            owner: Some(actor.clone()),
            placement: Some(placement.0),
        };
        let started = async {
            let thread = self.runtime.create_thread(CLAUDE, spec, options)?;
            let input = UserInput::text(prompt.0).from_actor(actor);
            let turn = self.runtime.prompt(&thread, input).await?;
            Ok(AgentsEvent::Started { thread, turn })
        };
        outcome(started.await)
    }

    /// The threads this caller may see, or one of them.
    #[plexus_rpc::macros::method(target = "thread")]
    pub async fn threads(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** one thread, or all of them */
        thread: Option<ThreadId>,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let listed = match thread {
            Some(t) => self.runtime.thread(&t).map(|v| vec![v]),
            None => self.runtime.threads(),
        };
        outcome(listed.map(|mut threads| {
            threads.retain(|v| caller.sees(&v.id));
            AgentsEvent::Threads { threads }
        }))
    }

    /// The harnesses this service runs.
    #[plexus_rpc::macros::method(public)]
    pub async fn harnesses(&self) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let harnesses = self.harnesses.iter().map(|h| HarnessInfo {
            name: h.name().to_string(),
            capabilities: h.capabilities(),
        });
        one(AgentsEvent::Harnesses {
            harnesses: harnesses.collect(),
        })
    }

    /// Replays from `since`, then follows live, across resumes and threads.
    #[plexus_rpc::macros::method(target = "thread")]
    pub async fn watch(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** one thread, or every thread this caller may see */
        thread: Option<ThreadId>,
        /** the last sequence seen; from the start if omitted */
        since: Option<GlobalSeq>,
        /** include each event's raw frame; default false */
        raw: Option<bool>,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let filter = ReadFilter {
            thread,
            without_raw: !raw.unwrap_or(false),
        };
        let mut events = self.runtime.read(filter, since.unwrap_or_default());
        async_stream::stream! {
            while let Some(event) = events.next().await {
                if !caller.stands() {
                    yield AgentsEvent::Ended;
                    break;
                }
                if caller.sees(&event.thread) {
                    yield AgentsEvent::Event { event: Box::new(event) };
                }
            }
        }
    }

    /// Sends a prompt, starting or resuming a process if there's none.
    #[plexus_rpc::macros::method(target = "thread")]
    pub async fn send(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** the thread */
        thread: ThreadId,
        /** what to say */
        prompt: Prompt,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let input = UserInput::text(prompt.0).from_actor(caller.actor().clone());
        let sent = self.runtime.prompt(&thread, input).await;
        outcome(sent.map(|turn| AgentsEvent::Sent { thread, turn }))
    }

    /// Stops the turn. On a thread with no process, cancels what it was waiting on.
    #[plexus_rpc::macros::method(target = "thread")]
    pub async fn interrupt(
        &self,
        #[from_auth(self.access.verify)] _caller: Caller,
        /** the thread */
        thread: ThreadId,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let done = self.runtime.interrupt(&thread).await;
        outcome(done.map(|outcome| AgentsEvent::Interrupted { thread, outcome }))
    }

    /// Stops the thread's process.
    #[plexus_rpc::macros::method(target = "thread")]
    pub async fn stop(
        &self,
        #[from_auth(self.access.verify)] _caller: Caller,
        /** the thread */
        thread: ThreadId,
        /** graceful, or force; default graceful */
        mode: Option<StopMode>,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let mode = mode.unwrap_or(StopMode::Graceful);
        let done = self.runtime.stop(&thread, mode).await;
        outcome(done.map(|report| AgentsEvent::Stopped { thread, report }))
    }

    /// Every undecided request this caller may see, oldest first, across threads.
    #[plexus_rpc::macros::method]
    pub async fn pending(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        outcome(self.runtime.pending().map(|mut requests| {
            requests.retain(|r| caller.sees(&r.thread));
            AgentsEvent::Pending { requests }
        }))
    }

    /// Answers a request on a thread. The first answer wins.
    #[plexus_rpc::macros::method(target = "thread")]
    pub async fn answer(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** the thread the request is on */
        thread: ThreadId,
        /** the request */
        request: InputRequestId,
        /** the answer */
        answer: InputAnswer,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let actor = caller.actor().clone();
        match self
            .runtime
            .answer_as(&thread, &request, answer, actor)
            .await
        {
            Ok(request) => one(AgentsEvent::Answered {
                thread,
                request: Box::new(request),
            }),
            Err(e) => one(failed(e)),
        }
    }

    /// Resumes a parked thread with no answer, so its call is asked again.
    #[plexus_rpc::macros::method(target = "thread")]
    pub async fn resume(
        &self,
        #[from_auth(self.access.verify)] _caller: Caller,
        /** the thread */
        thread: ThreadId,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let done = self.runtime.continue_thread(&thread).await;
        outcome(done.map(|()| AgentsEvent::Resumed { thread }))
    }

    /// The same surface, addressed by where it runs.
    #[plexus_rpc::macros::child(opaque)]
    fn on(&self) -> OnRouter {
        OnRouter::new(self.clone())
    }

    /// The Claude harness.
    #[plexus_rpc::macros::child(opaque)]
    fn claudecode(&self) -> HarnessActivation {
        let claude = self.harnesses.iter().find(|h| h.name() == CLAUDE);
        HarnessActivation::new("claudecode", claude.expect("composed with Claude").clone())
    }
}

/// The hub: the gate in front of every call, and `agents` behind it.
pub fn hub(
    runtime: Supervisor,
    harnesses: Vec<Arc<dyn Harness>>,
    grants: Vec<Grant>,
) -> Arc<DynamicHub> {
    hub_with_peers(runtime, harnesses, grants, Vec::new())
}

/// [`hub`], with the other services `on` can reach.
pub fn hub_with_peers(
    runtime: Supervisor,
    harnesses: Vec<Arc<dyn Harness>>,
    grants: Vec<Grant>,
    peers: Vec<Arc<Peer>>,
) -> Arc<DynamicHub> {
    hub_with_hosts(runtime, harnesses, grants, peers, None)
}

/// [`hub_with_peers`], with hosts that dial in (SUPERCONDUCTOR-15): `hosts.*`, and
/// `agents.on.<host>` over each host's own socket. Serve it with [`serve_with_hosts`].
pub fn hub_with_hosts(
    runtime: Supervisor,
    harnesses: Vec<Arc<dyn Harness>>,
    grants: Vec<Grant>,
    peers: Vec<Arc<Peer>>,
    hosts: Option<Arc<Hosts>>,
) -> Arc<DynamicHub> {
    let names: Vec<String> = peers.iter().map(|p| p.name().to_string()).collect();
    let store = hosts.as_ref().map(|h| h.store());
    let gate = auth::gate_with_hosts(&runtime, names.clone(), grants, store);
    let mut agents = AgentsActivation::new(runtime.clone(), harnesses, gate.clone(), peers);
    let mut hub = DynamicHub::new("plexus-agents").with_gate(Arc::new(gate));
    if let Some(hosts) = hosts {
        hosts.reserve(names.into_iter().chain(runtime.placements()));
        agents = agents.with_hosts(hosts.clone());
        hub = hub.register(HostsActivation::new(hosts));
    }
    Arc::new(hub.register(agents))
}

/// [`hub_with_hosts`], with `auth.*` mounted over `backend` (SUPERCONDUCTOR-19):
/// sign-in, `whoami`, `login {browser}` for `synapse _login`. The auth activation's
/// gate is built from the same rules as [`auth::gate_with_hosts`].
pub fn hub_with_login(
    runtime: Supervisor,
    harnesses: Vec<Arc<dyn Harness>>,
    grants: Vec<Grant>,
    peers: Vec<Arc<Peer>>,
    hosts: Option<Arc<Hosts>>,
    backend: Arc<dyn plexus_rpc::core::plexus::AuthBackend>,
) -> Arc<DynamicHub> {
    hub_with_ui(runtime, harnesses, grants, peers, hosts, backend, None)
}

/// The hub's name: a page's frames are `plexus-agents.call`.
pub const SERVICE: &str = "plexus-agents";

/// [`hub_with_login`], with plexus-ui's `ui` activation registered beside
/// `agents`, `hosts` and `auth` (SUPERCONDUCTOR-11), as menger links it: its
/// seam attached to this hub as a system caller of its own, `admin` on `*`
/// here and nowhere else (a key nobody holds), so a push's `uses` are looked up
/// in the running services.
pub fn hub_with_ui(
    runtime: Supervisor,
    harnesses: Vec<Arc<dyn Harness>>,
    mut grants: Vec<Grant>,
    peers: Vec<Arc<Peer>>,
    hosts: Option<Arc<Hosts>>,
    backend: Arc<dyn plexus_rpc::core::plexus::AuthBackend>,
    ui: Option<plexus_ui_service::UiActivation>,
) -> Arc<DynamicHub> {
    let system = ui.as_ref().map(|_| system_principal());
    if let Some(system) = &system {
        grants.push(
            format!("{system}=admin@*")
                .parse()
                .expect("a key principal is one"),
        );
    }
    let names: Vec<String> = peers.iter().map(|p| p.name().to_string()).collect();
    let store = hosts.as_ref().map(|h| h.store());
    let gate = auth::gate_with_hosts(&runtime, names.clone(), grants.clone(), store.clone());
    let setup = auth::auth_setup(&runtime, names.clone(), grants, store, backend);
    let mut agents = AgentsActivation::new(runtime.clone(), harnesses, gate, peers);
    let mut hub = DynamicHub::new(SERVICE);
    if let Some(hosts) = hosts {
        hosts.reserve(names.into_iter().chain(runtime.placements()));
        agents = agents.with_hosts(hosts.clone());
        hub = hub.register(HostsActivation::new(hosts));
    }
    let seam = ui.as_ref().map(|u| u.seam().clone());
    if let Some(ui) = ui {
        hub = hub.register(ui);
    }
    let hub = Arc::new(hub.register(agents).with_auth(setup));
    if let (Some(seam), Some(system)) = (seam, system) {
        let meta = serde_json::json!({ "principal": system, "name": SERVICE });
        let ctx = plexus_rpc::core::plexus::AuthContext::new(system, "system".into(), vec![], meta);
        seam.attach(&hub, ctx);
    }
    hub
}

/// A principal nobody can prove: 32 fresh bytes as a key's.
fn system_principal() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    plexus_rpc::auth_core::adapters::key_principal(&bytes).to_string()
}

/// Where tokens come from.
#[derive(Debug, Clone)]
pub struct TokenIssuer {
    /// The issuer's URL, exactly as tokens name it.
    pub issuer: String,
    /// The audience tokens must be for.
    pub audience: String,
}

impl TokenIssuer {
    /// Tokens validated as RS256 against the issuer's published keys.
    pub fn validator(&self) -> anyhow::Result<Arc<dyn SessionValidator>> {
        let url = self.issuer.parse().context("--issuer")?;
        let issuer = IssuerUrl::try_new(url).context("--issuer")?;
        let audience = Audience::try_new(&self.audience).context("--audience")?;
        let config = OidcConfig::new(issuer, audience);
        Ok(Arc::new(OidcSessionValidator::new(OidcValidator::new(
            config,
        ))))
    }
}

/// How callers sign in: tokens from an issuer, key proof from peer services, or both.
#[derive(Debug, Clone, Default)]
pub struct SignIn {
    pub tokens: Option<TokenIssuer>,
    /// The URLs this service is dialed at, which a key proof must name.
    pub key_audiences: Vec<String>,
}

impl SignIn {
    /// Whichever adapter claims a credential decides it. With neither configured
    /// there's no way in, and that's refused rather than served.
    pub fn validator(&self) -> anyhow::Result<Arc<dyn SessionValidator>> {
        let mut chain = AuthenticatorChain::new();
        if let Some(t) = &self.tokens {
            let adapter = OidcAdapter::plexus_idp(&t.issuer, &t.audience)
                .map_err(|e| anyhow::anyhow!("--issuer, --audience: {e}"))?;
            chain = chain.with(Arc::new(adapter));
        }
        if !self.key_audiences.is_empty() {
            let adapter = KeyProofAdapter::new(&self.key_audiences)
                .map_err(|e| anyhow::anyhow!("a key audience: {e}"))?;
            chain = chain.with(Arc::new(adapter));
        }
        anyhow::ensure!(
            !chain.names().is_empty(),
            "no way to sign in: give --issuer and --audience, or --key-audience"
        );
        Ok(Arc::new(Carriers::cookie_and_bearer(chain)))
    }
}

/// Serves `hub` on a WebSocket until it fails. A connection with no credential gets
/// the anonymous view, where the gate admits introspection and `public` methods and
/// refuses the rest. A credential that doesn't validate is refused at the upgrade.
pub async fn serve(
    hub: Arc<DynamicHub>,
    validator: Arc<dyn SessionValidator>,
    bind: IpAddr,
    port: u16,
) -> anyhow::Result<()> {
    serve_with_hosts(hub, validator, bind, port, None).await
}

/// [`serve`], with `hosts`' hook on every signed-in socket and its admission in
/// front of `validator`: an unpaired host's key proof waits, and only waits.
pub async fn serve_with_hosts(
    hub: Arc<DynamicHub>,
    validator: Arc<dyn SessionValidator>,
    bind: IpAddr,
    port: u16,
    hosts: Option<Arc<Hosts>>,
) -> anyhow::Result<()> {
    let serving = Serving {
        hosts,
        ..Serving::new(bind, port)
    };
    serve_with(hub, validator, serving).await
}

/// How [`serve_with`] serves.
#[derive(Clone)]
pub struct Serving {
    pub bind: IpAddr,
    pub port: u16,
    pub hosts: Option<Arc<Hosts>>,
    /// Proxies whose `X-Forwarded-For` is believed: CIDRs, or `none`.
    pub trusted_proxies: Vec<String>,
    /// SUPERCONDUCTOR-11: the ui store the public front's shell serves `/` and
    /// `/app/…` from ([`crate::front::serve_public`]); the socket alone takes none.
    pub ui: Option<Arc<plexus_ui_service::UiStore>>,
}

impl Serving {
    pub fn new(bind: IpAddr, port: u16) -> Self {
        Self {
            bind,
            port,
            hosts: None,
            trusted_proxies: Vec::new(),
            ui: None,
        }
    }
}

/// [`serve_with_hosts`], as `serving` says.
pub async fn serve_with(
    hub: Arc<DynamicHub>,
    validator: Arc<dyn SessionValidator>,
    serving: Serving,
) -> anyhow::Result<()> {
    let module =
        |h: Arc<DynamicHub>| DynamicHub::arc_into_rpc_module(h).map_err(|e| anyhow::anyhow!("{e}"));
    let mut builder = TransportServer::builder(hub, module);
    let mut validator = validator;
    if let Some(hosts) = serving.hosts {
        validator = hosts.admission(validator);
        builder = builder.with_hooks(hosts.hooks());
    }
    if !serving.trusted_proxies.is_empty() {
        let proxies = plexus_rpc::transport::proxy::TrustedProxies::parse(&serving.trusted_proxies)
            .map_err(|e| anyhow::anyhow!("--trusted-proxy: {e}"))?;
        builder = builder.with_trusted_proxies(proxies);
    }
    builder
        .without_registry()
        .with_websocket(serving.port)
        .bind(serving.bind)
        .with_session_validator(validator)
        .anonymous_view()
        .credential_cookies(["access_token"])
        .build()
        .await?
        .serve()
        .await
}

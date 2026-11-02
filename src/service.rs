//! The `agents` service: an activation over the [`Supervisor`], addressed by thread.
//!
//! Every method declares its [`Caller`] through `#[from_auth]`, and the activation is
//! `typed_params`, so a key that names no parameter is refused at dispatch: there is
//! no way to send an actor. Thread methods declare `target = "thread"`, and `start`
//! declares `target = "placement"`, so the gate decides each before it's routed.
//!
//! `claudecode` is a [`HarnessActivation`], AGENTS-30's adapter over `Arc<dyn Harness>`.
//! It describes a harness and starts nothing: runs start through `agents.start`.

use crate::auth::{self, Access, AgentsRequest, Caller, Grant};
use crate::runtime::{
    GlobalSeq, InputRecord, ReadFilter, RuntimeError, StoredEvent, Supervisor, ThreadId,
    ThreadOptions, ThreadView,
};
use anyhow::Context;
use futures::stream::{self, Stream, StreamExt};
use plexus_auth_core::oidc::{Audience, OidcConfig, OidcSessionValidator, OidcValidator};
use plexus_auth_core::IssuerUrl;
use plexus_core::plexus::{DynamicHub, SessionValidator};
use plexus_harness::{
    Capabilities, Harness, InputAnswer, InputRequest, InputRequestId, InterruptOutcome, ModelName,
    RunSpec, StopMode, StopReport, ToolName, ToolPolicy, TurnId, UserInput,
};
use plexus_transport::TransportServer;
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
    access: Access,
}

impl HarnessActivation {
    pub fn new(ns: &str, harness: Arc<dyn Harness>) -> Self {
        Self {
            harness,
            ns: ns.to_string(),
            access: Access,
        }
    }

    fn ns(&self) -> &str {
        &self.ns
    }
}

/// A coding-agent harness, served from its Harness impl.
#[plexus_macros::activation(
    trait = "HarnessWire",
    namespace = "harness",
    namespace_fn = "ns",
    version = "0.1.0",
    auth = "required",
    request = AgentsRequest
)]
impl HarnessActivation {
    /// What this harness can do.
    #[plexus_macros::method(streaming)]
    pub async fn describe(
        &self,
        #[from_auth(self.access.verify)] _caller: Caller,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
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
    runtime: Supervisor,
    harnesses: Vec<Arc<dyn Harness>>,
    access: Access,
}

impl AgentsActivation {
    pub fn new(runtime: Supervisor, harnesses: Vec<Arc<dyn Harness>>) -> Self {
        Self {
            runtime,
            harnesses,
            access: Access,
        }
    }
}

/// Coding agents as durable threads, addressed by thread.
#[plexus_macros::activation(
    version = "0.1.0",
    auth = "required",
    request = AgentsRequest,
    typed_params
)]
impl AgentsActivation {
    /// Starts a thread on a placement, and its first turn.
    #[plexus_macros::method(streaming, target = "placement")]
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
    #[plexus_macros::method(streaming, target = "thread")]
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
    #[plexus_macros::method(streaming)]
    pub async fn harnesses(
        &self,
        #[from_auth(self.access.verify)] _caller: Caller,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let harnesses = self.harnesses.iter().map(|h| HarnessInfo {
            name: h.name().to_string(),
            capabilities: h.capabilities(),
        });
        one(AgentsEvent::Harnesses {
            harnesses: harnesses.collect(),
        })
    }

    /// Replays from `since`, then follows live, across resumes and threads.
    #[plexus_macros::method(streaming, target = "thread")]
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
    #[plexus_macros::method(streaming, target = "thread")]
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
    #[plexus_macros::method(streaming, target = "thread")]
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
    #[plexus_macros::method(streaming, target = "thread")]
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
    #[plexus_macros::method(streaming)]
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
    #[plexus_macros::method(streaming, target = "thread")]
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
    #[plexus_macros::method(streaming, target = "thread")]
    pub async fn resume(
        &self,
        #[from_auth(self.access.verify)] _caller: Caller,
        /** the thread */
        thread: ThreadId,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let done = self.runtime.continue_thread(&thread).await;
        outcome(done.map(|()| AgentsEvent::Resumed { thread }))
    }

    /// The Claude harness.
    #[plexus_macros::child]
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
    let gate = auth::gate(&runtime, grants);
    Arc::new(
        DynamicHub::new("plexus-agents")
            .with_gate(Arc::new(gate))
            .register(AgentsActivation::new(runtime, harnesses)),
    )
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

/// Serves `hub` on a WebSocket until it fails. A connection with no credential gets
/// the anonymous view, where the gate refuses everything but introspection. A
/// credential that doesn't validate is refused at the upgrade.
pub async fn serve(
    hub: Arc<DynamicHub>,
    validator: Arc<dyn SessionValidator>,
    bind: IpAddr,
    port: u16,
) -> anyhow::Result<()> {
    let module =
        |h: Arc<DynamicHub>| DynamicHub::arc_into_rpc_module(h).map_err(|e| anyhow::anyhow!("{e}"));
    TransportServer::builder(hub, module)
        .without_registry()
        .with_websocket(port)
        .bind(bind)
        .with_session_validator(validator)
        .anonymous_view()
        .credential_cookies(["access_token"])
        .build()
        .await?
        .serve()
        .await
}

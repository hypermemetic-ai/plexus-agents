//! `agents.on.<placement>`: the service, addressed by where things run.
//!
//! `on` is a static child, with a dynamic child under it. `on.local.*` is this
//! service's own surface bound to that placement, and `on.hal.*` forwards the rest of
//! the path to the peer's `agents` (see [`crate::peer`]).
//!
//! # Who gets through
//!
//! On 0.7.0 the hub's gate sees `agents.on.hal.watch` as a path no schema describes,
//! so it checks that exact string with no target and only `admin` passes (AGENTS-20).
//! It admits every `.schema` open. So [`OnTarget::router_call`] checks the caller
//! itself, first, on every path: the grant `agents.on.<name>`, on the scope `<name>`.
//! That's also why an `admin` held on `hal` can't reach `dream`, which the hub's gate
//! alone would let through. A name that isn't configured is refused the same way, so a
//! refusal doesn't say which peers exist. A peer's `public` methods aren't reachable
//! anonymously through here, on purpose.
//!
//! # How a client finds one
//!
//! A dynamic child has no name a schema can list, so synapse can't navigate it
//! (AGENTS-32). [`OnRouter`] answers `agents.on.schema` with the places this caller
//! may reach in `children`, which is what synapse walks. An offline peer isn't in it,
//! and a call to one still says `child_unavailable`.
//!
//! A service key can't go on to another peer: `max_depth` is 0. With a HOP chain this
//! becomes the receiver's policy for that signer.

use crate::auth::{is_service, Access, AgentsRequest, Caller};
use crate::runtime::ThreadId;
use crate::service::{AgentsActivation, AgentsEvent, PlacementInfo};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use plexus_auth_core::grants::Grant;
use plexus_core::plexus::{
    Activation, AuthContext, AuthzDenyReason, ChildRouter, ForwardPolicy, PlexusContext,
    PlexusError, PlexusStream, PlexusStreamItem, PluginSchema, ScopeRef, StreamMetadata,
};
use plexus_core::request::RawRequestContext;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// How many services a call may cross past the one it arrived on.
const MAX_DEPTH: u64 = 0;

/// Places `on` can reach.
#[derive(Clone)]
pub struct OnActivation {
    agents: AgentsActivation,
    access: Access,
}

impl OnActivation {
    pub fn new(agents: AgentsActivation) -> Self {
        Self {
            agents,
            access: Access,
        }
    }
}

/// Where threads run: this service's placements, and its peers.
#[plexus_macros::activation(
    namespace = "on",
    version = "0.1.0",
    auth = "required",
    request = AgentsRequest
)]
impl OnActivation {
    /// The placements here, and the peers answering now. An offline peer isn't listed.
    #[plexus_macros::method(streaming)]
    pub async fn placements(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
    ) -> impl Stream<Item = AgentsEvent> + Send + 'static {
        let mut placements = Vec::new();
        for name in self.agents.runtime.placements() {
            if caller.reaches(&name) {
                placements.push(PlacementInfo { name, peer: false });
            }
        }
        for peer in &self.agents.peers {
            if caller.reaches(peer.name()) && peer.schema_hash().await.is_some() {
                let name = peer.name().to_string();
                placements.push(PlacementInfo { name, peer: true });
            }
        }
        if let Some(hosts) = &self.agents.hosts {
            for name in hosts.online_owned(&caller.actor().id) {
                placements.push(PlacementInfo { name, peer: true });
            }
        }
        futures::stream::iter([AgentsEvent::Placements { placements }])
    }

    /// One placement or peer.
    #[plexus_macros::child(forward = "same_service")]
    fn placement(&self, name: &str) -> Option<OnTarget> {
        Some(OnTarget {
            agents: self.agents.clone(),
            name: name.to_string(),
        })
    }
}

/// `on` as `agents` mounts it: the activation, with the live `children` in its schema.
#[derive(Clone)]
pub struct OnRouter(OnActivation);

impl OnRouter {
    pub fn new(agents: AgentsActivation) -> Self {
        Self(OnActivation::new(agents))
    }

    async fn children(&self, auth: Option<&AuthContext>) -> Vec<Value> {
        let agents = &self.0.agents;
        let may = |name: &str| {
            let grant = Grant::new(format!("agents.on.{name}"));
            agents.gate.check(auth, &grant, Some(name)).is_ok()
        };
        // A hash synapse can tell apart from its parent's (AGENTS-19), and one that
        // moves with the schema behind it, so a client's cache keyed on it moves too.
        let summary = |name: &str, about: &str, hash: &str| {
            let hash = format!("{name}:{hash}");
            json!({"namespace": name, "description": about, "hash": hash})
        };
        let mut children = Vec::new();
        let own = Activation::plugin_schema(agents).hash;
        for name in agents.runtime.placements() {
            if may(&name) {
                children.push(summary(&name, "A placement of this service.", &own));
            }
        }
        for peer in &agents.peers {
            if may(peer.name()) {
                if let Some(hash) = peer.schema_hash().await {
                    children.push(summary(peer.name(), "Another agents service.", &hash));
                }
            }
        }
        let caller = auth.and_then(AuthContext::principal).map(|p| p.to_string());
        if let (Some(hosts), Some(caller)) = (&agents.hosts, caller) {
            for name in hosts.online_owned(&caller) {
                children.push(summary(&name, "A host that dialed in.", &own));
            }
        }
        children
    }
}

#[async_trait]
impl ChildRouter for OnRouter {
    fn router_namespace(&self) -> &str {
        "on"
    }

    async fn router_call(
        &self,
        method: &str,
        params: Value,
        auth: Option<&AuthContext>,
        raw: Option<&RawRequestContext>,
    ) -> Result<PlexusStream, PlexusError> {
        let whole = method == "schema" && params.get("method").is_none();
        let children = if whole {
            self.children(auth).await
        } else {
            Vec::new()
        };
        let stream = self.0.router_call(method, params, auth, raw).await?;
        if !whole {
            return Ok(stream);
        }
        Ok(on_data(stream, move |schema| {
            schema["children"] = Value::Array(children.clone());
        }))
    }

    async fn get_child(&self, name: &str) -> Option<Box<dyn ChildRouter>> {
        self.0.get_child(name).await
    }

    fn forward_policy_for(&self, callee: &str) -> Option<Arc<dyn ForwardPolicy>> {
        self.0.forward_policy_for(callee)
    }

    fn router_schema(&self) -> Option<PluginSchema> {
        self.0.router_schema()
    }
}

/// `stream`, with `change` applied to each `data` item's content.
fn on_data(stream: PlexusStream, change: impl Fn(&mut Value) + Send + 'static) -> PlexusStream {
    Box::pin(stream.map(move |item| match item {
        PlexusStreamItem::Data {
            metadata,
            content_type,
            content,
        } => {
            let mut value = content.into_value();
            change(&mut value);
            PlexusStreamItem::Data {
                metadata,
                content_type,
                content: value.into(),
            }
        }
        other => other,
    }))
}

/// Whatever is at `agents.on.<name>`, refused until the caller is checked.
#[derive(Clone)]
pub struct OnTarget {
    agents: AgentsActivation,
    name: String,
}

impl OnTarget {
    fn local(&self, method: &str, mut params: Value) -> Result<(String, Value), PlexusError> {
        // Addressing `on` from under `on` is no address.
        if method == "on" || method.starts_with("on.") {
            return Err(PlexusError::MethodNotFound {
                activation: self.name.clone(),
                method: method.to_string(),
            });
        }
        if method == "start" {
            let Some(object) = params.as_object_mut() else {
                return Err(PlexusError::InvalidParams("start takes an object".into()));
            };
            if let Some(named) = object.get("placement").filter(|p| *p != &json!(self.name)) {
                return Err(PlexusError::InvalidParams(format!(
                    "this is `{}`, not {named}",
                    self.name
                )));
            }
            object.insert("placement".into(), json!(self.name));
        }
        Ok((method.to_string(), params))
    }

    /// A thread named under a placement has to be on it, and the caller's.
    fn own_thread(
        &self,
        grant: &Grant,
        params: &Value,
        auth: Option<&AuthContext>,
    ) -> Result<(), PlexusError> {
        let Some(thread) = params.get("thread").and_then(Value::as_str) else {
            return Ok(());
        };
        let agents = &self.agents;
        agents.gate.check(auth, grant, Some(thread))?;
        let home = agents.runtime.home(&ThreadId::new(thread));
        if home.and_then(|h| h.placement).as_deref() == Some(&self.name) {
            return Ok(());
        }
        Err(PlexusError::Forbidden {
            reason: AuthzDenyReason::MissingGrant {
                grant: grant.clone(),
                scope: ScopeRef::named(thread),
                roles: Vec::new(),
                held_on: Vec::new(),
            },
        })
    }
}

/// A schema of `agents` as `on.<name>` serves it: no `placement` to give, and no `on`.
fn bind_schema(schema: &mut Value) {
    let unbind = |method: &mut Value| {
        if method["name"] != "start" {
            return;
        }
        let params = &mut method["params"];
        if let Some(p) = params["properties"].as_object_mut() {
            p.remove("placement");
        }
        if let Some(r) = params["required"].as_array_mut() {
            r.retain(|n| n != "placement");
        }
    };
    unbind(schema);
    if let Some(methods) = schema["methods"].as_array_mut() {
        methods.retain(|m| m["name"] != "on");
        methods.iter_mut().for_each(unbind);
    }
    if let Some(children) = schema["children"].as_array_mut() {
        children.retain(|c| c["namespace"] != "on");
    }
}

fn depth_refused(name: &str) -> PlexusStream {
    let meta = StreamMetadata::new(vec![name.to_string()], PlexusContext::hash());
    let mut f = Map::new();
    f.insert("child".into(), name.into());
    f.insert("max_depth".into(), MAX_DEPTH.into());
    let message = format!("a service's call can't go on to {name}: max_depth is {MAX_DEPTH}");
    let item = PlexusStreamItem::typed_error(meta, message, "max_depth", false, f);
    Box::pin(futures::stream::iter([item]))
}

#[async_trait]
impl ChildRouter for OnTarget {
    fn router_namespace(&self) -> &str {
        &self.name
    }

    async fn router_call(
        &self,
        method: &str,
        params: Value,
        auth: Option<&AuthContext>,
        raw: Option<&RawRequestContext>,
    ) -> Result<PlexusStream, PlexusError> {
        let agents = &self.agents;
        // A host that dialed in (SUPERCONDUCTOR-15): its owner's, and only its
        // owner's, checked here first, on every path, `.schema` too (AGENTS-40).
        if let Some(hosts) = agents.hosts.as_ref().filter(|h| h.knows(&self.name)) {
            let principal = auth.and_then(AuthContext::principal).map(|p| p.to_string());
            let Some(principal) = principal else {
                return Err(PlexusError::Unauthenticated("sign in first".into()));
            };
            if hosts.owner_of(&self.name).as_deref() != Some(principal.as_str()) {
                return Err(crate::hosts::not_yours(&self.name));
            }
            // A service's own key goes no further. A person signed in by key
            // (SUPERCONDUCTOR-19: `--admin key:<hex>` where there's no WorkOS)
            // owns the hosts it confirmed, and reaches them.
            let agent = auth.and_then(|a| a.get_metadata_string("agent"));
            let from_service = matches!(
                agent.as_deref(),
                Some("plexus-agents" | crate::hosts::HOST_AGENT)
            );
            if is_service(&principal) && from_service {
                return Ok(depth_refused(&self.name));
            }
            return Ok(hosts.call(&self.name, method, params, principal).await);
        }
        let grant = Grant::new(format!("agents.on.{}", self.name));
        let reach = agents.gate.check(auth, &grant, Some(&self.name))?;
        if let Some(peer) = agents.peers.iter().find(|p| p.name() == self.name) {
            let principal = auth.and_then(AuthContext::principal);
            if principal.is_some_and(|p| is_service(&p.to_string())) {
                return Ok(depth_refused(&self.name));
            }
            return Ok(peer.call(method, params, reach).await);
        }
        self.own_thread(&grant, &params, auth)?;
        let (method, params) = self.local(method, params)?;
        let stream = Activation::call(agents, &method, params, auth, raw).await?;
        if method != "schema" {
            return Ok(stream);
        }
        Ok(on_data(stream, bind_schema))
    }

    async fn get_child(&self, _name: &str) -> Option<Box<dyn ChildRouter>> {
        None
    }
}

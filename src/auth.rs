//! Who may call `agents`, on Plexus 0.7.0's own auth.
//!
//! A token is validated at the WebSocket upgrade by `OidcSessionValidator` (RS256,
//! JWKS, issuer and audience from config), and the hub's grant [`Gate`] decides
//! every call before it's routed. This module gives that gate its three answers, and
//! turns what it admitted into a [`Caller`].
//!
//! | Question | Answer |
//! |---|---|
//! | What does a role list? ([`roles`]) | [`USER`] lists `agents.start`. [`OWNER`] lists every thread method. `admin` holds everything |
//! | Who holds what? ([`Members`]) | Configured memberships, and each caller holds [`OWNER`] on its own scope, unconfigured. A `key:` principal holds only what it's granted |
//! | Where does a target live? ([`Homes`]) | A thread: in itself, its owner's scope, its placement, `*`. A placement or peer: in itself, `*` |
//!
//! So starting a run needs [`USER`] on the placement, and everything on a thread
//! needs it to be yours, or `admin`. A read with no target answers from
//! [`Caller::sees`], which asks the gate again for each row and event.
//!
//! Anon is a minimal scope, opted into per method: `#[method(public)]` runs for
//! anyone with an empty reach, and takes no [`Caller`], so it can't touch a thread,
//! a request or a run. Those are `agents.harnesses` and `claudecode.describe`.
//!
//! A thread's chain starts with the thread itself, so a refusal for someone else's
//! thread reads the same as one for a thread that doesn't exist.

use crate::runtime::{Supervisor, ThreadId};
use plexus_auth_core::grants::{
    Gate, GrantSet, Membership, Memberships, Reach, Resolve, RoleTable, ScopeRef,
};
use plexus_auth_core::identity::Principal;
use plexus_auth_core::RoleName;
use plexus_core::plexus::{current_reach, AuthContext, PlexusError};
use plexus_harness::Actor;
use std::sync::Arc;

/// May start a run. Held on a placement, by config.
pub const USER: &str = "user";
/// May use a thread. Held on a caller's own scope, by everyone.
pub const OWNER: &str = "owner";

/// Every grant [`OWNER`] lists: what a thread's owner can do with it.
pub const OWNER_GRANTS: [&str; 8] = [
    "agents.threads",
    "agents.watch",
    "agents.send",
    "agents.interrupt",
    "agents.stop",
    "agents.pending",
    "agents.answer",
    "agents.resume",
];

/// What each role lists. `admin` is built in and holds everything.
pub fn roles() -> RoleTable {
    let mut t = RoleTable::new();
    t.set(USER, GrantSet::listed(["agents.start"]));
    t.set(OWNER, GrantSet::listed(OWNER_GRANTS));
    t.set(plexus_auth_core::grants::ADMIN, GrantSet::All);
    t
}

/// The scope a principal holds [`OWNER`] on.
fn owner_scope(id: &str) -> ScopeRef {
    ScopeRef::named(format!("owner:{id}"))
}

/// A configured grant: `principal=role@scope`, where a scope is a placement or `*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub principal: Principal,
    pub role: String,
    pub scope: ScopeRef,
}

impl std::str::FromStr for Grant {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("`{s}` is not principal=role@scope");
        let (principal, held) = s.split_once('=').ok_or_else(bad)?;
        let (role, scope) = held.split_once('@').ok_or_else(bad)?;
        Ok(Self {
            principal: principal
                .parse()
                .map_err(|e| format!("`{principal}`: {e}"))?,
            role: role.to_string(),
            scope: ScopeRef::parse(scope),
        })
    }
}

/// Configured memberships, and the [`OWNER`] scope of every caller who isn't a key.
/// A key is a service, and has nothing here until a peer grants it: a valid proof
/// shows who's calling and admits it to nothing.
pub struct Members(Vec<Membership>);

impl Members {
    pub fn new(grants: impl IntoIterator<Item = Grant>) -> Self {
        let held = grants.into_iter().map(|g| Membership {
            principal: g.principal,
            role: RoleName::new(&g.role),
            scope: g.scope,
        });
        Self(held.collect())
    }
}

impl Memberships for Members {
    fn held(&self, who: &Principal) -> Vec<Membership> {
        let mut held: Vec<Membership> = self
            .0
            .iter()
            .filter(|m| m.principal == *who)
            .cloned()
            .collect();
        let id = who.to_string();
        if !held.is_empty() || !is_service(&id) {
            held.push(Membership {
                principal: who.clone(),
                role: RoleName::new(OWNER),
                scope: owner_scope(&id),
            });
        }
        held
    }
}

/// Whether a principal is a service: a `key:<hex>` signed in with key proof. Callers
/// come through an IDP, so a key here is a peer acting for itself.
pub fn is_service(principal: &str) -> bool {
    principal.starts_with("key:")
}

/// Where a thread, placement or peer lives, from the runtime's memory and config.
pub struct Homes {
    pub runtime: Supervisor,
    pub peers: Vec<String>,
}

impl Resolve for Homes {
    fn chain(&self, target: &str) -> Option<Vec<ScopeRef>> {
        let here = ScopeRef::named(target);
        if let Some(home) = self.runtime.home(&ThreadId::new(target)) {
            let owner = home.owner.map(|a| owner_scope(&a.id));
            let placement = home.placement.map(ScopeRef::named);
            let chain = [Some(here), owner, placement, Some(ScopeRef::Everywhere)];
            return Some(chain.into_iter().flatten().collect());
        }
        let known = self.runtime.has_placement(target) || self.peers.iter().any(|p| p == target);
        known.then(|| vec![here, ScopeRef::Everywhere])
    }
}

/// The gate for a runtime: default deny, on every call.
pub fn gate(
    runtime: &Supervisor,
    peers: Vec<String>,
    grants: impl IntoIterator<Item = Grant>,
) -> Gate {
    Gate::new(
        Arc::new(Members::new(grants)),
        Arc::new(roles()),
        Arc::new(Homes {
            runtime: runtime.clone(),
            peers,
        }),
    )
}

/// A caller the service verified. Only [`Access::verify`] makes one, and it takes
/// nothing a client sends, so holding one is proof of who's calling.
#[derive(Clone)]
pub struct Caller {
    actor: Actor,
    reach: Reach,
}

impl Caller {
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// Whether this call may touch `thread`, as the grants stand now.
    pub fn sees(&self, thread: &ThreadId) -> bool {
        self.reach.allows(thread.as_str())
    }

    /// Whether this call may go to a placement or peer, as the grants stand now.
    pub fn reaches(&self, place: &str) -> bool {
        self.reach.allows(place)
    }

    /// Whether the sign-in behind this call still stands, for a call held open.
    pub fn stands(&self) -> bool {
        self.reach.recheck().is_ok()
    }
}

/// The per-call context an activation names. Optional, so the resolver is what
/// refuses a call and says why. What decides is [`Access::verify`], never this.
#[derive(plexus_macros::PlexusRequest)]
pub struct AgentsRequest {
    /// The identity the connection's upgrade proved, if it proved one.
    #[from_auth_context]
    pub auth: Option<AuthContext>,
}

/// `#[from_auth(self.access.verify)]`'s resolver.
#[derive(Clone, Copy, Default)]
pub struct Access;

impl Access {
    /// The caller of the call being dispatched. Fails closed: no sign-in, or an
    /// identity that proves no one, is refused.
    pub async fn verify(&self, ctx: &AuthContext) -> Result<Caller, PlexusError> {
        let sign_in = || PlexusError::Unauthenticated("sign in first".into());
        if !ctx.is_authenticated() {
            return Err(sign_in());
        }
        let principal = ctx.principal().ok_or_else(sign_in)?;
        Ok(Caller {
            actor: Actor {
                id: principal.to_string(),
                name: ctx.get_metadata_string("username"),
            },
            reach: current_reach(),
        })
    }
}

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
    Gate, GrantSet, Membership, Memberships, Reach, Resolve, RoleTable, Roles, ScopeRef,
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

/// What every signed-in user may do with hosts (SUPERCONDUCTOR-15, -18). Held
/// through [`OWNER`], so a `key:` principal, which holds no [`OWNER`], has none of
/// them: a key can't confirm a pairing. `agents.on.placements` lists only what the
/// caller reaches: its own hosts, for a user with no other grant.
pub const HOST_USER_GRANTS: [&str; 6] = [
    "hosts.pairing",
    "hosts.confirm",
    "hosts.unpair",
    "hosts.list",
    "hosts.attach",
    "agents.on.placements",
];

/// What each role lists. `admin` is built in and holds everything.
pub fn roles() -> RoleTable {
    let mut t = RoleTable::new();
    t.set(USER, GrantSet::listed(["agents.start"]));
    let owner = OWNER_GRANTS.iter().chain(HOST_USER_GRANTS.iter()).copied();
    t.set(OWNER, GrantSet::listed(owner));
    t.set(plexus_auth_core::grants::ADMIN, GrantSet::All);
    t
}

/// [`roles`], and each host's own role, from the host store as it stands now:
/// `host:<name>` lists `agents.on.<name>.<method>` for every method a host serves,
/// and [`crate::hosts::MACHINE`] lists `hosts.attach`. The hub's gate sees a path
/// under a dynamic child as a grant of that exact string (AGENTS-20), so this is
/// what lets a host's owner, and only its owner, past it.
pub struct AgentsRoles {
    table: RoleTable,
    hosts: Option<Arc<crate::hosts::HostStore>>,
}

impl Roles for AgentsRoles {
    fn grants(&self, role: &RoleName) -> Option<GrantSet> {
        if let Some(store) = &self.hosts {
            if role.as_str() == crate::hosts::MACHINE {
                // `hosts.wait` too: a host that waited answers paired once it is.
                return Some(GrantSet::listed(["hosts.attach", "hosts.wait"]));
            }
            if role.as_str() == crate::hosts::WAITING {
                return Some(GrantSet::listed(["hosts.request", "hosts.wait"]));
            }
            if let Some(name) = role.as_str().strip_prefix(crate::hosts::HOST_ROLE) {
                store.get(name)?;
                let grants = crate::hosts::HOST_METHODS
                    .iter()
                    .map(|m| format!("agents.on.{name}.{m}"));
                return Some(GrantSet::listed(grants));
            }
        }
        self.table.grants(role)
    }

    fn names(&self) -> Vec<RoleName> {
        self.table.names()
    }
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
pub struct Members(Vec<Membership>, Option<Arc<crate::hosts::HostStore>>);

impl Members {
    pub fn new(grants: impl IntoIterator<Item = Grant>) -> Self {
        let held = grants.into_iter().map(|g| Membership {
            principal: g.principal,
            role: RoleName::new(&g.role),
            scope: g.scope,
        });
        Self(held.collect(), None)
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
        // A host's owner holds its role on it, and a paired key the one that lets
        // it attach. Added last, so a paired key still holds no [`OWNER`].
        if let Some(store) = &self.1 {
            for host in store.held_by(&id) {
                let key = id.strip_prefix("key:");
                let role = if key.is_some() && host.public_key.as_deref() == key {
                    RoleName::new(crate::hosts::MACHINE)
                } else {
                    RoleName::new(format!("{}{}", crate::hosts::HOST_ROLE, host.name))
                };
                let scope = ScopeRef::named(&host.name);
                held.push(Membership {
                    principal: who.clone(),
                    role,
                    scope,
                });
            }
            // An unpaired host key in the waiting room: it may ask, and wait.
            let waiting = id.strip_prefix("key:").filter(|k| store.waiting(k));
            if waiting.is_some_and(|k| store.by_key(k).is_none()) {
                held.push(Membership {
                    principal: who.clone(),
                    role: RoleName::new(crate::hosts::WAITING),
                    scope: ScopeRef::named("waiting"),
                });
            }
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
    gate_with_hosts(runtime, peers, grants, None)
}

/// [`gate`], with the hosts that dialed in (SUPERCONDUCTOR-15) in its memberships
/// and roles, read from the store at every call.
pub fn gate_with_hosts(
    runtime: &Supervisor,
    peers: Vec<String>,
    grants: impl IntoIterator<Item = Grant>,
    hosts: Option<Arc<crate::hosts::HostStore>>,
) -> Gate {
    let mut members = Members::new(grants);
    members.1 = hosts.clone();
    Gate::new(
        Arc::new(members),
        Arc::new(AgentsRoles {
            table: roles(),
            hosts,
        }),
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

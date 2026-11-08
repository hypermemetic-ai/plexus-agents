//! Acting as anyone, on test: agents' half of plexus-login's `people.*`
//! ([`plexus_login::acting`]).
//!
//! plexus-login owns identity — the session switched to any principal, the admin
//! kept as `signed_in_as`, the audit, the test-only gate. This module owns what
//! agents' grants mean, through plexus-login's seam, [`LoginAccess`]:
//!
//! | `LoginAccess` | here |
//! | --- | --- |
//! | `Role` | [`AgentsRole`]: `user` (lists `agents.start`) and `admin` (everything) |
//! | `scope_of` | `*` only: a created user's `user` reaches every placement |
//! | `grantable` | `user@*` (admin is never given to a test user) |
//! | `dev_grant` / `dev_set_grants` | written to [`GrantStore`], the `people_grants` table in the agents db |
//! | `is_admin` | `--admin`/`AGENTS_ADMINS` and `--grant …=admin@*`, then the store |
//! | `held`, `known_users` | the configured grants and the store (never a `key:`) |
//! | everything else | [`SimpleAccess`]'s, as before (sessions, minting, the waiting list) |
//!
//! A created user's roles then reach the gate: [`crate::auth::Members`] reads the
//! store on every call, so `user@*` held there is `agents.start` on any placement,
//! and every signed-in person holds `owner` on their own scope as always. Hosts stay
//! their owners' (pairing, not grants): a test user sees no one else's host.
//!
//! The env (`--env`/`AGENTS_ENV`: `test`, `prod`, `local`; default `local`) decides
//! whether `people.*` is on: only `test`, and never on [`PROD_HOSTS`]. Elsewhere it
//! is mounted but off — `whoami` answers `can_act: false`, the rest are refused
//! with `acting_off` — so the one UI bundle pushes to every environment.

use async_trait::async_trait;
use plexus_login::simple::SimpleAccess;
use plexus_login::LoginAccess;
use plexus_rpc::auth_core::grants::{Membership, ScopeRef, ADMIN};
use plexus_rpc::auth_core::identity::Principal;
use plexus_rpc::auth_core::RoleName;
use plexus_rpc::core::plexus::{AuthContext, Session, Waiting};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};

pub use plexus_login::acting::Environment;

/// Where acting is refused whatever the env says: production's public hosts.
pub const PROD_HOSTS: [&str; 1] = ["agents.menger.sh"];

/// A role agents gives through the people page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentsRole {
    /// [`crate::auth::USER`]: may start runs.
    User,
    /// Holds everything.
    Admin,
}

impl fmt::Display for AgentsRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::User => crate::auth::USER,
            Self::Admin => ADMIN,
        })
    }
}

impl FromStr for AgentsRole {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" => Ok(Self::User),
            "admin" => Ok(Self::Admin),
            other => Err(format!("unknown role `{other}`: the roles are user, admin")),
        }
    }
}

/// Grants written through the people page, kept in the agents db
/// (`people_grants`), read by the gate on every call.
pub struct GrantStore {
    db: Mutex<rusqlite::Connection>,
    rows: RwLock<Vec<(String, String, String)>>,
}

impl fmt::Debug for GrantStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrantStore").field("grants", &self.rows.read().unwrap().len()).finish()
    }
}

impl GrantStore {
    pub fn open(path: &Path) -> rusqlite::Result<Arc<Self>> {
        let db = rusqlite::Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "busy_timeout", 5000)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS people_grants (
                principal TEXT NOT NULL,
                role TEXT NOT NULL,
                scope TEXT NOT NULL,
                granted_at INTEGER NOT NULL,
                PRIMARY KEY (principal, role, scope)
            );",
        )?;
        let rows = {
            let mut q = db.prepare("SELECT principal, role, scope FROM people_grants ORDER BY granted_at")?;
            let r = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            r.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(Arc::new(Self { db: Mutex::new(db), rows: RwLock::new(rows) }))
    }

    /// Every `(principal, role, scope)` kept.
    pub fn all(&self) -> Vec<(String, String, String)> {
        self.rows.read().unwrap().clone()
    }

    /// The memberships `who` holds here.
    pub fn held(&self, who: &Principal) -> Vec<Membership> {
        let id = who.to_string();
        self.rows
            .read()
            .unwrap()
            .iter()
            .filter(|(p, _, _)| *p == id)
            .map(|(_, role, scope)| Membership { principal: who.clone(), role: RoleName::new(role), scope: ScopeRef::parse(scope) })
            .collect()
    }

    pub fn grant(&self, principal: &str, role: &str, scope: &str) -> Result<(), String> {
        let at = plexus_login::sessions::now();
        self.db
            .lock()
            .unwrap()
            .execute(
                "INSERT OR IGNORE INTO people_grants (principal, role, scope, granted_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![principal, role, scope, at],
            )
            .map_err(|e| format!("the grant could not be kept: {e}"))?;
        let mut rows = self.rows.write().unwrap();
        let row = (principal.to_string(), role.to_string(), scope.to_string());
        if !rows.contains(&row) {
            rows.push(row);
        }
        Ok(())
    }

    /// `principal`'s grants here, exactly `grants`.
    pub fn set(&self, principal: &str, grants: &[(String, String)]) -> Result<(), String> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM people_grants WHERE principal = ?1", [principal])
            .map_err(|e| format!("the grants could not be kept: {e}"))?;
        self.rows.write().unwrap().retain(|(p, _, _)| p != principal);
        for (role, scope) in grants {
            self.grant(principal, role, scope)?;
        }
        Ok(())
    }
}

/// agents' [`LoginAccess`]: [`SimpleAccess`] for sessions, with agents' own role
/// vocabulary and grants. See the module docs.
#[derive(Clone)]
pub struct AgentsAccess {
    simple: SimpleAccess,
    /// `--grant` and `--admin` (as `admin@*`), as configured.
    configured: Arc<Vec<crate::auth::Grant>>,
    store: Option<Arc<GrantStore>>,
}

impl AgentsAccess {
    pub fn new(simple: SimpleAccess, configured: Vec<crate::auth::Grant>, store: Option<Arc<GrantStore>>) -> Self {
        Self { simple, configured: Arc::new(configured), store }
    }

    /// The grant store, when there is one.
    pub fn store(&self) -> Option<&Arc<GrantStore>> {
        self.store.as_ref()
    }

    fn rows(&self, principal: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .configured
            .iter()
            .filter(|g| g.principal.to_string() == principal)
            .map(|g| (g.role.clone(), g.scope.as_str().to_string()))
            .collect();
        if let Some(s) = &self.store {
            out.extend(s.all().into_iter().filter(|(p, _, _)| p == principal).map(|(_, r, s)| (r, s)));
        }
        out
    }
}

#[async_trait]
impl LoginAccess for AgentsAccess {
    type Role = AgentsRole;

    fn scope_of(text: &str) -> Result<String, String> {
        match text.trim() {
            "" | "*" => Ok("*".into()),
            other => Err(format!("`{other}`: a role given on the people page holds everywhere — the scope is `*`")),
        }
    }

    async fn dev_grant(&self, principal: &str, scope: &str, role: AgentsRole) -> Result<(), String> {
        principal.parse::<Principal>().map_err(|e| format!("`{principal}` is not a principal ({e})"))?;
        let store = self.store.as_ref().ok_or("no grant store here: grants are set by flags")?;
        store.grant(principal, &role.to_string(), &Self::scope_of(scope)?)
    }

    async fn dev_set_grants(&self, principal: &str, grants: &[(AgentsRole, String)]) -> Result<(), String> {
        let store = self.store.as_ref().ok_or("no grant store here: grants are set by flags")?;
        let rows: Vec<(String, String)> = grants.iter().map(|(r, s)| Ok((r.to_string(), Self::scope_of(s)?))).collect::<Result<_, String>>()?;
        store.set(principal, &rows)
    }

    fn notice(&self, ctx: &AuthContext) {
        self.simple.notice(ctx)
    }
    fn alive(&self, ctx: &AuthContext) -> Result<(), String> {
        self.simple.alive(ctx)
    }
    fn mint(&self, caller: Option<&AuthContext>, ttl_secs: Option<u64>, ceiling: Option<Vec<String>>) -> Result<Session, String> {
        self.simple.mint(caller, ttl_secs, ceiling)
    }
    fn mint_for(&self, ctx: AuthContext, ttl_secs: u64) -> Result<Session, String> {
        self.simple.mint_for(ctx, ttl_secs)
    }
    fn ask_access(&self, ctx: &AuthContext) -> Result<Waiting, String> {
        self.simple.ask_access(ctx)
    }
    fn waiting(&self) -> Vec<Waiting> {
        self.simple.waiting()
    }
    fn logout(&self, caller: Option<&AuthContext>) -> Result<(), String> {
        self.simple.logout(caller)
    }
    fn session_key(&self) -> Option<&str> {
        self.simple.session_key()
    }

    fn is_admin(&self, principal: &str) -> bool {
        self.rows(principal).iter().any(|(r, s)| r == ADMIN && s == "*")
    }

    fn held(&self, principal: &str) -> Vec<String> {
        self.rows(principal).into_iter().map(|(r, s)| format!("{r}@{s}")).collect()
    }

    fn grantable(&self) -> Vec<(AgentsRole, String)> {
        if self.store.is_none() {
            return Vec::new();
        }
        vec![(AgentsRole::User, "*".into())]
    }

    fn known_users(&self) -> Vec<(String, Option<String>)> {
        let mut seen: BTreeMap<String, Option<String>> = BTreeMap::new();
        for g in self.configured.iter() {
            seen.entry(g.principal.to_string()).or_default();
        }
        if let Some(s) = &self.store {
            for (p, _, _) in s.all() {
                seen.entry(p).or_default();
            }
        }
        for w in self.simple.waiting() {
            seen.insert(w.principal, w.name);
        }
        seen.into_iter()
            .filter(|(p, _)| !crate::auth::is_service(p))
            .map(|(p, name)| {
                let name = name.or_else(|| p.strip_prefix("oidc:test/").map(str::to_string));
                (p, name)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_and_scopes_are_agents_own() {
        assert_eq!("User".parse::<AgentsRole>().unwrap(), AgentsRole::User);
        assert!("owner".parse::<AgentsRole>().is_err());
        assert_eq!(AgentsAccess::scope_of("").unwrap(), "*");
        assert!(AgentsAccess::scope_of("local").is_err());
        assert!(<AgentsAccess as LoginAccess>::is_admin_role(&AgentsRole::Admin));
    }

    #[tokio::test]
    async fn a_grant_is_kept_in_the_db_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("agents.db");
        let names = plexus_login::LoginNames::for_app("agents");
        let sessions = Arc::new(plexus_login::sessions::Sessions::new(60, &names));
        let admin = "oidc:workos/user_ADMIN";
        let configured = vec![format!("{admin}=admin@*").parse().unwrap()];
        let a = AgentsAccess::new(SimpleAccess::new(&sessions, &[]).unwrap(), configured, Some(GrantStore::open(&db).unwrap()));
        assert!(a.is_admin(admin) && !a.is_admin("oidc:test/carol"));
        assert_eq!(a.grantable(), vec![(AgentsRole::User, "*".to_string())]);
        a.dev_grant("oidc:test/carol", "*", AgentsRole::User).await.unwrap();
        assert_eq!(a.held("oidc:test/carol"), ["user@*"]);
        let again = GrantStore::open(&db).unwrap();
        let p: Principal = "oidc:test/carol".parse().unwrap();
        assert_eq!(again.held(&p).len(), 1, "kept across a restart");
        let known = a.known_users();
        assert!(known.contains(&("oidc:test/carol".to_string(), Some("carol".to_string()))));
        assert!(known.iter().any(|(p, _)| p == admin));
    }
}

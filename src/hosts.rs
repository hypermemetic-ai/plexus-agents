//! SUPERCONDUCTOR-15, this service's side: hosts that dial in.
//!
//! A host is a machine behind NAT running `plexus-agents host` (see [`crate::host`]).
//! It can't be dialed, so it dials here, and the one socket it opened carries Plexus
//! RPC both ways: its calls to `hosts.*` come in as any client's do, and this
//! service's calls to `agents.on.<host>.*` go out on the same socket and run on the
//! host's own hub, under the host's own gate.
//!
//! # Signing in
//!
//! Both are first class, and the host picks:
//!
//! | mode | credential | principal | owner |
//! |---|---|---|---|
//! | `user` | a plexus-idp access token, validated like any user's | the user | the user |
//! | `machine` | an Ed25519 key proof naming agent [`HOST_AGENT`] | `key:<hex>` | whoever paired the key |
//!
//! A key proof naming [`HOST_AGENT`] is refused at the upgrade unless its key is
//! paired now ([`Hosts::admission`]), and its socket is closed within a second of
//! `hosts.unpair` (the transport's liveness watch asks [`SessionValidator::alive`]).
//! Other key proofs (peer services, AGENTS-18) are unchanged.
//!
//! # Which socket is a host's
//!
//! A method here learns who called, not on which connection: plexus-core 0.7.0
//! routes `{ns}.call` with the `AuthContext` only. So every signed-in socket gets a
//! [`ConnectionHook`] that sees its frames before the method table does. When one
//! carries a `hosts.attach` call, the hook files the connection under the call's
//! `nonce`, and `hosts.attach` takes it from there and checks it's the caller's own.
//! That's the workaround; the fix is plexus-core passing the connection into `.call`.
//!
//! # Calls out
//!
//! [`Wire::call`] writes `{ns}.call` through the connection's `ConnectionWriter`
//! with an id of this side's own (`a-<n>`). The hook takes the answer and the
//! subscription's items, which the transport would otherwise drop (LOCAL-7), and
//! hands them to the call's stream. Dropping the stream sends `{ns}.call_unsub`.
//!
//! # What's kept
//!
//! `{host_id, name, owner, public_key, mode, created, last_seen}` per host, in
//! SQLite. A public key, never a private one, and no token.

use crate::auth::{is_service, Access, AgentsRequest, Caller};
use crate::peer::{meta, one, timed_out, unavailable};
use async_trait::async_trait;
use futures::Stream;
use plexus_auth_core::grants::Grant;
use plexus_core::plexus::{
    AuthContext, AuthzDenyReason, PlexusError, PlexusStream, PlexusStreamItem, ScopeRef,
    SessionValidator,
};
use plexus_transport::websocket::Connection;
use plexus_transport::{ConnectionHook, ConnectionWriter, Hooks, Outbound};
use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

/// The agent id a host's key proof names. A proof naming it must be for a paired key.
pub const HOST_AGENT: &str = "plexus-agents-host";
/// `host:<name>`: the role a host's owner holds on it.
pub const HOST_ROLE: &str = "host:";
/// The role a paired key holds on its host: it may attach, and nothing else.
pub const MACHINE: &str = "host-machine";
/// What `agents.on.<host>.` reaches: a host's own `agents` surface.
pub const HOST_METHODS: [&str; 11] = [
    "start",
    "threads",
    "harnesses",
    "watch",
    "send",
    "interrupt",
    "stop",
    "pending",
    "answer",
    "resume",
    "claudecode.describe",
];
/// The method both sides open a call with, and close it with.
pub const CALL: &str = "plexus-agents.call";
pub const CALL_UNSUB: &str = "plexus-agents.call_unsub";

/// How many items one call may hold unread before it's dropped as behind.
const CALL_BUFFER: usize = 4096;
/// How long a host has to open a call.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

fn now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |d| d.as_secs() as i64)
}

macro_rules! text_param {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub String);
    };
}

text_param!(
    /// A host's name: what `agents.on.<name>` reaches.
    HostName
);
text_param!(
    /// An Ed25519 public key: `key:<64 hex>`, or the 64 hex alone.
    PublicKey
);
text_param!(
    /// A one-time value the attaching connection names its `hosts.attach` with.
    Nonce
);

/// How a host signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Its owner's own plexus-idp login.
    User,
    /// A key its owner paired.
    Machine,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::User => "user",
            Mode::Machine => "machine",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "machine" {
            Mode::Machine
        } else {
            Mode::User
        }
    }
}

/// One host, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostRecord {
    pub host_id: String,
    pub name: String,
    pub owner: String,
    /// The paired key's 64 hex, if one is paired.
    pub public_key: Option<String>,
    /// How it last signed in, or how it will: `machine` once a key is paired.
    pub mode: Mode,
    pub created: i64,
    pub last_seen: Option<i64>,
}

/// A host, as its owner reads it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HostInfo {
    pub name: String,
    pub mode: Mode,
    /// A live, signed-in socket is attached as it now.
    pub online: bool,
    pub last_seen: Option<i64>,
}

/// Everything `hosts` yields.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostsEvent {
    Paired {
        host_id: String,
        name: String,
        public_key: String,
    },
    Unpaired {
        name: String,
    },
    Hosts {
        hosts: Vec<HostInfo>,
    },
    Attached {
        name: String,
        mode: Mode,
    },
    Failed {
        message: String,
    },
}

/// The hosts, in SQLite and in memory. Reads are from memory: the gate asks often.
pub struct HostStore {
    db: Mutex<rusqlite::Connection>,
    rows: RwLock<HashMap<String, HostRecord>>,
}

impl HostStore {
    pub fn open(path: &Path) -> rusqlite::Result<Arc<Self>> {
        let db = rusqlite::Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "busy_timeout", 5000)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS hosts (
                host_id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                owner TEXT NOT NULL,
                public_key TEXT UNIQUE,
                mode TEXT NOT NULL,
                created INTEGER NOT NULL,
                last_seen INTEGER
            );",
        )?;
        let mut rows = HashMap::new();
        {
            let mut q = db.prepare(
                "SELECT host_id, name, owner, public_key, mode, created, last_seen FROM hosts",
            )?;
            let found = q.query_map([], |r| {
                Ok(HostRecord {
                    host_id: r.get(0)?,
                    name: r.get(1)?,
                    owner: r.get(2)?,
                    public_key: r.get(3)?,
                    mode: Mode::parse(&r.get::<_, String>(4)?),
                    created: r.get(5)?,
                    last_seen: r.get(6)?,
                })
            })?;
            for row in found {
                let row = row?;
                rows.insert(row.name.clone(), row);
            }
        }
        Ok(Arc::new(Self {
            db: Mutex::new(db),
            rows: RwLock::new(rows),
        }))
    }

    pub fn get(&self, name: &str) -> Option<HostRecord> {
        self.rows.read().expect("hosts").get(name).cloned()
    }

    pub fn by_key(&self, hex: &str) -> Option<HostRecord> {
        let rows = self.rows.read().expect("hosts");
        rows.values()
            .find(|r| r.public_key.as_deref() == Some(hex))
            .cloned()
    }

    /// The hosts `principal` owns, or is the paired key of.
    pub fn held_by(&self, principal: &str) -> Vec<HostRecord> {
        let key = principal.strip_prefix("key:");
        let rows = self.rows.read().expect("hosts");
        let held = rows
            .values()
            .filter(|r| r.owner == principal || (key.is_some() && r.public_key.as_deref() == key));
        held.cloned().collect()
    }

    /// `owner`'s hosts, by name.
    pub fn owned_by(&self, owner: &str) -> Vec<HostRecord> {
        let rows = self.rows.read().expect("hosts");
        let mut owned: Vec<_> = rows
            .values()
            .filter(|r| r.owner == owner)
            .cloned()
            .collect();
        owned.sort_by(|a, b| a.name.cmp(&b.name));
        owned
    }

    fn put(&self, r: HostRecord) -> Result<(), String> {
        let db = self.db.lock().expect("hosts db");
        db.execute(
            "INSERT INTO hosts (host_id, name, owner, public_key, mode, created, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(host_id) DO UPDATE SET name = ?2, owner = ?3, public_key = ?4,
                 mode = ?5, last_seen = ?7",
            params![
                r.host_id,
                r.name,
                r.owner,
                r.public_key,
                r.mode.as_str(),
                r.created,
                r.last_seen
            ],
        )
        .map_err(|e| e.to_string())?;
        self.rows.write().expect("hosts").insert(r.name.clone(), r);
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<Option<HostRecord>, String> {
        let db = self.db.lock().expect("hosts db");
        db.execute("DELETE FROM hosts WHERE name = ?1", params![name])
            .map_err(|e| e.to_string())?;
        Ok(self.rows.write().expect("hosts").remove(name))
    }

    fn touch(&self, name: &str) {
        let db = self.db.lock().expect("hosts db");
        let at = now();
        let done = db.execute(
            "UPDATE hosts SET last_seen = ?2 WHERE name = ?1",
            params![name, at],
        );
        if matches!(done, Ok(n) if n > 0) {
            if let Some(r) = self.rows.write().expect("hosts").get_mut(name) {
                r.last_seen = Some(at);
            }
        }
    }
}

/// `name` can be a host's: one segment of a method path, `[a-z][a-z0-9_]*`, as
/// `agents.on.<name>.<method>` must be one.
fn valid_name(name: &str) -> Result<(), String> {
    let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
    let ok = name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.len() <= 64
        && name.chars().all(plain)
        && name != "placements"
        && name != "schema";
    ok.then_some(())
        .ok_or_else(|| format!("`{name}` can't be a host's name"))
}

/// A key as `key:<hex>` or the hex alone, as the 64 lowercase hex.
fn key_hex(key: &str) -> Result<String, String> {
    let hex = key.strip_prefix("key:").unwrap_or(key);
    let ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    ok.then(|| hex.to_string())
        .ok_or_else(|| "a public key is `key:` and 64 lowercase hex".to_string())
}

/// The hosts this service knows, and the sockets they're attached on.
pub struct Hosts {
    store: Arc<HostStore>,
    live: Mutex<HashMap<String, Arc<Wire>>>,
    claims: Mutex<HashMap<String, Arc<Wire>>>,
    /// What this service stamps on each call out as `hop.server`: the URL hosts
    /// dial it at. A host refuses a call whose stamp isn't the URL it dialed.
    server: String,
    /// Placements and peers: names a host can't take.
    reserved: RwLock<Vec<String>>,
}

impl Hosts {
    /// The store at `db`, and calls out stamped with `server`.
    pub fn open(db: &Path, server: impl Into<String>) -> anyhow::Result<Arc<Self>> {
        let store = HostStore::open(db)?;
        Ok(Arc::new(Self {
            store,
            live: Mutex::default(),
            claims: Mutex::default(),
            server: server.into().trim_end_matches('/').to_string(),
            reserved: RwLock::default(),
        }))
    }

    pub fn store(&self) -> Arc<HostStore> {
        self.store.clone()
    }

    /// Names a host can't take: this service's placements and peers.
    pub fn reserve(&self, names: impl IntoIterator<Item = String>) {
        self.reserved.write().expect("hosts").extend(names);
    }

    fn reserved(&self, name: &str) -> bool {
        self.reserved
            .read()
            .expect("hosts")
            .iter()
            .any(|n| n == name)
    }

    pub fn knows(&self, name: &str) -> bool {
        self.store.get(name).is_some()
    }

    pub fn owner_of(&self, name: &str) -> Option<String> {
        self.store.get(name).map(|r| r.owner)
    }

    fn wire(&self, name: &str) -> Option<Arc<Wire>> {
        let live = self.live.lock().expect("hosts");
        live.get(name).filter(|w| !w.closed()).cloned()
    }

    /// Whether a live, signed-in socket is attached as `name`.
    pub fn online(&self, name: &str) -> bool {
        self.wire(name).is_some()
    }

    /// The calls this service has open on `name`'s socket, closes not yet answered
    /// among them.
    pub fn open_calls(&self, name: &str) -> usize {
        self.wire(name).map_or(0, |w| w.open())
    }

    /// The names of `owner`'s hosts that are online.
    pub fn online_owned(&self, owner: &str) -> Vec<String> {
        let owned = self.store.owned_by(owner).into_iter().map(|r| r.name);
        owned.filter(|n| self.online(n)).collect()
    }

    /// Each signed-in socket's hook: see the module docs.
    pub fn hooks(self: &Arc<Self>) -> Hooks {
        let hosts = Arc::downgrade(self);
        Hooks::new(move |conn, ext| {
            let ctx = ext.get::<Arc<AuthContext>>()?;
            let principal = ctx.principal()?.to_string();
            let wire = Arc::new(Wire {
                conn: conn.clone(),
                writer: conn.writer()?,
                principal,
                next: AtomicU64::new(1),
                state: Mutex::default(),
                attached: Mutex::default(),
                hosts: hosts.clone(),
            });
            Some(Arc::new(WireHook(wire)) as Arc<dyn ConnectionHook>)
        })
    }

    /// `inner`, refusing a host's key proof whose key isn't paired, at the upgrade
    /// and for as long as its socket is open.
    pub fn admission(&self, inner: Arc<dyn SessionValidator>) -> Arc<dyn SessionValidator> {
        Arc::new(Admission {
            inner,
            store: self.store.clone(),
        })
    }

    fn attach(&self, caller: &str, nonce: &str, name: Option<&str>) -> Result<HostsEvent, String> {
        let wire = self.claims.lock().expect("hosts").remove(nonce);
        let wire = wire.ok_or("no attach with that nonce came in on a signed-in socket")?;
        if wire.principal != caller {
            return Err("that attach came in on another caller's socket".into());
        }
        let at = now();
        let (mut record, mode) = if let Some(key) = caller.strip_prefix("key:") {
            let record = self.store.by_key(key).ok_or("this key is not paired")?;
            if name.is_some_and(|n| n != record.name) {
                return Err(format!("this key is paired as `{}`", record.name));
            }
            (record, Mode::Machine)
        } else {
            let name = name.ok_or("a host attaching as a user names itself")?;
            valid_name(name)?;
            if self.reserved(name) {
                return Err(format!("`{name}` is taken"));
            }
            let record = match self.store.get(name) {
                Some(r) if r.owner != caller => return Err(format!("`{name}` is taken")),
                Some(r) => r,
                None => HostRecord {
                    host_id: uuid::Uuid::new_v4().to_string(),
                    name: name.to_string(),
                    owner: caller.to_string(),
                    public_key: None,
                    mode: Mode::User,
                    created: at,
                    last_seen: None,
                },
            };
            (record, Mode::User)
        };
        record.mode = mode;
        record.last_seen = Some(at);
        let name = record.name.clone();
        self.store.put(record)?;
        *wire.attached.lock().expect("wire") = Some(name.clone());
        self.live.lock().expect("hosts").insert(name.clone(), wire);
        Ok(HostsEvent::Attached { name, mode })
    }

    fn pair(&self, caller: &str, name: &str, key: &str) -> Result<HostsEvent, String> {
        if is_service(caller) {
            return Err("pair as a signed-in user, not as a key".into());
        }
        valid_name(name)?;
        let key = key_hex(key)?;
        if self.reserved(name) {
            return Err(format!("`{name}` is taken"));
        }
        if let Some(other) = self.store.by_key(&key) {
            if other.name != name || other.owner != caller {
                return Err("that key is paired to another host".into());
            }
        }
        let mut record = match self.store.get(name) {
            Some(r) if r.owner != caller => return Err(format!("`{name}` is taken")),
            Some(r) => r,
            None => HostRecord {
                host_id: uuid::Uuid::new_v4().to_string(),
                name: name.to_string(),
                owner: caller.to_string(),
                public_key: None,
                mode: Mode::Machine,
                created: now(),
                last_seen: None,
            },
        };
        record.public_key = Some(key.clone());
        let event = HostsEvent::Paired {
            host_id: record.host_id.clone(),
            name: record.name.clone(),
            public_key: format!("key:{key}"),
        };
        self.store.put(record)?;
        Ok(event)
    }

    fn unpair(&self, caller: &str, name: &str) -> Result<HostsEvent, String> {
        let mine = self.store.get(name).filter(|r| r.owner == caller);
        mine.ok_or_else(|| format!("you have no host `{name}`"))?;
        self.store.remove(name)?;
        if let Some(wire) = self.live.lock().expect("hosts").remove(name) {
            wire.conn.close();
        }
        Ok(HostsEvent::Unpaired {
            name: name.to_string(),
        })
    }

    fn list(&self, caller: &str) -> HostsEvent {
        let hosts = self.store.owned_by(caller).into_iter().map(|r| HostInfo {
            online: self.online(&r.name),
            name: r.name,
            mode: r.mode,
            last_seen: r.last_seen,
        });
        HostsEvent::Hosts {
            hosts: hosts.collect(),
        }
    }

    /// Forwards `agents.<method>` to the host's own hub, over the socket it dialed.
    /// The router has checked `caller` owns it; each item asks again.
    pub async fn call(
        &self,
        name: &str,
        method: &str,
        params: Value,
        caller: String,
    ) -> PlexusStream {
        let Some(wire) = self.wire(name) else {
            return one(unavailable(name, "the host is not connected"));
        };
        let hop = json!({"server": self.server, "call": uuid::Uuid::new_v4().to_string()});
        let store = self.store.clone();
        let host = name.to_string();
        let still = move || store.get(&host).is_some_and(|r| r.owner == caller);
        wire.call(name, method, params, hop, still).await
    }

    fn detached(&self, wire: &Arc<Wire>) {
        self.claims
            .lock()
            .expect("hosts")
            .retain(|_, w| !Arc::ptr_eq(w, wire));
        let Some(name) = wire.attached.lock().expect("wire").clone() else {
            return;
        };
        let mut live = self.live.lock().expect("hosts");
        if live.get(&name).is_some_and(|w| Arc::ptr_eq(w, wire)) {
            live.remove(&name);
        }
        drop(live);
        self.store.touch(&name);
    }
}

/// The refusal for a host that isn't the caller's, worded as one that isn't there.
pub fn not_yours(name: &str) -> PlexusError {
    PlexusError::Forbidden {
        reason: AuthzDenyReason::MissingGrant {
            grant: Grant::new(format!("agents.on.{name}")),
            scope: ScopeRef::named(name),
            roles: Vec::new(),
            held_on: Vec::new(),
        },
    }
}

struct Admission {
    inner: Arc<dyn SessionValidator>,
    store: Arc<HostStore>,
}

impl Admission {
    fn paired(&self, ctx: &AuthContext) -> Result<(), String> {
        if ctx.get_metadata_string("agent").as_deref() != Some(HOST_AGENT) {
            return Ok(());
        }
        let principal = ctx.principal().map(|p| p.to_string()).unwrap_or_default();
        let key = principal.strip_prefix("key:").unwrap_or_default();
        match self.store.by_key(key) {
            Some(_) => Ok(()),
            None => Err("this host's key is not paired".into()),
        }
    }
}

#[async_trait]
impl SessionValidator for Admission {
    async fn validate(&self, value: &str) -> Option<AuthContext> {
        let ctx = self.inner.validate(value).await?;
        self.paired(&ctx).ok()?;
        Some(ctx)
    }

    fn alive(&self, ctx: &AuthContext) -> Result<(), String> {
        self.inner.alive(ctx)?;
        self.paired(ctx)
    }
}

enum Pending {
    /// A call this side opened, waiting for its subscription id.
    Open {
        items: mpsc::Sender<Value>,
        opened: oneshot::Sender<Result<String, String>>,
    },
    /// A close this side sent.
    Close,
}

#[derive(Default)]
struct WireState {
    pending: HashMap<String, Pending>,
    subs: HashMap<String, mpsc::Sender<Value>>,
}

/// One signed-in socket, as this side calls out on it.
pub struct Wire {
    conn: Arc<Connection>,
    writer: ConnectionWriter,
    principal: String,
    next: AtomicU64,
    state: Mutex<WireState>,
    attached: Mutex<Option<String>>,
    hosts: Weak<Hosts>,
}

impl Wire {
    fn closed(&self) -> bool {
        self.conn.is_closed() || self.writer.is_closed()
    }

    fn open(&self) -> usize {
        let st = self.state.lock().expect("wire");
        st.pending.len() + st.subs.len()
    }

    fn id(&self) -> String {
        format!("a-{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn close_call(&self, sub: &str) {
        let id = self.id();
        {
            let mut st = self.state.lock().expect("wire");
            if st.subs.remove(sub).is_none() {
                return;
            }
            st.pending.insert(id.clone(), Pending::Close);
        }
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": CALL_UNSUB, "params": [sub]});
        if self
            .writer
            .try_send(Outbound::Text(frame.to_string()))
            .is_err()
        {
            self.state.lock().expect("wire").pending.remove(&id);
        }
    }

    async fn call(
        self: &Arc<Self>,
        name: &str,
        method: &str,
        params: Value,
        hop: Value,
        still: impl Fn() -> bool + Send + 'static,
    ) -> PlexusStream {
        let id = self.id();
        let (items, rx) = mpsc::channel(CALL_BUFFER);
        let (opened, answer) = oneshot::channel();
        let open = Pending::Open { items, opened };
        self.state
            .lock()
            .expect("wire")
            .pending
            .insert(id.clone(), open);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": CALL,
            "params": {"method": format!("agents.{method}"), "params": params, "hop": hop},
        });
        if self
            .writer
            .send(Outbound::Text(frame.to_string()))
            .await
            .is_err()
        {
            self.state.lock().expect("wire").pending.remove(&id);
            return one(unavailable(name, "the connection closed"));
        }
        let sub = match tokio::time::timeout(OPEN_TIMEOUT, answer).await {
            Ok(Ok(Ok(sub))) => sub,
            Ok(Ok(Err(why))) => return one(unavailable(name, &why)),
            Ok(Err(_)) => return one(unavailable(name, "the connection closed")),
            Err(_) => {
                self.state.lock().expect("wire").pending.remove(&id);
                return one(timed_out(name, OPEN_TIMEOUT));
            }
        };
        let guard = SubGuard {
            wire: self.clone(),
            sub,
        };
        Box::pin(relay(name.to_string(), rx, guard, still))
    }
}

/// Closes the call at the host when its stream is dropped before its end.
struct SubGuard {
    wire: Arc<Wire>,
    sub: String,
}

impl Drop for SubGuard {
    fn drop(&mut self) {
        self.wire.close_call(&self.sub);
    }
}

fn relay(
    name: String,
    mut rx: mpsc::Receiver<Value>,
    guard: SubGuard,
    still: impl Fn() -> bool + Send + 'static,
) -> impl Stream<Item = PlexusStreamItem> + Send + 'static {
    async_stream::stream! {
        let _guard = guard;
        loop {
            let Some(value) = rx.recv().await else {
                yield unavailable(&name, "the connection closed");
                break;
            };
            let Ok(item) = serde_json::from_value::<PlexusStreamItem>(value) else {
                continue;
            };
            let terminal = matches!(
                item,
                PlexusStreamItem::Done { .. } | PlexusStreamItem::Error { .. }
            );
            if !terminal && !still() {
                let mut f = Map::new();
                f.insert("child".into(), name.clone().into());
                let message = format!("host {name} is no longer yours");
                yield PlexusStreamItem::typed_error(meta(&name), message, "forbidden", false, f);
                break;
            }
            yield item;
            if terminal {
                break;
            }
        }
    }
}

/// A frame's id or subscription, as a key: a string as it is, a number as its text.
fn key_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

struct WireHook(Arc<Wire>);

impl ConnectionHook for WireHook {
    fn on_frame(&self, frame: &[u8]) -> bool {
        let wire = &self.0;
        let quiet = {
            let st = wire.state.lock().expect("wire");
            st.pending.is_empty() && st.subs.is_empty()
        };
        let attach = b"hosts.attach";
        if quiet && !frame.windows(attach.len()).any(|w| w == attach) {
            return false;
        }
        let Ok(mut v) = serde_json::from_slice::<Value>(frame) else {
            return false;
        };
        let (has_method, id) = (v.get("method").is_some(), v.get("id").and_then(key_of));
        match (has_method, id) {
            // A call to this side: let it go on, noting an attach's connection.
            (true, Some(_)) => {
                let params = &v["params"];
                if params["method"] == "hosts.attach" {
                    if let (Some(nonce), Some(hosts)) =
                        (params["params"]["nonce"].as_str(), wire.hosts.upgrade())
                    {
                        let mut claims = hosts.claims.lock().expect("hosts");
                        claims.insert(nonce.to_string(), wire.clone());
                    }
                }
                false
            }
            // An answer: to one of this side's calls, or to nothing.
            (false, Some(id)) => {
                let mut st = wire.state.lock().expect("wire");
                let Some(pending) = st.pending.remove(&id) else {
                    return false;
                };
                if let Pending::Open { items, opened } = pending {
                    match v.get("result").and_then(key_of) {
                        Some(sub) => {
                            st.subs.insert(sub.clone(), items);
                            let _ = opened.send(Ok(sub));
                        }
                        None => {
                            let why = v["error"]["message"].as_str().unwrap_or("refused");
                            let _ = opened.send(Err(why.to_string()));
                        }
                    }
                }
                true
            }
            // An item of one of this side's calls, or a notification to nothing.
            (true, None) => {
                let Some(sub) = v["params"].get("subscription").and_then(key_of) else {
                    return false;
                };
                let mut st = wire.state.lock().expect("wire");
                let Some(tx) = st.subs.get(&sub) else {
                    return false;
                };
                if tx.try_send(v["params"]["result"].take()).is_err() {
                    // Behind, or its stream is gone: it ends.
                    st.subs.remove(&sub);
                }
                true
            }
            (false, None) => false,
        }
    }

    fn on_close(&self) {
        {
            let mut st = self.0.state.lock().expect("wire");
            st.pending.clear();
            st.subs.clear();
        }
        if let Some(hosts) = self.0.hosts.upgrade() {
            hosts.detached(&self.0);
        }
    }
}

/// `hosts`: pairing, the registry, and attaching.
#[derive(Clone)]
pub struct HostsActivation {
    hosts: Arc<Hosts>,
    access: Access,
}

impl HostsActivation {
    pub fn new(hosts: Arc<Hosts>) -> Self {
        Self {
            hosts,
            access: Access,
        }
    }
}

fn outcome(r: Result<HostsEvent, String>) -> futures::stream::Iter<std::vec::IntoIter<HostsEvent>> {
    let event = r.unwrap_or_else(|message| HostsEvent::Failed { message });
    futures::stream::iter(vec![event])
}

/// Hosts that dial in: pair a key, list yours, attach a socket.
#[plexus_macros::activation(
    trait = "HostsRpc",
    namespace = "hosts",
    version = "0.1.0",
    auth = "required",
    request = AgentsRequest,
    typed_params
)]
impl HostsActivation {
    /// Pairs a key with a host of yours, so it can sign in as a machine. No restart.
    #[plexus_macros::method(streaming)]
    pub async fn pair(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** the host's name */
        name: HostName,
        /** its Ed25519 public key, `key:<hex>` */
        public_key: PublicKey,
    ) -> impl Stream<Item = HostsEvent> + Send + 'static {
        outcome(self.hosts.pair(&caller.actor().id, &name.0, &public_key.0))
    }

    /// Forgets a host of yours: its key is refused, and its socket is closed.
    #[plexus_macros::method(streaming)]
    pub async fn unpair(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** the host's name */
        name: HostName,
    ) -> impl Stream<Item = HostsEvent> + Send + 'static {
        outcome(self.hosts.unpair(&caller.actor().id, &name.0))
    }

    /// Your hosts: name, mode, whether it's online, when it was last seen.
    #[plexus_macros::method(streaming)]
    pub async fn list(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
    ) -> impl Stream<Item = HostsEvent> + Send + 'static {
        futures::stream::iter(vec![self.hosts.list(&caller.actor().id)])
    }

    /// Makes the socket this call came in on the host's: `agents.on.<name>` goes
    /// out on it. A machine is attached as the name its key is paired with.
    #[plexus_macros::method(streaming)]
    pub async fn attach(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** the host's name; a machine's is its pairing's */
        name: Option<HostName>,
        /** a one-time value naming this call */
        nonce: Nonce,
    ) -> impl Stream<Item = HostsEvent> + Send + 'static {
        let name = name.as_ref().map(|n| n.0.as_str());
        outcome(self.hosts.attach(&caller.actor().id, &nonce.0, name))
    }
}

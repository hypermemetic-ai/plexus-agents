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
//! | `machine` | an Ed25519 key proof naming agent [`HOST_AGENT`] | `key:<hex>` | whoever confirmed its pairing |
//!
//! # Pairing (SUPERCONDUCTOR-18)
//!
//! The Mac never holds its user's credential. A key proof naming [`HOST_AGENT`] whose
//! key isn't paired is admitted to a waiting room ([`Hosts::admission`]): it holds
//! [`WAITING`], which lists `hosts.request` and `hosts.wait` and nothing else, for
//! [`PENDING_TTL`]. `hosts.request` answers with this service's key (`key_s`) and its
//! signature over the Mac's nonce, so the Mac knows whom it's talking to before it
//! shows a code. Both sides compute [`crate::pairing::code`] from the two keys. A
//! signed-in user reads the pending host and the code (`hosts.pairing`), and confirms
//! it (`hosts.confirm`): the user becomes the owner, `hosts.wait` answers `paired`,
//! and the same socket goes on to `hosts.attach` as the machine. Pending requests
//! live in memory only, at most [`MAX_PER_IP`] per client address.
//!
//! A paired key's socket is closed within a second of `hosts.unpair` (the transport's
//! liveness watch asks [`SessionValidator::alive`]). Other key proofs (peer services,
//! AGENTS-18) are unchanged.
//!
//! Each call out carries `hop.sig`: `key_s`'s signature over (call id, method,
//! nonce), which the host checks against the key it pinned. `hosts.attach` answers
//! with `key_s`'s signature over the attach nonce, so a host checks the pin on every
//! dial.
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
//! SQLite. A public key, never a private one, and no token. `key_s`'s seed is a file
//! the deploy holds, never the store.

use crate::auth::{Access, AgentsRequest, Caller};
use crate::pairing::{self, ServerKey};
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
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, watch};

/// The agent id a host's key proof names. A proof naming it must be for a paired key.
pub const HOST_AGENT: &str = "plexus-agents-host";
/// `host:<name>`: the role a host's owner holds on it.
pub const HOST_ROLE: &str = "host:";
/// The role a paired key holds on its host: it may attach, and nothing else.
pub const MACHINE: &str = "host-machine";
/// The role an unpaired host key holds while it waits: `hosts.request`, `hosts.wait`.
pub const WAITING: &str = "host-waiting";
/// How long an unpaired key may wait, and a pairing request stands.
pub const PENDING_TTL: Duration = Duration::from_secs(600);
/// Pairing requests pending at once from one client address.
pub const MAX_PER_IP: usize = 4;
/// The metadata a socket admitted to the waiting room carries: when it came.
const WAITING_SINCE: &str = "host_waiting_since";
/// Unpaired keys waiting at once, from anywhere.
const MAX_WAITING: usize = 1024;
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
    /// A one-time value the attaching connection names its `hosts.attach` with.
    Nonce
);
text_param!(
    /// A pending pairing's id.
    PairingId
);
text_param!(
    /// A pairing code, `XXXX-XXXX`; case, spaces and dashes don't matter.
    PairingCode
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
        owner: String,
    },
    /// `hosts.request`'s answer: this service's key, proven over the Mac's nonce.
    Pairing {
        pairing_id: String,
        /// `key_s`, `key:<hex>`.
        server_key: String,
        /// `key_s`'s signature over [`pairing::pair_message`], base64.
        signature: String,
    },
    /// A pending pairing, as a signed-in user reads it before confirming.
    Pending {
        pairing_id: String,
        name: String,
        /// What the Mac's screen shows, if no one is in the middle.
        code: String,
        public_key: String,
        created: i64,
        expires: i64,
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
        /// `key_s`, and its signature over [`pairing::attach_message`] of the nonce.
        server_key: String,
        signature: String,
    },
    Failed {
        message: String,
    },
}

/// The hosts, in SQLite and in memory. Reads are from memory: the gate asks often.
pub struct HostStore {
    db: Mutex<rusqlite::Connection>,
    rows: RwLock<HashMap<String, HostRecord>>,
    /// Unpaired host keys in the waiting room, and when each came. Memory only.
    waiting: Mutex<HashMap<String, Instant>>,
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
            waiting: Mutex::default(),
        }))
    }

    /// Whether `hex` is an unpaired key waiting, its time not up.
    pub fn waiting(&self, hex: &str) -> bool {
        let waiting = self.waiting.lock().expect("hosts");
        waiting
            .get(hex)
            .is_some_and(|at| at.elapsed() < PENDING_TTL)
    }

    /// Lets `hex` wait, if there's room. A key already waiting keeps its first time.
    fn wait(&self, hex: &str) -> Result<(), String> {
        let mut waiting = self.waiting.lock().expect("hosts");
        waiting.retain(|_, at| at.elapsed() < PENDING_TTL);
        if waiting.contains_key(hex) {
            return Ok(());
        }
        if waiting.len() >= MAX_WAITING {
            return Err("too many hosts are waiting to pair".into());
        }
        waiting.insert(hex.to_string(), Instant::now());
        Ok(())
    }

    fn unwait(&self, hex: &str) {
        self.waiting.lock().expect("hosts").remove(hex);
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

/// A pairing a Mac asked for and no one has confirmed yet. Memory only.
struct PendingPair {
    id: String,
    name: String,
    /// The Mac's key, 64 hex.
    key: String,
    ip: Option<IpAddr>,
    created: Instant,
    created_unix: i64,
    done: watch::Sender<Option<HostsEvent>>,
}

impl PendingPair {
    fn standing(&self) -> bool {
        self.created.elapsed() < PENDING_TTL
    }

    fn code(&self, server: &ServerKey) -> String {
        let key = pairing::key_bytes(&self.key).unwrap_or_default();
        pairing::code(&key, &server.public_bytes())
    }
}

/// The hosts this service knows, and the sockets they're attached on.
pub struct Hosts {
    store: Arc<HostStore>,
    live: Mutex<HashMap<String, Arc<Wire>>>,
    claims: Mutex<HashMap<String, Arc<Wire>>>,
    /// What this service stamps on each call out as `hop.server`: the URL hosts
    /// dial it at. Informational; what a host checks is `hop.sig`.
    server: String,
    /// `key_s`: this service's identity, which hosts pin.
    key: ServerKey,
    pending: Mutex<HashMap<String, PendingPair>>,
    /// Placements and peers: names a host can't take.
    reserved: RwLock<Vec<String>>,
}

impl Hosts {
    /// The store at `db`, calls out stamped with `server` and signed by `key`.
    pub fn open(db: &Path, server: impl Into<String>, key: ServerKey) -> anyhow::Result<Arc<Self>> {
        let store = HostStore::open(db)?;
        Ok(Arc::new(Self {
            store,
            live: Mutex::default(),
            claims: Mutex::default(),
            server: server.into().trim_end_matches('/').to_string(),
            key,
            pending: Mutex::default(),
            reserved: RwLock::default(),
        }))
    }

    /// `key_s`'s public half, `key:<hex>`.
    pub fn server_key(&self) -> String {
        self.key.public()
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
            let forwarded = ext.get::<plexus_transport::proxy::ForwardedClient>();
            let ip = forwarded.map_or(conn.peer().ip(), |f| f.ip);
            let wire = Arc::new(Wire {
                ip: Some(ip),
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

    /// The socket a `hosts.attach` or `hosts.request` naming `nonce` came in on, if
    /// it's the caller's.
    fn claimed(&self, caller: &str, nonce: &str) -> Result<Arc<Wire>, String> {
        let wire = self.claims.lock().expect("hosts").remove(nonce);
        let wire = wire.ok_or("no call with that nonce came in on a signed-in socket")?;
        if wire.principal != caller {
            return Err("that call came in on another caller's socket".into());
        }
        Ok(wire)
    }

    fn attach(&self, caller: &str, nonce: &str, name: Option<&str>) -> Result<HostsEvent, String> {
        let wire = self.claimed(caller, nonce)?;
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
        Ok(HostsEvent::Attached {
            name,
            mode,
            server_key: self.key.public(),
            signature: self.key.sign(&pairing::attach_message(nonce)),
        })
    }

    /// An unpaired key asks to be paired as `name`: see the module docs.
    fn request(&self, caller: &str, name: &str, nonce: &str) -> Result<HostsEvent, String> {
        let key = caller
            .strip_prefix("key:")
            .ok_or("only a host's key asks to pair")?;
        if self.store.by_key(key).is_some() {
            return Err("this key is paired already: attach".into());
        }
        valid_name(name)?;
        if self.reserved(name) {
            return Err(format!("`{name}` is taken"));
        }
        if !(16..=256).contains(&nonce.len()) {
            return Err("a nonce is 16 to 256 characters".into());
        }
        let wire = self.claimed(caller, nonce)?;
        let mut pending = self.pending.lock().expect("hosts");
        pending.retain(|_, p| p.standing());
        let id = match pending.values_mut().find(|p| p.key == key) {
            Some(p) => {
                p.name = name.to_string();
                p.id.clone()
            }
            None => {
                let from_ip = pending.values().filter(|p| p.ip == wire.ip).count();
                if from_ip >= MAX_PER_IP {
                    return Err("too many pairing requests from this address".into());
                }
                let id = uuid::Uuid::new_v4().to_string();
                let request = PendingPair {
                    id: id.clone(),
                    name: name.to_string(),
                    key: key.to_string(),
                    ip: wire.ip,
                    created: Instant::now(),
                    created_unix: now(),
                    done: watch::channel(None).0,
                };
                pending.insert(id.clone(), request);
                id
            }
        };
        Ok(HostsEvent::Pairing {
            pairing_id: id,
            server_key: self.key.public(),
            signature: self.key.sign(&pairing::pair_message(nonce, key)),
        })
    }

    /// What `hosts.wait` waits on: the pairing's outcome, and when it lapses. A key
    /// paired already has its answer now.
    fn waiter(
        &self,
        caller: &str,
        id: &str,
    ) -> Result<(watch::Receiver<Option<HostsEvent>>, Instant), String> {
        let key = caller
            .strip_prefix("key:")
            .ok_or("only a host's key waits")?;
        if let Some(r) = self.store.by_key(key) {
            let done = HostsEvent::Paired {
                host_id: r.host_id,
                name: r.name,
                public_key: caller.to_string(),
                owner: r.owner,
            };
            return Ok((watch::channel(Some(done)).1, Instant::now() + PENDING_TTL));
        }
        let pending = self.pending.lock().expect("hosts");
        let p = pending.get(id).filter(|p| p.key == key && p.standing());
        let p = p.ok_or("no such pairing is pending for this key")?;
        Ok((p.done.subscribe(), p.created + PENDING_TTL))
    }

    fn find_pending(&self, id: Option<&str>, code: Option<&str>) -> Result<HostsEvent, String> {
        let pending = self.pending.lock().expect("hosts");
        let wanted = code.map(pairing::normalize);
        let found = pending
            .values()
            .filter(|p| p.standing())
            .find(|p| match (id, &wanted) {
                (Some(id), _) => p.id == id,
                (None, Some(code)) => pairing::normalize(&p.code(&self.key)) == *code,
                (None, None) => false,
            });
        let p = found.ok_or("no pairing like that is pending")?;
        Ok(HostsEvent::Pending {
            pairing_id: p.id.clone(),
            name: p.name.clone(),
            code: p.code(&self.key),
            public_key: format!("key:{}", p.key),
            created: p.created_unix,
            expires: p.created_unix + PENDING_TTL.as_secs() as i64,
        })
    }

    /// A signed-in user confirms a pending pairing: it becomes theirs.
    /// The gate let only a user, or a key granted it (`--admin key:<hex>` where
    /// there's no WorkOS), this far; a host's own key never confirms.
    fn confirm(&self, caller: &str, id: &str) -> Result<HostsEvent, String> {
        let mut pending = self.pending.lock().expect("hosts");
        let p = pending.get(id).filter(|p| p.standing());
        let p = p.ok_or("no such pairing is pending")?;
        if let Some(key) = caller.strip_prefix("key:") {
            if key == p.key || self.store.by_key(key).is_some() {
                return Err("a host's key can't confirm a pairing".into());
            }
        }
        let event = self.pair(caller, &p.name, &p.key)?;
        let p = pending.remove(id).expect("found above");
        drop(pending);
        self.store.unwait(&p.key);
        p.done.send_replace(Some(event.clone()));
        Ok(event)
    }

    fn pair(&self, caller: &str, name: &str, key: &str) -> Result<HostsEvent, String> {
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
            owner: record.owner.clone(),
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
        let (call, nonce) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let sig = self.key.sign(&pairing::call_message(
            &call,
            &format!("agents.{method}"),
            &nonce,
        ));
        let hop = json!({"server": self.server, "call": call, "nonce": nonce, "sig": sig});
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
    /// A host's key, if `ctx` is a host's key proof.
    fn host_key(ctx: &AuthContext) -> Option<String> {
        if ctx.get_metadata_string("agent").as_deref() != Some(HOST_AGENT) {
            return None;
        }
        let principal = ctx.principal()?.to_string();
        Some(principal.strip_prefix("key:")?.to_string())
    }

    /// Paired, or a socket admitted to the waiting room with time left. A socket
    /// admitted as paired doesn't become a waiting one when its key is unpaired:
    /// it closes.
    fn paired(&self, ctx: &AuthContext) -> Result<(), String> {
        let Some(key) = Self::host_key(ctx) else {
            return Ok(());
        };
        if self.store.by_key(&key).is_some() {
            return Ok(());
        }
        let since = ctx.metadata.get(WAITING_SINCE).and_then(Value::as_i64);
        if since.is_some_and(|t| now() - t < PENDING_TTL.as_secs() as i64) {
            return Ok(());
        }
        Err("this host's key is not paired, and its time to pair is up".into())
    }
}

#[async_trait]
impl SessionValidator for Admission {
    async fn validate(&self, value: &str) -> Option<AuthContext> {
        let mut ctx = self.inner.validate(value).await?;
        if let Some(key) = Self::host_key(&ctx) {
            if self.store.by_key(&key).is_none() {
                self.store.wait(&key).ok()?;
                if let Some(m) = ctx.metadata.as_object_mut() {
                    m.insert(WAITING_SINCE.into(), now().into());
                }
            }
        }
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
    /// The client's address, as the transport (and a trusted proxy) says.
    ip: Option<IpAddr>,
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

impl WireHook {
    /// Whether this socket is an unpaired host key's, in the waiting room.
    fn waiting(&self) -> bool {
        let Some(key) = self.0.principal.strip_prefix("key:") else {
            return false;
        };
        let Some(hosts) = self.0.hosts.upgrade() else {
            return false;
        };
        hosts.store.by_key(key).is_none() && hosts.store.waiting(key)
    }

    /// In the waiting room only `hosts.request` and `hosts.wait` answer, and the
    /// closes of their own calls: the gate would let a `public` method or a
    /// `.schema` through to anyone, so this refuses them first.
    fn refuse_waiting(&self, v: &Value, id: &Value) -> bool {
        let method = v["method"].as_str().unwrap_or_default();
        let allowed = if method == CALL {
            matches!(
                v["params"]["method"].as_str(),
                Some("hosts.request" | "hosts.wait")
            )
        } else {
            method == CALL_UNSUB
        };
        if allowed {
            return false;
        }
        let error = json!({"code": -32001,
            "message": "an unpaired host may call hosts.request and hosts.wait, and nothing else"});
        if !id.is_null() {
            let refusal = json!({"jsonrpc": "2.0", "id": id, "error": error});
            let _ = self.0.writer.try_send(Outbound::Text(refusal.to_string()));
        }
        true
    }
}

impl ConnectionHook for WireHook {
    fn on_frame(&self, frame: &[u8]) -> bool {
        let wire = &self.0;
        if self.waiting() {
            // Batches and frames that don't parse go nowhere from the waiting room.
            let Ok(v) = serde_json::from_slice::<Value>(frame) else {
                return true;
            };
            if !v.is_object() {
                return true;
            }
            if v.get("method").is_some() {
                let id = v.get("id").cloned().unwrap_or(Value::Null);
                if self.refuse_waiting(&v, &id) {
                    return true;
                }
            }
        }
        let quiet = {
            let st = wire.state.lock().expect("wire");
            st.pending.is_empty() && st.subs.is_empty()
        };
        let names = |m: &[u8]| frame.windows(m.len()).any(|w| w == m);
        if quiet && !names(b"hosts.attach") && !names(b"hosts.request") {
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
                if params["method"] == "hosts.attach" || params["method"] == "hosts.request" {
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

async fn waited(mut rx: watch::Receiver<Option<HostsEvent>>, until: Instant) -> HostsEvent {
    let lapsed = || HostsEvent::Failed {
        message: "the pairing lapsed".into(),
    };
    let got = tokio::time::timeout_at(until.into(), rx.wait_for(Option::is_some)).await;
    match got {
        Ok(Ok(done)) => done.clone().unwrap_or_else(lapsed),
        _ => lapsed(),
    }
}

fn outcome(r: Result<HostsEvent, String>) -> futures::stream::Iter<std::vec::IntoIter<HostsEvent>> {
    let event = r.unwrap_or_else(|message| HostsEvent::Failed { message });
    futures::stream::iter(vec![event])
}

/// Hosts that dial in: pair a key by a code both sides show, list yours, attach a socket.
#[plexus_macros::activation(
    trait = "HostsRpc",
    namespace = "hosts",
    version = "0.1.0",
    auth = "required",
    request = AgentsRequest,
    typed_params
)]
impl HostsActivation {
    /// An unpaired host's key asks to pair as `name`. Answers with this service's
    /// key and its signature over `nonce`, so the host can check whom it's talking to.
    #[plexus_macros::method(streaming)]
    pub async fn request(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** the name the host asks for */
        name: HostName,
        /** a one-time value the service signs: 16 to 256 characters */
        nonce: Nonce,
    ) -> impl Stream<Item = HostsEvent> + Send + 'static {
        outcome(self.hosts.request(&caller.actor().id, &name.0, &nonce.0))
    }

    /// Waits until a signed-in user confirms this key's pairing, or it lapses.
    #[plexus_macros::method(streaming)]
    pub async fn wait(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** what `hosts.request` answered */
        pairing_id: PairingId,
    ) -> impl Stream<Item = HostsEvent> + Send + 'static {
        let waiter = self.hosts.waiter(&caller.actor().id, &pairing_id.0);
        async_stream::stream! {
            yield match waiter {
                Err(message) => HostsEvent::Failed { message },
                Ok((rx, until)) => waited(rx, until).await,
            };
        }
    }

    /// A pending pairing, by its id (the link) or the code its host shows.
    #[plexus_macros::method(streaming)]
    pub async fn pairing(
        &self,
        #[from_auth(self.access.verify)] _caller: Caller,
        /** the id in the host's link */
        pairing_id: Option<PairingId>,
        /** or the code the host shows, `XXXX-XXXX` */
        code: Option<PairingCode>,
    ) -> impl Stream<Item = HostsEvent> + Send + 'static {
        let id = pairing_id.as_ref().map(|p| p.0.as_str());
        outcome(
            self.hosts
                .find_pending(id, code.as_ref().map(|c| c.0.as_str())),
        )
    }

    /// Confirms a pending pairing: the host becomes yours, and goes on as a machine.
    #[plexus_macros::method(streaming)]
    pub async fn confirm(
        &self,
        #[from_auth(self.access.verify)] caller: Caller,
        /** the pending pairing */
        pairing_id: PairingId,
    ) -> impl Stream<Item = HostsEvent> + Send + 'static {
        outcome(self.hosts.confirm(&caller.actor().id, &pairing_id.0))
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

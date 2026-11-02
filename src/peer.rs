//! One peer service, and the socket to it.
//!
//! [`crate::on`] is the router. This is its far end: a WebSocket to the peer's
//! `{ns}.call`, signed in as this service with key proof. It takes RemoteChild's hop
//! shape, error kinds and backoff, and isn't RemoteChild, which refuses a remote with
//! children, can't mount after the hub is shared, and stops at a schema change. Nothing
//! is mounted here and no schema is kept: `.schema` is a call like any other.
//!
//! No caller's credential crosses. The peer sees this service's `key:<hex>`.
//!
//! # The seam
//!
//! [`Peer::hop`] and [`Peer::dial`] are where a HOP chain goes. `dial` proves the
//! service, and stays. `hop` says who the call is for: nothing today, and the signed
//! `hop.chain` of HOP-4 once AGENTS-24 builds the chain from the verified caller,
//! with `peer_accepts` read from the peer's `_info` here in `dial`.

use base64::Engine;
use futures::StreamExt;
use jsonrpsee::core::client::{Subscription, SubscriptionClientT};
use jsonrpsee::core::params::{ArrayParams, ObjectParams};
use jsonrpsee::core::ClientError;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use plexus_auth_core::adapters::{key_principal, sign_key_proof};
use plexus_auth_core::grants::Reach;
use plexus_core::plexus::{
    error_item, Backoff, PlexusContext, PlexusError, PlexusStream, PlexusStreamItem, StreamMetadata,
};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many items one call may hold unread before it's dropped as behind.
const CALL_BUFFER: usize = 4096;

/// How to reach one peer.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub name: String,
    pub url: String,
    /// A 32-byte Ed25519 seed, mode 0600. Read at each dial and never kept.
    pub key: PathBuf,
    /// The proof's agent id: `plexus-agents`.
    pub agent: String,
    /// The dial, `_info`, and each call's open.
    pub timeout: Duration,
    /// Calls open at once. Past it, `too_many_calls`.
    pub max_calls: usize,
    pub backoff: Backoff,
    /// A proof's sign-in ends 300 s after it's made and the peer then closes the
    /// socket, so a new call goes out on a new socket before that.
    pub refresh: Duration,
}

impl PeerConfig {
    /// `name=url`, with the defaults.
    pub fn parse(spec: &str, key: &str) -> Result<Self, String> {
        let (name, url) = spec
            .split_once('=')
            .ok_or(format!("`{spec}` is not name=url"))?;
        let plain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if name.is_empty() || !name.chars().all(plain) || name == "placements" || name == "schema" {
            return Err(format!("`{name}` can't be a peer's name"));
        }
        if !url.starts_with("ws://") && !url.starts_with("wss://") {
            return Err(format!("`{url}` is not a WebSocket URL"));
        }
        Ok(Self {
            name: name.to_string(),
            url: url.to_string(),
            key: key.into(),
            agent: "plexus-agents".to_string(),
            timeout: Duration::from_secs(5),
            max_calls: 64,
            backoff: Backoff::default(),
            refresh: Duration::from_secs(240),
        })
    }
}

/// The file's seed. Refused when group or others may read it.
fn read_seed(path: &Path) -> Result<[u8; 32], String> {
    use std::os::unix::fs::PermissionsExt;
    let shown = path.display();
    let about = |e: std::io::Error| format!("the key file {shown}: {e}");
    let mode = std::fs::metadata(path).map_err(about)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!("the key file {shown} must be mode 0600"));
    }
    let raw = std::fs::read(path).map_err(about)?;
    let text: Vec<u8> = raw
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let decoded = base64::engine::general_purpose::STANDARD.decode(&text);
    let bytes = decoded.unwrap_or(raw);
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| format!("{shown} holds no 32-byte seed"))
}

fn principal_of(seed: &[u8; 32]) -> String {
    let key = ed25519_dalek::SigningKey::from_bytes(seed);
    key_principal(key.verifying_key().as_bytes()).to_string()
}

/// The `key:<hex>` a key file proves.
pub fn principal_of_file(path: &Path) -> Result<String, String> {
    read_seed(path).map(|seed| principal_of(&seed))
}

fn meta(name: &str) -> StreamMetadata {
    StreamMetadata::new(vec![name.to_string()], PlexusContext::hash())
}

fn fields(name: &str) -> Map<String, Value> {
    let mut f = Map::new();
    f.insert("child".into(), name.into());
    f
}

/// `child_unavailable`: one error, and the end.
fn unavailable(name: &str, why: &str) -> PlexusStreamItem {
    let mut f = fields(name);
    f.insert("reason".into(), why.into());
    let message = format!("child {name} is unavailable: {why}");
    PlexusStreamItem::typed_error(meta(name), message, "child_unavailable", true, f)
}

/// `child_timeout`: the peer didn't open the call in time.
fn timed_out(name: &str, timeout: Duration) -> PlexusStreamItem {
    let mut f = fields(name);
    f.insert("timeout_ms".into(), (timeout.as_millis() as u64).into());
    let message = format!("child {name} did not open the call in {timeout:?}");
    PlexusStreamItem::typed_error(meta(name), message, "child_timeout", true, f)
}

fn too_many(name: &str, max: usize) -> PlexusStreamItem {
    let mut f = fields(name);
    f.insert("max".into(), max.into());
    let message = format!("too many calls open on child {name} (at most {max})");
    PlexusStreamItem::typed_error(meta(name), message, "too_many_calls", true, f)
}

fn one(item: PlexusStreamItem) -> PlexusStream {
    Box::pin(futures::stream::iter([item]))
}

/// The gate's refusal, as the last item of a call held open.
fn refused(name: &str, e: PlexusError) -> PlexusStreamItem {
    let item = error_item(name, &e);
    serde_json::from_value(item).unwrap_or_else(|_| unavailable(name, "refused"))
}

/// One socket, and when its proof was made.
struct Link {
    client: Arc<WsClient>,
    ns: String,
    made: Instant,
}

#[derive(Default)]
struct State {
    link: Option<Arc<Link>>,
    down_since: Option<Instant>,
    why: String,
    window: VecDeque<Instant>,
}

/// A service `agents.on.<name>` forwards to.
pub struct Peer {
    config: PeerConfig,
    principal: String,
    state: Mutex<State>,
    dialing: tokio::sync::Mutex<()>,
    open: Arc<AtomicUsize>,
}

impl Peer {
    /// Reads the key once, so a bad one stops the service at start.
    pub fn new(config: PeerConfig) -> Result<Arc<Self>, String> {
        let principal = principal_of(&read_seed(&config.key)?);
        Ok(Arc::new(Self {
            config,
            principal,
            state: Mutex::new(State::default()),
            dialing: tokio::sync::Mutex::new(()),
            open: Arc::new(AtomicUsize::new(0)),
        }))
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// What this call says beyond its method: the service acts, for no one, with no
    /// limit. The peer ignores it on 0.7.0, and it's where the chain goes (see above).
    fn hop(&self) -> Value {
        json!({
            "principal": self.principal,
            "for": [],
            "limit": null,
            "call": uuid::Uuid::new_v4().to_string(),
        })
    }

    /// A new socket, signed in as this service, and the peer's namespace from `_info`.
    async fn dial(&self) -> Result<Link, String> {
        let c = &self.config;
        let seed = read_seed(&c.key)?;
        let mut nonce = [0u8; 32];
        nonce[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        nonce[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let now = SystemTime::now().duration_since(UNIX_EPOCH);
        let ts = now.map_or(0, |d| d.as_secs() as i64);
        let proof = sign_key_proof(&seed, c.url.trim_end_matches('/'), &c.agent, &nonce, ts);
        let mut headers = HeaderMap::new();
        let bearer =
            HeaderValue::from_str(&format!("Bearer {proof}")).map_err(|e| e.to_string())?;
        headers.insert("authorization", bearer);
        let made = Instant::now();
        let client = WsClientBuilder::default()
            .set_headers(headers)
            .connection_timeout(c.timeout)
            .request_timeout(c.timeout)
            .max_buffer_capacity_per_subscription(CALL_BUFFER)
            .build(&c.url)
            .await
            .map_err(|e| format!("dialing {}: {e}", c.url))?;
        let mut info: Subscription<Value> = client
            .subscribe("_info", ArrayParams::new(), "_info_unsub")
            .await
            .map_err(|e| format!("its `_info`: {e}"))?;
        let first = tokio::time::timeout(c.timeout, info.next()).await;
        let item = first.ok().flatten().and_then(Result::ok);
        let ns = item.as_ref().and_then(|i| i["content"]["backend"].as_str());
        let ns = ns.ok_or("its `_info` names no backend")?.to_string();
        Ok(Link {
            client: Arc::new(client),
            ns,
            made,
        })
    }

    /// The link if it's up and its proof is young. One that closed is noted as down.
    fn live(&self) -> Option<Arc<Link>> {
        let mut st = self.state.lock().expect("peer state");
        let link = st.link.clone()?;
        if !link.client.is_connected() {
            st.link = None;
            st.down_since = Some(Instant::now());
            st.why = "the connection closed".to_string();
            return None;
        }
        (link.made.elapsed() < self.config.refresh).then_some(link)
    }

    fn ageing(&self) -> bool {
        let st = self.state.lock().expect("peer state");
        st.link.as_ref().is_some_and(|l| l.client.is_connected())
    }

    /// A live link, dialing if there's none, within the backoff. An old proof is
    /// replaced with no wait: nothing went wrong. Calls open on the old link keep it.
    async fn link(&self) -> Result<Arc<Link>, PlexusStreamItem> {
        if let Some(l) = self.live() {
            return Ok(l);
        }
        let _dialing = self.dialing.lock().await;
        if let Some(l) = self.live() {
            return Ok(l);
        }
        let name = self.name();
        if !self.ageing() {
            let wait = {
                let mut st = self.state.lock().expect("peer state");
                let b = self.config.backoff;
                let now = Instant::now();
                while st
                    .window
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
                {
                    st.window.pop_front();
                }
                if st.window.len() >= b.per_minute {
                    let why = format!("{} (dialed too often: waiting a minute)", st.why);
                    return Err(unavailable(name, &why));
                }
                let due = st.down_since.map(|d| d + b.delay(st.window.len()));
                due.map_or(Duration::ZERO, |d| d.saturating_duration_since(now))
            };
            if wait > self.config.timeout {
                let st = self.state.lock().expect("peer state");
                let why = format!("{} (dialing again in {:.1} s)", st.why, wait.as_secs_f64());
                return Err(unavailable(name, &why));
            }
            tokio::time::sleep(wait).await;
            let mut st = self.state.lock().expect("peer state");
            st.window.push_back(Instant::now());
        }
        match self.dial().await {
            Ok(link) => {
                let link = Arc::new(link);
                let mut st = self.state.lock().expect("peer state");
                st.link = Some(link.clone());
                st.down_since = None;
                Ok(link)
            }
            Err(why) => {
                let mut st = self.state.lock().expect("peer state");
                st.link = None;
                st.down_since = Some(Instant::now());
                st.why.clone_from(&why);
                Err(unavailable(name, &why))
            }
        }
    }

    /// The hash of the peer's `agents` schema, if it answers now.
    pub async fn schema_hash(&self) -> Option<String> {
        let mut stream = self.call("schema", json!({}), Reach::everywhere()).await;
        while let Some(item) = stream.next().await {
            if let PlexusStreamItem::Data { content, .. } = item {
                let hash = content.into_value().get("hash")?.as_str()?.to_string();
                return Some(hash);
            }
        }
        None
    }

    /// Forwards `agents.<method>`. Items come back unchanged until the first terminal
    /// one. Dropping the stream unsubscribes, which drops the call at the peer. `reach`
    /// is the tunnel's check, asked again per item, as a grant can move mid-stream.
    pub async fn call(&self, method: &str, params: Value, reach: Reach) -> PlexusStream {
        let max = self.config.max_calls;
        if self.open.fetch_add(1, Ordering::AcqRel) >= max {
            self.open.fetch_sub(1, Ordering::AcqRel);
            return one(too_many(self.name(), max));
        }
        let guard = OpenGuard(self.open.clone());
        let link = match self.link().await {
            Ok(l) => l,
            Err(item) => return one(item),
        };
        let mut call = ObjectParams::new();
        let frame = [
            ("method", json!(format!("agents.{method}"))),
            ("params", params),
            ("hop", self.hop()),
        ];
        for (k, v) in frame {
            if let Err(e) = call.insert(k, v) {
                return one(unavailable(self.name(), &e.to_string()));
            }
        }
        let (open, close) = (
            format!("{}.call", link.ns),
            format!("{}.call_unsub", link.ns),
        );
        let sub: Subscription<Value> = match link.client.subscribe(&open, call, &close).await {
            Ok(s) => s,
            Err(ClientError::RequestTimeout) => {
                return one(timed_out(self.name(), self.config.timeout))
            }
            Err(e) => return one(unavailable(self.name(), &e.to_string())),
        };
        let name = self.name().to_string();
        Box::pin(relay(name, link, sub, reach, guard))
    }
}

struct OpenGuard(Arc<AtomicUsize>);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn relay(
    name: String,
    link: Arc<Link>,
    mut sub: Subscription<Value>,
    reach: Reach,
    guard: OpenGuard,
) -> impl futures::Stream<Item = PlexusStreamItem> + Send + 'static {
    async_stream::stream! {
        // The socket dies with its last client, so a call holds it open.
        let _held = (link, guard);
        loop {
            let value = match sub.next().await {
                Some(Ok(v)) => v,
                Some(Err(e)) => {
                    yield unavailable(&name, &e.to_string());
                    break;
                }
                None => {
                    let why = match sub.close_reason() {
                        Some(jsonrpsee::core::client::SubscriptionCloseReason::Lagged) => {
                            "the caller fell behind"
                        }
                        _ => "the connection closed",
                    };
                    yield unavailable(&name, why);
                    break;
                }
            };
            // A kind this service doesn't know is skipped, as a reader does.
            let Ok(item) = serde_json::from_value::<PlexusStreamItem>(value) else {
                continue;
            };
            let terminal = matches!(
                item,
                PlexusStreamItem::Done { .. } | PlexusStreamItem::Error { .. }
            );
            if !terminal {
                if let Err(refusal) = reach.recheck() {
                    yield refused(&name, refusal.into());
                    break;
                }
            }
            yield item;
            if terminal {
                break;
            }
        }
    }
}

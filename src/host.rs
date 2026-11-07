//! SUPERCONDUCTOR-15, a host's side: `plexus-agents host --server <ws> --name <n>`.
//!
//! A host is behind NAT and opens no port. It dials the agents service, and that one
//! socket carries both directions (see [`crate::hosts`] for the service's side):
//!
//! | frame from the service | what this does |
//! |---|---|
//! | a request, `{ns}.call` | puts it through [`Session::admit`], then routes it on this host's own hub, gate and all, and answers as a subscription |
//! | a request, `{ns}.call_unsub` | drops that call |
//! | any other request | `-32601` |
//! | an answer, or an item of a call this host opened | to that call (`hosts.attach`, `hosts.pair`) |
//!
//! # This host's gate
//!
//! A call is routed only when `hop.sig` is the server key's signature over (call
//! id, method, nonce) (SUPERCONDUCTOR-18), a call id is used once, and its method is
//! under `agents.` and not `agents.on.` (`max_depth` is 0 here too). It runs as
//! [`SERVER_PRINCIPAL`], which [`server_grant`] makes `admin` on this hub and
//! nothing else holds: nothing else reaches this hub at all.
//!
//! The server key is the one this host pinned (`<pins>/<server host>.pin`). Each
//! dial's `hosts.attach` names a fresh nonce and the server answers with its
//! signature over it; a server that can't prove the pinned key is refused, and the
//! host stops. With no pin yet, the key the server proves is pinned.
//!
//! # Signing in, and again
//!
//! - **User:** `--token-file`, a plexus-idp access token, read again at each dial so
//!   whatever refreshes it (synapse's credential store, `idp refresh`) rotates it
//!   here. The service closes the socket at the token's `exp`, so this host dials a
//!   new one a minute before, and the old one serves its open calls until then. It
//!   doesn't pair: the token's user owns the host.
//! - **Machine:** `--key`, an Ed25519 seed (mode 0600). A proof stands 300 s, so a
//!   new socket every 240 s. An unpaired key pairs on its first dial: it asks
//!   (`hosts.request`), checks the server's signature, prints the code
//!   [`crate::pairing::code`] of both keys with a link and a QR code, and waits
//!   (`hosts.wait`) for a signed-in user to confirm the same code in the browser.
//!   Then it pins the server key and attaches as the machine on the same socket.
//!   This host never holds its user's credential.
//!
//! A socket that drops is dialed again with backoff, and attaches as the same name.

use crate::hosts::{CALL, CALL_UNSUB, HOST_AGENT};
use crate::pairing;
use crate::peer::{meta, principal_of, read_seed};
use anyhow::{anyhow, Context};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use plexus_auth_core::adapters::sign_key_proof;
use plexus_auth_core::identity::Authenticated;
use plexus_core::plexus::{error_item, AuthContext, DynamicHub, PlexusStreamItem};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// Who calls arriving on the socket run as, on this host's hub.
pub const SERVER_PRINCIPAL: &str = "apikey:dialed-server";

/// The one grant this host's hub needs: the server it dialed is `admin` here.
pub fn server_grant() -> crate::auth::Grant {
    format!("{SERVER_PRINCIPAL}=admin@*")
        .parse()
        .expect("a grant")
}

/// How a host dials.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// The service's WebSocket URL.
    pub server: String,
    pub name: String,
    /// A plexus-idp access token, read at each dial: user mode.
    pub token_file: Option<PathBuf>,
    /// A 32-byte Ed25519 seed, mode 0600: machine mode.
    pub key: Option<PathBuf>,
    /// Where server keys are pinned, one `<server host>.pin` each. None: pinned for
    /// the process only.
    pub pins: Option<PathBuf>,
    /// The page a person confirms a pairing on; from `server` if not given.
    pub app: Option<String>,
    /// How long one `hosts.wait` may take before this host asks again.
    pub pair_wait: Duration,
    /// A machine's new socket, before its proof's 300 s are up.
    pub refresh: Duration,
    /// A user's new socket, this long before its token's `exp`.
    pub margin: Duration,
    /// This host's own calls to the service.
    pub timeout: Duration,
    pub backoff_max: Duration,
}

impl HostConfig {
    pub fn new(server: &str, name: &str) -> Self {
        Self {
            server: server.trim_end_matches('/').to_string(),
            name: name.to_string(),
            token_file: None,
            key: None,
            pins: None,
            app: None,
            pair_wait: Duration::from_secs(600),
            refresh: Duration::from_secs(240),
            margin: Duration::from_secs(60),
            timeout: Duration::from_secs(5),
            backoff_max: Duration::from_secs(5),
        }
    }
}

fn unix_now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |d| d.as_secs() as i64)
}

/// A JWT's `exp`, read but not checked: the service checks it.
fn token_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?["exp"].as_i64()
}

/// A credential for one dial, and when to dial again.
struct SignIn {
    bearer: String,
    renew: Option<Instant>,
    machine: bool,
}

fn sign_in(config: &HostConfig, machine: bool) -> anyhow::Result<SignIn> {
    if machine {
        let key = config.key.as_ref().context("machine mode needs --key")?;
        let seed = read_seed(key).map_err(|e| anyhow!(e))?;
        let mut nonce = [0u8; 32];
        nonce[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        nonce[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let proof = sign_key_proof(&seed, &config.server, HOST_AGENT, &nonce, unix_now());
        return Ok(SignIn {
            bearer: proof,
            renew: Some(Instant::now() + config.refresh),
            machine,
        });
    }
    let file = config
        .token_file
        .as_ref()
        .context("user mode needs --token-file")?;
    let token = std::fs::read_to_string(file).with_context(|| format!("{}", file.display()))?;
    let token = token.trim().to_string();
    let renew = token_exp(&token).map(|exp| {
        let left = (exp - unix_now()).max(0) as u64;
        let left = Duration::from_secs(left).saturating_sub(config.margin);
        Instant::now() + left.max(Duration::from_secs(1))
    });
    Ok(SignIn {
        bearer: token,
        renew,
        machine,
    })
}

/// Dials, attaches, and dials again, until the process ends.
pub async fn run(hub: Arc<DynamicHub>, config: HostConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.token_file.is_some() || config.key.is_some(),
        "give --token-file, --key, or both"
    );
    let machine = config.token_file.is_none();
    let mut backoff = Duration::from_millis(200);
    let mut shown = None;
    loop {
        let session = match dial(&hub, &config, machine, &mut shown).await {
            Ok(s) => s,
            Err(e) if e.is::<Untrusted>() => {
                eprintln!("plexus-agents host: {e:#}");
                return Err(e);
            }
            Err(e) => {
                eprintln!("plexus-agents host: {e:#}; dialing again in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(config.backoff_max);
                continue;
            }
        };
        backoff = Duration::from_millis(200);
        let mut closed = session.closed.clone();
        let renew = session.renew;
        tokio::select! {
            _ = closed.wait_for(|c| *c) => {
                eprintln!("plexus-agents host: the socket closed; dialing again");
            }
            _ = async {
                match renew {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            } => {
                // The old socket serves its open calls until the service ends it.
            }
        }
    }
}

/// A server that can't prove the key this host pinned, or proves none: the host
/// stops rather than dial it again.
#[derive(Debug)]
pub struct Untrusted(pub String);

impl std::fmt::Display for Untrusted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refusing the server: {}", self.0)
    }
}

impl std::error::Error for Untrusted {}

/// `wss://agents.example:443/ws` as `agents.example_443`: a pin file's name.
pub fn server_host(server: &str) -> String {
    let rest = server.split_once("://").map_or(server, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let safe = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '-';
    authority
        .chars()
        .map(|c| if safe(c) { c } else { '_' })
        .collect()
}

/// Where `server`'s key is pinned.
pub fn pin_path(pins: &Path, server: &str) -> PathBuf {
    pins.join(format!("{}.pin", server_host(server)))
}

fn read_pin(config: &HostConfig) -> Option<String> {
    let path = pin_path(config.pins.as_ref()?, &config.server);
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.trim().to_string())
}

fn write_pin(config: &HostConfig, key: &str) -> anyhow::Result<()> {
    let Some(pins) = &config.pins else {
        return Ok(());
    };
    std::fs::create_dir_all(pins)?;
    let path = pin_path(pins, &config.server);
    std::fs::write(&path, format!("{key}\n")).with_context(|| format!("{}", path.display()))?;
    eprintln!("plexus-agents host: pinned {key} for {}", config.server);
    Ok(())
}

/// The server key `answer` proves over `message`, if it's the pinned one (or there's
/// no pin yet). Anything else is [`Untrusted`].
fn proven(answer: &Value, message: &[u8], pin: Option<&str>) -> anyhow::Result<String> {
    let key = answer["server_key"].as_str().unwrap_or_default();
    let sig = answer["signature"].as_str().unwrap_or_default();
    if !pairing::verify(key, message, sig) {
        return Err(Untrusted("it didn't prove a key".into()).into());
    }
    if let Some(pin) = pin.filter(|p| *p != key) {
        let why = format!("it proves {key}, not the pinned {pin}");
        return Err(Untrusted(why).into());
    }
    Ok(key.to_string())
}

/// The page a person confirms a pairing on: the server's origin, over http(s).
fn app_url(config: &HostConfig) -> String {
    if let Some(app) = &config.app {
        return app.trim_end_matches('/').to_string();
    }
    let (scheme, rest) = config
        .server
        .split_once("://")
        .unwrap_or(("ws", &config.server));
    let scheme = if scheme == "wss" { "https" } else { "http" };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{authority}")
}

async fn dial(
    hub: &Arc<DynamicHub>,
    config: &HostConfig,
    machine: bool,
    shown: &mut Option<String>,
) -> anyhow::Result<Session> {
    let creds = sign_in(config, machine)?;
    let session = Session::open(hub.clone(), config, &creds).await?;
    let attached = match attach(&session, config, creds.machine).await {
        Ok(a) => a,
        Err(e) => {
            session.close();
            return Err(e);
        }
    };
    if attached["type"] == "attached" {
        return Ok(session);
    }
    if !creds.machine {
        session.close();
        return Err(anyhow!("attaching: {}", attached["message"]));
    }
    // An unpaired key: pair it, then go on as the machine on this socket.
    let paired = pair(&session, config, shown).await;
    let attached = match paired {
        Ok(()) => attach(&session, config, true).await,
        Err(e) => Err(e),
    };
    match attached {
        Ok(a) if a["type"] == "attached" => Ok(session),
        Ok(a) => {
            session.close();
            Err(anyhow!("attaching: {}", a["message"]))
        }
        Err(e) => {
            session.close();
            Err(e)
        }
    }
}

/// `hosts.attach`, its answer checked against the pin. A refusal comes back as it
/// is; a server that proves the wrong key, or none, is [`Untrusted`].
async fn attach(session: &Session, config: &HostConfig, machine: bool) -> anyhow::Result<Value> {
    let name = (!machine).then(|| config.name.clone());
    let nonce = uuid::Uuid::new_v4().to_string();
    let attached = match session
        .request("hosts.attach", json!({"name": name, "nonce": nonce}))
        .await
    {
        Ok(a) => a,
        Err(why) => return Ok(json!({"type": "failed", "message": why})),
    };
    if attached["type"] != "attached" {
        return Ok(attached);
    }
    let pin = read_pin(config);
    let key = proven(&attached, &pairing::attach_message(&nonce), pin.as_deref())?;
    if pin.is_none() {
        write_pin(config, &key)?;
    }
    session.trust(&key);
    let mode = attached["mode"].as_str().unwrap_or_default();
    eprintln!(
        "plexus-agents host: attached to {} as {} ({mode})",
        config.server, attached["name"]
    );
    Ok(attached)
}

/// Asks to pair, shows the code, and waits for a person to confirm it.
async fn pair(
    session: &Session,
    config: &HostConfig,
    shown: &mut Option<String>,
) -> anyhow::Result<()> {
    let key = config.key.as_ref().context("pairing needs --key")?;
    let seed = read_seed(key).map_err(|e| anyhow!(e))?;
    let principal = principal_of(&seed);
    let mine = principal
        .strip_prefix("key:")
        .unwrap_or_default()
        .to_string();
    let nonce = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let params = json!({"name": config.name, "nonce": nonce});
    let asked = session
        .request("hosts.request", params)
        .await
        .map_err(|e| anyhow!("asking to pair: {e}"))?;
    if asked["type"] != "pairing" {
        return Err(anyhow!("asking to pair: {}", asked["message"]));
    }
    let pin = read_pin(config);
    let server = proven(
        &asked,
        &pairing::pair_message(&nonce, &mine),
        pin.as_deref(),
    )?;
    let id = asked["pairing_id"].as_str().unwrap_or_default().to_string();
    if shown.as_deref() != Some(id.as_str()) {
        let host_key = pairing::key_bytes(&mine).expect("our own key");
        let server_key = pairing::key_bytes(&server).expect("verified above");
        show(config, &id, &pairing::code(&host_key, &server_key));
        *shown = Some(id.clone());
    }
    let waited = session
        .request_for("hosts.wait", json!({"pairing_id": id}), config.pair_wait)
        .await
        .map_err(|e| anyhow!("waiting to be paired: {e}"))?;
    if waited["type"] != "paired" {
        return Err(anyhow!("waiting to be paired: {}", waited["message"]));
    }
    write_pin(config, &server)?;
    eprintln!(
        "plexus-agents host: paired as {}, owned by {}",
        waited["name"], waited["owner"]
    );
    Ok(())
}

/// The code, the link, and the link as a QR code, on stdout.
fn show(config: &HostConfig, id: &str, code: &str) {
    use std::io::IsTerminal;
    let app = app_url(config);
    let link = format!("{app}/#/pair?id={id}");
    let paint = if std::io::stdout().is_terminal() {
        pairing::Paint::Ansi
    } else {
        pairing::Paint::Plain
    };
    println!("plexus-agents host: pair `{}` as yours.", config.name);
    println!("link: {link}");
    println!("code: {code}");
    println!("Open the link signed in, or type the code at {app}/#/pair, and check the");
    println!("page shows this same code before you press Pair.");
    if let Ok(lines) = pairing::qr_lines(&link, paint) {
        for line in lines {
            println!("{line}");
        }
    }
}

enum Pending {
    Call {
        items: mpsc::Sender<Value>,
        opened: oneshot::Sender<Result<String, String>>,
    },
    Close,
}

struct Inner {
    hub: Arc<DynamicHub>,
    ctx: AuthContext,
    out: mpsc::Sender<Message>,
    next: AtomicU64,
    pending: Mutex<HashMap<String, Pending>>,
    subs: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    served: Mutex<HashMap<String, AbortHandle>>,
    /// The server key this socket's attach proved: what `hop.sig` is checked with.
    trusted: Mutex<Option<String>>,
    /// Call ids already served on this socket: each is used once.
    seen: Mutex<HashSet<String>>,
}

/// One socket to the service.
pub struct Session {
    inner: Arc<Inner>,
    closed: watch::Receiver<bool>,
    renew: Option<Instant>,
    timeout: Duration,
}

fn key_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn text(v: Value) -> Message {
    Message::Text(v.to_string().into())
}

impl Session {
    async fn open(
        hub: Arc<DynamicHub>,
        config: &HostConfig,
        creds: &SignIn,
    ) -> anyhow::Result<Self> {
        let mut request = config.server.as_str().into_client_request()?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", creds.bearer))?;
        request.headers_mut().insert("authorization", bearer);
        let dialing = tokio_tungstenite::connect_async(request);
        let (ws, _) = tokio::time::timeout(config.timeout, dialing)
            .await
            .map_err(|_| anyhow!("dialing {}: timed out", config.server))?
            .with_context(|| format!("dialing {}", config.server))?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut outgoing) = mpsc::channel::<Message>(1024);
        let principal = SERVER_PRINCIPAL.parse().expect("a principal");
        let claims = json!({"server": config.server, "username": "server"});
        let ctx = Authenticated::new(principal, "dialed", vec![], claims).into_auth_context();
        let inner = Arc::new(Inner {
            hub,
            ctx,
            out,
            next: AtomicU64::new(1),
            pending: Mutex::default(),
            subs: Mutex::default(),
            served: Mutex::default(),
            trusted: Mutex::default(),
            seen: Mutex::default(),
        });
        let (closed_tx, closed) = watch::channel(false);
        let closed_tx = Arc::new(closed_tx);
        let done = closed_tx.clone();
        tokio::spawn(async move {
            while let Some(m) = outgoing.recv().await {
                let close = matches!(m, Message::Close(_));
                if sink.send(m).await.is_err() || close {
                    break;
                }
            }
            let _ = sink.close().await;
            done.send_replace(true);
        });
        let reading = inner.clone();
        tokio::spawn(async move {
            while let Some(m) = stream.next().await {
                match m {
                    Ok(Message::Text(t)) => reading.frame(t.as_bytes()),
                    Ok(Message::Binary(b)) => reading.frame(&b),
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            reading.ended();
            closed_tx.send_replace(true);
        });
        Ok(Self {
            inner,
            closed,
            renew: creds.renew,
            timeout: config.timeout,
        })
    }

    /// Closes the socket. Calls it serves end.
    pub fn close(&self) {
        let _ = self.inner.out.try_send(Message::Close(None));
    }

    /// Calls from the service are checked against `key` from now on.
    fn trust(&self, key: &str) {
        *self.inner.trusted.lock().expect("host") = Some(key.to_string());
    }

    /// Calls `method` on the service and returns its first item's content, or its
    /// error's message.
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_for(method, params, self.timeout).await
    }

    /// [`Self::request`], waiting up to `wait` for the item.
    async fn request_for(
        &self,
        method: &str,
        params: Value,
        wait: Duration,
    ) -> Result<Value, String> {
        let inner = &self.inner;
        let id = format!("b-{}", inner.next.fetch_add(1, Ordering::Relaxed));
        let (items, mut rx) = mpsc::channel(64);
        let (opened, answer) = oneshot::channel();
        let call = Pending::Call { items, opened };
        inner.pending.lock().expect("host").insert(id.clone(), call);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": CALL,
                           "params": {"method": method, "params": params}});
        inner
            .out
            .send(text(frame))
            .await
            .map_err(|_| "the socket closed")?;
        let sub = tokio::time::timeout(self.timeout, answer)
            .await
            .map_err(|_| format!("{method}: no answer"))?
            .map_err(|_| "the socket closed")??;
        let result = loop {
            let item = tokio::time::timeout(wait, rx.recv()).await;
            let Some(item) = item.map_err(|_| format!("{method}: no item"))? else {
                break Err("the socket closed".to_string());
            };
            match item["type"].as_str() {
                Some("data") => break Ok(item["content"].clone()),
                Some("error") => break Err(item["message"].as_str().unwrap_or("").to_string()),
                _ => continue,
            }
        };
        inner.subs.lock().expect("host").remove(&sub);
        let id = format!("b-{}", inner.next.fetch_add(1, Ordering::Relaxed));
        inner
            .pending
            .lock()
            .expect("host")
            .insert(id.clone(), Pending::Close);
        let close = json!({"jsonrpc": "2.0", "id": id, "method": CALL_UNSUB, "params": [sub]});
        let _ = inner.out.try_send(text(close));
        result
    }
}

impl Inner {
    fn send(&self, v: Value) {
        // A full queue is a socket that can't keep up; what doesn't fit is lost
        // and the call it belongs to ends at the service when the socket does.
        let _ = self.out.try_send(text(v));
    }

    fn frame(self: &Arc<Self>, data: &[u8]) {
        let Ok(mut v) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        let method = v.get("method").and_then(Value::as_str).map(str::to_string);
        let id = v.get("id").cloned().filter(|i| !i.is_null());
        match (method, id) {
            (Some(method), Some(id)) => self.request(&method, id, v["params"].take()),
            (None, Some(id)) => {
                let Some(key) = key_of(&id) else { return };
                let Some(Pending::Call { items, opened }) =
                    self.pending.lock().expect("host").remove(&key)
                else {
                    return;
                };
                match v.get("result").and_then(key_of) {
                    Some(sub) => {
                        self.subs.lock().expect("host").insert(sub.clone(), items);
                        let _ = opened.send(Ok(sub));
                    }
                    None => {
                        let why = v["error"]["message"].as_str().unwrap_or("refused");
                        let _ = opened.send(Err(why.to_string()));
                    }
                }
            }
            (Some(_), None) => {
                let Some(sub) = v["params"].get("subscription").and_then(key_of) else {
                    return;
                };
                let subs = self.subs.lock().expect("host");
                if let Some(tx) = subs.get(&sub) {
                    let _ = tx.try_send(v["params"]["result"].take());
                }
            }
            (None, None) => {}
        }
    }

    fn ended(&self) {
        for (_, call) in self.served.lock().expect("host").drain() {
            call.abort();
        }
        self.pending.lock().expect("host").clear();
        self.subs.lock().expect("host").clear();
    }

    /// This host's gate: a call the pinned server key signed, once, under
    /// `agents.*` only.
    fn admit(&self, params: &Value) -> Result<(String, Value), String> {
        let method = params["method"].as_str().unwrap_or_default();
        let hop = &params["hop"];
        let (call, nonce) = (hop["call"].as_str(), hop["nonce"].as_str());
        let (Some(call), Some(nonce), Some(sig)) = (call, nonce, hop["sig"].as_str()) else {
            return Err("the call carries no server signature".into());
        };
        let trusted = self.trusted.lock().expect("host").clone();
        let Some(key) = trusted else {
            return Err("this host trusts no server key on this socket yet".into());
        };
        if !pairing::verify(&key, &pairing::call_message(call, method, nonce), sig) {
            return Err(format!(
                "the call isn't signed by {key}, the server key this host pinned"
            ));
        }
        {
            let mut seen = self.seen.lock().expect("host");
            if seen.len() > 100_000 {
                seen.clear();
            }
            if !seen.insert(call.to_string()) {
                return Err("that call id was used already".into());
            }
        }
        if !method.starts_with("agents.") || method.starts_with("agents.on.") {
            return Err(format!("this host serves agents.*, not `{method}`"));
        }
        let args = params.get("params").cloned().filter(|p| !p.is_null());
        Ok((method.to_string(), args.unwrap_or_else(|| json!({}))))
    }

    fn request(self: &Arc<Self>, method: &str, id: Value, params: Value) {
        if method.ends_with(".call_unsub") {
            let sub = params.get(0).and_then(key_of).unwrap_or_default();
            if let Some(call) = self.served.lock().expect("host").remove(&sub) {
                call.abort();
            }
            self.send(json!({"jsonrpc": "2.0", "id": id, "result": true}));
            return;
        }
        if !method.ends_with(".call") {
            let error = json!({"code": -32601, "message": "a host answers `.call` only"});
            self.send(json!({"jsonrpc": "2.0", "id": id, "error": error}));
            return;
        }
        let sub = format!("h-{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": sub}));
        let item = |sub: &str, item: Value| {
            json!({"jsonrpc": "2.0", "method": CALL,
                   "params": {"subscription": sub, "result": item}})
        };
        let (method, args) = match self.admit(&params) {
            Ok(call) => call,
            Err(why) => {
                let mut f = Map::new();
                f.insert("reason".into(), why.clone().into());
                let refused =
                    PlexusStreamItem::typed_error(meta("host"), why, "forbidden", false, f);
                let refused = serde_json::to_value(refused).unwrap_or_default();
                self.send(item(&sub, refused));
                return;
            }
        };
        let me = self.clone();
        let mut served = self.served.lock().expect("host");
        let key = sub.clone();
        let task = tokio::spawn(async move {
            let routed = me
                .hub
                .route_with_ctx(&method, args, Some(&me.ctx), None)
                .await;
            let mut ended = false;
            match routed {
                Err(e) => {
                    me.send(item(&sub, error_item("plexus-agents", &e)));
                    ended = true;
                }
                Ok(mut stream) => {
                    while let Some(next) = stream.next().await {
                        ended = matches!(
                            next,
                            PlexusStreamItem::Done { .. } | PlexusStreamItem::Error { .. }
                        );
                        let value = serde_json::to_value(&next).unwrap_or_default();
                        if me.out.send(text(item(&sub, value))).await.is_err() || ended {
                            break;
                        }
                    }
                }
            }
            if !ended {
                let done = PlexusStreamItem::Done {
                    metadata: meta("host"),
                };
                me.send(item(&sub, serde_json::to_value(done).unwrap_or_default()));
            }
            me.served.lock().expect("host").remove(&sub);
        });
        served.insert(key, task.abort_handle());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tokens_exp_is_read_without_checking_it() {
        let b64 = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        let token = format!(
            "{}.{}.sig",
            b64(json!({"alg": "RS256"})),
            b64(json!({"exp": 42}))
        );
        assert_eq!(token_exp(&token), Some(42));
        assert_eq!(token_exp("not a token"), None);
    }
}

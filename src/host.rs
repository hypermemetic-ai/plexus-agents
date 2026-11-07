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
//! A call is routed only when its `hop.server` is the URL this host dialed, and its
//! method is under `agents.` and not `agents.on.` (`max_depth` is 0 here too). It
//! runs as [`SERVER_PRINCIPAL`], which [`server_grant`] makes `admin` on this hub
//! and nothing else holds: nothing else reaches this hub at all.
//!
//! # Signing in, and again
//!
//! - **User:** `--token-file`, a plexus-idp access token, read again at each dial so
//!   whatever refreshes it (synapse's credential store, `idp refresh`) rotates it
//!   here. The service closes the socket at the token's `exp`, so this host dials a
//!   new one a minute before, and the old one serves its open calls until then.
//! - **Machine:** `--key`, an Ed25519 seed (mode 0600). A proof stands 300 s, so a
//!   new socket every 240 s.
//! - **Both, with `--pair`:** signs in as the user, pairs the key over that socket
//!   with `hosts.pair`, then signs in as the machine and closes the user's socket.
//!
//! A socket that drops is dialed again with backoff, and attaches as the same name.

use crate::hosts::{CALL, CALL_UNSUB, HOST_AGENT};
use crate::peer::{meta, principal_of, read_seed};
use anyhow::{anyhow, Context};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use plexus_auth_core::adapters::sign_key_proof;
use plexus_auth_core::identity::Authenticated;
use plexus_core::plexus::{error_item, AuthContext, DynamicHub, PlexusStreamItem};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
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
    /// With both: sign in as the user, pair the key, go on as the machine.
    pub pair: bool,
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
            pair: false,
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
    let mut machine = config.token_file.is_none();
    let mut backoff = Duration::from_millis(200);
    // A user's socket, kept until the machine's is attached.
    let mut switching: Option<Session> = None;
    loop {
        let session = match dial(&hub, &config, machine).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("plexus-agents host: {e:#}; dialing again in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(config.backoff_max);
                continue;
            }
        };
        backoff = Duration::from_millis(200);
        if let Some(old) = switching.take() {
            old.close();
        }
        if !machine && config.pair && config.key.is_some() {
            match pair(&session, &config).await {
                Ok(()) => {
                    machine = true;
                    switching = Some(session);
                    continue;
                }
                Err(e) => eprintln!("plexus-agents host: pairing: {e:#}"),
            }
        }
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

async fn dial(
    hub: &Arc<DynamicHub>,
    config: &HostConfig,
    machine: bool,
) -> anyhow::Result<Session> {
    let creds = sign_in(config, machine)?;
    let session = Session::open(hub.clone(), config, &creds).await?;
    let name = (!creds.machine).then(|| config.name.clone());
    let nonce = uuid::Uuid::new_v4().to_string();
    let attached = session
        .request("hosts.attach", json!({"name": name, "nonce": nonce}))
        .await
        .map_err(|e| anyhow!("attaching: {e}"))?;
    if attached["type"] != "attached" {
        session.close();
        return Err(anyhow!("attaching: {}", attached["message"]));
    }
    let mode = attached["mode"].as_str().unwrap_or_default();
    eprintln!(
        "plexus-agents host: attached to {} as {} ({mode})",
        config.server, attached["name"]
    );
    Ok(session)
}

async fn pair(session: &Session, config: &HostConfig) -> anyhow::Result<()> {
    let key = config.key.as_ref().context("--pair needs --key")?;
    let principal = principal_of(&read_seed(key).map_err(|e| anyhow!(e))?);
    let params = json!({"name": config.name, "public_key": principal});
    let paired = session
        .request("hosts.pair", params)
        .await
        .map_err(|e| anyhow!(e))?;
    if paired["type"] != "paired" {
        return Err(anyhow!("{}", paired["message"]));
    }
    eprintln!("plexus-agents host: paired {principal} as {}", config.name);
    Ok(())
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
    server: String,
    ctx: AuthContext,
    out: mpsc::Sender<Message>,
    next: AtomicU64,
    pending: Mutex<HashMap<String, Pending>>,
    subs: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    served: Mutex<HashMap<String, AbortHandle>>,
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
            server: config.server.clone(),
            ctx,
            out,
            next: AtomicU64::new(1),
            pending: Mutex::default(),
            subs: Mutex::default(),
            served: Mutex::default(),
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

    /// Calls `method` on the service and returns its first item's content, or its
    /// error's message.
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
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
            let item = tokio::time::timeout(self.timeout, rx.recv()).await;
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

    /// This host's gate: the server it dialed, and `agents.*` only.
    fn admit(&self, params: &Value) -> Result<(String, Value), String> {
        let from = params["hop"]["server"].as_str().unwrap_or_default();
        if from.trim_end_matches('/') != self.server {
            return Err(format!("this host dialed {}, not `{from}`", self.server));
        }
        let method = params["method"].as_str().unwrap_or_default();
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

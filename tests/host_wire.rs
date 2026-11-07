//! SUPERCONDUCTOR-15 through the wire: a host behind NAT dials the agents service,
//! and the one socket it opened carries Plexus RPC both ways. The service runs here
//! on a real socket, with real RS256 tokens against a JWKS and the real key-proof
//! adapter. The host is `examples/host_stub`, a separate process that opens no
//! port, replaying a real run.

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use jsonrpsee::core::client::{Subscription, SubscriptionClientT};
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use plexus_agents::auth::Grant;
use plexus_agents::host::{self, HostConfig};
use plexus_agents::hosts::{Hosts, HOST_AGENT};
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{hub, hub_with_hosts, serve_with_hosts};
use plexus_agents::ClaudeHarness;
use plexus_auth_core::adapters::{sign_key_proof, Carriers, KeyProofAdapter, OidcAdapter};
use plexus_auth_core::identity::AuthenticatorChain;
use plexus_auth_core::oidc::{
    Audience, FetchError, FetchedDocument, JwksFetcher, OidcConfig, OidcValidator,
};
use plexus_auth_core::IssuerUrl;
use plexus_core::plexus::SessionValidator;
use plexus_harness::{Environment, Harness};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const ISSUER: &str = "http://127.0.0.1:1";
const AUDIENCE: &str = "plexus:agents";
const ADA: &str = "00000000-0000-4000-8000-00000000000a";
const BEN: &str = "00000000-0000-4000-8000-00000000000b";

struct Signer {
    pem: EncodingKey,
    jwks: Value,
}

fn signer() -> &'static Signer {
    static S: OnceLock<Signer> = OnceLock::new();
    S.get_or_init(|| {
        let key = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let b64 = |b: Vec<u8>| URL_SAFE_NO_PAD.encode(b);
        let pem = key.to_pkcs1_pem(Default::default()).unwrap();
        Signer {
            pem: EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
            jwks: json!({"keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "test",
                "n": b64(key.n().to_bytes_be()), "e": b64(key.e().to_bytes_be()),
            }]}),
        }
    })
}

fn token(sub: &str) -> String {
    token_for(sub, 600)
}

fn token_for(sub: &str, ttl: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({
        "iss": ISSUER, "sub": sub, "aud": AUDIENCE, "iat": now, "exp": now + ttl,
        "jti": format!("{sub}-{now}"), "username": sub,
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test".into());
    jsonwebtoken::encode(&header, &claims, &signer().pem).unwrap()
}

/// The IDP's two documents, with no network.
struct Issuer;

#[async_trait]
impl JwksFetcher for Issuer {
    async fn fetch(&self, url: &url::Url) -> Result<FetchedDocument, FetchError> {
        let body = if url.path().ends_with("openid-configuration") {
            json!({"issuer": ISSUER, "jwks_uri": format!("{ISSUER}/jwks")})
        } else {
            signer().jwks.clone()
        };
        Ok(FetchedDocument {
            body: body.to_string(),
            cache_control: None,
        })
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

async fn up(port: u16) {
    for _ in 0..500 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not up");
}

/// Service A: the hosted agents service.
struct Service {
    port: u16,
    url: String,
    hosts: Arc<Hosts>,
    db: PathBuf,
    dir: tempfile::TempDir,
}

/// No run happens on A itself.
struct Nowhere;

impl Environment for Nowhere {
    fn name(&self) -> &str {
        "nowhere"
    }
    fn spawn(
        &self,
        _: plexus_harness::LaunchPlan,
    ) -> futures::future::BoxFuture<
        'static,
        Result<plexus_harness::Process, plexus_harness::SpawnError>,
    > {
        Box::pin(async { Err(plexus_harness::SpawnError::Io("not here".into())) })
    }
}

async fn service() -> Service {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("agents.db");
    let env: Arc<dyn Environment> = Arc::new(Nowhere);
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: db.clone(),
        environment: env.clone(),
        placements: HashMap::from([("local".to_string(), env)]),
        harnesses: harnesses.clone(),
    })
    .await
    .unwrap();
    let port = free_port();
    let url = format!("ws://127.0.0.1:{port}");
    let issuer = IssuerUrl::try_new(ISSUER.parse().unwrap()).unwrap();
    let config = OidcConfig::new(issuer, Audience::try_new(AUDIENCE).unwrap());
    let oidc = OidcAdapter::plexus_idp_with(OidcValidator::with_fetcher(config, Arc::new(Issuer)));
    let chain = AuthenticatorChain::new()
        .with(Arc::new(oidc))
        .with(Arc::new(KeyProofAdapter::new([&url]).unwrap()));
    let validator: Arc<dyn SessionValidator> = Arc::new(Carriers::cookie_and_bearer(chain));
    let hosts = Hosts::open(&db, &url).unwrap();
    let grants: Vec<Grant> = vec![];
    let h = hub_with_hosts(runtime, harnesses, grants, vec![], Some(hosts.clone()));
    let bind = [127, 0, 0, 1].into();
    tokio::spawn(serve_with_hosts(
        h,
        validator,
        bind,
        port,
        Some(hosts.clone()),
    ));
    up(port).await;
    Service {
        port,
        url,
        hosts,
        db,
        dir,
    }
}

impl Service {
    fn file(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn token_file(&self, sub: &str) -> (PathBuf, String) {
        let t = token(sub);
        (self.file(&format!("{sub}.token"), &t), t)
    }

    /// A host's key, as a seed file, and its seed.
    fn key(&self, byte: u8) -> (PathBuf, [u8; 32]) {
        let seed = [byte; 32];
        let text = base64::engine::general_purpose::STANDARD.encode(seed);
        (self.file(&format!("host-{byte}.key"), &text), seed)
    }

    fn proof(&self, seed: &[u8; 32]) -> String {
        let nonce: [u8; 32] = rand::random();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        sign_key_proof(seed, &self.url, HOST_AGENT, &nonce, ts)
    }
}

fn principal(seed: &[u8; 32]) -> String {
    let key = ed25519_dalek::SigningKey::from_bytes(seed);
    let hex: String = key
        .verifying_key()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("key:{hex}")
}

/// Service B: `examples/host_stub`, a process of its own.
struct Host(Child, #[allow(dead_code)] tempfile::TempDir);

impl Host {
    fn start(a: &Service, name: &str, args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let test = std::env::current_exe().unwrap();
        let bin = test.parent().unwrap().parent().unwrap();
        let mut c = Command::new(bin.join("examples/host_stub"));
        c.args(["--server", &a.url, "--name", name])
            .args(["--db", dir.path().join("host.db").to_str().unwrap()])
            .args(args);
        Self(c.spawn().expect("cargo test builds examples"), dir)
    }

    fn kill(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn connect(port: u16, token: Option<&str>) -> Result<WsClient, String> {
    let mut headers = HeaderMap::new();
    if let Some(t) = token {
        let cookie = HeaderValue::from_str(&format!("access_token={t}")).unwrap();
        headers.insert("cookie", cookie);
    }
    WsClientBuilder::default()
        .set_headers(headers)
        .build(format!("ws://127.0.0.1:{port}"))
        .await
        .map_err(|e| e.to_string())
}

async fn connect_bearer(port: u16, bearer: &str) -> Result<WsClient, String> {
    let mut headers = HeaderMap::new();
    let value = HeaderValue::from_str(&format!("Bearer {bearer}")).unwrap();
    headers.insert("authorization", value);
    WsClientBuilder::default()
        .set_headers(headers)
        .build(format!("ws://127.0.0.1:{port}"))
        .await
        .map_err(|e| e.to_string())
}

async fn open(c: &WsClient, method: &str, params: Value) -> Subscription<Value> {
    let mut p = ObjectParams::new();
    p.insert("method", method).unwrap();
    p.insert("params", params).unwrap();
    c.subscribe("plexus-agents.call", p, "plexus-agents.call_unsub")
        .await
        .unwrap()
}

async fn next(s: &mut Subscription<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), s.next())
        .await
        .expect("an item in time")
        .expect("open")
        .unwrap()
}

async fn call(c: &WsClient, method: &str, params: Value) -> Value {
    let mut s = open(c, method, params).await;
    loop {
        let item = next(&mut s).await;
        match item["type"].as_str() {
            Some("data") => return item["content"].clone(),
            Some("error") => return item,
            _ => continue,
        }
    }
}

async fn hosts_of(c: &WsClient) -> Vec<Value> {
    let listed = call(c, "hosts.list", json!({})).await;
    assert_eq!(listed["type"], "hosts", "{listed}");
    listed["hosts"].as_array().unwrap().clone()
}

/// Waits until `c`'s `hosts.list` shows `name` online in `mode`.
async fn online(c: &WsClient, name: &str, mode: &str) -> Value {
    for _ in 0..500 {
        let hosts = hosts_of(c).await;
        let found = hosts.into_iter().find(|h| h["name"] == name);
        if let Some(h) = found.filter(|h| h["online"] == true && h["mode"] == mode) {
            return h;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{name} never came online as {mode}");
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..300 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never: {what}");
}

/// Whether `pid` listens on any TCP port, by lsof; `None` with no lsof.
fn listens(pid: u32) -> Option<bool> {
    let out = Command::new("lsof")
        .args(["-nP", "-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN"])
        .output()
        .ok()?;
    Some(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_starts_and_streams_over_the_socket_the_host_dialed() {
    let a = service().await;
    let (file, ada_token) = a.token_file(ADA);
    let b = Host::start(&a, "nat", &["--token-file", file.to_str().unwrap()]);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    online(&ada, "nat", "user").await;
    if let Some(l) = listens(b.0.id()) {
        assert!(!l, "the host opens no port");
    }

    let run = json!({"placement": "local", "cwd": "/recorded/cwd", "prompt": "p"});
    let started = call(&ada, "agents.on.nat.start", run).await;
    assert_eq!(started["type"], "started", "{started}");
    let thread = started["thread"].as_str().unwrap().to_string();

    let mut watch = open(
        &ada,
        "agents.on.nat.watch",
        json!({"thread": thread, "since": 0}),
    )
    .await;
    let first = next(&mut watch).await;
    assert_eq!(first["content"]["type"], "event", "{first}");

    let mut requests = Vec::new();
    for _ in 0..500 {
        let p = call(&ada, "agents.on.nat.pending", json!({})).await;
        requests = p["requests"].as_array().unwrap().clone();
        if !requests.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(requests.len(), 1, "parked at the host");
    let answer = json!({"thread": thread, "request": requests[0]["request"]["id"],
                        "answer": {"answer": "allow", "updated_input": null}});
    let answered = call(&ada, "agents.on.nat.answer", answer).await;
    assert_eq!(answered["type"], "answered", "{answered}");
    loop {
        let item = next(&mut watch).await;
        let event = &item["content"]["event"]["event"];
        if event["kind"] == "input.resolved" {
            // The host's gate ran it as the server it dialed.
            assert_eq!(event["by"]["id"], json!(host::SERVER_PRINCIPAL), "{item}");
            break;
        }
    }
    assert!(
        a.hosts.open_calls("nat") >= 1,
        "the watch is open at the host"
    );
    drop(watch);
    eventually("the dropped watch is closed at the host", || {
        a.hosts.open_calls("nat") == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_mode_host_is_listed_to_its_owner_only() {
    let a = service().await;
    let (file, ada_token) = a.token_file(ADA);
    let _b = Host::start(&a, "ada_box", &["--token-file", file.to_str().unwrap()]);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    let listed = online(&ada, "ada_box", "user").await;
    assert!(listed["last_seen"].is_i64(), "{listed}");

    let ben = connect(a.port, Some(&token(BEN))).await.unwrap();
    assert!(hosts_of(&ben).await.is_empty(), "ben sees none of ada's");
    for method in ["agents.on.ada_box.threads", "agents.on.ada_box.schema"] {
        let r = call(&ben, method, json!({})).await;
        assert_eq!(r["error_kind"], "forbidden", "{method}: {r}");
    }
    let on = call(&ben, "agents.on.placements", json!({})).await;
    assert_eq!(on["placements"], json!([]), "{on}");
    let on = call(&ben, "agents.on.schema", json!({})).await;
    assert!(!on.to_string().contains("ada_box"), "{on}");
    let anon = connect(a.port, None).await.unwrap();
    let r = call(&anon, "agents.on.ada_box.threads", json!({})).await;
    assert_eq!(r["error_kind"], "unauthenticated", "{r}");
    let r = call(&anon, "hosts.list", json!({})).await;
    assert_eq!(r["error_kind"], "unauthenticated", "{r}");

    let mine = call(&ada, "agents.on.ada_box.harnesses", json!({})).await;
    assert_eq!(mine["type"], "harnesses", "{mine}");
    // The host's own schema, through its socket: every method it serves is one
    // its owner's role lets past the hub's gate.
    let schema = call(&ada, "agents.on.ada_box.schema", json!({})).await;
    let methods = schema["methods"].as_array().expect("the host's schema");
    let children = ["on", "claudecode", "schema"];
    let served = methods.iter().filter_map(|m| m["name"].as_str());
    for name in served.filter(|n| !children.contains(n)) {
        assert!(plexus_agents::hosts::HOST_METHODS.contains(&name), "{name}");
    }
    let on = call(&ada, "agents.on.placements", json!({})).await;
    assert_eq!(
        on["placements"],
        json!([{"name": "ada_box", "peer": true}]),
        "{on}"
    );
    let on = call(&ada, "agents.on.schema", json!({})).await;
    let children = on["children"].as_array().unwrap();
    assert!(children.iter().any(|c| c["namespace"] == "ada_box"), "{on}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_paired_at_runtime_signs_in_as_the_machine_with_no_restart() {
    let a = service().await;
    let (file, ada_token) = a.token_file(ADA);
    let (key, seed) = a.key(7);
    let args = [
        "--token-file",
        file.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "--pair",
    ];
    let _b = Host::start(&a, "rig", &args);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    online(&ada, "rig", "machine").await;
    let record = a.hosts.store().get("rig").unwrap();
    assert_eq!(
        format!("key:{}", record.public_key.unwrap()),
        principal(&seed)
    );
    assert_eq!(record.owner, format!("idp:{ADA}"));
    let r = call(&ada, "agents.on.rig.harnesses", json!({})).await;
    assert_eq!(r["type"], "harnesses", "{r}");

    // The key holds nothing but its attach: not pairing, not another's host.
    let machine = connect_bearer(a.port, &a.proof(&seed)).await.unwrap();
    let r = call(
        &machine,
        "hosts.pair",
        json!({"name": "x", "public_key": principal(&[9; 32])}),
    )
    .await;
    assert_eq!(r["error_kind"], "forbidden", "{r}");
    let r = call(&machine, "agents.on.rig.threads", json!({})).await;
    assert_eq!(r["error_kind"], "forbidden", "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpaired_key_is_refused_and_unpair_closes_the_socket() {
    let a = service().await;
    let (_, stranger) = a.key(3);
    let refused = connect_bearer(a.port, &a.proof(&stranger)).await;
    let e = refused.expect_err("an unpaired host key is refused");
    assert!(e.contains("401"), "{e}");

    let (file, ada_token) = a.token_file(ADA);
    let (key, seed) = a.key(5);
    let args = [
        "--token-file",
        file.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "--pair",
    ];
    let _b = Host::start(&a, "gone", &args);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    online(&ada, "gone", "machine").await;
    // A second socket on the paired key, which the test can watch close.
    let held = connect_bearer(a.port, &a.proof(&seed)).await.unwrap();
    assert!(held.is_connected());

    let r = call(&ada, "hosts.unpair", json!({"name": "gone"})).await;
    assert_eq!(r["type"], "unpaired", "{r}");
    eventually("the key's open socket closes", || !held.is_connected()).await;
    assert!(!a.hosts.online("gone"));
    assert!(hosts_of(&ada).await.is_empty());
    let r = call(&ada, "agents.on.gone.threads", json!({})).await;
    assert_eq!(r["error_kind"], "forbidden", "{r}");
    let again = connect_bearer(a.port, &a.proof(&seed)).await;
    let e = again.expect_err("redial with the unpaired key is refused");
    assert!(e.contains("401"), "{e}");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !a.hosts.online("gone"),
        "the host's own redials are refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_host_is_child_unavailable_and_its_redial_restores_the_name() {
    let a = service().await;
    let (file, ada_token) = a.token_file(ADA);
    let args = ["--token-file", file.to_str().unwrap()];
    let mut b = Host::start(&a, "k", &args);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    online(&ada, "k", "user").await;
    let mut watch = open(&ada, "agents.on.k.watch", json!({})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    b.kill();
    let last = loop {
        let item = next(&mut watch).await;
        if item["type"] == "error" {
            break item;
        }
    };
    assert_eq!(last["error_kind"], "child_unavailable", "{last}");
    assert_eq!(last["child"], "k", "{last}");
    let r = call(&ada, "agents.on.k.harnesses", json!({})).await;
    assert_eq!(r["error_kind"], "child_unavailable", "{r}");
    let listed = hosts_of(&ada).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["online"], false, "{listed:?}");

    let _back = Host::start(&a, "k", &args);
    online(&ada, "k", "user").await;
    let r = call(&ada, "agents.on.k.harnesses", json!({})).await;
    assert_eq!(r["type"], "harnesses", "{r}");
    assert_eq!(
        hosts_of(&ada).await.len(),
        1,
        "the same name, not a second host"
    );
}

/// A server of the test's own, which the host dials, and which sends what the
/// agents service never would.
#[tokio::test(flavor = "multi_thread")]
async fn the_host_refuses_calls_outside_agents_and_from_a_server_it_didnt_dial() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("t");
    std::fs::write(&token, "a token the test's server takes").unwrap();
    let env: Arc<dyn Environment> = Arc::new(Nowhere);
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: dir.path().join("host.db"),
        environment: env.clone(),
        placements: HashMap::from([("local".to_string(), env)]),
        harnesses: harnesses.clone(),
    })
    .await
    .unwrap();
    let mut config = HostConfig::new(&url, "h");
    config.token_file = Some(token);
    let b = hub(runtime, harnesses, vec![host::server_grant()]);
    let running = tokio::spawn(host::run(b, config));

    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let mut read = async || -> Value {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), ws.next()).await;
            if let Message::Text(t) = m.unwrap().unwrap().unwrap() {
                return serde_json::from_str(&t).unwrap();
            }
        }
    };
    let attach = read().await;
    assert_eq!(attach["params"]["method"], "hosts.attach", "{attach}");
    let id = attach["id"].clone();
    let attached =
        json!({"type": "data", "content": {"type": "attached", "name": "h", "mode": "user"}});
    for frame in [
        json!({"jsonrpc": "2.0", "id": id, "result": "s1"}),
        json!({"jsonrpc": "2.0", "method": "plexus-agents.call",
               "params": {"subscription": "s1", "result": attached}}),
    ] {
        ws.send(Message::Text(frame.to_string().into()))
            .await
            .unwrap();
    }

    let mut n = 0;
    let mut ask = async |ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
                         method: &str,
                         server: &str|
           -> Value {
        n += 1;
        let id = format!("a-{n}");
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": "plexus-agents.call",
                           "params": {"method": method, "params": {}, "hop": {"server": server}}});
        ws.send(Message::Text(frame.to_string().into()))
            .await
            .unwrap();
        let mut sub = None;
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), ws.next()).await;
            let Message::Text(t) = m.unwrap().unwrap().unwrap() else {
                continue;
            };
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["id"] == json!(id) {
                sub = Some(v["result"].clone());
                continue;
            }
            if sub.is_some() && v["params"]["subscription"] == sub.clone().unwrap() {
                return v["params"]["result"].clone();
            }
        }
    };
    let r = ask(&mut ws, "plexus-agents.hash", &url).await;
    assert_eq!(r["error_kind"], "forbidden", "outside agents.*: {r}");
    let r = ask(&mut ws, "agents.on.local.harnesses", &url).await;
    assert_eq!(r["error_kind"], "forbidden", "on, past the host: {r}");
    let r = ask(&mut ws, "agents.harnesses", "ws://another.example:4470").await;
    assert_eq!(r["error_kind"], "forbidden", "a server it didn't dial: {r}");
    let r = ask(&mut ws, "agents.harnesses", &url).await;
    assert_eq!(
        r["type"], "data",
        "the server it dialed, under agents.*: {r}"
    );
    assert_eq!(r["content"]["type"], "harnesses", "{r}");

    let schema =
        json!({"jsonrpc": "2.0", "id": "x", "method": "plexus-agents.schema", "params": {}});
    ws.send(Message::Text(schema.to_string().into()))
        .await
        .unwrap();
    loop {
        let m = tokio::time::timeout(Duration::from_secs(10), ws.next()).await;
        let Message::Text(t) = m.unwrap().unwrap().unwrap() else {
            continue;
        };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["id"] == "x" {
            assert_eq!(v["error"]["code"], -32601, "{v}");
            break;
        }
    }
    running.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_store_holds_no_token_or_private_key() {
    let a = service().await;
    let (file, ada_token) = a.token_file(ADA);
    let (key, seed) = a.key(11);
    let args = [
        "--token-file",
        file.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "--pair",
    ];
    let _b = Host::start(&a, "vault", &args);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    online(&ada, "vault", "machine").await;
    let r = call(&ada, "agents.on.vault.harnesses", json!({})).await;
    assert_eq!(r["type"], "harnesses", "{r}");

    let dump = dump(&a.db);
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    let b64 = base64::engine::general_purpose::STANDARD.encode(seed);
    let payload = ada_token.split('.').nth(1).unwrap();
    for secret in [
        ada_token.as_str(),
        payload,
        hex.as_str(),
        b64.as_str(),
        "plexus-key.",
    ] {
        assert!(
            !dump.contains(secret),
            "the store holds `{secret}`:\n{dump}"
        );
    }
    let public = principal(&seed);
    assert!(
        dump.contains(public.strip_prefix("key:").unwrap()),
        "{dump}"
    );
    assert!(dump.contains("vault"), "{dump}");
    // Nor do the files under it, WAL included.
    for entry in std::fs::read_dir(a.dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("agents.db")
        {
            let raw = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).to_string();
            for secret in [ada_token.as_str(), hex.as_str(), b64.as_str()] {
                assert!(!raw.contains(secret), "{} holds a secret", path.display());
            }
        }
    }
}

/// Every row of every table, as text.
fn dump(db: &Path) -> String {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut out = String::new();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for table in tables {
        let mut q = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
        let columns = q.column_count();
        let rows = q
            .query_map([], |r| {
                let mut row = Vec::new();
                for i in 0..columns {
                    let v: rusqlite::types::Value = r.get(i)?;
                    row.push(match v {
                        rusqlite::types::Value::Text(t) => t,
                        rusqlite::types::Value::Blob(b) => String::from_utf8_lossy(&b).to_string(),
                        other => format!("{other:?}"),
                    });
                }
                Ok(row.join(" | "))
            })
            .unwrap();
        for row in rows {
            out.push_str(&format!("{table}: {}\n", row.unwrap()));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_mode_host_redials_with_the_rotated_token_before_its_expiry() {
    let a = service().await;
    // Expires in 62 s; the host dials again 60 s before, so in about 2.
    let file = a.file("rotating.token", &token_for(ADA, 62));
    let args = [
        "--token-file",
        file.to_str().unwrap(),
        "--margin-secs",
        "60",
    ];
    let _b = Host::start(&a, "rot", &args);
    let ada = connect(a.port, Some(&token(ADA))).await.unwrap();
    let first = online(&ada, "rot", "user").await["last_seen"]
        .as_i64()
        .unwrap();
    // Whatever refreshes the token (synapse, `idp refresh`) writes the new one.
    std::fs::write(&file, token(ADA)).unwrap();
    for _ in 0..300 {
        let hosts = hosts_of(&ada).await;
        let seen = hosts[0]["last_seen"].as_i64().unwrap();
        if seen > first && hosts[0]["online"] == true {
            let r = call(&ada, "agents.on.rot.harnesses", json!({})).await;
            assert_eq!(r["type"], "harnesses", "{r}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the host never attached again");
}

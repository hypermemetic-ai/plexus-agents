//! SUPERCONDUCTOR-15 through the wire: a host behind NAT dials the agents service,
//! and the one socket it opened carries Plexus RPC both ways. The service runs here
//! on a real socket, with real RS256 tokens against a JWKS and the real key-proof
//! adapter. The host is `examples/host_stub`, a separate process that opens no
//! port, replaying a real run.
//!
//! SUPERCONDUCTOR-18: a host's key pairs by a code both sides show. The test reads
//! the code and link the host prints, and a signed-in user's `hosts.pairing` and
//! `hosts.confirm` stand in for the browser.

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use jsonrpsee::core::client::{ClientT, Subscription, SubscriptionClientT};
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use plexus_agents::auth::Grant;
use plexus_agents::host::{self, HostConfig};
use plexus_agents::hosts::{Hosts, HOST_AGENT, MAX_PER_IP};
use plexus_agents::pairing::{self, ServerKey};
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{hub, hub_with_hosts, serve_with_hosts};
use plexus_agents::ClaudeHarness;
use plexus_rpc::auth_core::adapters::{sign_key_proof, Carriers, KeyProofAdapter, OidcAdapter};
use plexus_rpc::auth_core::identity::AuthenticatorChain;
use plexus_rpc::auth_core::oidc::{
    Audience, FetchError, FetchedDocument, JwksFetcher, OidcConfig, OidcValidator,
};
use plexus_rpc::auth_core::IssuerUrl;
use plexus_rpc::core::plexus::SessionValidator;
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
/// The service's identity key, `key_s`.
const SERVER_SEED: [u8; 32] = [42; 32];

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
    service_placed(true).await
}

/// A service with a `local` placement, or, as `--no-local` runs it, none.
async fn service_placed(local: bool) -> Service {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("agents.db");
    let env: Arc<dyn Environment> = Arc::new(Nowhere);
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: db.clone(),
        environment: env.clone(),
        placements: if local {
            HashMap::from([("local".to_string(), env)])
        } else {
            HashMap::new()
        },
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
    let hosts = Hosts::open(&db, &url, ServerKey::from_seed(SERVER_SEED)).unwrap();
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

fn public(seed: &[u8; 32]) -> [u8; 32] {
    pairing::key_bytes(&principal(seed)).unwrap()
}

/// Service B: `examples/host_stub`, a process of its own. What it prints comes
/// back line by line.
struct Host(
    Child,
    tempfile::TempDir,
    tokio::sync::mpsc::UnboundedReceiver<String>,
);

impl Host {
    fn start(a: &Service, name: &str, args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let test = std::env::current_exe().unwrap();
        let bin = test.parent().unwrap().parent().unwrap();
        let mut c = Command::new(bin.join("examples/host_stub"));
        c.args(["--server", &a.url, "--name", name])
            .args(["--db", dir.path().join("host.db").to_str().unwrap()])
            .args(["--pins", dir.path().join("pins").to_str().unwrap()])
            .args(args)
            .stdout(std::process::Stdio::piped());
        let mut child = c.spawn().expect("cargo test builds examples");
        let out = child.stdout.take().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        Self(child, dir, rx)
    }

    /// The rest of the next printed line starting with `prefix`.
    async fn printed(&mut self, prefix: &str) -> String {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(10), self.2.recv()).await;
            let line = line.expect("printed in time").expect("still running");
            if let Some(rest) = line.strip_prefix(prefix) {
                return rest.to_string();
            }
        }
    }

    /// The server key this host pinned for `a`.
    fn pin(&self, a: &Service) -> Option<String> {
        let path = host::pin_path(&self.1.path().join("pins"), &a.url);
        std::fs::read_to_string(path)
            .ok()
            .map(|t| t.trim().to_string())
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
    try_open(c, method, params).await.unwrap()
}

async fn try_open(
    c: &WsClient,
    method: &str,
    params: Value,
) -> Result<Subscription<Value>, String> {
    let mut p = ObjectParams::new();
    p.insert("method", method).unwrap();
    p.insert("params", params).unwrap();
    c.subscribe("plexus-agents.call", p, "plexus-agents.call_unsub")
        .await
        .map_err(|e| e.to_string())
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

/// Starts host `name` on an unpaired key, reads the code and link it prints, and
/// confirms it as `user`, as the page would: checks the page's code is the host's
/// first. Returns the host and the code.
async fn pair(a: &Service, user: &WsClient, name: &str, key: &Path) -> (Host, String) {
    let mut b = Host::start(a, name, &["--key", key.to_str().unwrap()]);
    let link = b.printed("link: ").await;
    let code = b.printed("code: ").await;
    let (page, id) = link.split_once("/#/pair/").expect("a pairing link");
    assert_eq!(page, a.url.replace("ws://", "http://"), "{link}");
    let pending = call(user, "hosts.pairing", json!({"pairing_id": id})).await;
    assert_eq!(pending["type"], "pending", "{pending}");
    assert_eq!(
        pending["code"], code,
        "the browser shows the code the host shows"
    );
    assert_eq!(pending["name"], name, "{pending}");
    let done = call(user, "hosts.confirm", json!({"pairing_id": id})).await;
    assert_eq!(done["type"], "paired", "{done}");
    (b, code)
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
    // The history is read at the host, by its name in the path and as a parameter.
    let page = call(&ada, "agents.on.nat.history", json!({"thread": thread})).await;
    assert_eq!(page["type"], "history", "{page}");
    let by_name = call(&ada, "hosts.history", json!({"host": "nat", "thread": thread})).await;
    assert_eq!(by_name["items"], page["items"], "{by_name}");
    let rows = call(&ada, "hosts.threads", json!({"host": "nat", "thread": thread})).await;
    assert_eq!(rows["threads"][0]["title"], "p", "{rows}");
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
async fn pairing_a_fresh_key_shows_one_code_on_both_screens_and_goes_on_as_the_machine() {
    let a = service().await;
    let ada = connect(a.port, Some(&token(ADA))).await.unwrap();
    let (key, seed) = a.key(7);
    let (b, code) = pair(&a, &ada, "rig", &key).await;
    // The host computed it from the key the server proved, and that is key_s.
    let server = ServerKey::from_seed(SERVER_SEED);
    assert_eq!(code, pairing::code(&public(&seed), &server.public_bytes()));
    // Someone in the middle substitutes a key, and the screens differ.
    let mitm = ServerKey::from_seed([66; 32]);
    assert_ne!(code, pairing::code(&public(&seed), &mitm.public_bytes()));

    online(&ada, "rig", "machine").await;
    let record = a.hosts.store().get("rig").unwrap();
    assert_eq!(
        format!("key:{}", record.public_key.unwrap()),
        principal(&seed)
    );
    assert_eq!(record.owner, format!("idp:{ADA}"));
    // The service counts the host online once the confirm lands; the host writes
    // its pin when its wait answers, which may be a moment later.
    eventually("the host pinned key_s", || b.pin(&a) == Some(server.public())).await;
    let r = call(&ada, "agents.on.rig.harnesses", json!({})).await;
    assert_eq!(r["type"], "harnesses", "{r}");

    // The key holds nothing but its attach: not confirming, not another's host.
    let machine = connect_bearer(a.port, &a.proof(&seed)).await.unwrap();
    let r = call(&machine, "hosts.confirm", json!({"pairing_id": "x"})).await;
    assert_eq!(r["error_kind"], "forbidden", "{r}");
    let r = call(&machine, "agents.on.rig.threads", json!({})).await;
    assert_eq!(r["error_kind"], "forbidden", "{r}");
    // And there's no way for a user's token to pair a key any more.
    let r = try_open(
        &ada,
        "hosts.pair",
        json!({"name": "x", "public_key": principal(&[9; 32])}),
    )
    .await;
    let refused = match r {
        Err(e) => e,
        Ok(mut s) => next(&mut s).await.to_string(),
    };
    assert!(
        refused.contains("not found") || refused.contains("error"),
        "{refused}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpaired_key_can_call_nothing_but_request_and_wait() {
    let a = service().await;
    let (_, seed) = a.key(3);
    let waiting = connect_bearer(a.port, &a.proof(&seed))
        .await
        .expect("the waiting room");
    for (method, params) in [
        ("hosts.list", json!({})),
        ("hosts.attach", json!({"nonce": "n-123456789abcdef"})),
        ("hosts.pairing", json!({"pairing_id": "x"})),
        ("hosts.confirm", json!({"pairing_id": "x"})),
        ("agents.harnesses", json!({})),
        ("agents.threads", json!({})),
        ("agents.on.placements", json!({})),
        ("agents.claudecode.describe", json!({})),
        ("agents.schema", json!({})),
    ] {
        let r = try_open(&waiting, method, params).await;
        assert!(r.is_err(), "{method} answered an unpaired key");
    }
    let direct: Result<Value, _> = waiting
        .request("plexus-agents.schema", jsonrpsee::rpc_params![])
        .await;
    assert!(direct.is_err(), "{direct:?}");

    let nonce = "a-nonce-of-the-macs-own-0001";
    let asked = call(
        &waiting,
        "hosts.request",
        json!({"name": "box", "nonce": nonce}),
    )
    .await;
    assert_eq!(asked["type"], "pairing", "{asked}");
    let hex = principal(&seed).strip_prefix("key:").unwrap().to_string();
    let server = asked["server_key"].as_str().unwrap();
    assert_eq!(server, ServerKey::from_seed(SERVER_SEED).public());
    let sig = asked["signature"].as_str().unwrap();
    assert!(pairing::verify(
        server,
        &pairing::pair_message(nonce, &hex),
        sig
    ));
    assert!(!pairing::verify(
        server,
        &pairing::pair_message("another", &hex),
        sig
    ));
    let id = asked["pairing_id"].as_str().unwrap().to_string();

    // At most MAX_PER_IP pending from one address.
    let mut others = Vec::new();
    for byte in 0..MAX_PER_IP as u8 {
        let (_, seed) = a.key(100 + byte);
        let c = connect_bearer(a.port, &a.proof(&seed)).await.unwrap();
        let nonce = format!("nonce-of-another-mac-{byte:04}");
        let r = call(
            &c,
            "hosts.request",
            json!({"name": format!("o{byte}"), "nonce": nonce}),
        )
        .await;
        others.push((c, r));
    }
    let (last, r) = others.pop().unwrap();
    assert!(
        r["message"]
            .as_str()
            .unwrap_or_default()
            .contains("too many"),
        "{r}"
    );
    drop(last);
    assert!(
        others.iter().all(|(_, r)| r["type"] == "pairing"),
        "{others:?}"
    );

    // Ada types the code (in any case) and confirms; the wait answers paired.
    let mut wait = open(&waiting, "hosts.wait", json!({"pairing_id": id})).await;
    let code = pairing::code(
        &public(&seed),
        &ServerKey::from_seed(SERVER_SEED).public_bytes(),
    );
    let ada = connect(a.port, Some(&token(ADA))).await.unwrap();
    let typed = code.to_lowercase().replace('-', " ");
    let pending = call(&ada, "hosts.pairing", json!({"code": typed})).await;
    assert_eq!(pending["pairing_id"], json!(id), "{pending}");
    let anon = connect(a.port, None).await.unwrap();
    let r = call(&anon, "hosts.confirm", json!({"pairing_id": id})).await;
    assert_eq!(
        r["error_kind"], "unauthenticated",
        "only a signed-in user confirms: {r}"
    );
    let done = call(&ada, "hosts.confirm", json!({"pairing_id": id})).await;
    assert_eq!(done["type"], "paired", "{done}");
    let paired = next(&mut wait).await;
    assert_eq!(paired["content"]["type"], "paired", "{paired}");
    assert_eq!(paired["content"]["owner"], format!("idp:{ADA}"), "{paired}");

    // The same socket goes on as the machine.
    let nonce = "attach-nonce-000000001";
    let r = call(&waiting, "hosts.attach", json!({"nonce": nonce})).await;
    assert_eq!(r["type"], "attached", "{r}");
    assert_eq!(r["mode"], "machine", "{r}");
    let sig = r["signature"].as_str().unwrap();
    assert!(pairing::verify(
        server,
        &pairing::attach_message(nonce),
        sig
    ));
    assert!(a.hosts.online("box"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unpair_closes_the_socket_and_the_key_goes_back_to_waiting() {
    let a = service().await;
    let ada = connect(a.port, Some(&token(ADA))).await.unwrap();
    let (key, seed) = a.key(5);
    let (_b, _) = pair(&a, &ada, "gone", &key).await;
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
    // A redial is back in the waiting room: it can't attach.
    let again = connect_bearer(a.port, &a.proof(&seed)).await.unwrap();
    let r = try_open(
        &again,
        "hosts.attach",
        json!({"nonce": "n-after-unpair-0001"}),
    )
    .await;
    assert!(r.is_err(), "an unpaired key attached");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !a.hosts.online("gone"),
        "the host's own redials don't attach"
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

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn read_ws(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(10), ws.next()).await;
        if let Message::Text(t) = m.unwrap().unwrap().unwrap() {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

async fn send_ws(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// A server of the test's own, holding `key`, which a user-mode host dials: it
/// takes the host's attach and answers with `key`'s proof.
async fn fake_server(
    key: &ServerKey,
    pins: Option<PathBuf>,
) -> (
    Ws,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    tempfile::TempDir,
) {
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
    config.pins = pins;
    let b = hub(runtime, harnesses, vec![host::server_grant()]);
    let running = tokio::spawn(host::run(b, config));

    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let attach = read_ws(&mut ws).await;
    assert_eq!(attach["params"]["method"], "hosts.attach", "{attach}");
    let nonce = attach["params"]["params"]["nonce"].as_str().unwrap();
    let attached = json!({"type": "data", "content": {
        "type": "attached", "name": "h", "mode": "user",
        "server_key": key.public(), "signature": key.sign(&pairing::attach_message(nonce)),
    }});
    send_ws(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": attach["id"], "result": "s1"}),
    )
    .await;
    send_ws(
        &mut ws,
        json!({"jsonrpc": "2.0", "method": "plexus-agents.call",
               "params": {"subscription": "s1", "result": attached}}),
    )
    .await;
    (ws, running, dir)
}

/// Sends `method` to the host with `hop`, and returns the first item it answers.
async fn ask(ws: &mut Ws, id: &str, method: &str, hop: Value) -> Value {
    let frame = json!({"jsonrpc": "2.0", "id": id, "method": "plexus-agents.call",
                       "params": {"method": method, "params": {}, "hop": hop}});
    send_ws(ws, frame).await;
    let mut sub = None;
    loop {
        let v = read_ws(ws).await;
        if v["id"] == json!(id) {
            sub = Some(v["result"].clone());
            continue;
        }
        if sub.is_some() && v["params"]["subscription"] == sub.clone().unwrap() {
            return v["params"]["result"].clone();
        }
    }
}

fn signed(key: &ServerKey, call: &str, method: &str) -> Value {
    let nonce = format!("nonce-{call}");
    let sig = key.sign(&pairing::call_message(call, method, &nonce));
    json!({"call": call, "nonce": nonce, "sig": sig})
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_refuses_calls_the_server_key_didnt_sign_and_outside_agents() {
    let key = ServerKey::from_seed(SERVER_SEED);
    let (mut ws, running, _dir) = fake_server(&key, None).await;
    let other = ServerKey::from_seed([66; 32]);

    let r = ask(
        &mut ws,
        "a-1",
        "agents.harnesses",
        json!({"server": "ws://x"}),
    )
    .await;
    assert_eq!(r["error_kind"], "forbidden", "no signature: {r}");
    let r = ask(
        &mut ws,
        "a-2",
        "agents.harnesses",
        signed(&other, "c2", "agents.harnesses"),
    )
    .await;
    assert_eq!(r["error_kind"], "forbidden", "another key's signature: {r}");
    let r = ask(
        &mut ws,
        "a-3",
        "agents.harnesses",
        signed(&key, "c3", "agents.threads"),
    )
    .await;
    assert_eq!(
        r["error_kind"], "forbidden",
        "signed for another method: {r}"
    );
    let mut bent = signed(&key, "c4", "agents.harnesses");
    bent["nonce"] = json!("another-nonce");
    let r = ask(&mut ws, "a-4", "agents.harnesses", bent).await;
    assert_eq!(
        r["error_kind"], "forbidden",
        "signed over another nonce: {r}"
    );
    let r = ask(
        &mut ws,
        "a-5",
        "plexus-agents.hash",
        signed(&key, "c5", "plexus-agents.hash"),
    )
    .await;
    assert_eq!(r["error_kind"], "forbidden", "outside agents.*: {r}");
    let on = "agents.on.local.harnesses";
    let r = ask(&mut ws, "a-6", on, signed(&key, "c6", on)).await;
    assert_eq!(r["error_kind"], "forbidden", "on, past the host: {r}");
    let r = ask(
        &mut ws,
        "a-7",
        "agents.harnesses",
        signed(&key, "c7", "agents.harnesses"),
    )
    .await;
    assert_eq!(r["type"], "data", "signed by the key, under agents.*: {r}");
    assert_eq!(r["content"]["type"], "harnesses", "{r}");
    let r = ask(
        &mut ws,
        "a-8",
        "agents.harnesses",
        signed(&key, "c7", "agents.harnesses"),
    )
    .await;
    assert_eq!(r["error_kind"], "forbidden", "a call id used again: {r}");

    let schema =
        json!({"jsonrpc": "2.0", "id": "x", "method": "plexus-agents.schema", "params": {}});
    send_ws(&mut ws, schema).await;
    loop {
        let v = read_ws(&mut ws).await;
        if v["id"] == "x" {
            assert_eq!(v["error"]["code"], -32601, "{v}");
            break;
        }
    }
    running.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_refuses_a_server_that_cant_prove_the_pinned_key() {
    let pins = tempfile::tempdir().unwrap();
    let pinned = ServerKey::from_seed(SERVER_SEED);
    let impostor = ServerKey::from_seed([66; 32]);
    // A free port, its URL pinned to key_s before the host dials it.
    let dir = pins.path().to_path_buf();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
    drop(listener);
    std::fs::write(host::pin_path(&dir, &url), pinned.public()).unwrap();
    let token = dir.join("t");
    std::fs::write(&token, "a token").unwrap();
    let env: Arc<dyn Environment> = Arc::new(Nowhere);
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: dir.join("host.db"),
        environment: env.clone(),
        placements: HashMap::from([("local".to_string(), env)]),
        harnesses: harnesses.clone(),
    })
    .await
    .unwrap();
    let mut config = HostConfig::new(&url, "h");
    config.token_file = Some(token);
    config.pins = Some(dir.clone());
    let listener = tokio::net::TcpListener::bind(url.trim_start_matches("ws://"))
        .await
        .unwrap();
    let b = hub(runtime, harnesses, vec![host::server_grant()]);
    let running = tokio::spawn(host::run(b, config));
    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let attach = read_ws(&mut ws).await;
    let nonce = attach["params"]["params"]["nonce"].as_str().unwrap();
    // The impostor's own key, properly signed: still not the pinned one.
    let attached = json!({"type": "data", "content": {
        "type": "attached", "name": "h", "mode": "user",
        "server_key": impostor.public(), "signature": impostor.sign(&pairing::attach_message(nonce)),
    }});
    send_ws(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": attach["id"], "result": "s1"}),
    )
    .await;
    send_ws(
        &mut ws,
        json!({"jsonrpc": "2.0", "method": "plexus-agents.call",
               "params": {"subscription": "s1", "result": attached}}),
    )
    .await;
    let ended = tokio::time::timeout(Duration::from_secs(10), running).await;
    let e = ended
        .expect("the host stops")
        .unwrap()
        .expect_err("refused");
    assert!(e.to_string().contains("pinned"), "{e}");
    assert!(e.is::<host::Untrusted>(), "{e}");
    assert_eq!(
        std::fs::read_to_string(host::pin_path(&dir, &url))
            .unwrap()
            .trim(),
        pinned.public(),
        "the pin stands"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_mode_host_pins_the_key_its_first_attach_proves() {
    let a = service().await;
    let (file, ada_token) = a.token_file(ADA);
    let b = Host::start(&a, "tofu", &["--token-file", file.to_str().unwrap()]);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    online(&ada, "tofu", "user").await;
    eventually("the pin is written", || b.pin(&a).is_some()).await;
    assert_eq!(b.pin(&a), Some(a.hosts.server_key()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_store_holds_no_token_or_private_key() {
    let a = service().await;
    let ada_token = token(ADA);
    let (key, seed) = a.key(11);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    let (_b, _) = pair(&a, &ada, "vault", &key).await;
    online(&ada, "vault", "machine").await;
    let r = call(&ada, "agents.on.vault.harnesses", json!({})).await;
    assert_eq!(r["type"], "harnesses", "{r}");

    let dump = dump(&a.db);
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    let b64 = base64::engine::general_purpose::STANDARD.encode(seed);
    let server_hex: String = SERVER_SEED.iter().map(|b| format!("{b:02x}")).collect();
    let server_b64 = base64::engine::general_purpose::STANDARD.encode(SERVER_SEED);
    let payload = ada_token.split('.').nth(1).unwrap();
    for secret in [
        ada_token.as_str(),
        payload,
        hex.as_str(),
        b64.as_str(),
        server_hex.as_str(),
        server_b64.as_str(),
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
    // Only public keys, names, owners, modes and timestamps: one row, these columns.
    let hosts = dump.lines().filter(|l| l.starts_with("hosts: ")).count();
    assert_eq!(hosts, 1, "{dump}");
    let conn = rusqlite::Connection::open(&a.db).unwrap();
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('hosts')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let expected = [
        "host_id",
        "name",
        "owner",
        "public_key",
        "mode",
        "created",
        "last_seen",
    ];
    assert_eq!(columns, expected);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_service_with_no_local_placement_runs_nothing_itself() {
    let a = service_placed(false).await;
    let (file, ada_token) = a.token_file(ADA);
    let _b = Host::start(&a, "mac", &["--token-file", file.to_str().unwrap()]);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    online(&ada, "mac", "user").await;
    let run = json!({"placement": "local", "cwd": "/tmp", "prompt": "p"});
    let r = call(&ada, "agents.start", run).await;
    assert_eq!(r["error_kind"], "forbidden", "no local placement: {r}");
    let on = call(&ada, "agents.on.placements", json!({})).await;
    assert_eq!(
        on["placements"],
        json!([{"name": "mac", "peer": true}]),
        "{on}"
    );
    let r = call(&ada, "agents.on.mac.harnesses", json!({})).await;
    assert_eq!(r["type"], "harnesses", "{r}");
}

/// A host's owner may put a thread there in `bypassPermissions`, change its mode,
/// and list the folders it may start in; nobody else can reach any of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosts_owner_picks_its_threads_modes_and_sees_its_folders() {
    let a = service().await;
    let roots = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(roots.path().join("repo/.git")).unwrap();
    std::fs::create_dir_all(roots.path().join(".secret/.git")).unwrap();
    let root = roots.path().to_str().unwrap();
    let (file, ada_token) = a.token_file(ADA);
    let _b = Host::start(
        &a,
        "nat",
        &[
            "--token-file",
            file.to_str().unwrap(),
            "--root",
            root,
            "--allow-bypass",
        ],
    );
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    online(&ada, "nat", "user").await;

    let caps = call(&ada, "hosts.capabilities", json!({"host": "nat"})).await;
    assert_eq!(caps["type"], "capabilities", "{caps}");
    assert_eq!(caps["bypass"], true, "{caps}");
    assert_eq!(
        caps["modes"],
        json!(["default", "acceptEdits", "plan", "bypassPermissions"]),
        "{caps}"
    );
    let run = json!({"host": "nat", "cwd": "/recorded/cwd", "prompt": "p",
                     "mode": "bypassPermissions"});
    let started = call(&ada, "hosts.start", run).await;
    assert_eq!(started["type"], "started", "{started}");
    let thread = started["thread"].as_str().unwrap().to_string();
    let listed = call(
        &ada,
        "hosts.threads",
        json!({"host": "nat", "thread": thread}),
    )
    .await;
    assert_eq!(
        listed["threads"][0]["mode"], "bypassPermissions",
        "{listed}"
    );
    let set = call(
        &ada,
        "hosts.set_mode",
        json!({"host": "nat", "thread": thread, "mode": "plan"}),
    )
    .await;
    assert_eq!(set["type"], "mode_set", "{set}");
    assert_eq!(set["mode"], "plan", "{set}");
    let set = call(
        &ada,
        "agents.on.nat.set_mode",
        json!({"thread": thread, "mode": "default"}),
    )
    .await;
    assert_eq!(set["mode"], "default", "{set}");

    let canon = roots.path().canonicalize().unwrap();
    for (method, params) in [
        ("hosts.folders", json!({"host": "nat"})),
        ("agents.on.nat.folders", json!({})),
    ] {
        let got = call(&ada, method, params).await;
        assert_eq!(got["type"], "folders", "{method}: {got}");
        assert_eq!(got["roots"], json!([canon.to_string_lossy()]), "{got}");
        let names: Vec<_> = got["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"repo".to_string()), "{got}");
        assert!(!names.contains(&".secret".to_string()), "{got}");
    }
    let outside = call(&ada, "hosts.folders", json!({"host": "nat", "under": "/"})).await;
    assert_eq!(outside["type"], "failed", "{outside}");

    let ben = connect(a.port, Some(&token(BEN))).await.unwrap();
    for (method, params) in [
        ("hosts.folders", json!({"host": "nat"})),
        (
            "hosts.set_mode",
            json!({"host": "nat", "thread": thread, "mode": "plan"}),
        ),
        (
            "hosts.start",
            json!({"host": "nat", "cwd": "/x", "prompt": "p", "mode": "bypassPermissions"}),
        ),
    ] {
        let r = call(&ben, method, params).await;
        assert_eq!(r["type"], "failed", "{method}: {r}");
    }
    for method in ["agents.on.nat.folders", "agents.on.nat.set_mode"] {
        let r = call(&ben, method, json!({"thread": thread, "mode": "plan"})).await;
        assert_eq!(r["error_kind"], "forbidden", "{method}: {r}");
    }
    let r = call(&ben, "hosts.capabilities", json!({"host": "nat"})).await;
    assert_eq!(r["type"], "failed", "{r}");
}

/// Without `--allow-bypass` a host refuses `bypassPermissions` even from its owner,
/// says how to allow it, and says so in `capabilities`; every other mode is as
/// with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_without_allow_bypass_refuses_its_owner_bypass() {
    let a = service().await;
    let (file, ada_token) = a.token_file(ADA);
    let _b = Host::start(&a, "nat", &["--token-file", file.to_str().unwrap()]);
    let ada = connect(a.port, Some(&ada_token)).await.unwrap();
    online(&ada, "nat", "user").await;

    for (method, params) in [
        ("hosts.capabilities", json!({"host": "nat"})),
        ("agents.on.nat.capabilities", json!({})),
    ] {
        let caps = call(&ada, method, params).await;
        assert_eq!(caps["type"], "capabilities", "{method}: {caps}");
        assert_eq!(caps["bypass"], false, "{method}: {caps}");
    }
    let run = json!({"host": "nat", "cwd": "/recorded/cwd", "prompt": "p",
                     "mode": "bypassPermissions"});
    let refused = call(&ada, "hosts.start", run).await;
    assert_eq!(refused["type"], "failed", "{refused}");
    let why = refused["message"].as_str().unwrap_or_default();
    assert!(why.contains("--allow-bypass"), "{refused}");

    let run = json!({"host": "nat", "cwd": "/recorded/cwd", "prompt": "p", "mode": "plan"});
    let started = call(&ada, "hosts.start", run).await;
    assert_eq!(started["type"], "started", "{started}");
    let thread = started["thread"].as_str().unwrap().to_string();
    let set = call(
        &ada,
        "hosts.set_mode",
        json!({"host": "nat", "thread": thread, "mode": "bypassPermissions"}),
    )
    .await;
    assert_eq!(set["type"], "failed", "{set}");
    assert!(
        set["message"]
            .as_str()
            .unwrap_or_default()
            .contains("--allow-bypass"),
        "{set}"
    );
    let listed = call(
        &ada,
        "hosts.threads",
        json!({"host": "nat", "thread": thread}),
    )
    .await;
    assert_eq!(listed["threads"][0]["mode"], "plan", "{listed}");
}

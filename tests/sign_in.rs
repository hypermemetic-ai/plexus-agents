//! SUPERCONDUCTOR-19 through the wire: the service as `plexus-agents` runs it, on
//! one port: the page, plexus-login's sign-in, and the socket behind an origin
//! check. No WorkOS API is called: a WorkOS client is only configured, never used.

use base64::Engine;
use jsonrpsee::core::client::{Subscription, SubscriptionClientT};
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use plexus_agents::front::{self, SignInConfig};
use plexus_agents::hosts::Hosts;
use plexus_agents::pairing::ServerKey;
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{hub_with_people, Serving};
use plexus_ui_service::{Signers, UiActivation, UiStore, DEFAULT_CHANNEL};
use plexus_agents::ClaudeHarness;
use plexus_rpc::auth_core::adapters::sign_key_proof;
use plexus_harness::{Environment, Harness};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const ADMIN_SEED: [u8; 32] = [21; 32];
const OTHER_SEED: [u8; 32] = [22; 32];
/// The key the service takes a UI push signed by (`--ui-signer`).
const UI_SEED: [u8; 32] = [23; 32];

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

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

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

struct Service {
    port: u16,
    hosts: Arc<Hosts>,
    dir: tempfile::TempDir,
}

/// The service, wired as `main` wires it: no local placement, an admin key.
async fn service(workos: Option<&str>) -> Service {
    service_allowing(workos, &[]).await
}

/// [`service`], with `--allow` rules.
async fn service_allowing(workos: Option<&str>, allow: &[&str]) -> Service {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("agents.db");
    let env: Arc<dyn Environment> = Arc::new(Nowhere);
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: db.clone(),
        environment: env,
        placements: HashMap::new(),
        harnesses: harnesses.clone(),
    })
    .await
    .unwrap();
    let port = free_port();
    let url = format!("ws://127.0.0.1:{port}");
    let key_file = workos.map(|key| {
        let path = dir.path().join("workos-api-key");
        std::fs::write(&path, key).unwrap();
        path
    });
    let admin = principal(&ADMIN_SEED);
    let config = SignInConfig {
        public_url: Some(format!("http://127.0.0.1:{port}")),
        workos_client: Some("client_01TESTTESTTESTTESTTESTTEST".into()),
        workos_key_file: key_file,
        admins: vec![admin.clone()],
        loopback: true,
        allow: allow.iter().map(|r| r.to_string()).collect(),
        ..SignInConfig::default()
    };
    let login = front::sign_in(&config, port).unwrap();
    let hosts = Hosts::open(&db, &url, ServerKey::from_seed([42; 32])).unwrap();
    let grants = vec![format!("{admin}=admin@*").parse().unwrap()];
    // plexus-ui, linked: its store beside the agents file, a push signed by UI_SEED.
    let store = Arc::new(UiStore::open_channel(&dir.path().join("ui.db"), DEFAULT_CHANNEL).unwrap());
    let signer = principal(&UI_SEED).trim_start_matches("key:").to_string();
    let ui = UiActivation::new(store.clone(), Signers::parse(&[signer]).unwrap());
    let hub = hub_with_people(
        runtime,
        harnesses,
        grants,
        vec![],
        Some(hosts.clone()),
        login.backend(),
        Some(ui),
        Some(login.people.clone()),
        login.people_grants.clone(),
    );
    let serving = Serving {
        hosts: Some(hosts.clone()),
        ui: Some(store),
        ..Serving::new([127, 0, 0, 1].into(), port)
    };
    tokio::spawn(front::serve_public(hub, login, serving));
    for _ in 0..500 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Service { port, hosts, dir };
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not up");
}

impl Service {
    fn proof(&self, seed: &[u8; 32], agent: &str) -> String {
        let nonce: [u8; 32] = rand::random();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let aud = format!("ws://127.0.0.1:{}", self.port);
        sign_key_proof(seed, &aud, agent, &nonce, ts)
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// A plain HTTP/1.1 exchange: the status and the body.
fn http(port: u16, request: &str) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split(' ').nth(1).unwrap_or("0").parse().unwrap_or(0);
    let body = out
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    (status, body)
}

fn get(port: u16, path: &str) -> (u16, String) {
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    http(port, &req)
}

async fn connect(port: u16, path: &str, headers: &[(&str, String)]) -> Result<WsClient, String> {
    let mut h = HeaderMap::new();
    for (name, value) in headers {
        let name: axum::http::HeaderName = name.parse().unwrap();
        h.insert(name, HeaderValue::from_str(value).unwrap());
    }
    WsClientBuilder::default()
        .set_headers(h)
        .build(format!("ws://127.0.0.1:{port}{path}"))
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

/// Every data item of a call, or its error.
async fn items(c: &WsClient, method: &str, params: Value) -> Result<Vec<Value>, Value> {
    let mut s = open(c, method, params).await;
    let mut out = Vec::new();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(10), s.next())
            .await
            .expect("in time")
            .expect("open")
            .unwrap();
        match item["type"].as_str() {
            Some("data") => out.push(item["content"].clone()),
            Some("error") => return Err(item),
            Some("done") => return Ok(out),
            _ => {}
        }
    }
}

async fn call(c: &WsClient, method: &str, params: Value) -> Value {
    match items(c, method, params).await {
        Ok(v) => v.into_iter().next().unwrap_or(Value::Null),
        Err(e) => e,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_port_serves_the_shell_the_sign_in_and_the_socket() {
    let a = service(None).await;
    // The shell is plexus-ui's: nothing pushed yet, so its placeholder, at / and /app/.
    for path in ["/", "/app/"] {
        let (status, page) = get(a.port, path);
        assert_eq!(status, 200, "{path}");
        assert!(page.contains("No UI is live yet"), "{path}: {page}");
    }
    let (status, _) = get(a.port, "/login");
    assert_eq!(status, 200);
    let (status, _) = get(a.port, "/login/me");
    assert_eq!(status, 401, "signed out");

    // The socket, at / and at /ws, for a client that sends no Origin.
    for path in ["/", "/ws"] {
        let anon = connect(a.port, path, &[]).await.unwrap();
        let r = call(&anon, "agents.harnesses", json!({})).await;
        assert_eq!(r["type"], "harnesses", "{path}: {r}");
        let r = call(&anon, "hosts.list", json!({})).await;
        assert_eq!(r["error_kind"], "unauthenticated", "{r}");
    }
    // A page of this origin may open it; a page anywhere else may not.
    let mine = connect(a.port, "/ws", &[("origin", a.origin())]).await;
    assert!(mine.is_ok(), "{:?}", mine.err());
    let theirs = connect(a.port, "/ws", &[("origin", "https://evil.example".into())]).await;
    let e = theirs.expect_err("a foreign origin");
    assert!(e.contains("403"), "{e}");
    // A refused credential is refused through the front as at the transport.
    let bad = connect(
        a.port,
        "/ws",
        &[("authorization", "Bearer mgs_nothing".into())],
    )
    .await;
    assert!(bad.expect_err("a bad token").contains("401"));
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_workos_key_key_sign_in_works_and_there_is_no_workos_button() {
    assert!(
        std::env::var("AGENTS_WORKOS_API_KEY").is_err(),
        "the test needs it unset"
    );
    let a = service(None).await;
    let (_, login) = get(a.port, "/login");
    assert!(!login.contains("/login/email"), "no emailed-code form");
    assert!(!login.contains("Continue with"), "no provider button");
    let anon = connect(a.port, "/ws", &[]).await.unwrap();
    let providers = items(&anon, "auth.providers", json!({})).await.unwrap();
    let names: Vec<&str> = providers
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(!names.contains(&"workos"), "{providers:?}");
    assert!(names.contains(&"key"), "{providers:?}");

    // Key sign-in: a key proof, then a session minted on it.
    let bearer = format!("Bearer {}", a.proof(&ADMIN_SEED, "synapse"));
    let admin = connect(a.port, "/ws", &[("authorization", bearer)])
        .await
        .unwrap();
    let me = call(&admin, "auth.whoami", json!({})).await;
    assert!(me.to_string().contains(&principal(&ADMIN_SEED)), "{me}");
    let s = call(
        &admin,
        "auth.login",
        json!({"flow": {"session": {"ttl_secs": 600}}}),
    )
    .await;
    assert_eq!(s["type"], "session", "{s}");
    let token = s["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("mgs_"), "{s}");
    let by_token = connect(
        a.port,
        "/ws",
        &[("authorization", format!("Bearer {token}"))],
    )
    .await
    .unwrap();
    let me = call(&by_token, "auth.whoami", json!({})).await;
    assert!(me.to_string().contains(&principal(&ADMIN_SEED)), "{me}");
    // `--admin key:<hex>` is admin: it lists hosts.
    let r = call(&by_token, "hosts.list", json!({})).await;
    assert_eq!(r["type"], "hosts", "{r}");
    // Another key signs in, and holds nothing.
    let bearer = format!("Bearer {}", a.proof(&OTHER_SEED, "synapse"));
    let other = connect(a.port, "/ws", &[("authorization", bearer)])
        .await
        .unwrap();
    let r = call(&other, "hosts.list", json!({})).await;
    assert_eq!(r["error_kind"], "forbidden", "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_a_workos_key_the_sign_in_offers_github_alone() {
    let a = service(Some("sk_test_00000000000000000000000000")).await;
    let (_, login) = get(a.port, "/login");
    // SUPERCONDUCTOR-25: GitHub alone, no emailed code.
    assert!(!login.contains("/login/email"), "no emailed-code form: {login}");
    assert!(login.contains("/login/social") && login.to_lowercase().contains("github"), "{login}");
    assert!(
        login.contains(r#"name="next""#),
        "it carries next, and the hash with it"
    );
    let anon = connect(a.port, "/ws", &[]).await.unwrap();
    let providers = items(&anon, "auth.providers", json!({})).await.unwrap();
    let names: Vec<&str> = providers
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(names.contains(&"workos"), "{providers:?}");
    // `synapse _login`'s first step: a URL to open, on this service.
    let r = call(
        &anon,
        "auth.login",
        json!({"flow": {"browser": {"redirect": "http://127.0.0.1:9/cb"}}}),
    )
    .await;
    assert_eq!(r["type"], "login_url", "{r}");
    assert!(r["url"].as_str().unwrap().contains("/login"), "{r}");
}

async fn printed(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>, prefix: &str) -> String {
    loop {
        let line = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;
        if let Some(rest) = line.unwrap().unwrap().strip_prefix(prefix) {
            return rest.to_string();
        }
    }
}

/// The host stub, dialing the front.
struct Host(Child, #[allow(dead_code)] tempfile::TempDir);

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_pairs_through_the_page_socket_with_a_session_cookie() {
    let a = service(None).await;
    let dir = tempfile::tempdir().unwrap();
    let key: PathBuf = dir.path().join("mac.key");
    std::fs::write(
        &key,
        base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
    )
    .unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let test = std::env::current_exe().unwrap();
    let bin = test.parent().unwrap().parent().unwrap();
    let mut child = Command::new(bin.join("examples/host_stub"))
        .args([
            "--server",
            &format!("ws://127.0.0.1:{}/ws", a.port),
            "--name",
            "mac",
        ])
        .args(["--db", dir.path().join("h.db").to_str().unwrap()])
        .args(["--pins", dir.path().join("pins").to_str().unwrap()])
        .args(["--key", key.to_str().unwrap()])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let _host = Host(child, dir);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let link = printed(&mut rx, "link: ").await;
    let code = printed(&mut rx, "code: ").await;
    let (page, id) = link.split_once("/#/pair/").unwrap();
    assert_eq!(page, a.origin(), "the link is this service's page");

    // The page's socket: same origin, the session as a cookie.
    let bearer = format!("Bearer {}", a.proof(&ADMIN_SEED, "synapse"));
    let admin = connect(a.port, "/ws", &[("authorization", bearer)])
        .await
        .unwrap();
    let s = call(&admin, "auth.login", json!({"flow": {"session": {}}})).await;
    let cookie = format!("access_token={}", s["token"].as_str().unwrap());
    let browser = connect(a.port, "/ws", &[("origin", a.origin()), ("cookie", cookie)])
        .await
        .unwrap();
    let pending = call(&browser, "hosts.pairing", json!({"pairing_id": id})).await;
    assert_eq!(pending["code"], code, "{pending}");
    let done = call(&browser, "hosts.confirm", json!({"pairing_id": id})).await;
    assert_eq!(done["type"], "paired", "{done}");
    for _ in 0..500 {
        if a.hosts.online("mac") {
            let r = call(&browser, "agents.on.mac.harnesses", json!({})).await;
            assert_eq!(r["type"], "harnesses", "{r}");
            // SUPERCONDUCTOR-11: the same host by a fixed name, as the bundle calls it.
            let r = call(&browser, "hosts.threads", json!({"host": "mac"})).await;
            assert_eq!(r["type"], "threads", "{r}");
            let r = call(&browser, "hosts.pending", json!({"host": "mac"})).await;
            assert_eq!(r["type"], "pending", "{r}");
            assert!(a.dir.path().exists());
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the host never came online as the machine");
}

/// The waiting room's per-address limit counts the client a trusted proxy
/// forwards, not the proxy: here the test is the proxy, on loopback.
#[tokio::test(flavor = "multi_thread")]
async fn the_pairing_limit_counts_the_forwarded_client() {
    let a = service(None).await;
    let ask = |seed: u8, from: &'static str| {
        let bearer = format!(
            "Bearer {}",
            a.proof(&[seed; 32], plexus_agents::hosts::HOST_AGENT)
        );
        let port = a.port;
        async move {
            let headers = [
                ("authorization", bearer),
                ("x-forwarded-for", from.to_string()),
            ];
            let c = connect(port, "/ws", &headers).await.unwrap();
            let nonce = format!("nonce-of-mac-number-{seed:04}");
            let r = call(
                &c,
                "hosts.request",
                json!({"name": format!("m{seed}"), "nonce": nonce}),
            )
            .await;
            (c, r)
        }
    };
    let mut held = Vec::new();
    for seed in 0..plexus_agents::hosts::MAX_PER_IP as u8 {
        let (c, r) = ask(100 + seed, "203.0.113.1").await;
        assert_eq!(r["type"], "pairing", "{r}");
        held.push(c);
    }
    let (_c, r) = ask(120, "203.0.113.1").await;
    assert!(
        r["message"]
            .as_str()
            .unwrap_or_default()
            .contains("too many"),
        "{r}"
    );
    let (_c, r) = ask(121, "203.0.113.2").await;
    assert_eq!(r["type"], "pairing", "another client: {r}");
}

/// A tar of `files`, in order, as plexus-ui's push reads one.
fn tar_of(files: &[(&str, &str)]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, body) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, name, body.as_bytes()).unwrap();
    }
    b.into_inner().unwrap()
}

/// `ui.push`'s params for `bundle`, signed by `seed` over its sha256.
fn push_params(seed: &[u8; 32], bundle: &[u8], manifest: &str) -> Value {
    use ed25519_dalek::Signer;
    use sha2::Digest;
    let signature = ed25519_dalek::SigningKey::from_bytes(seed).sign(&sha2::Sha256::digest(bundle));
    let b64 = base64::engine::general_purpose::STANDARD;
    json!({ "bundle": b64.encode(bundle), "manifest": manifest, "signature": b64.encode(signature.to_bytes()), "commit": "test@0000000" })
}

/// SUPERCONDUCTOR-11: a bundle pushed by the admin, signed by a `--ui-signer`, is
/// what `/` serves, stamped with this port and this service; one signed by any
/// other key, or pushed by someone who isn't a deployer, is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_pushed_bundle_is_the_shell() {
    let a = service(None).await;
    let index = "<!doctype html><title>{{TITLE}}</title><meta name=\"plexus-service\" content=\"{{SERVICE}}\"><meta name=\"plexus-ws-port\" content=\"{{WS_PORT}}\"><p>superconductor test bundle</p>";
    let bundle = tar_of(&[("index.html", index)]);
    let manifest = json!({ "abi": "engine/1", "contract": "unknown", "routes": {}, "components": {}, "slots": {} }).to_string();

    let bearer = format!("Bearer {}", a.proof(&ADMIN_SEED, "synapse"));
    let admin = connect(a.port, "/ws", &[("authorization", bearer)]).await.unwrap();
    let wrong = call(&admin, "ui.push", push_params(&OTHER_SEED, &bundle, &manifest)).await;
    assert_eq!(wrong["type"], "refused", "{wrong}");
    let bearer = format!("Bearer {}", a.proof(&OTHER_SEED, "synapse"));
    let other = connect(a.port, "/ws", &[("authorization", bearer)]).await.unwrap();
    let r = call(&other, "ui.push", push_params(&UI_SEED, &bundle, &manifest)).await;
    assert_eq!(r["error_kind"], "forbidden", "not a deployer: {r}");

    let pushed = call(&admin, "ui.push", push_params(&UI_SEED, &bundle, &manifest)).await;
    assert_eq!(pushed["type"], "pushed", "{pushed}");
    assert_eq!(pushed["live"], true, "{pushed}");
    let (status, page) = get(a.port, "/");
    assert_eq!(status, 200);
    assert!(page.contains("superconductor test bundle"), "{page}");
    assert!(page.contains(&format!("content=\"{}\"", a.port)), "the socket is this port: {page}");
    assert!(page.contains("content=\"plexus-agents\""), "frames are plexus-agents.call: {page}");
    let versions = items(&admin, "ui.versions", json!({})).await.unwrap();
    assert!(versions.iter().any(|v| v["live"] == true), "{versions:?}");
}

/// SUPERCONDUCTOR-25: with an allow rule (github, so only a WorkOS sign-in with
/// that GitHub id could get a session), key sign-in, sessions minted on it and
/// a host's pairing go on as before, and the sign-in offers GitHub alone.
#[tokio::test(flavor = "multi_thread")]
async fn with_an_allow_rule_keys_and_their_sessions_still_sign_in() {
    let a = service_allowing(Some("sk_test_00000000000000000000000000"), &["github:251524973"]).await;
    let (_, login) = get(a.port, "/login");
    assert!(!login.contains("/login/email"), "{login}");
    let bearer = format!("Bearer {}", a.proof(&ADMIN_SEED, "synapse"));
    let admin = connect(a.port, "/ws", &[("authorization", bearer)]).await.unwrap();
    let r = call(&admin, "hosts.list", json!({})).await;
    assert_eq!(r["type"], "hosts", "{r}");
    let s = call(&admin, "auth.login", json!({"flow": {"session": {"ttl_secs": 600}}})).await;
    let token = s["token"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();
    let by_token = connect(a.port, "/ws", &[("authorization", format!("Bearer {token}"))]).await.unwrap();
    let me = call(&by_token, "auth.whoami", json!({})).await;
    assert!(me.to_string().contains(&principal(&ADMIN_SEED)), "{me}");
}

/// SUPERCONDUCTOR-11: a host's agents by a fixed name, the host a parameter
/// (`hosts.threads {host}`): its owner's alone, refused to anyone else as a host
/// that isn't there.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_by_a_fixed_name_is_its_owner_alone() {
    let a = service(None).await;
    let bearer = format!("Bearer {}", a.proof(&ADMIN_SEED, "synapse"));
    let admin = connect(a.port, "/ws", &[("authorization", bearer)]).await.unwrap();
    let r = call(&admin, "hosts.threads", json!({"host": "nobody"})).await;
    assert_eq!(r["type"], "failed", "{r}");
    let r = call(&admin, "hosts.watch", json!({"host": "nobody", "thread": "t"})).await;
    assert_eq!(r["type"], "failed", "{r}");
    let anon = connect(a.port, "/ws", &[]).await.unwrap();
    let r = call(&anon, "hosts.threads", json!({"host": "nobody"})).await;
    assert_eq!(r["error_kind"], "unauthenticated", "{r}");
}

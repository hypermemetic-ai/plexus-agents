//! AGENTS-18 through the wire: `agents.on.<placement>` between real services on real
//! WebSockets, signed in with key proof. A service here replays real runs. Real Claude
//! and real synapse are `scripts/on-demo`.

use async_trait::async_trait;
use base64::Engine;
use futures::future::BoxFuture;
use jsonrpsee::core::client::{Subscription, SubscriptionClientT};
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use plexus_agents::auth::Grant;
use plexus_agents::peer::{Peer, PeerConfig};
use plexus_agents::runtime::{Supervisor, SupervisorConfig, ThreadId};
use plexus_agents::service::{hub_with_peers, serve, SignIn};
use plexus_agents::ClaudeHarness;
use plexus_rpc::auth_core::identity::{Authenticated, Principal};
use plexus_rpc::core::plexus::{AuthContext, Backoff, SessionValidator};
use plexus_harness::conformance::replay::{ExitTrigger, ReplayEnvironment, ReplayScript};
use plexus_harness::{Environment, Harness, LaunchPlan, Process, SpawnError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const ADMIN: &str = "00000000-0000-4000-8000-00000000000a";
const SCOPED: &str = "00000000-0000-4000-8000-00000000000b";
const USER: &str = "00000000-0000-4000-8000-00000000000c";

fn phase(dir: PathBuf, killed: bool) -> ReplayScript {
    let trigger = if killed {
        ExitTrigger::OnSignal
    } else {
        ExitTrigger::OnInputClose
    };
    let exit = ReplayScript::exit_from_rc(0);
    ReplayScript::load(&dir, "1-claude", exit, trigger, 0).unwrap()
}

struct Sequence(Vec<ReplayEnvironment>, AtomicUsize);

impl Environment for Sequence {
    fn name(&self) -> &str {
        "sequence"
    }
    fn spawn(&self, plan: LaunchPlan) -> BoxFuture<'static, Result<Process, SpawnError>> {
        self.0[self.1.fetch_add(1, Ordering::SeqCst)].spawn(plan)
    }
}

fn parked_run() -> Sequence {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/recorded/agents-9-2026-11-01/restart-parked");
    let steps = [
        phase(dir.join("svc1"), true),
        phase(dir.join("svc2"), false),
    ];
    Sequence(
        steps.into_iter().map(ReplayEnvironment::new).collect(),
        AtomicUsize::new(0),
    )
}

/// A caller's token here is just its id: what's under test is what happens after the
/// upgrade. Key proof is the real adapter.
struct Tokens(Arc<dyn SessionValidator>);

#[async_trait]
impl SessionValidator for Tokens {
    async fn validate(&self, value: &str) -> Option<AuthContext> {
        if let Some(id) = value.strip_prefix("access_token=") {
            let principal: Principal = format!("idp:{id}").parse().ok()?;
            let ok = Authenticated::new(principal, id, vec![], json!({}));
            return Some(ok.into_auth_context());
        }
        self.0.validate(value).await
    }
}

struct Key(PathBuf, #[allow(dead_code)] tempfile::TempDir);

fn key(byte: u8) -> Key {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.key");
    let text = base64::engine::general_purpose::STANDARD.encode([byte; 32]);
    std::fs::write(&path, text).unwrap();
    let mode = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&path, mode).unwrap();
    Key(path, dir)
}

impl Key {
    fn principal(&self) -> String {
        plexus_agents::peer::principal_of_file(&self.0).unwrap()
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

fn peer(name: &str, port: u16, key: &Key) -> Arc<Peer> {
    let spec = format!("{name}=ws://127.0.0.1:{port}");
    let mut config = PeerConfig::parse(&spec, key.0.to_str().unwrap()).unwrap();
    config.timeout = Duration::from_secs(2);
    config.backoff = Backoff {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(200),
        per_minute: 1000,
    };
    Peer::new(config).unwrap()
}

struct Service {
    port: u16,
    runtime: Supervisor,
    _dir: tempfile::TempDir,
}

async fn service(port: u16, grants: &[String], peers: Vec<Arc<Peer>>) -> Service {
    let dir = tempfile::tempdir().unwrap();
    let env: Arc<dyn Environment> = Arc::new(parked_run());
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let places = ["local", "other"].map(|p| (p.to_string(), env.clone()));
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: dir.path().join("agents.db"),
        environment: env.clone(),
        placements: HashMap::from(places),
        harnesses: harnesses.clone(),
    })
    .await
    .unwrap();
    let grants: Vec<Grant> = grants.iter().map(|g| g.parse().unwrap()).collect();
    let h = hub_with_peers(runtime.clone(), harnesses, grants, peers);
    let sign_in = SignIn {
        tokens: None,
        key_audiences: vec![format!("ws://127.0.0.1:{port}")],
    };
    let validator = Arc::new(Tokens(sign_in.validator().unwrap()));
    tokio::spawn(serve(h, validator, [127, 0, 0, 1].into(), port));
    up(port).await;
    Service {
        port,
        runtime,
        _dir: dir,
    }
}

async fn connect(port: u16, who: Option<&str>) -> Result<WsClient, String> {
    let mut headers = HeaderMap::new();
    if let Some(id) = who {
        let cookie = HeaderValue::from_str(&format!("access_token={id}")).unwrap();
        headers.insert("cookie", cookie);
    }
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

fn hal_grants(root: &Key) -> Vec<String> {
    vec![format!("{}=admin@*", root.principal())]
}

fn root_grants() -> Vec<String> {
    vec![
        format!("idp:{ADMIN}=admin@*"),
        format!("idp:{USER}=user@local"),
        format!("idp:{SCOPED}=admin@local"),
    ]
}

async fn pair() -> (Service, Service, Key) {
    let root_key = key(1);
    let hal = service(free_port(), &hal_grants(&root_key), vec![]).await;
    let peers = vec![peer("hal", hal.port, &root_key)];
    let root = service(free_port(), &root_grants(), peers).await;
    (root, hal, root_key)
}

const RUN: fn() -> Value = || json!({"placement": "local", "cwd": "/recorded/cwd", "prompt": "p"});

#[tokio::test(flavor = "multi_thread")]
async fn an_anonymous_caller_gets_nothing_from_a_peer() {
    let (root, hal, _key) = pair().await;
    let anon = connect(root.port, None).await.unwrap();
    for (method, params) in [
        ("agents.on.hal.schema", json!({})),
        ("agents.on.hal.schema", json!({"method": "start"})),
        ("agents.on.hal.claudecode.schema", json!({})),
        ("agents.on.hal.threads", json!({})),
        ("agents.on.hal.harnesses", json!({})),
        ("agents.on.hal.start", RUN()),
        ("agents.on.nobody.schema", json!({})),
        ("agents.on.local.schema", json!({})),
        ("agents.on.local.harnesses", json!({})),
        ("agents.on.placements", json!({})),
    ] {
        let r = call(&anon, method, params).await;
        assert_eq!(r["error_kind"], "unauthenticated", "{method}: {r}");
    }
    assert!(hal.runtime.threads().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_without_the_grant_gets_nothing_from_a_peer() {
    let (root, _hal, _key) = pair().await;
    let user = connect(root.port, Some(USER)).await.unwrap();
    for method in ["agents.on.hal.schema", "agents.on.hal.threads"] {
        let r = call(&user, method, json!({})).await;
        assert_eq!(r["error_kind"], "forbidden", "{method}: {r}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_held_on_one_place_is_refused_another() {
    let root_key = key(1);
    let hal = service(free_port(), &hal_grants(&root_key), vec![]).await;
    let peers = vec![peer("hal", hal.port, &root_key)];
    let grants = vec![format!("idp:{SCOPED}=admin@hal")];
    let root = service(free_port(), &grants, peers).await;
    let scoped = connect(root.port, Some(SCOPED)).await.unwrap();
    let ok = call(&scoped, "agents.on.hal.threads", json!({})).await;
    assert_eq!(ok["type"], "threads", "{ok}");
    let no = call(&scoped, "agents.on.local.threads", json!({})).await;
    assert_eq!(no["error_kind"], "forbidden", "{no}");
    let unknown = call(&scoped, "agents.on.nobody.threads", json!({})).await;
    assert_eq!(unknown["error_kind"], "forbidden", "{unknown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peers_schema_is_its_own() {
    let (root, _hal, _key) = pair().await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    let schema = call(&admin, "agents.on.hal.schema", json!({})).await;
    assert_eq!(schema["namespace"], "agents", "{schema}");
    let names: Vec<_> = schema["methods"].as_array().unwrap().iter().collect();
    assert!(names.iter().any(|m| m["name"] == "watch"));
    let start = call(&admin, "agents.on.hal.schema", json!({"method": "start"})).await;
    assert_eq!(start["name"], "start", "{start}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_goes_through_the_tunnel_and_the_peer_keeps_the_record() {
    let (root, hal, key) = pair().await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    let started = call(&admin, "agents.on.hal.start", RUN()).await;
    assert_eq!(started["type"], "started", "{started}");
    let thread = started["thread"].as_str().unwrap().to_string();
    assert!(root.runtime.threads().unwrap().is_empty(), "nothing here");

    let mut watch = open(
        &admin,
        "agents.on.hal.watch",
        json!({"thread": thread, "since": 0}),
    )
    .await;
    let first = next(&mut watch).await;
    assert_eq!(first["content"]["type"], "event", "{first}");

    let mut requests = Vec::new();
    for _ in 0..500 {
        let p = call(&admin, "agents.on.hal.pending", json!({})).await;
        requests = p["requests"].as_array().unwrap().clone();
        if !requests.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(requests.len(), 1, "parked at hal");
    assert!(root.runtime.pending().unwrap().is_empty());
    let answer = json!({"thread": thread, "request": requests[0]["request"]["id"],
                        "answer": {"answer": "allow", "updated_input": null}});
    let answered = call(&admin, "agents.on.hal.answer", answer).await;
    assert_eq!(answered["type"], "answered", "{answered}");

    let view = hal.runtime.thread(&ThreadId::new(&thread)).unwrap();
    assert_eq!(view.owner.unwrap().id, key.principal(), "the service");
    loop {
        let item = next(&mut watch).await;
        let event = &item["content"]["event"]["event"];
        if event["kind"] == "input.resolved" {
            assert_eq!(event["by"]["id"], json!(key.principal()), "{item}");
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn on_local_is_the_same_surface_bound_to_its_placement() {
    let (root, _hal, _key) = pair().await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    let run = json!({"cwd": "/recorded/cwd", "prompt": "p"});
    let started = call(&admin, "agents.on.local.start", run).await;
    assert_eq!(started["type"], "started", "{started}");
    let thread = started["thread"].as_str().unwrap();
    let home = root.runtime.home(&ThreadId::new(thread)).unwrap();
    assert_eq!(home.placement.as_deref(), Some("local"));

    let elsewhere = json!({"placement": "other", "cwd": "/x", "prompt": "p"});
    let r = call(&admin, "agents.on.local.start", elsewhere).await;
    assert_eq!(r["error_kind"], "invalid_params", "{r}");

    let t = json!({"thread": thread});
    let seen = call(&admin, "agents.on.local.threads", t.clone()).await;
    assert_eq!(seen["threads"].as_array().unwrap().len(), 1, "{seen}");
    let wrong = call(&admin, "agents.on.other.threads", t).await;
    assert_eq!(wrong["error_kind"], "forbidden", "{wrong}");

    let schema = call(&admin, "agents.on.local.schema", json!({})).await;
    let methods = schema["methods"].as_array().unwrap();
    assert!(methods.iter().all(|m| m["name"] != "on"), "{schema}");
    let start = methods.iter().find(|m| m["name"] == "start").unwrap();
    assert!(start["params"]["properties"].get("placement").is_none());
    assert!(start["params"]["properties"].get("prompt").is_some());

    let nested = call(&admin, "agents.on.local.on.hal.threads", json!({})).await;
    assert_eq!(nested["type"], "error", "{nested}");
}

#[tokio::test(flavor = "multi_thread")]
async fn placements_come_from_config_and_an_offline_peer_isnt_listed() {
    let root_key = key(1);
    let hal = service(free_port(), &hal_grants(&root_key), vec![]).await;
    let peers = vec![
        peer("hal", hal.port, &root_key),
        peer("dream", free_port(), &root_key),
    ];
    let root = service(free_port(), &root_grants(), peers).await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    let p = call(&admin, "agents.on.placements", json!({})).await;
    let names: Vec<_> = p["placements"].as_array().unwrap().iter().collect();
    let names: Vec<_> = names.iter().map(|n| n["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["local", "other", "hal"], "{p}");
    let on = call(&admin, "agents.on.schema", json!({})).await;
    let children = on["children"].as_array().expect("children");
    let children: Vec<_> = children.iter().map(|c| &c["namespace"]).collect();
    assert_eq!(children, ["local", "other", "hal"], "{on}");
    let r = call(&admin, "agents.on.dream.threads", json!({})).await;
    assert_eq!(r["error_kind"], "child_unavailable", "{r}");
    let scoped = connect(root.port, Some(SCOPED)).await.unwrap();
    let on = call(&scoped, "agents.on.schema", json!({})).await;
    let children = on["children"].as_array().expect("children");
    assert_eq!(children.len(), 1, "{on}");
    assert_eq!(children[0]["namespace"], "local", "{on}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_the_peer_didnt_grant_gets_nothing() {
    let granted = key(1);
    let rogue = key(2);
    let hal = service(free_port(), &hal_grants(&granted), vec![]).await;
    let peers = vec![peer("hal", hal.port, &rogue)];
    let root = service(free_port(), &root_grants(), peers).await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    for (method, params) in [
        ("agents.on.hal.threads", json!({})),
        ("agents.on.hal.pending", json!({})),
        ("agents.on.hal.start", RUN()),
    ] {
        let r = call(&admin, method, params).await;
        assert_eq!(r["error_kind"], "forbidden", "{method}: {r}");
    }
    assert!(hal.runtime.threads().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_two_peer_loop_is_refused() {
    let (root_key, hal_key) = (key(1), key(3));
    let (root_port, hal_port) = (free_port(), free_port());
    let mut hal_grants = hal_grants(&root_key);
    hal_grants.push(format!("{}=admin@*", hal_key.principal()));
    let back = vec![peer("root", root_port, &hal_key)];
    let hal = service(hal_port, &hal_grants, back).await;
    let peers = vec![peer("hal", hal.port, &root_key)];
    let mut grants = root_grants();
    grants.push(format!("{}=admin@*", hal_key.principal()));
    let root = service(root_port, &grants, peers).await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    let r = call(&admin, "agents.on.hal.on.root.threads", json!({})).await;
    assert_eq!(r["error_kind"], "max_depth", "{r}");
    assert_eq!(r["max_depth"], 0, "{r}");
    let r = call(&admin, "agents.on.hal.on.root.schema", json!({})).await;
    assert_eq!(r["error_kind"], "max_depth", "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proof_that_doesnt_verify_is_refused_at_the_upgrade() {
    let hal = service(free_port(), &[], vec![]).await;
    let mut headers = HeaderMap::new();
    let bad = HeaderValue::from_static("Bearer plexus-key.e30");
    headers.insert("authorization", bad);
    let dialed = WsClientBuilder::default()
        .set_headers(headers)
        .build(format!("ws://127.0.0.1:{}", hal.port))
        .await;
    let Err(e) = dialed else {
        panic!("admitted");
    };
    assert!(e.to_string().contains("401"), "{e}");
}

/// A real process the test can kill, or restart with another schema.
struct Stub(Child, u16);

impl Stub {
    fn start(port: u16, added: bool) -> Self {
        let test = std::env::current_exe().unwrap();
        let bin = test.parent().unwrap().parent().unwrap();
        let mut c = Command::new(bin.join("examples/on_stub"));
        c.args(["--port", &port.to_string()])
            .args(["--key-audience", &format!("ws://127.0.0.1:{port}")]);
        if added {
            c.arg("--added");
        }
        Self(c.spawn().expect("cargo test builds examples"), port)
    }

    fn kill(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn with_stub(refresh: Option<Duration>) -> (Service, Stub, Key) {
    let root_key = key(1);
    let port = free_port();
    let stub = Stub::start(port, false);
    up(port).await;
    let mut config = PeerConfig::parse(
        &format!("hal=ws://127.0.0.1:{port}"),
        root_key.0.to_str().unwrap(),
    )
    .unwrap();
    config.timeout = Duration::from_secs(2);
    config.backoff = Backoff {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(200),
        per_minute: 1000,
    };
    if let Some(r) = refresh {
        config.refresh = r;
    }
    let peers = vec![Peer::new(config).unwrap()];
    let root = service(free_port(), &root_grants(), peers).await;
    (root, stub, root_key)
}

async fn open_ticks(c: &WsClient) -> u64 {
    call(c, "agents.on.hal.open", json!({}))
        .await
        .as_u64()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_passes_through_and_dropping_it_cancels_at_the_peer() {
    let (root, _stub, _key) = with_stub(None).await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    let mut ticks = open(&admin, "agents.on.hal.ticks", json!({})).await;
    for want in 0..5 {
        assert_eq!(next(&mut ticks).await["content"], want);
    }
    assert_eq!(open_ticks(&admin).await, 1);
    drop(ticks);
    for _ in 0..200 {
        if open_ticks(&admin).await == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the peer's call was never dropped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_that_disconnects_cancels_at_the_peer() {
    let (root, _stub, _key) = with_stub(None).await;
    let other = connect(root.port, Some(ADMIN)).await.unwrap();
    {
        let gone = connect(root.port, Some(ADMIN)).await.unwrap();
        let mut ticks = open(&gone, "agents.on.hal.ticks", json!({})).await;
        next(&mut ticks).await;
        assert_eq!(open_ticks(&other).await, 1);
    }
    for _ in 0..200 {
        if open_ticks(&other).await == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the peer's call was never dropped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_stopping_mid_stream_is_child_unavailable_and_calls_recover() {
    let (root, mut stub, _key) = with_stub(None).await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    let mut ticks = open(&admin, "agents.on.hal.ticks", json!({})).await;
    next(&mut ticks).await;
    stub.kill();
    let last = loop {
        let item = next(&mut ticks).await;
        if item["type"] == "error" {
            break item;
        }
    };
    assert_eq!(last["error_kind"], "child_unavailable", "{last}");
    assert_eq!(last["child"], "hal", "{last}");

    let r = call(&admin, "agents.on.hal.open", json!({})).await;
    assert_eq!(r["error_kind"], "child_unavailable", "{r}");

    let _back = Stub::start(stub.1, false);
    up(stub.1).await;
    for _ in 0..300 {
        let r = call(&admin, "agents.on.hal.open", json!({})).await;
        if r == json!(0) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("calls never recovered");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schema_change_at_the_peer_is_seen_with_no_restart_here() {
    let (root, mut stub, _key) = with_stub(None).await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    let before = call(&admin, "agents.on.hal.schema", json!({})).await;
    let root_hash = call(&admin, "plexus-agents.hash", json!({})).await;
    let first = call(&admin, "agents.on.schema", json!({})).await;
    let has = |s: &Value| {
        let methods = s["methods"].as_array();
        methods.is_some_and(|m| m.iter().any(|x| x["name"] == "added"))
    };
    assert!(before["methods"].is_array(), "{before}");
    assert!(!has(&before), "{before}");
    let r = call(&admin, "agents.on.hal.added", json!({})).await;
    assert_eq!(r["type"], "error", "{r}");

    stub.kill();
    let _next = Stub::start(stub.1, true);
    up(stub.1).await;
    for _ in 0..300 {
        let after = call(&admin, "agents.on.hal.schema", json!({})).await;
        if has(&after) {
            assert_ne!(before["hash"], after["hash"]);
            let r = call(&admin, "agents.on.hal.added", json!({})).await;
            assert_eq!(r, json!(1), "{r}");
            // Nothing here moved, so a client's cache keyed on this hash is stale.
            let same = call(&admin, "plexus-agents.hash", json!({})).await;
            assert_eq!(root_hash, same);
            let hal = |on: &Value| {
                let children = on["children"].as_array().unwrap().clone();
                let found = children.into_iter().find(|c| c["namespace"] == "hal");
                found.unwrap()["hash"].clone()
            };
            let now = call(&admin, "agents.on.schema", json!({})).await;
            assert_ne!(hal(&first), hal(&now), "`children` moved with the peer");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the new schema was never seen");
}

/// A proof's sign-in ends at the peer, so a new call goes out on a new socket, and a
/// call still open keeps the old one until the peer ends it.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_call_takes_a_new_proof_and_an_open_one_goes_on() {
    let (root, _stub, _key) = with_stub(Some(Duration::from_millis(1))).await;
    let admin = connect(root.port, Some(ADMIN)).await.unwrap();
    let mut ticks = open(&admin, "agents.on.hal.ticks", json!({})).await;
    next(&mut ticks).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(open_ticks(&admin).await, 1);
    assert_eq!(next(&mut ticks).await["type"], "data");
}

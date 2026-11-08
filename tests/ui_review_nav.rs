//! SC-33: the nav and the palette, looked at. The bundle in Chromium against the
//! one binary's wiring (as tests/ui_e2e.rs), through the states the nav has to draw:
//! no hosts; three hosts paired — `mac` with several threads (one waiting on an
//! approval, one finished, in more than one folder), `studio` with one waiting on
//! an approval, `laptop` gone offline; the palette open; and the stale pill — an
//! approval answered while the page is open leaves "Needs you" within one refresh.
//! Screenshots at 1280×800 and 390×844 go to $SC_UI_SHOTS (default ./shots).
//!
//! Ignored by default, like ui_e2e: `SC_UI_BUNDLE=… PLEXUS_UI_NODE_MODULES=…
//! cargo test --test ui_review_nav -- --ignored --nocapture` (after
//! `cargo build --examples`, for host_stub).

use base64::Engine;
use jsonrpsee::core::client::SubscriptionClientT;
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use plexus_agents::front::{self, SignInConfig};
use plexus_agents::hosts::Hosts;
use plexus_agents::pairing::ServerKey;
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{hub_with_people, Serving};
use plexus_agents::ClaudeHarness;
use plexus_harness::{Environment, Harness};
use plexus_rpc::auth_core::adapters::sign_key_proof;
use plexus_ui_service::{Signers, UiActivation, UiStore, DEFAULT_CHANNEL};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const ADMIN_SEED: [u8; 32] = [31; 32];

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

/// A call's first data item (or its error), or `None` if nothing came in `secs`.
async fn call_for(c: &WsClient, method: &str, params: Value, secs: u64) -> Option<Value> {
    let mut p = ObjectParams::new();
    p.insert("method", method).unwrap();
    p.insert("params", params).unwrap();
    let mut s = c
        .subscribe::<Value, _>("plexus-agents.call", p, "plexus-agents.call_unsub")
        .await
        .ok()?;
    loop {
        let item = tokio::time::timeout(Duration::from_secs(secs), s.next())
            .await
            .ok()??
            .ok()?;
        match item["type"].as_str() {
            Some("data") => return Some(item["content"].clone()),
            Some("error") | Some("done") => return Some(item),
            _ => {}
        }
    }
}

async fn call(c: &WsClient, method: &str, params: Value) -> Value {
    call_for(c, method, params, 60)
        .await
        .unwrap_or_else(|| panic!("{method}: no answer"))
}

async fn client(port: u16, headers: &[(&str, String)]) -> WsClient {
    let mut h = HeaderMap::new();
    for (name, value) in headers {
        let name: axum::http::HeaderName = name.parse().unwrap();
        h.insert(name, HeaderValue::from_str(value).unwrap());
    }
    WsClientBuilder::default()
        .set_headers(h)
        .build(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .unwrap()
}

/// A host_stub dialing in, unpaired; its pairing link's id.
fn dial(dir: &std::path::Path, port: u16, name: &str, seed: u8) -> (Child, String) {
    let b64 = base64::engine::general_purpose::STANDARD;
    let key = dir.join(format!("{name}.key"));
    std::fs::write(&key, b64.encode([seed; 32])).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut host = Command::new(bin.join("examples/host_stub"))
        .args([
            "--server",
            &format!("ws://127.0.0.1:{port}/ws"),
            "--name",
            name,
        ])
        .args(["--db", dir.join(format!("{name}.db")).to_str().unwrap()])
        .args(["--pins", dir.join(format!("{name}.pins")).to_str().unwrap()])
        .args(["--key", key.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = host.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    loop {
        let line = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        if let Some(l) = line.strip_prefix("link: ") {
            return (host, l.split_once("/#/pair/").unwrap().1.to_string());
        }
    }
}

async fn until(what: &str, mut ok: impl AsyncFnMut() -> bool) {
    for _ in 0..300 {
        if ok().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("never: {what}");
}

async fn pending(c: &WsClient, host: &str) -> Vec<Value> {
    let r = call(c, "hosts.pending", json!({ "host": host })).await;
    r["requests"].as_array().cloned().unwrap_or_default()
}

async fn start(c: &WsClient, host: &str, cwd: &str, prompt: &str) -> Option<String> {
    let r = call_for(
        c,
        "hosts.start",
        json!({"host": host, "cwd": cwd, "prompt": prompt}),
        20,
    )
    .await;
    eprintln!("start on {host} in {cwd}: {r:?}");
    r.and_then(|r| r["thread"].as_str().map(String::from))
}

async fn status(c: &WsClient, host: &str, thread: &str) -> String {
    let r = call(c, "hosts.threads", json!({ "host": host })).await;
    let t = r["threads"]
        .as_array()
        .and_then(|ts| ts.iter().find(|t| t["id"] == thread).cloned())
        .unwrap_or_default();
    format!("{}", t["status"]).trim_matches('"').to_string()
}

/// The browser: one stage of tests/ui_review_nav.mjs, its stdout lines handed back.
fn browser(stage: &str, env: &[(&str, &str)]) -> Child {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui_review_nav.mjs");
    let mut cmd = Command::new("node");
    cmd.arg(script)
        .env("STAGE", stage)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

fn finish(mut child: Child) {
    let out = child.stdout.take().unwrap();
    for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
        eprintln!("browser: {line}");
    }
    assert!(child.wait().unwrap().success(), "the browser stage failed");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built bundle (SC_UI_BUNDLE) and Playwright (PLEXUS_UI_NODE_MODULES)"]
async fn the_nav_and_palette_through_their_states() {
    let bundle = std::fs::read(std::env::var("SC_UI_BUNDLE").expect("SC_UI_BUNDLE")).unwrap();
    let modules = std::env::var("PLEXUS_UI_NODE_MODULES").expect("PLEXUS_UI_NODE_MODULES");
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("agents.db");
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: db.clone(),
        environment: Arc::new(Nowhere),
        placements: HashMap::new(),
        harnesses: harnesses.clone(),
    })
    .await
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let origin = format!("http://127.0.0.1:{port}");
    let admin = principal(&ADMIN_SEED);
    let login = front::sign_in(
        &SignInConfig {
            public_url: Some(origin.clone()),
            admins: vec![admin.clone()],
            loopback: true,
            ..SignInConfig::default()
        },
        port,
    )
    .unwrap();
    let hosts = Hosts::open(
        &db,
        format!("ws://127.0.0.1:{port}/ws"),
        ServerKey::from_seed([42; 32]),
    )
    .unwrap();
    let store =
        Arc::new(UiStore::open_channel(&dir.path().join("ui.db"), DEFAULT_CHANNEL).unwrap());
    let signer = admin.trim_start_matches("key:").to_string();
    let ui = UiActivation::new(store.clone(), Signers::parse(&[signer]).unwrap());
    let grants = vec![format!("{admin}=admin@*").parse().unwrap()];
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
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err()
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The admin pushes the bundle, and mints the browser's session.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let proof = sign_key_proof(
        &ADMIN_SEED,
        &format!("ws://127.0.0.1:{port}"),
        "deploy",
        &rand::random::<[u8; 32]>(),
        ts,
    );
    let c = client(port, &[("authorization", format!("Bearer {proof}"))]).await;
    let manifest = {
        let mut a = tar::Archive::new(bundle.as_slice());
        let mut text = String::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            if e.path().unwrap().to_string_lossy() == "manifest.json" {
                std::io::Read::read_to_string(&mut e, &mut text).unwrap();
            }
        }
        text
    };
    let signature = {
        use ed25519_dalek::Signer;
        use sha2::Digest;
        ed25519_dalek::SigningKey::from_bytes(&ADMIN_SEED).sign(&sha2::Sha256::digest(&bundle))
    };
    let b64 = base64::engine::general_purpose::STANDARD;
    let pushed = call(&c, "ui.push", json!({"bundle": b64.encode(&bundle), "manifest": manifest, "signature": b64.encode(signature.to_bytes())})).await;
    assert_eq!(pushed["type"], "pushed", "{pushed}");
    let session = call(
        &c,
        "auth.login",
        json!({"flow": {"session": {"ttl_secs": 3600}}}),
    )
    .await;
    let token = session["token"]
        .as_str()
        .unwrap_or_else(|| panic!("{session}"))
        .to_string();
    // The page's socket: the session as a cookie, as the browser holds it.
    let me = client(
        port,
        &[
            ("origin", origin.clone()),
            ("cookie", format!("access_token={token}")),
        ],
    )
    .await;

    let shots = std::env::var("SC_UI_SHOTS").unwrap_or_else(|_| {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("shots")
            .display()
            .to_string()
    });
    std::fs::create_dir_all(&shots).unwrap();
    let env = [
        ("NODE_PATH", modules.as_str()),
        ("BASE", origin.as_str()),
        ("TOKEN", token.as_str()),
        ("SHOTS", shots.as_str()),
    ];

    // 1. No hosts yet.
    finish(browser("empty", &env));

    // 2. Three hosts, paired from the page's socket.
    let mut children = Vec::new();
    for (name, seed) in [("mac", 9u8), ("studio", 10), ("laptop", 11)] {
        let (child, id) = dial(dir.path(), port, name, seed);
        let done = call(&me, "hosts.confirm", json!({ "pairing_id": id })).await;
        assert_eq!(done["type"], "paired", "{done}");
        children.push((name, child));
        until(&format!("{name} online"), async || hosts.online(name)).await;
    }

    // mac: one parked on an approval, one that ran to its end, one more (host_stub
    // replays two runs; a third start may fail, which is a state the nav draws too).
    let t1 = start(
        &me,
        "mac",
        "/Users/you/dev/plexus-agents",
        "Fix the flaky reconnect test in host_wire.rs and explain the race",
    )
    .await
    .unwrap();
    until("mac's approval", async || {
        !pending(&me, "mac").await.is_empty()
    })
    .await;
    let _t2 = start(
        &me,
        "mac",
        "/Users/you/dev/menger",
        "Summarise what changed in the board filters this week",
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _t3 = start(
        &me,
        "mac",
        "/Users/you/dev/plexus-agents",
        "Add a test for the pairing code expiry",
    )
    .await;
    // studio: one parked on an approval — the one answered later, page open.
    let t4 = start(
        &me,
        "studio",
        "/home/you/src/hyperforge",
        "Bump the workspace to plexus-rpc 0.9 and run the tests",
    )
    .await
    .unwrap();
    until("studio's approval", async || {
        !pending(&me, "studio").await.is_empty()
    })
    .await;
    // laptop: a thread, then it goes away.
    let _t5 = start(
        &me,
        "laptop",
        "/Users/you/notes",
        "Tidy the meeting notes folder",
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (_, mut laptop) = children.remove(2);
    let _ = laptop.kill();
    let _ = laptop.wait();
    until("laptop offline", async || !hosts.online("laptop")).await;
    eprintln!("mac t1 {}: {}", t1, status(&me, "mac", &t1).await);

    // 3. The full nav, the palette, the phone — then, the page still open, studio's
    //    approval is answered: its row must leave "Needs you" within one refresh.
    let signal = format!("{shots}/.answered");
    let _ = std::fs::remove_file(&signal);
    let mut env_full = env.to_vec();
    env_full.push(("SIGNAL", signal.as_str()));
    env_full.push(("STALE_THREAD", t4.as_str()));
    let mut page = browser("full", &env_full);
    let out = page.stdout.take().unwrap();
    let mut lines = std::io::BufReader::new(out).lines();
    loop {
        let line = lines.next().expect("the browser ended early").unwrap();
        eprintln!("browser: {line}");
        if line == "waiting for the answer" {
            break;
        }
    }
    let req = pending(&me, "studio").await[0].clone();
    let answered = call(
        &me,
        "hosts.answer",
        json!({"host": "studio", "thread": t4, "request": req["request"]["id"], "answer": {"answer": "allow"}}),
    )
    .await;
    eprintln!("answered: {answered}");
    until("studio's approval gone", async || {
        pending(&me, "studio").await.is_empty()
    })
    .await;
    eprintln!(
        "studio's thread, answered: {}",
        status(&me, "studio", &t4).await
    );
    std::fs::write(&signal, b"1").unwrap();
    for line in lines.map_while(Result::ok) {
        eprintln!("browser: {line}");
    }
    let ok = page.wait().unwrap().success();
    for (_, mut ch) in children {
        let _ = ch.kill();
        let _ = ch.wait();
    }
    assert!(ok, "the browser stage failed");
}

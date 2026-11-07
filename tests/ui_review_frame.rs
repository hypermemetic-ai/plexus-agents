//! SC-30: a look at the frame, not a test of it. Builds a copy of ui/ with a
//! component sheet at `#/sheet` (tests/ui_review_frame.sheet.svelte — never in
//! the shipped bundle), serves it from the one binary's wiring with a paired
//! stub host and a thread parked on an approval, and screenshots home, a host,
//! the thread, the sheet and pairing at 1280×800 and 390×844, light and dark
//! (tests/ui_review_frame.mjs), into $SC_UI_SHOTS. Ignored by default; run
//!
//! ```text
//! source ~/sc-ui-frame/env.sh   # PLEXUS_UI, PLEXUS_UI_ENGINE, PLEXUS_UI_NODE_MODULES
//! cargo build --examples && SC_UI_SHOTS=~/sc-ui-frame/shots \
//!   cargo test --test ui_review_frame -- --ignored --nocapture
//! ```

use base64::Engine;
use jsonrpsee::core::client::SubscriptionClientT;
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use plexus_agents::front::{self, SignInConfig};
use plexus_agents::hosts::Hosts;
use plexus_agents::pairing::ServerKey;
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{hub_with_ui, Serving};
use plexus_agents::ClaudeHarness;
use plexus_harness::{Environment, Harness};
use plexus_rpc::auth_core::adapters::sign_key_proof;
use plexus_ui_service::{Signers, UiActivation, UiStore, DEFAULT_CHANNEL};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
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

async fn call(c: &WsClient, method: &str, params: Value) -> Value {
    let mut p = ObjectParams::new();
    p.insert("method", method).unwrap();
    p.insert("params", params).unwrap();
    let mut s = c
        .subscribe::<Value, _>("plexus-agents.call", p, "plexus-agents.call_unsub")
        .await
        .unwrap();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(60), s.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match item["type"].as_str() {
            Some("data") => return item["content"].clone(),
            Some("error") | Some("done") => return item,
            _ => {}
        }
    }
}


fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else {
            std::fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

/// ui/ with the sheet at `#/sheet` inside the frame, built by plexus-ui into a tar.
/// Unless SC_UI_REVIEW_AS_IS=1, each page's own `<Top/>` and `<Nav/>` are taken
/// out of the copy first — what the page builders (SC-31..34) do — so the frame
/// is seen as it will be, not stepping aside for a page drawn whole.
fn bundle_with_sheet(dir: &std::path::Path) -> Vec<u8> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let ui = dir.join("ui");
    copy_dir(&root.join("ui"), &ui);
    std::fs::copy(root.join("tests/ui_review_frame.sheet.svelte"), ui.join("src/sc-sheet.svelte")).unwrap();
    let frame = ui.join("src/sc-frame.svelte");
    let text = std::fs::read_to_string(&frame).unwrap();
    let patched = text.replacen(": \"sc-home\",", ": hash === \"#/sheet\" ? \"sc-sheet\" : \"sc-home\",", 1);
    assert_ne!(text, patched, "sc-frame's page choice moved; patch the review harness");
    std::fs::write(&frame, patched).unwrap();
    if std::env::var("SC_UI_REVIEW_AS_IS").as_deref() != Ok("1") {
        for e in std::fs::read_dir(ui.join("src")).unwrap() {
            let p = e.unwrap().path();
            let t = std::fs::read_to_string(&p).unwrap();
            if p.file_name().unwrap() == "sc-frame.svelte" || !(t.contains("<Top/>") || t.contains("<Nav/>")) {
                continue;
            }
            let t = t.replace("<Top/>", "").replace("<Nav/>", "");
            let t = t.lines().filter(|l| !l.contains("import Top from") && !l.contains("import Nav from")).collect::<Vec<_>>().join("\n");
            std::fs::write(&p, t).unwrap();
        }
    }
    let mut routes: serde_json::Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(ui.join("routes.json")).unwrap()).unwrap();
    routes.insert("#/sheet".into(), json!("sc-frame"));
    std::fs::write(ui.join("routes.json"), serde_json::to_string_pretty(&routes).unwrap()).unwrap();
    let tar = dir.join("ui.tar");
    let out = Command::new(std::env::var("PLEXUS_UI").unwrap_or_else(|_| "plexus-ui".into()))
        .arg("build")
        .arg(&ui)
        .args(["--engine", &std::env::var("PLEXUS_UI_ENGINE").expect("PLEXUS_UI_ENGINE")])
        .args(["--node-modules", &std::env::var("PLEXUS_UI_NODE_MODULES").expect("PLEXUS_UI_NODE_MODULES")])
        .arg("--strict-colours")
        .arg("-o")
        .arg(&tar)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    std::fs::read(tar).unwrap()
}

fn find_str(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(m) => m.get(key).and_then(|s| s.as_str().map(str::to_string)).or_else(|| m.values().find_map(|x| find_str(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_str(x, key)),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a review harness: needs plexus-ui (PLEXUS_UI*), Playwright and the examples built"]
async fn the_frame_screenshots_home_and_a_thread() {
    let modules = std::env::var("PLEXUS_UI_NODE_MODULES").expect("PLEXUS_UI_NODE_MODULES");
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle_with_sheet(dir.path());
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
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let admin = principal(&ADMIN_SEED);
    let login = front::sign_in(
        &SignInConfig { public_url: Some(origin.clone()), admins: vec![admin.clone()], loopback: true, ..SignInConfig::default() },
        port,
    )
    .unwrap();
    let hosts = Hosts::open(&db, format!("ws://127.0.0.1:{port}/ws"), ServerKey::from_seed([42; 32])).unwrap();
    let store = Arc::new(UiStore::open_channel(&dir.path().join("ui.db"), DEFAULT_CHANNEL).unwrap());
    let signer = admin.trim_start_matches("key:").to_string();
    let ui = UiActivation::new(store.clone(), Signers::parse(&[signer]).unwrap());
    let grants = vec![format!("{admin}=admin@*").parse().unwrap()];
    let hub = hub_with_ui(runtime, harnesses, grants, vec![], Some(hosts.clone()), login.backend(), Some(ui));
    let serving = Serving { hosts: Some(hosts.clone()), ui: Some(store), ..Serving::new([127, 0, 0, 1].into(), port) };
    tokio::spawn(front::serve_public(hub, login, serving));
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let proof = sign_key_proof(&ADMIN_SEED, &format!("ws://127.0.0.1:{port}"), "deploy", &rand::random::<[u8; 32]>(), ts);
    let mut h = HeaderMap::new();
    h.insert("authorization", HeaderValue::from_str(&format!("Bearer {proof}")).unwrap());
    let c = WsClientBuilder::default().set_headers(h).build(format!("ws://127.0.0.1:{port}/ws")).await.unwrap();
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
    let session = call(&c, "auth.login", json!({"flow": {"session": {"ttl_secs": 3600}}})).await;
    let token = session["token"].as_str().unwrap_or_else(|| panic!("{session}")).to_string();

    // A stub host dials in; the admin confirms its pairing and starts a thread on it.
    let key = dir.path().join("mac.key");
    std::fs::write(&key, b64.encode([9u8; 32])).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let bin = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().to_path_buf();
    let mut host = Command::new(bin.join("examples/host_stub"))
        .args(["--server", &format!("ws://127.0.0.1:{port}/ws"), "--name", "mac"])
        .args(["--db", dir.path().join("h.db").to_str().unwrap(), "--pins", dir.path().join("pins").to_str().unwrap()])
        .args(["--key", key.to_str().unwrap()])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = host.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut link = String::new();
    while link.is_empty() {
        let line = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        if let Some(l) = line.strip_prefix("link: ") {
            link = l.to_string();
        }
    }
    let pairing = link.rsplit('/').next().unwrap().to_string();
    let confirmed = call(&c, "hosts.confirm", json!({"pairing_id": pairing})).await;
    assert_eq!(confirmed["type"], "paired", "{confirmed}");
    for _ in 0..300 {
        if hosts.online("mac") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(hosts.online("mac"), "mac came back online, paired");
    let started = call(&c, "hosts.start", json!({"host": "mac", "cwd": "/tmp/superconductor", "prompt": "Find why pairing refuses a host's own key, and fix it."})).await;
    let thread = find_str(&started, "thread").unwrap_or_else(|| panic!("{started}"));

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui_review_frame.mjs");
    let shots = std::env::var("SC_UI_SHOTS").unwrap_or_else(|_| dir.path().display().to_string());
    std::fs::create_dir_all(&shots).unwrap();
    let run = tokio::task::spawn_blocking(move || {
        Command::new("node")
            .arg(script)
            .env("NODE_PATH", &modules)
            .env("BASE", &origin)
            .env("TOKEN", &token)
            .env("THREAD", &thread)
            .env("SHOTS", &shots)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let _ = host.kill();
    let _ = host.wait();
    let said = format!("{}{}", String::from_utf8_lossy(&run.stdout), String::from_utf8_lossy(&run.stderr));
    eprintln!("{said}");
    assert!(run.status.success(), "{said}");
}

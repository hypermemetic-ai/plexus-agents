//! The people page in a real browser (test deployments only): an admin, signed in
//! through the page (a browser session cookie), opens "Act as…" from the account
//! menu, creates a test user and acts as them — the band shows, the admin-only
//! entry and page are gone — and returns; then the same at phone size, acting as
//! an existing user. Screenshots at 1280×800 and 390×844.
//!
//! Ignored by default: it needs the bundle built (scripts/ui-gate.sh) and
//! Playwright's Chromium. Run with
//!
//! ```text
//! SC_UI_BUNDLE=…/ui.tar PLEXUS_UI_NODE_MODULES=…/plexus-ui/node_modules SC_UI_SHOTS=DIR \
//!   cargo test --test ui_review_people -- --ignored
//! ```

use base64::Engine;
use jsonrpsee::core::client::SubscriptionClientT;
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use plexus_agents::front::{self, SignInConfig};
use plexus_agents::hosts::Hosts;
use plexus_agents::pairing::ServerKey;
use plexus_agents::people::{Environment, GrantStore};
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{hub_with_people, Serving};
use plexus_agents::ClaudeHarness;
use plexus_harness::Harness;
use plexus_rpc::auth_core::adapters::sign_key_proof;
use plexus_rpc::core::plexus::AuthContext;
use plexus_ui_service::{Signers, UiActivation, UiStore, DEFAULT_CHANNEL};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

const DEPLOY_SEED: [u8; 32] = [61; 32];
const ADMIN: &str = "oidc:workos/user_01ADAADAADAADAADAADAADAADA";
const BOB: &str = "oidc:workos/user_01BOBBOBBOBBOBBOBBOBBOBBOB";

fn key_principal(seed: &[u8; 32]) -> String {
    let key = ed25519_dalek::SigningKey::from_bytes(seed);
    let hex: String = key.verifying_key().as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("key:{hex}")
}

struct Nowhere;

impl plexus_harness::Environment for Nowhere {
    fn name(&self) -> &str {
        "nowhere"
    }
    fn spawn(&self, _: plexus_harness::LaunchPlan) -> futures::future::BoxFuture<'static, Result<plexus_harness::Process, plexus_harness::SpawnError>> {
        Box::pin(async { Err(plexus_harness::SpawnError::Io("not here".into())) })
    }
}

async fn call(c: &WsClient, method: &str, params: Value) -> Value {
    let mut p = ObjectParams::new();
    p.insert("method", method).unwrap();
    p.insert("params", params).unwrap();
    let mut s = c.subscribe::<Value, _>("plexus-agents.call", p, "plexus-agents.call_unsub").await.unwrap();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(60), s.next()).await.unwrap().unwrap().unwrap();
        match item["type"].as_str() {
            Some("data") => return item["content"].clone(),
            Some("error") | Some("done") => return item,
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built bundle (SC_UI_BUNDLE) and Playwright (PLEXUS_UI_NODE_MODULES)"]
async fn an_admin_acts_as_a_test_user_and_returns_in_a_browser() {
    let bundle = std::fs::read(std::env::var("SC_UI_BUNDLE").expect("SC_UI_BUNDLE")).unwrap();
    let modules = std::env::var("PLEXUS_UI_NODE_MODULES").expect("PLEXUS_UI_NODE_MODULES");
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("agents.db");
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let (runtime, _) = Supervisor::open(SupervisorConfig { db: db.clone(), environment: Arc::new(Nowhere), placements: HashMap::new(), harnesses: harnesses.clone() })
        .await
        .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let deploy = key_principal(&DEPLOY_SEED);
    let admins = vec![ADMIN.to_string(), deploy.clone()];
    let mut grants: Vec<plexus_agents::auth::Grant> = admins.iter().map(|a| format!("{a}=admin@*").parse().unwrap()).collect();
    grants.push(format!("{BOB}=user@*").parse().unwrap());
    let login = front::sign_in(
        &SignInConfig {
            public_url: Some(origin.clone()),
            admins,
            loopback: true,
            env: Environment::Test,
            grants: grants.clone(),
            people_grants: Some(GrantStore::open(&db).unwrap()),
            ..SignInConfig::default()
        },
        port,
    )
    .unwrap();
    // The admin signed in through the page twice (a laptop, a phone): two browser sessions.
    let cookie = login.wired.sessions.cookie_name().to_string();
    let ada = || AuthContext::new(ADMIN.into(), "sid".into(), vec![], json!({ "principal": ADMIN, "name": "Ada" }));
    let (desk, _) = login.wired.sessions.create(ada()).unwrap();
    let (phone, _) = login.wired.sessions.create(ada()).unwrap();
    let hosts = Hosts::open(&db, format!("ws://127.0.0.1:{port}/ws"), ServerKey::from_seed([42; 32])).unwrap();
    let store = Arc::new(UiStore::open_channel(&dir.path().join("ui.db"), DEFAULT_CHANNEL).unwrap());
    let ui = UiActivation::new(store.clone(), Signers::parse(&[deploy.trim_start_matches("key:").to_string()]).unwrap());
    let hub = hub_with_people(runtime, harnesses, grants, vec![], Some(hosts.clone()), login.backend(), Some(ui), Some(login.people.clone()), login.people_grants.clone());
    assert!(login.people.off().is_none(), "test, on loopback: the people page is on");
    tokio::spawn(front::serve_public(hub, login, Serving { hosts: Some(hosts), ui: Some(store), ..Serving::new([127, 0, 0, 1].into(), port) }));
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The deploy key pushes the bundle.
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let proof = sign_key_proof(&DEPLOY_SEED, &format!("ws://127.0.0.1:{port}"), "deploy", &rand::random::<[u8; 32]>(), ts);
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
        ed25519_dalek::SigningKey::from_bytes(&DEPLOY_SEED).sign(&sha2::Sha256::digest(&bundle))
    };
    let b64 = base64::engine::general_purpose::STANDARD;
    let pushed = call(&c, "ui.push", json!({"bundle": b64.encode(&bundle), "manifest": manifest, "signature": b64.encode(signature.to_bytes())})).await;
    assert_eq!(pushed["type"], "pushed", "{pushed}");

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui_review_people.mjs");
    let shots = std::env::var("SC_UI_SHOTS").unwrap_or_else(|_| dir.path().display().to_string());
    let run = tokio::task::spawn_blocking(move || {
        Command::new("node")
            .arg(script)
            .env("NODE_PATH", &modules)
            .env("BASE", &origin)
            .env("COOKIE", &cookie)
            .env("DESK", &desk)
            .env("PHONE", &phone)
            .env("SHOTS", &shots)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let said = format!("{}{}", String::from_utf8_lossy(&run.stdout), String::from_utf8_lossy(&run.stderr));
    eprintln!("{said}");
    assert!(run.status.success(), "{said}");
}

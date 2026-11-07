//! The people page through the wire (plexus-login's `people.*`, agents' grants):
//! on `--env test` an admin on the sign-in cookie makes a test user, acts as it —
//! the gate then answers as that user, with the role it was given — and returns;
//! on `prod`, or on a production host, the methods don't exist.

use jsonrpsee::core::client::SubscriptionClientT;
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use plexus_agents::front::{self, SignInConfig};
use plexus_agents::people::{Environment, GrantStore};
use plexus_agents::runtime::{Supervisor, SupervisorConfig};
use plexus_agents::service::{hub_with_people, Serving};
use plexus_agents::ClaudeHarness;
use plexus_harness::Harness;
use plexus_rpc::auth_core::adapters::sign_key_proof;
use plexus_rpc::core::plexus::AuthContext;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const ADMIN: &str = "oidc:workos/user_01ADMINADMINADMINADMINADMIN";
const KEY_SEED: [u8; 32] = [51; 32];

struct Nowhere;

impl plexus_harness::Environment for Nowhere {
    fn name(&self) -> &str {
        "nowhere"
    }
    fn spawn(&self, _: plexus_harness::LaunchPlan) -> futures::future::BoxFuture<'static, Result<plexus_harness::Process, plexus_harness::SpawnError>> {
        Box::pin(async { Err(plexus_harness::SpawnError::Io("not here".into())) })
    }
}

fn key_principal(seed: &[u8; 32]) -> String {
    let key = ed25519_dalek::SigningKey::from_bytes(seed);
    let hex: String = key.verifying_key().as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("key:{hex}")
}

struct Up {
    port: u16,
    /// `name=value` of the admin's browser session cookie.
    cookie: String,
    _dir: tempfile::TempDir,
}

async fn up(env: Environment, public_url: Option<String>) -> Up {
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
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let keyed = key_principal(&KEY_SEED);
    let admins = vec![ADMIN.to_string(), keyed.clone()];
    let grants: Vec<plexus_agents::auth::Grant> = admins.iter().map(|a| format!("{a}=admin@*").parse().unwrap()).collect();
    let config = SignInConfig {
        public_url: Some(public_url.unwrap_or_else(|| format!("http://127.0.0.1:{port}"))),
        admins,
        loopback: true,
        trusted_proxies: vec!["none".into()],
        env,
        grants: grants.clone(),
        people_grants: (env == Environment::Test).then(|| GrantStore::open(&db).unwrap()),
        ..SignInConfig::default()
    };
    let login = front::sign_in(&config, port).unwrap();
    // The admin signed in through the page: a browser session, its cookie.
    let ctx = AuthContext::new(ADMIN.into(), "sid".into(), vec![], json!({ "principal": ADMIN, "name": "Ada Admin" }));
    let (id, _) = login.wired.sessions.create(ctx).unwrap();
    let cookie = format!("{}={id}", login.wired.sessions.cookie_name());
    let hub = hub_with_people(runtime, harnesses, grants, vec![], None, login.backend(), None, login.people.clone(), login.people_grants.clone());
    tokio::spawn(front::serve_public(hub, login, Serving::new([127, 0, 0, 1].into(), port)));
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Up { port, cookie, _dir: dir }
}

async fn socket(port: u16, header: (&'static str, String)) -> WsClient {
    let mut h = HeaderMap::new();
    h.insert(header.0, HeaderValue::from_str(&header.1).unwrap());
    WsClientBuilder::default().set_headers(h).build(format!("ws://127.0.0.1:{port}/ws")).await.unwrap()
}

/// The first data item a call yields, or its error item.
async fn call(c: &WsClient, method: &str, params: Value) -> Value {
    let mut p = ObjectParams::new();
    p.insert("method", method).unwrap();
    p.insert("params", params).unwrap();
    let mut s = c.subscribe::<Value, _>("plexus-agents.call", p, "plexus-agents.call_unsub").await.unwrap();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(20), s.next()).await.unwrap().unwrap().unwrap();
        match item["type"].as_str() {
            Some("data") => return item["content"].clone(),
            Some("error") | Some("done") => return item,
            _ => {}
        }
    }
}

/// Every data item a call yields.
async fn all(c: &WsClient, method: &str, params: Value) -> Vec<Value> {
    let mut p = ObjectParams::new();
    p.insert("method", method).unwrap();
    p.insert("params", params).unwrap();
    let mut s = c.subscribe::<Value, _>("plexus-agents.call", p, "plexus-agents.call_unsub").await.unwrap();
    let mut out = Vec::new();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(20), s.next()).await.unwrap().unwrap().unwrap();
        match item["type"].as_str() {
            Some("data") => out.push(item["content"].clone()),
            Some("error") => {
                out.push(item);
                return out;
            }
            Some("done") => return out,
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn on_test_an_admin_makes_a_user_acts_as_it_and_returns() {
    let s = up(Environment::Test, None).await;
    let c = socket(s.port, ("cookie", s.cookie.clone())).await;
    let me = call(&c, "people.whoami", json!({})).await;
    assert_eq!((me["type"].as_str(), me["admin"].as_bool(), me["can_act"].as_bool()), (Some("whoami"), Some(true), Some(true)), "{me}");

    let listed = all(&c, "people.known_users", json!({})).await;
    let roles: Vec<&str> = listed.iter().filter(|e| e["type"] == "role").filter_map(|e| e["id"].as_str()).collect();
    assert_eq!(roles, ["user@*"], "{listed:?}");
    assert!(listed.iter().any(|e| e["type"] == "user" && e["principal"] == ADMIN && e["you"] == true));
    assert!(!listed.iter().any(|e| e["principal"].as_str().is_some_and(|p| p.starts_with("key:"))), "no keys");

    let refused = call(&c, "people.create_user", json!({"name": "carol", "roles": ["admin@*"]})).await;
    assert_eq!(refused["reason"], "admin_role", "{refused}");
    let became = call(&c, "people.create_user", json!({"name": "carol", "roles": ["user@*"]})).await;
    assert_eq!((became["type"].as_str(), became["principal"].as_str(), became["signed_in_as"].as_str()), (Some("became"), Some("oidc:test/carol"), Some(ADMIN)), "{became}");

    // The page reconnects with the same cookie: carol, for the admin.
    let c = socket(s.port, ("cookie", s.cookie.clone())).await;
    let who = call(&c, "people.whoami", json!({})).await;
    assert_eq!(who["principal"], "oidc:test/carol", "{who}");
    assert_eq!((who["acting_as"].as_bool(), who["signed_in_as"].as_str(), who["signed_in_as_name"].as_str()), (Some(true), Some(ADMIN), Some("Ada Admin")));
    assert_eq!((who["admin"].as_bool(), who["can_act"].as_bool()), (Some(false), Some(false)));
    // The gate answers as carol, with what she was given: user@* held, no admin.
    let auth = call(&c, "auth.whoami", json!({})).await;
    assert_eq!(auth["principal"], "oidc:test/carol", "{auth}");
    let held: Vec<String> = auth["memberships"].as_array().unwrap().iter().map(|m| format!("{}@{}", m["role"].as_str().unwrap(), m["scope"].as_str().unwrap())).collect();
    assert!(held.contains(&"user@*".to_string()) && !held.iter().any(|h| h.starts_with("admin")), "{held:?}");
    let not_admin = call(&c, "people.known_users", json!({})).await;
    assert!(not_admin["type"] == "error" || not_admin["type"] == "refused", "an admin-only method, refused: {not_admin}");
    let threads = call(&c, "agents.threads", json!({})).await;
    assert_eq!(threads["type"], "threads", "carol lists her own threads, as any signed-in person: {threads}");

    let back = call(&c, "people.unbecome", json!({})).await;
    assert_eq!((back["type"].as_str(), back["principal"].as_str()), (Some("returned"), Some(ADMIN)), "{back}");
    let c = socket(s.port, ("cookie", s.cookie.clone())).await;
    let me = call(&c, "people.whoami", json!({})).await;
    assert_eq!((me["principal"].as_str(), me["acting_as"].as_bool(), me["can_act"].as_bool()), (Some(ADMIN), Some(false), Some(true)), "{me}");
    // Carol is known now; acting as her again works.
    let again = call(&c, "people.act_as", json!({"principal": "oidc:test/carol"})).await;
    assert_eq!(again["type"], "became", "{again}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admins_bearer_cannot_act() {
    let s = up(Environment::Test, None).await;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let proof = sign_key_proof(&KEY_SEED, &format!("ws://127.0.0.1:{}", s.port), "people", &rand::random::<[u8; 32]>(), ts);
    let c = socket(s.port, ("authorization", format!("Bearer {proof}"))).await;
    let me = call(&c, "people.whoami", json!({})).await;
    assert_eq!((me["admin"].as_bool(), me["can_act"].as_bool()), (Some(true), Some(false)), "{me}");
    let r = call(&c, "people.create_user", json!({"name": "mallory", "roles": []})).await;
    assert_eq!(r["reason"], "not_a_browser_session", "{r}");
}

fn absent(v: &Value) -> bool {
    v["type"] == "error"
}

#[tokio::test(flavor = "multi_thread")]
async fn on_prod_and_on_a_production_host_the_methods_do_not_exist() {
    let prod = up(Environment::Prod, None).await;
    let c = socket(prod.port, ("cookie", prod.cookie.clone())).await;
    for m in ["people.whoami", "people.known_users", "people.unbecome"] {
        let r = call(&c, m, json!({})).await;
        assert!(absent(&r), "{m} on prod: {r}");
    }
    assert_eq!(call(&c, "auth.whoami", json!({})).await["principal"], ADMIN, "sign-in itself is untouched");
    let local = up(Environment::Local, None).await;
    let c = socket(local.port, ("cookie", local.cookie.clone())).await;
    assert!(absent(&call(&c, "people.whoami", json!({})).await), "local");
    // Test, but its public URL is production's host.
    let host = up(Environment::Test, Some("https://agents.menger.sh".into())).await;
    let c = socket(host.port, ("cookie", host.cookie.clone())).await;
    assert!(absent(&call(&c, "people.whoami", json!({})).await), "a production host");
}

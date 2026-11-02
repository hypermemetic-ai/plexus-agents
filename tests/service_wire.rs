//! AGENTS-13 and AGENTS-38 through the wire: a real WebSocket, real RS256 tokens
//! against a JWKS, the hub's gate, and replays of real runs behind it. The real IDP
//! and real Claude are `scripts/agents-real`.

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures::future::BoxFuture;
use jsonrpsee::core::client::{Subscription, SubscriptionClientT};
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use plexus_agents::auth::Grant;
use plexus_agents::runtime::{Supervisor, SupervisorConfig, ThreadId, ThreadOptions};
use plexus_agents::service::{hub, serve};
use plexus_agents::ClaudeHarness;
use plexus_auth_core::oidc::{
    Audience, FetchError, FetchedDocument, JwksFetcher, OidcConfig, OidcSessionValidator,
    OidcValidator,
};
use plexus_auth_core::IssuerUrl;
use plexus_harness::conformance::replay::{ExitTrigger, ReplayEnvironment, ReplayScript};
use plexus_harness::{Actor, Environment, Harness, LaunchPlan, Process, RunSpec, SpawnError};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const ISSUER: &str = "http://127.0.0.1:1";
const AUDIENCE: &str = "plexus:agents";
const ADA: &str = "00000000-0000-4000-8000-00000000000a";
const BEN: &str = "00000000-0000-4000-8000-00000000000b";
const CAL: &str = "00000000-0000-4000-8000-00000000000c";
const ROOT: &str = "00000000-0000-4000-8000-00000000000d";

struct Signer {
    pem: EncodingKey,
    jwks: Value,
}

fn signer_with(kid: &str) -> Signer {
    let key = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    let b64 = |b: Vec<u8>| URL_SAFE_NO_PAD.encode(b);
    let pem = key.to_pkcs1_pem(Default::default()).unwrap();
    Signer {
        pem: EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        jwks: json!({"keys": [{
            "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid,
            "n": b64(key.n().to_bytes_be()), "e": b64(key.e().to_bytes_be()),
        }]}),
    }
}

/// One key for the whole file: generating one is slow, and each is throwaway.
fn signer() -> &'static Signer {
    static S: OnceLock<Signer> = OnceLock::new();
    S.get_or_init(|| signer_with("test"))
}

fn token_from(signer: &Signer, sub: &str, name: &str, aud: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({
        "iss": ISSUER, "sub": sub, "aud": aud, "iat": now, "exp": now + 600,
        "jti": format!("{sub}-session"), "username": name,
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test".into());
    jsonwebtoken::encode(&header, &claims, &signer.pem).unwrap()
}

fn token(sub: &str, name: &str) -> String {
    token_from(signer(), sub, name, AUDIENCE)
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

/// A real run that parks, and the resume that takes the answer.
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

struct Service {
    port: u16,
    runtime: Supervisor,
    _dir: tempfile::TempDir,
}

fn grants() -> Vec<Grant> {
    [
        format!("idp:{ADA}=user@local"),
        format!("idp:{BEN}=user@local"),
        format!("idp:{ROOT}=admin@*"),
    ]
    .iter()
    .map(|g| g.parse().unwrap())
    .collect()
}

async fn service() -> Service {
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
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let issuer = IssuerUrl::try_new(ISSUER.parse().unwrap()).unwrap();
    let config = OidcConfig::new(issuer, Audience::try_new(AUDIENCE).unwrap());
    let validator = Arc::new(OidcSessionValidator::new(OidcValidator::with_fetcher(
        config,
        Arc::new(Issuer),
    )));
    let h = hub(runtime.clone(), harnesses, grants());
    tokio::spawn(serve(h, validator, [127, 0, 0, 1].into(), port));
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Service {
        port,
        runtime,
        _dir: dir,
    }
}

async fn try_connect(port: u16, token: Option<&str>) -> Result<WsClient, String> {
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

async fn connect(port: u16, token: &str) -> WsClient {
    try_connect(port, Some(token)).await.expect("admitted")
}

async fn open(c: &WsClient, method: &str, params: Value) -> Subscription<Value> {
    let mut p = ObjectParams::new();
    p.insert("method", method).unwrap();
    p.insert("params", params).unwrap();
    c.subscribe("plexus-agents.call", p, "plexus-agents.call_unsub")
        .await
        .unwrap()
}

/// A finite call's one result: the event, or the whole error item.
async fn call(c: &WsClient, method: &str, params: Value) -> Value {
    let mut s = open(c, method, params).await;
    loop {
        let item = tokio::time::timeout(Duration::from_secs(10), s.next())
            .await
            .expect("an answer in time")
            .expect("open")
            .unwrap();
        match item["type"].as_str() {
            Some("data") => return item["content"].clone(),
            Some("error") => return item,
            _ => continue,
        }
    }
}

async fn start(c: &WsClient) -> String {
    let started = call(
        c,
        "agents.start",
        json!({"placement": "local", "cwd": "/recorded/cwd", "prompt": "the recorded prompt"}),
    )
    .await;
    assert_eq!(started["type"], "started", "{started}");
    started["thread"].as_str().unwrap().to_string()
}

async fn pending(c: &WsClient) -> Vec<Value> {
    let p = call(c, "agents.pending", json!({})).await;
    p["requests"].as_array().expect("requests").clone()
}

async fn until_pending(c: &WsClient) -> Vec<Value> {
    for _ in 0..500 {
        let p = pending(c).await;
        if !p.is_empty() {
            return p;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("nothing parked");
}

fn allow() -> Value {
    json!({"answer": "allow", "updated_input": null})
}

fn forbidden(reply: &Value) {
    assert_eq!(reply["error_kind"], "forbidden", "{reply}");
}

/// Anon is a minimal scope: `public` methods run, with an empty reach, and the rest
/// refuse.
#[tokio::test(flavor = "multi_thread")]
async fn an_anonymous_call_is_refused() {
    let s = service().await;
    let ada = connect(s.port, &token(ADA, "ada")).await;
    let thread = start(&ada).await;
    let anon = try_connect(s.port, None).await.unwrap();
    for (method, params) in [
        ("agents.threads", json!({})),
        ("agents.threads", json!({"thread": thread})),
        ("agents.pending", json!({})),
        ("agents.watch", json!({})),
        ("agents.watch", json!({"thread": thread})),
        (
            "agents.start",
            json!({"placement": "local", "cwd": "/x", "prompt": "p"}),
        ),
        ("agents.send", json!({"thread": "t", "prompt": "p"})),
        ("agents.on.local.harnesses", json!({})),
        ("agents.on.placements", json!({})),
    ] {
        let r = call(&anon, method, params).await;
        assert_eq!(r["error_kind"], "unauthenticated", "{method}: {r}");
    }
    assert_eq!(
        s.runtime.threads().unwrap().len(),
        1,
        "anon started nothing"
    );
    for method in ["agents.harnesses", "agents.claudecode.describe"] {
        let r = call(&anon, method, json!({})).await;
        assert_eq!(r["type"], "harnesses", "{method}: {r}");
        assert_eq!(r["harnesses"][0]["name"], "claude", "{method}: {r}");
        let text = r.to_string();
        assert!(!text.contains(&thread), "{method} names no thread");
    }
}

/// Exactly these are `public`, read from the schemas a client reads.
#[tokio::test(flavor = "multi_thread")]
async fn exactly_the_listed_methods_are_public() {
    let s = service().await;
    let root = connect(s.port, &token(ROOT, "root")).await;
    let mut public = Vec::new();
    for (at, ns) in [
        ("agents.schema", "agents"),
        ("agents.claudecode.schema", "claudecode"),
        ("agents.on.schema", "on"),
    ] {
        let schema = call(&root, at, json!({})).await;
        let methods = schema["methods"].as_array().expect(at);
        assert!(!methods.is_empty(), "{at}");
        for m in methods {
            if m["public"] == true {
                public.push(format!("{ns}.{}", m["name"].as_str().unwrap()));
            }
        }
    }
    assert_eq!(public, ["agents.harnesses", "claudecode.describe"]);
}

/// What anon's own view lists: the public methods, and no thread data anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn the_anonymous_view_lists_only_public_methods() {
    let s = service().await;
    let ada = connect(s.port, &token(ADA, "ada")).await;
    let thread = start(&ada).await;
    let anon = try_connect(s.port, None).await.unwrap();
    let names = |schema: &Value| -> Vec<String> {
        let methods = schema["methods"].as_array();
        let methods = methods.map(|m| m.iter()).into_iter().flatten();
        methods
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect()
    };
    let agents = call(&anon, "agents.schema", json!({})).await;
    assert_eq!(
        names(&agents),
        ["harnesses", "claudecode", "schema"],
        "{agents}"
    );
    let children = agents["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "no `on`: {agents}");
    assert_eq!(children[0]["namespace"], "claudecode");
    let child = call(&anon, "agents.claudecode.schema", json!({})).await;
    assert_eq!(names(&child), ["describe", "schema"], "{child}");
    let hub = call(&anon, "plexus-agents.schema", json!({})).await;
    for seen in [&agents, &child, &hub] {
        assert!(!seen.to_string().contains(&thread));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_that_doesnt_verify_is_refused_at_the_upgrade() {
    let s = service().await;
    let other = signer_with("test");
    for bad in [
        "not-a-token".to_string(),
        token_from(&other, ADA, "ada", AUDIENCE),
        token_from(signer(), ADA, "ada", "plexus:menger"),
    ] {
        match try_connect(s.port, Some(&bad)).await {
            Ok(_) => panic!("admitted"),
            Err(e) => assert!(e.contains("401"), "{e}"),
        }
    }
}

/// `typed_params`: a client can't name an actor, in any spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_supplied_actor_is_refused() {
    let s = service().await;
    let ada = connect(s.port, &token(ADA, "ada")).await;
    let thread = start(&ada).await;
    let request = until_pending(&ada).await[0]["request"]["id"].clone();
    for key in ["actor", "by", "owner", "caller", "auth", "user"] {
        let mut params = json!({"thread": thread, "request": request, "answer": allow()});
        params[key] = json!({"id": format!("idp:{BEN}")});
        let r = call(&ada, "agents.answer", params).await;
        assert_eq!(r["error_kind"], "invalid_params", "{key}: {r}");
        assert!(r["message"].as_str().unwrap().contains("unknown field"));
    }
    assert_eq!(pending(&ada).await.len(), 1, "nothing was answered");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_method_takes_an_actor() {
    let s = service().await;
    let ada = connect(s.port, &token(ADA, "ada")).await;
    let schema = call(&ada, "agents.schema", json!({})).await;
    let text = schema.to_string();
    assert!(text.contains("\"target\":\"placement\""), "{text}");
    for method in schema["methods"].as_array().expect("methods") {
        let props = method["params"]["properties"].as_object();
        for (key, prop) in props.into_iter().flatten() {
            assert!(!["actor", "by", "owner"].contains(&key.as_str()), "{key}");
            if ["auth", "caller"].contains(&key.as_str()) {
                assert!(prop.get("x-plexus-source").is_some(), "{key}: {prop}");
            }
        }
    }
}

/// One client sees the request and a second answers, as the same user, and what's
/// recorded is the verified caller, on the request, the log and the thread.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_connection_answers_and_the_verified_caller_is_recorded() {
    let s = service().await;
    let ada_token = token(ADA, "ada");
    let first = connect(s.port, &ada_token).await;
    let thread = start(&first).await;
    let request = until_pending(&first).await[0]["request"]["id"].clone();
    let mut watch = open(
        &first,
        "agents.watch",
        json!({"thread": thread, "since": 0}),
    )
    .await;

    let second = connect(s.port, &ada_token).await;
    let answered = call(
        &second,
        "agents.answer",
        json!({"thread": thread, "request": request, "answer": allow()}),
    )
    .await;
    assert_eq!(answered["type"], "answered", "{answered}");
    let ada = json!({"id": format!("idp:{ADA}"), "name": "ada"});
    assert_eq!(answered["request"]["state"]["by"], ada);

    let (mut prompted, mut resolved, mut terminal) = (None, None, false);
    while !terminal {
        let item = tokio::time::timeout(Duration::from_secs(20), watch.next())
            .await
            .expect("the run finishes")
            .expect("open")
            .unwrap();
        let event = &item["content"]["event"]["event"];
        match event["kind"].as_str() {
            Some("user.message") if prompted.is_none() => prompted = Some(event.clone()),
            Some("input.resolved") => resolved = Some(event.clone()),
            Some("turn.terminal") => terminal = true,
            _ => {}
        }
    }
    assert_eq!(prompted.unwrap()["by"], ada);
    assert_eq!(resolved.unwrap()["by"], ada);
    let view = s.runtime.thread(&ThreadId::new(&thread)).unwrap();
    let owner = Actor {
        id: format!("idp:{ADA}"),
        name: Some("ada".into()),
    };
    assert_eq!(view.owner, Some(owner));
    assert_eq!(view.placement.as_deref(), Some("local"));
}

/// Ben holds the same role as Ada and owns none of what she does.
#[tokio::test(flavor = "multi_thread")]
async fn one_user_cant_see_or_answer_anothers_requests() {
    let s = service().await;
    let ada = connect(s.port, &token(ADA, "ada")).await;
    let ben = connect(s.port, &token(BEN, "ben")).await;
    let thread = start(&ada).await;
    let request = until_pending(&ada).await[0]["request"]["id"].clone();

    assert!(pending(&ben).await.is_empty());
    let listed = call(&ben, "agents.threads", json!({})).await;
    assert_eq!(listed["threads"], json!([]));

    let on_ada = |method: &'static str, extra: Value| {
        let mut p = json!({"thread": thread});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        (method, p)
    };
    for (method, params) in [
        on_ada("agents.threads", json!({})),
        on_ada("agents.watch", json!({})),
        on_ada("agents.send", json!({"prompt": "p"})),
        on_ada("agents.interrupt", json!({})),
        on_ada("agents.stop", json!({})),
        on_ada("agents.resume", json!({})),
        on_ada(
            "agents.answer",
            json!({"request": request, "answer": allow()}),
        ),
    ] {
        forbidden(&call(&ben, method, params).await);
    }

    // Allowed on a thread of his own, and that reaches nothing on hers.
    let his = s
        .runtime
        .create_thread(
            "claude",
            RunSpec::default(),
            ThreadOptions {
                owner: Some(Actor {
                    id: format!("idp:{BEN}"),
                    name: None,
                }),
                placement: Some("local".into()),
                ..ThreadOptions::default()
            },
        )
        .unwrap();
    let crossed = call(
        &ben,
        "agents.answer",
        json!({"thread": his, "request": request, "answer": allow()}),
    )
    .await;
    assert_eq!(crossed["type"], "failed", "{crossed}");
    assert!(crossed["message"].as_str().unwrap().contains("no input"));

    // An unfiltered watch never shows him hers, and it does show his own.
    s.runtime
        .log()
        .tx(|tx| {
            let event = plexus_harness::AgentEvent {
                seq: plexus_harness::EventSeq(1),
                at_ms: 0,
                in_turn: None,
                agent: None,
                kind: plexus_harness::AgentEventKind::Native { name: "his".into() },
                raw: None,
            };
            tx.append(&his, &plexus_harness::RunId::new("r"), &event)?;
            Ok(())
        })
        .unwrap();
    let mut watch = open(&ben, "agents.watch", json!({"since": 0})).await;
    let item = tokio::time::timeout(Duration::from_secs(10), watch.next())
        .await
        .expect("his event")
        .expect("open")
        .unwrap();
    assert_eq!(item["content"]["event"]["thread"], json!(his), "{item}");

    assert_eq!(pending(&ada).await.len(), 1, "hers is still undecided");
}

/// A thread that exists and isn't yours reads like one that doesn't exist, and
/// nobody learns its owner.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_doesnt_say_whether_a_thread_exists_or_whose_it_is() {
    let s = service().await;
    let ada = connect(s.port, &token(ADA, "ada")).await;
    let ben = connect(s.port, &token(BEN, "ben")).await;
    let thread = start(&ada).await;
    let scrub = |mut v: Value, t: &str| {
        let o = v.as_object_mut().unwrap();
        o.remove("held_on");
        v.to_string().replace(t, "<thread>")
    };
    let hers = call(&ben, "agents.threads", json!({"thread": thread})).await;
    let none = call(&ben, "agents.threads", json!({"thread": "no-such"})).await;
    assert!(!hers.to_string().contains(ADA), "{hers}");
    assert_eq!(scrub(hers, &thread), scrub(none, "no-such"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_without_the_grant_cant_start_a_run() {
    let s = service().await;
    let start_on = |placement: &str| json!({"placement": placement, "cwd": "/x", "prompt": "p"});
    let cal = connect(s.port, &token(CAL, "cal")).await;
    forbidden(&call(&cal, "agents.start", start_on("local")).await);
    let ben = connect(s.port, &token(BEN, "ben")).await;
    forbidden(&call(&ben, "agents.start", start_on("other")).await);
    forbidden(&call(&ben, "agents.start", start_on("no-such-place")).await);
    assert!(s.runtime.threads().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_reaches_everyones() {
    let s = service().await;
    let ada = connect(s.port, &token(ADA, "ada")).await;
    let root = connect(s.port, &token(ROOT, "root")).await;
    let thread = start(&ada).await;
    let request = until_pending(&ada).await[0]["request"]["id"].clone();
    assert_eq!(pending(&root).await.len(), 1);
    let answered = call(
        &root,
        "agents.answer",
        json!({"thread": thread, "request": request, "answer": allow()}),
    )
    .await;
    let root_id = format!("idp:{ROOT}");
    assert_eq!(answered["request"]["state"]["by"]["id"], root_id.as_str());
}

/// A reconnecting client picks up from its cursor.
#[tokio::test(flavor = "multi_thread")]
async fn watch_replays_from_since_over_the_wire() {
    let s = service().await;
    let ada = connect(s.port, &token(ADA, "ada")).await;
    let thread = start(&ada).await;
    until_pending(&ada).await;
    let mut all = open(&ada, "agents.watch", json!({"thread": thread})).await;
    let mut seqs = Vec::new();
    for _ in 0..3 {
        let item = all.next().await.unwrap().unwrap();
        seqs.push(item["content"]["event"]["seq"].as_u64().unwrap());
    }
    let mut after = open(
        &ada,
        "agents.watch",
        json!({"thread": thread, "since": seqs[0]}),
    )
    .await;
    let item = after.next().await.unwrap().unwrap();
    assert_eq!(item["content"]["event"]["seq"], seqs[1]);
    assert!(
        item["content"]["event"]["event"].get("raw").is_none(),
        "raw only on request"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_harness_describes_itself_through_its_child() {
    let s = service().await;
    let ada = connect(s.port, &token(ADA, "ada")).await;
    let r = call(&ada, "agents.claudecode.describe", json!({})).await;
    assert_eq!(r["harnesses"][0]["name"], "claude", "{r}");
    let r = call(&ada, "agents.harnesses", json!({})).await;
    assert_eq!(r["harnesses"][0]["name"], "claude", "{r}");
}

#[test]
fn plexus_harness_has_no_plexus_crate_under_it() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plexus-harness/Cargo.toml");
    let out = std::process::Command::new(env!("CARGO"))
        .args(["tree", "--all-features", "--prefix", "none"])
        .args(["--edges", "normal,build"])
        .arg("--manifest-path")
        .arg(manifest)
        .output()
        .unwrap();
    assert!(out.status.success());
    let tree = String::from_utf8(out.stdout).unwrap();
    assert!(tree.lines().count() > 1, "{tree}");
    let plexus: Vec<&str> = tree
        .lines()
        .filter(|l| l.starts_with("plexus") && !l.starts_with("plexus-harness "))
        .collect();
    assert!(plexus.is_empty(), "{plexus:?}");
}

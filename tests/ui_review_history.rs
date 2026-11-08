//! SUPERCONDUCTOR UI history, walked: a host whose store holds a long thread (~2,000
//! events, every reply streamed as deltas) is opened in Chromium. The page reads
//! `hosts.history`'s newest page, then follows from its `last_seq`; scrolling to
//! the top reads the page before without moving what the reader sees, until the
//! thread's first prompt. The socket's frames and bytes up to the first render
//! are counted and printed. With `SC_UI_HISTORY_BEFORE=1` the walk only opens the
//! thread and counts, for a bundle from before the history (the replay).
//!
//! ```text
//! SC_UI_BUNDLE=dist/ui.tar SC_UI_HOST_STUB=... PLEXUS_UI_NODE_MODULES=.../plexus-ui/node_modules \
//!   cargo test --test ui_review_history -- --ignored --nocapture
//! ```

use base64::Engine;
use jsonrpsee::core::client::SubscriptionClientT;
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::{HeaderMap, HeaderValue, WsClient, WsClientBuilder};
use plexus_agents::front::{self, SignInConfig};
use plexus_agents::hosts::Hosts;
use plexus_agents::pairing::ServerKey;
use plexus_agents::runtime::{EventLog, Supervisor, SupervisorConfig, ThreadId, ThreadOptions};
use plexus_agents::service::{hub_with_people, Serving};
use plexus_agents::ClaudeHarness;
use plexus_harness::{
    AgentEvent, AgentEventKind, Environment, EventSeq, Harness, RunId, RunSpec,
    ThreadDisposition, ToolName, ToolUseId, TurnId, TurnOrigin, TurnStatus, TurnTerminal,
};
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

/// A long thread in the host's store, before the host opens it: `turns` turns
/// of a prompt, a reply streamed in `words` deltas, a command and its output, a
/// second reply, an end. Returns its id and how many events it holds.
async fn long_thread(db: &std::path::Path, turns: u64, words: usize) -> (ThreadId, u64) {
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(ClaudeHarness::default())];
    let local: Arc<dyn Environment> = Arc::new(Nowhere);
    let (runtime, _) = Supervisor::open(SupervisorConfig {
        db: db.to_path_buf(),
        environment: local.clone(),
        placements: HashMap::from([("local".to_string(), local)]),
        harnesses,
    })
    .await
    .unwrap();
    let spec = RunSpec {
        cwd: Some("/home/op/dev/lantern".into()),
        ..RunSpec::default()
    };
    let options = ThreadOptions {
        park: true,
        owner: None,
        placement: Some("local".into()),
    };
    let thread = runtime.create_thread("claude", spec, options).unwrap();
    let log = runtime.log();
    let run = RunId::new("synthetic");
    let mut n = 0u64;
    let mut clock = 1_790_000_000_000u64;
    let mut put = |turn: Option<u64>, kind: AgentEventKind| {
        n += 1;
        clock += 40;
        let e = AgentEvent {
            seq: EventSeq(n),
            at_ms: clock,
            in_turn: turn.map(TurnId),
            agent: None,
            kind,
            raw: None,
        };
        log.append(&thread, &run, &[e]).unwrap();
    };
    for turn in 1..=turns {
        let t = Some(turn);
        put(t, AgentEventKind::TurnStarted { turn: TurnId(turn), origin: TurnOrigin::Prompt });
        put(t, AgentEventKind::UserMessage { text: format!("Prompt {turn}: look at the parser again and tell me what changed."), by: None });
        let mut said = String::new();
        for i in 0..words {
            let w = format!("{}word{i}{}", if i % 12 == 0 { "\n\n" } else { "" }, if i % 12 == 11 { "." } else { " " });
            said.push_str(&w);
            put(t, AgentEventKind::TextDelta { text: w });
        }
        put(t, AgentEventKind::Message { text: format!("Reply {turn}. {said}") });
        let id = ToolUseId::new(format!("toolu_{turn}"));
        put(t, AgentEventKind::ToolCall { id: id.clone(), name: ToolName::new("Bash"), input: json!({"command": format!("cargo test parser_{turn}")}) });
        put(t, AgentEventKind::ToolResult { id, is_error: false, output: json!(format!("test parser_{turn} ... ok")) });
        put(t, AgentEventKind::Message { text: format!("Turn {turn} is done.") });
        put(t, AgentEventKind::TurnTerminal(TurnTerminal {
            turn: TurnId(turn),
            status: TurnStatus::Completed,
            thread_disposition: ThreadDisposition::Reusable,
            stopped_by: None,
        }));
    }
    (thread, n)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built bundle (SC_UI_BUNDLE) and Playwright (PLEXUS_UI_NODE_MODULES)"]
async fn a_long_thread_opens_at_its_newest_page_and_scrolls_back() {
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
    let mut h = HeaderMap::new();
    h.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {proof}")).unwrap(),
    );
    let c = WsClientBuilder::default()
        .set_headers(h)
        .build(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .unwrap();
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

    let (thread, events) = long_thread(&dir.path().join("h.db"), 30, 64).await;
    eprintln!("the thread: {events} events");

    // A host dials in, unpaired: its link is the bundle's pairing route.
    let key = dir.path().join("mac.key");
    std::fs::write(&key, b64.encode([9u8; 32])).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    // The shared target dir is rebuilt by other worktrees: $SC_UI_HOST_STUB pins this one.
    let stub = std::env::var("SC_UI_HOST_STUB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| bin.join("examples/host_stub"));
    let mut host = Command::new(stub)
        .args([
            "--server",
            &format!("ws://127.0.0.1:{port}/ws"),
            "--name",
            "mac",
        ])
        .args([
            "--db",
            dir.path().join("h.db").to_str().unwrap(),
            "--pins",
            dir.path().join("pins").to_str().unwrap(),
        ])
        .args(["--key", key.to_str().unwrap()])
        .args([
            "--fixture",
            concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/ui-review/transcript"),
        ])
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
    let (mut link, mut code) = (String::new(), String::new());
    while link.is_empty() || code.is_empty() {
        let line = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        if let Some(l) = line.strip_prefix("link: ") {
            link = l.to_string();
        }
        if let Some(c) = line.strip_prefix("code: ") {
            code = c.to_string();
        }
    }
    assert!(link.starts_with(&format!("{origin}/#/pair/")), "{link}");

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui_review_history.mjs");
    let shots = std::env::var("SC_UI_SHOTS").unwrap_or_else(|_| {
        format!("{}/sc-ui-history/shots", std::env::var("HOME").unwrap())
    });
    std::fs::create_dir_all(&shots).unwrap();
    let run = tokio::task::spawn_blocking(move || {
        Command::new("node")
            .arg(script)
            .env("NODE_PATH", &modules)
            .env("BASE", &origin)
            .env("LINK", &link)
            .env("CODE", &code)
            .env("TOKEN", &token)
            .env("SHOTS", &shots)
            .env("THREAD", thread.as_str())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let _ = host.kill();
    let _ = host.wait();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(run.status.success(), "{said}");
    assert!(hosts.knows("mac"), "paired from the page");
    eprintln!("{said}");
}

//! AGENTS-9: SIGKILL the service. The service is `examples/agents_service`, a real
//! process, playing the frames of the real run where this happened
//! (`scripts/runtime-real`). The real run proved the tool started once. This proves
//! the runtime's half, on every test run: what a restart hands the next process.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

fn set() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/recorded/agents-9-2026-11-01")
}

fn service_binary() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    test.parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/agents_service")
}

struct Service {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    ready: Value,
}

impl Service {
    fn start(dir: &Path, replay: &[String]) -> Self {
        let mut command = Command::new(service_binary());
        command
            .arg("--db")
            .arg(dir.join("runtime.db"))
            .arg("--state")
            .arg(dir.join("state"))
            .arg("--plans")
            .arg(dir.join("plans.jsonl"));
        for r in replay {
            command.arg("--replay").arg(r);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("cargo test builds examples");
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        Self {
            child,
            stdin,
            stdout,
            ready: serde_json::from_str(&line).unwrap(),
        }
    }

    fn op(&mut self, op: Value) -> Value {
        writeln!(self.stdin, "{op}").unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn status(&mut self, t: &str) -> Value {
        self.op(json!({"op": "status", "thread": t}))["ok"].take()
    }

    fn kinds(&mut self) -> Vec<String> {
        let v = self.op(json!({"op": "events"}));
        v["ok"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["event"]["kind"].as_str().unwrap().to_string())
            .collect()
    }

    fn wait(&mut self, mut what: impl FnMut(&mut Self) -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while !what(self) {
            assert!(Instant::now() < end, "timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// SIGKILL: nothing gets to clean up.
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

fn phase(case: &str, name: &str, flags: &str) -> String {
    format!("{}{flags}", set().join(case).join(name).display())
}

fn create(s: &mut Service, park: bool) -> String {
    let v = s.op(json!({"op": "create", "cwd": "/recorded/cwd", "park": park}));
    v["ok"]["thread"].as_str().unwrap().to_string()
}

fn plans(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("plans.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn answer_files(plan: &Value) -> Vec<String> {
    plan["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|p| p.contains("/answers/") || p.contains("answer"))
        .map(str::to_string)
        .collect()
}

#[test]
fn a_crash_mid_tool_never_hands_the_answer_to_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Service::start(
        dir.path(),
        &[
            phase("crash-mid-tool", "svc1/1-claude", ""),
            phase("crash-mid-tool", "svc1/2-claude", ":signal:unprompted"),
        ],
    );
    let t = create(&mut a, true);
    a.op(json!({"op": "prompt", "thread": t, "text": "recorded"}));
    a.wait(|s| s.status(&t)["thread"]["status"] == "parked");
    let first = a.status(&t)["thread"]["pending"][0]
        .as_str()
        .unwrap()
        .to_string();
    let v = a.op(json!({"op": "answer", "thread": t, "id": first, "allow": true}));
    assert!(v.get("ok").is_some(), "{v}");
    a.wait(|s| s.status(&t)["inputs"][0]["delivery"] == "delivered");
    // The tool is running in the replayed process. Now the service dies.
    a.wait(|s| s.kinds().contains(&"tool.started".to_string()) || s.status(&t)["live"] == true);
    a.kill();
    assert_eq!(
        answer_files(&plans(dir.path())[1]).len(),
        1,
        "the first resume had it"
    );

    let mut b = Service::start(
        dir.path(),
        &[
            phase("crash-mid-tool", "svc2/1-claude", ":unprompted"),
            phase("crash-mid-tool", "svc2/2-claude", ":unprompted"),
        ],
    );
    assert_eq!(b.ready["recovery"]["outcome_unknown"], json!([first]));
    assert_eq!(
        plans(dir.path()).len(),
        2,
        "a restart starts nothing by itself"
    );
    let s = b.status(&t);
    assert_eq!(s["thread"]["status"], "parked");
    assert_eq!(s["thread"]["outcome_unknown"], json!([first]));
    assert_eq!(s["inputs"][0]["delivery"], "outcome_unknown");
    // An answer can't be given twice, and that row is no longer answerable.
    let v = b.op(json!({"op": "answer", "thread": t, "id": first, "allow": true}));
    assert!(v.get("error").is_some(), "{v}");
    assert_eq!(plans(dir.path()).len(), 2);

    // Carrying on hands the next process no answer, so the call comes back undecided.
    let v = b.op(json!({"op": "continue", "thread": t}));
    assert!(v.get("ok").is_some(), "{v}");
    let all = plans(dir.path());
    assert_eq!(all.len(), 3);
    assert_eq!(answer_files(&all[2]), Vec::<String>::new());
    b.wait(|s| s.status(&t)["thread"]["status"] == "parked" && s.status(&t)["live"] == false);
    b.wait(|s| s.status(&t)["thread"]["pending"].as_array().unwrap().len() == 1);
    let s = b.status(&t);
    let pending: Vec<&Value> = s["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["delivery"] == "pending")
        .collect();
    assert_eq!(pending[0]["after_unknown_outcome"], true);
    assert_eq!(pending[0]["tool_use_id"], s["inputs"][0]["tool_use_id"]);
    b.kill();
}

#[test]
fn a_restart_mid_run_closes_it_out_as_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let case = "restart-mid-run";
    let mut a = Service::start(dir.path(), &[phase(case, "svc1/1-claude", ":signal")]);
    let t = create(&mut a, false);
    a.op(json!({"op": "prompt", "thread": t, "text": "recorded"}));
    a.wait(|s| s.kinds().contains(&"text.delta".to_string()));
    a.kill();

    let mut b = Service::start(dir.path(), &[phase(case, "svc2/1-claude", "")]);
    assert_eq!(
        b.ready["recovery"]["interrupted"].as_array().unwrap().len(),
        1
    );
    let v = b.op(json!({"op": "events"}));
    let events = v["ok"]["events"].as_array().unwrap();
    let terminal = &events[events.len() - 2]["event"];
    assert_eq!(terminal["kind"], "turn.terminal");
    assert_eq!(terminal["status"]["status"], "interrupted", "{terminal}");
    let last = &events[events.len() - 1]["event"];
    assert_eq!(last["kind"], "run.ended");
    assert_eq!(last["reason"], "lost");
    assert_eq!(b.status(&t)["thread"]["status"], "idle");

    // Resumable, as the next turn of the same thread.
    let v = b.op(json!({"op": "prompt", "thread": t, "text": "recorded"}));
    assert!(v.get("ok").is_some(), "{v}");
    b.wait(|s| s.kinds().iter().filter(|k| *k == "turn.terminal").count() == 2);
    let s = b.status(&t);
    assert_eq!(s["thread"]["last_turn"], 2);
    assert_eq!(s["thread"]["runs"], 2);
    b.kill();
}

#[test]
fn a_parked_thread_survives_the_service_and_a_new_one_answers_it() {
    let dir = tempfile::tempdir().unwrap();
    let case = "restart-parked";
    let mut a = Service::start(dir.path(), &[phase(case, "svc1/1-claude", "")]);
    let t = create(&mut a, true);
    a.op(json!({"op": "prompt", "thread": t, "text": "recorded"}));
    a.wait(|s| s.status(&t)["thread"]["status"] == "parked");
    a.kill();

    let mut b = Service::start(dir.path(), &[phase(case, "svc2/1-claude", ":unprompted")]);
    let s = b.status(&t);
    let id = s["thread"]["pending"][0].as_str().unwrap().to_string();
    let v = b.op(json!({"op": "prompt", "thread": t, "text": "no"}));
    assert!(v["error"].as_str().unwrap().contains("undecided"), "{v}");
    b.op(json!({"op": "answer", "thread": t, "id": id, "allow": true}));
    b.wait(|s| s.status(&t)["inputs"][0]["delivery"] == "applied");
    assert_eq!(answer_files(&plans(dir.path())[1]).len(), 1);
    b.wait(|s| s.kinds().contains(&"turn.terminal".to_string()));
    b.kill();
}

#[test]
fn a_live_request_outlives_its_process_as_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let case = "live-pending-killed";
    let mut a = Service::start(dir.path(), &[phase(case, "svc1/1-claude", ":signal")]);
    let t = create(&mut a, false);
    a.op(json!({"op": "prompt", "thread": t, "text": "recorded"}));
    a.wait(|s| s.status(&t)["thread"]["pending"].as_array().unwrap().len() == 1);
    a.kill();

    let mut b = Service::start(dir.path(), &[phase(case, "svc2/1-claude", "")]);
    assert_eq!(b.ready["recovery"]["rerouted"].as_array().unwrap().len(), 1);
    let s = b.status(&t);
    assert_eq!(s["inputs"][0]["route"], "message");
    let id = s["thread"]["pending"][0].as_str().unwrap().to_string();
    b.op(json!({"op": "answer", "thread": t, "id": id, "allow": true}));
    b.wait(|s| s.kinds().iter().filter(|k| *k == "turn.terminal").count() == 2);
    let s = b.status(&t);
    assert_eq!(s["inputs"][0]["delivery"], "superseded");
    assert_eq!(
        s["inputs"][1]["delivery"], "applied",
        "the retry took the answer"
    );
    assert_eq!(s["thread"]["pending"], json!([]));
    b.kill();
}

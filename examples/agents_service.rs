//! The runtime as a process that can be killed, for AGENTS-9's crash checks. This
//! is a test driver and not the service: the activation is AGENTS-13.
//!
//! ```text
//! agents_service --db FILE --state DIR [--record DIR | --replay PHASE[:signal][:unprompted]...]
//!                [--plans FILE]
//! ```
//! Reads one JSON op a line, writes one JSON reply a line:
//! `create {cwd, model, park}`, `prompt {thread, text}`, `answer {id, allow}`,
//! `continue {thread}`, `interrupt {thread}`, `status {thread}`, `events {since}`.
//!
//! `--replay` plays `<dir>/<phase>` recordings in place of `claude`, for a check
//! that spends nothing. `:signal` holds a phase open until its process is killed,
//! which is how a recording that ended in a crash ends. `:unprompted` is a resume that
//! carries on by itself. `--plans` appends each
//! plan's file paths, so a check can see what a process was handed.

use futures::future::BoxFuture;
use plexus_agents::runtime::{
    EventLog, GlobalSeq, ReadFilter, Supervisor, SupervisorConfig, ThreadId, ThreadOptions,
};
use plexus_agents::{ClaudeHarness, LocalEnvironment, RecordingEnvironment};
use plexus_harness::conformance::replay::{ExitTrigger, ReplayEnvironment, ReplayScript};
use plexus_harness::{
    Environment, InputAnswer, InputRequestId, LaunchPlan, ModelName, Process, RunSpec, SpawnError,
    ToolName, ToolPolicy, UserInput,
};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Sequence(Vec<ReplayEnvironment>, AtomicUsize);

impl Environment for Sequence {
    fn name(&self) -> &str {
        "sequence"
    }
    fn spawn(&self, plan: LaunchPlan) -> BoxFuture<'static, Result<Process, SpawnError>> {
        self.0[self.1.fetch_add(1, Ordering::SeqCst)].spawn(plan)
    }
}

/// Notes what each process was handed.
struct Noted {
    inner: Arc<dyn Environment>,
    file: Option<PathBuf>,
}

impl Environment for Noted {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn spawn(&self, plan: LaunchPlan) -> BoxFuture<'static, Result<Process, SpawnError>> {
        if let Some(file) = &self.file {
            let paths: Vec<&str> = plan.files.iter().map(|f| f.path.as_str()).collect();
            let line = json!({"cwd": plan.cwd, "files": paths, "argv": plan.argv});
            let mut out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(file)
                .expect("plans file");
            writeln!(out, "{line}").expect("plans file");
        }
        self.inner.spawn(plan)
    }
    fn identity(&self, pid: u32) -> BoxFuture<'static, Option<String>> {
        self.inner.identity(pid)
    }
    fn reap(&self, pid: u32, identity: &str) -> BoxFuture<'static, bool> {
        self.inner.reap(pid, identity)
    }
}

fn replayed(phases: &[String]) -> Result<Sequence, String> {
    let steps = phases.iter().map(|p| {
        let (p, unprompted) = match p.strip_suffix(":unprompted") {
            Some(p) => (p, true),
            None => (p.as_str(), false),
        };
        let (path, trigger) = match p.strip_suffix(":signal") {
            Some(path) => (path, ExitTrigger::OnSignal),
            None => (p, ExitTrigger::OnInputClose),
        };
        let path = Path::new(path);
        let (dir, phase) = (path.parent().unwrap(), path.file_name().unwrap());
        let exit = ReplayScript::exit_from_rc(0);
        let mut s = ReplayScript::load(dir, &phase.to_string_lossy(), exit, trigger, 0)?;
        s.unprompted = unprompted;
        Ok(ReplayEnvironment::new(s))
    });
    Ok(Sequence(
        steps.collect::<Result<_, String>>()?,
        AtomicUsize::new(0),
    ))
}

fn spec(op: &Value) -> RunSpec {
    let tool = ToolName::new("Bash");
    RunSpec {
        cwd: op["cwd"].as_str().map(str::to_string),
        model: Some(ModelName::new(op["model"].as_str().unwrap_or("haiku"))),
        tools: ToolPolicy {
            enabled: Some(vec![tool]),
            preapproved: vec![],
            denied: vec![],
        },
        ..RunSpec::default()
    }
    .with_host_env(["USER", "PATH"])
}

async fn handle(s: &Supervisor, op: &Value) -> Result<Value, String> {
    let text = |k: &str| op[k].as_str().unwrap_or_default().to_string();
    let thread = ThreadId::new(text("thread"));
    let err = |e: plexus_agents::runtime::RuntimeError| e.to_string();
    match op["op"].as_str().unwrap_or_default() {
        "create" => {
            let options = ThreadOptions {
                park: op["park"].as_bool().unwrap_or(false),
            };
            let id = s.create_thread("claude", spec(op), options).map_err(err)?;
            Ok(json!({"thread": id}))
        }
        "prompt" => {
            let turn = s
                .prompt(&thread, UserInput::text(text("text")))
                .await
                .map_err(err)?;
            Ok(json!({"turn": turn}))
        }
        "answer" => {
            let id = InputRequestId::new(text("id"));
            let answer = if op["allow"].as_bool().unwrap_or(false) {
                InputAnswer::Allow {
                    updated_input: None,
                }
            } else {
                InputAnswer::Deny {
                    message: text("message"),
                    stop_turn: false,
                }
            };
            let r = s
                .inputs(&thread)
                .answer(&id, answer)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({"state": r.state}))
        }
        "continue" => s
            .continue_thread(&thread)
            .await
            .map(|()| json!({}))
            .map_err(err),
        "interrupt" => s
            .interrupt(&thread)
            .await
            .map(|o| json!({"outcome": format!("{o:?}")}))
            .map_err(err),
        "status" => {
            let view = s.thread(&thread).map_err(err)?;
            let inputs: Vec<Value> = s
                .records(&thread)
                .map_err(err)?
                .iter()
                .map(|r| {
                    json!({
                        "id": r.request.id, "delivery": r.delivery, "route": r.request.route,
                        "tool_use_id": r.tool_use_id,
                        "after_unknown_outcome": r.after_unknown_outcome,
                    })
                })
                .collect();
            Ok(json!({"thread": view, "inputs": inputs, "live": s.is_live(&thread)}))
        }
        "events" => {
            let filter = ReadFilter {
                thread: None,
                without_raw: true,
            };
            let since = GlobalSeq(op["since"].as_u64().unwrap_or(0));
            let page = s
                .log()
                .page(&filter, since, 100_000)
                .map_err(|e| e.to_string())?;
            Ok(json!({"events": page}))
        }
        other => Err(format!("no op {other}")),
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let (mut db, mut state, mut record, mut plans, mut replay) =
        (None, None, None, None, Vec::new());
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => db = Some(value()?),
            "--state" => state = Some(value()?),
            "--record" => record = Some(value()?),
            "--plans" => plans = Some(PathBuf::from(value()?)),
            "--replay" => replay.push(value()?),
            other => return Err(format!("no flag {other}")),
        }
    }
    let inner: Arc<dyn Environment> = match record {
        _ if !replay.is_empty() => Arc::new(replayed(&replay)?),
        Some(dir) => Arc::new(
            RecordingEnvironment::new(LocalEnvironment::new(), dir).map_err(|e| e.to_string())?,
        ),
        None => Arc::new(LocalEnvironment::new()),
    };
    let (supervisor, recovery) = Supervisor::open(SupervisorConfig {
        db: db.ok_or("--db")?.into(),
        state_root: state.ok_or("--state")?,
        environment: Arc::new(Noted { inner, file: plans }),
        harnesses: vec![Arc::new(ClaudeHarness::default())],
    })
    .await
    .map_err(|e| e.to_string())?;
    println!(
        "{}",
        json!({"ready": true, "recovery": {
            "interrupted": recovery.interrupted, "reaped": recovery.reaped,
            "outcome_unknown": recovery.outcome_unknown, "rerouted": recovery.rerouted,
            "expired": recovery.expired,
        }})
    );
    let (lines, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    while let Some(line) = rx.recv().await {
        let op: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        let reply = match handle(&supervisor, &op).await {
            Ok(v) => json!({"ok": v}),
            Err(e) => json!({"error": e}),
        };
        println!("{reply}");
    }
    Ok(())
}

//! AGENTS-9's risk: what per-delta SQLite writes cost. Run with `--release`.
//!
//! ```text
//! log_bench [EVENTS]
//! ```
//! Appends frame-sized events to a fresh log on this disk: one transaction an
//! event, then batched as the supervisor's pump batches them, then reads them back.

use plexus_agents::runtime::{EventLog, GlobalSeq, ReadFilter, Store, ThreadId};
use plexus_harness::{AgentEvent, AgentEventKind, EventSeq, RawFrame, RunId, TurnId};
use std::time::Instant;

const FRAME: usize = 400;
const BATCH: usize = 256;

fn event(n: u64) -> AgentEvent {
    AgentEvent {
        seq: EventSeq(n + 1),
        at_ms: 1,
        in_turn: Some(TurnId(1)),
        agent: None,
        kind: AgentEventKind::TextDelta {
            text: "delta".into(),
        },
        raw: Some(RawFrame::new(format!(
            "{{\"delta\":\"{}\"}}",
            "x".repeat(FRAME)
        ))),
    }
}

fn report(name: &str, n: usize, started: Instant) {
    let s = started.elapsed().as_secs_f64();
    println!(
        "{name}: {n} events in {:.0} ms: {:.0} events/s, {:.1} us each",
        s * 1e3,
        n as f64 / s,
        s * 1e6 / n as f64
    );
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map_or(10_000, |a| a.parse().expect("EVENTS"));
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("log.db")).expect("open");
    let (thread, run) = (ThreadId::new("t"), RunId::new("r"));
    let events: Vec<AgentEvent> = (0..n as u64).map(event).collect();

    let started = Instant::now();
    for e in &events {
        store
            .append(&thread, &run, std::slice::from_ref(e))
            .expect("append");
    }
    report("one transaction per event", n, started);

    let started = Instant::now();
    for chunk in events.chunks(BATCH) {
        store.append(&thread, &run, chunk).expect("append");
    }
    report(&format!("batches of {BATCH}"), n, started);

    let started = Instant::now();
    let (mut after, mut seen) = (GlobalSeq(0), 0);
    let filter = ReadFilter::default();
    while seen < 2 * n {
        let page = store.page(&filter, after, 512).expect("page");
        seen += page.len();
        after = page.last().expect("a full log").seq;
    }
    report("read back", seen, started);
}

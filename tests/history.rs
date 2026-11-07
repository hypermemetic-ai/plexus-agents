//! A thread's history (`agents.history`): finished items, paged newest first, read
//! by seeking; `watch {settled}` catching up without finished messages' deltas;
//! a thread's `title` and `last_activity` kept as it advances; an older store
//! migrated. And what opening a long thread costs, before and after:
//!
//! ```text
//! cargo test --test history -- --nocapture opening_a_long_thread
//! ```

use futures::StreamExt;
use plexus_agents::runtime::{
    EventLog, GlobalSeq, HistoryItem, ItemBody, ReadFilter, Store, ThreadId, TITLE_CHARS,
};
use plexus_agents::service::AgentsEvent;
use plexus_harness::{
    AgentEvent, AgentEventKind, EventSeq, InputOutcome, InputRequestId, RawFrame, RunId,
    SubagentId, ThreadDisposition, ToolName, ToolUseId, TurnId, TurnOrigin, TurnStatus,
    TurnTerminal,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

/// Builds a thread's log, one event at a time, `at_ms` a clock it moves.
struct Thread {
    store: Arc<Store>,
    id: ThreadId,
    run: RunId,
    n: u64,
    clock: u64,
    turn: Option<TurnId>,
}

impl Thread {
    fn new(store: &Arc<Store>, id: &str) -> Self {
        store
            .tx(|tx| {
                tx.sql
                    .execute(
                        "INSERT INTO threads (id, harness, spec, status, created_ms)
                         VALUES (?, 'claude', '{}', 'idle', 0)",
                        [id],
                    )
                    .map_err(|e| plexus_agents::runtime::StoreError(e.to_string()))?;
                Ok(())
            })
            .unwrap();
        Self {
            store: store.clone(),
            id: ThreadId::new(id),
            run: RunId::new(format!("{id}-run")),
            n: 0,
            clock: 1_000_000,
            turn: None,
        }
    }

    fn at(&mut self, ms: u64, agent: Option<&str>, kind: AgentEventKind, raw: bool) -> u64 {
        self.n += 1;
        self.clock += ms;
        let event = AgentEvent {
            seq: EventSeq(self.n),
            at_ms: self.clock,
            in_turn: self.turn,
            agent: agent.map(SubagentId::new),
            kind,
            raw: raw.then(|| {
                RawFrame::new(format!(
                    "{{\"frame\":{},\"pad\":\"{}\"}}",
                    self.n,
                    "x".repeat(200)
                ))
            }),
        };
        self.store.append(&self.id, &self.run, &[event]).unwrap()[0].0
    }

    fn ev(&mut self, kind: AgentEventKind) -> u64 {
        self.at(10, None, kind, true)
    }

    fn start(&mut self, turn: u64, prompt: &str) {
        self.turn = Some(TurnId(turn));
        self.ev(AgentEventKind::TurnStarted {
            turn: TurnId(turn),
            origin: TurnOrigin::Prompt,
        });
        self.ev(AgentEventKind::UserMessage {
            text: prompt.into(),
            by: None,
        });
    }

    /// A reply streamed in `words` deltas, framed as Claude frames it; finished
    /// with its `message` unless `open`.
    fn reply(&mut self, words: usize, open: bool) -> String {
        let mut text = String::new();
        self.ev(AgentEventKind::Native {
            name: "stream_event/content_block_start".into(),
        });
        for i in 0..words {
            let w = format!("word{i} ");
            text.push_str(&w);
            self.ev(AgentEventKind::TextDelta { text: w });
        }
        if !open {
            self.ev(AgentEventKind::Native {
                name: "stream_event/content_block_stop".into(),
            });
            self.ev(AgentEventKind::Message { text: text.clone() });
        }
        text
    }

    fn tool(&mut self, id: &str) {
        self.ev(AgentEventKind::ToolCall {
            id: ToolUseId::new(id),
            name: ToolName::new("Bash"),
            input: json!({"command": format!("echo {id}")}),
        });
        self.ev(AgentEventKind::ToolResult {
            id: ToolUseId::new(id),
            is_error: false,
            output: json!(format!("{id}\n")),
        });
    }

    fn end(&mut self, turn: u64) {
        self.ev(AgentEventKind::Tokens {
            turn: TurnId(turn),
            usage: Default::default(),
        });
        self.ev(AgentEventKind::TurnTerminal(TurnTerminal {
            turn: TurnId(turn),
            status: TurnStatus::Completed,
            thread_disposition: ThreadDisposition::Reusable,
            stopped_by: None,
        }));
        self.turn = None;
    }
}

fn store() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("agents.db")).unwrap();
    (dir, store)
}

/// A long thread: `turns` finished turns of a streamed reply, a tool call and a
/// second reply each, a request answered in turn 2, then a turn still streaming.
fn long_thread(store: &Arc<Store>, turns: u64, words: usize) -> Thread {
    let mut t = Thread::new(store, "long");
    for turn in 1..=turns {
        t.start(turn, &format!("prompt {turn}"));
        t.reply(words, false);
        if turn == 2 {
            t.ev(AgentEventKind::ToolCall {
                id: ToolUseId::new("asked"),
                name: ToolName::new("Bash"),
                input: json!({"command": "rm -i x"}),
            });
            t.ev(AgentEventKind::InputRequested {
                id: InputRequestId::new("req-1"),
            });
            t.at(
                5_000,
                None,
                AgentEventKind::InputResolved {
                    id: InputRequestId::new("req-1"),
                    outcome: InputOutcome::Allowed,
                    by: None,
                },
                false,
            );
            t.ev(AgentEventKind::ToolResult {
                id: ToolUseId::new("asked"),
                is_error: false,
                output: json!("ok"),
            });
        }
        t.tool(&format!("call-{turn}"));
        t.reply(words / 2, false);
        t.end(turn);
    }
    t.start(turns + 1, "the last prompt");
    t.reply(5, true);
    t
}

fn kinds(items: &[HistoryItem]) -> Vec<&'static str> {
    items
        .iter()
        .map(|i| match &i.body {
            ItemBody::User { .. } => "user",
            ItemBody::Message { partial: true, .. } => "partial",
            ItemBody::Message { .. } => "message",
            ItemBody::Tool { .. } => "tool",
            ItemBody::Input { .. } => "input",
            ItemBody::TurnStarted { .. } => "turn.started",
            ItemBody::TurnEnded { .. } => "turn.ended",
            ItemBody::Notice { .. } => "notice",
        })
        .collect()
}

#[test]
fn the_newest_page_is_finished_items_and_the_reply_still_streaming() {
    let (_d, store) = store();
    let t = long_thread(&store, 3, 20);
    let page = store.history(&t.id, None, 6).unwrap();
    assert_eq!(
        page.last_seq,
        GlobalSeq(t.n),
        "one thread in the log: seq is n"
    );
    assert_eq!(
        kinds(&page.items),
        [
            "message",
            "tool",
            "message",
            "turn.ended",
            "turn.started",
            "user",
            "partial"
        ]
    );
    let ItemBody::Message { text, .. } = &page.items[6].body else {
        unreachable!()
    };
    assert_eq!(
        text, "word0 word1 word2 word3 word4 ",
        "the deltas so far, joined"
    );
    let ItemBody::Tool { result, .. } = &page.items[1].body else {
        unreachable!()
    };
    assert_eq!(result.as_ref().unwrap().output, json!("call-3\n"), "joined");
    assert!(
        page.items.windows(2).all(|w| w[0].seq < w[1].seq),
        "oldest first"
    );
    assert!(page.next_before.is_some());
}

#[test]
fn paging_back_reads_every_item_once_and_ends_at_the_start() {
    let (_d, store) = store();
    let t = long_thread(&store, 12, 30);
    let mut all: Vec<HistoryItem> = Vec::new();
    let mut before = None;
    let mut pages = 0;
    loop {
        let page = store.history(&t.id, before, 7).unwrap();
        pages += 1;
        let mut items = page.items;
        items.extend(all);
        all = items;
        match page.next_before {
            Some(b) => before = Some(b),
            None => break,
        }
    }
    // 12 turns × (started, user, message, tool, message, ended) + turn 2's
    // request and its call + the last turn's start, prompt and partial reply.
    assert_eq!(all.len(), 12 * 6 + 2 + 3);
    assert_eq!(pages, all.len().div_ceil(7));
    assert!(
        all.windows(2).all(|w| w[0].seq < w[1].seq),
        "each once, in order"
    );
    let input = all
        .iter()
        .find(|i| matches!(i.body, ItemBody::Input { .. }))
        .unwrap();
    let ItemBody::Input { resolution, .. } = &input.body else {
        unreachable!()
    };
    assert_eq!(resolution.as_ref().unwrap().outcome, InputOutcome::Allowed);
    // Turn 2 waited 5 s on its request: not counted in its duration.
    let ended: Vec<_> = all
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::TurnEnded {
                duration_ms,
                waited_ms,
                ..
            } => Some((*duration_ms, *waited_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(ended.len(), 12);
    assert!(ended[1].1 >= 5_000, "turn 2 waited: {:?}", ended[1]);
    assert!(
        ended[1].0.unwrap() < 5_000,
        "and its duration leaves it out: {:?}",
        ended[1]
    );
    assert_eq!(ended[0].1, 0);
}

#[test]
fn a_subagents_own_steps_are_not_items_but_its_start_and_end_are() {
    let (_d, store) = store();
    let mut t = Thread::new(&store, "sub");
    t.start(1, "go");
    t.ev(AgentEventKind::SubagentStarted {
        id: SubagentId::new("a1"),
        parent: None,
        tool_use: None,
        agent_type: None,
        description: "helper".into(),
        prompt: None,
        background: false,
        depth: 1,
        resumed: false,
    });
    t.at(
        1,
        Some("a1"),
        AgentEventKind::Message {
            text: "inside".into(),
        },
        false,
    );
    t.at(
        1,
        Some("a1"),
        AgentEventKind::TextDelta {
            text: "inside".into(),
        },
        false,
    );
    t.at(
        1,
        Some("a1"),
        AgentEventKind::SubagentEnded {
            id: SubagentId::new("a1"),
            status: plexus_harness::SubagentStatus::Completed,
            summary: None,
            usage: Default::default(),
        },
        false,
    );
    let page = store.history(&t.id, None, 50).unwrap();
    assert_eq!(
        kinds(&page.items),
        ["turn.started", "user", "notice", "notice"]
    );
}

#[test]
fn a_runtime_prompt_is_a_notice_and_names_no_thread() {
    let (_d, store) = store();
    let mut t = Thread::new(&store, "rt");
    t.start(1, "[runtime] The service restarted.");
    let long = "word ".repeat(100);
    t.start(2, &format!("  {long}\n\n  more  "));
    t.start(3, "a later prompt");
    let page = store.history(&t.id, None, 50).unwrap();
    assert_eq!(
        kinds(&page.items),
        [
            "turn.started",
            "notice",
            "turn.started",
            "user",
            "turn.started",
            "user"
        ]
    );
    let (title, last): (String, i64) = store
        .with(|c| {
            Ok(c.query_row(
                "SELECT title, last_ms FROM threads WHERE id = 'rt'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(title.chars().count(), TITLE_CHARS, "{title}");
    assert!(title.starts_with("word word") && title.ends_with('…'));
    assert_eq!(last as u64, t.clock, "the newest event's time");
}

#[tokio::test]
async fn a_settled_watch_leaves_out_finished_messages_deltas_only() {
    let (_d, store) = store();
    let t = long_thread(&store, 3, 20);
    let filter = ReadFilter {
        thread: Some(t.id.clone()),
        without_raw: true,
    };
    let cut = store.settled(&t.id).unwrap();
    let mut read = store.read_settled(filter.clone(), GlobalSeq(0), cut);
    let mut got = Vec::new();
    while let Ok(Some(e)) =
        tokio::time::timeout(std::time::Duration::from_millis(300), read.next()).await
    {
        got.push(e);
    }
    let deltas = got
        .iter()
        .filter(|e| matches!(e.event.kind, AgentEventKind::TextDelta { .. }))
        .count();
    assert_eq!(deltas, 5, "only the reply still streaming");
    let natives = got
        .iter()
        .filter(|e| matches!(e.event.kind, AgentEventKind::Native { .. }))
        .count();
    assert_eq!(natives, 1, "only the streaming reply's frame");
    let every = store.page(&filter, GlobalSeq(0), 1_000_000).unwrap();
    let finished = every
        .iter()
        .filter(|e| {
            !matches!(
                e.event.kind,
                AgentEventKind::TextDelta { .. }
                    | AgentEventKind::ThinkingDelta { .. }
                    | AgentEventKind::Native { .. }
            )
        })
        .count();
    assert_eq!(got.len(), finished + 5 + 1, "everything else, as ever");
    // Live events after the cutoff come whole.
    let mut t = t;
    let more = t.ev(AgentEventKind::TextDelta {
        text: "live".into(),
    });
    let next = tokio::time::timeout(std::time::Duration::from_secs(2), read.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.seq, GlobalSeq(more));
}

/// The store as it was before the history: no `kind`, `ref`, `item`, `title`,
/// `last_ms`. Opening it fills them in.
#[test]
fn an_older_store_is_migrated_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, harness TEXT NOT NULL, spec TEXT NOT NULL, cwd TEXT,
                session TEXT, status TEXT NOT NULL, last_turn INTEGER NOT NULL DEFAULT 0,
                started INTEGER NOT NULL DEFAULT 0, parked TEXT, note TEXT, created_ms INTEGER NOT NULL,
                owner TEXT, placement TEXT);
             CREATE TABLE events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, thread TEXT NOT NULL, run TEXT NOT NULL,
                at_ms INTEGER NOT NULL, body TEXT NOT NULL, raw TEXT);
             INSERT INTO threads (id, harness, spec, status, created_ms) VALUES ('old', 'claude', '{}', 'idle', 0);",
        )
        .unwrap();
        let put = |n: u64, at: u64, kind: AgentEventKind| {
            let e = AgentEvent {
                seq: EventSeq(n),
                at_ms: at,
                in_turn: Some(TurnId(1)),
                agent: None,
                kind,
                raw: None,
            };
            c.execute(
                "INSERT INTO events (thread, run, at_ms, body) VALUES ('old', 'r', ?, ?)",
                rusqlite::params![at as i64, serde_json::to_string(&e).unwrap()],
            )
            .unwrap();
        };
        put(
            1,
            10,
            AgentEventKind::TurnStarted {
                turn: TurnId(1),
                origin: TurnOrigin::Prompt,
            },
        );
        put(
            2,
            11,
            AgentEventKind::UserMessage {
                text: "[runtime] hello".into(),
                by: None,
            },
        );
        put(
            3,
            12,
            AgentEventKind::UserMessage {
                text: "the  first\nprompt".into(),
                by: None,
            },
        );
        put(4, 13, AgentEventKind::TextDelta { text: "hi".into() });
        put(5, 14, AgentEventKind::Message { text: "hi".into() });
        put(
            6,
            15,
            AgentEventKind::ToolCall {
                id: ToolUseId::new("c"),
                name: ToolName::new("Bash"),
                input: json!({}),
            },
        );
        put(
            7,
            16,
            AgentEventKind::ToolResult {
                id: ToolUseId::new("c"),
                is_error: true,
                output: json!("no"),
            },
        );
    }
    let store = Store::open(&path).unwrap();
    let page = store.history(&ThreadId::new("old"), None, 50).unwrap();
    assert_eq!(
        kinds(&page.items),
        ["turn.started", "notice", "user", "message", "tool"]
    );
    let ItemBody::Tool { result, .. } = &page.items[4].body else {
        unreachable!()
    };
    assert!(result.as_ref().unwrap().is_error);
    let (title, last): (String, i64) = store
        .with(|c| {
            Ok(c.query_row("SELECT title, last_ms FROM threads", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?)
        })
        .unwrap();
    assert_eq!((title.as_str(), last), ("the first prompt", 16));
    drop(store);
    // Again: nothing to do, and nothing lost.
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .history(&ThreadId::new("old"), None, 50)
            .unwrap()
            .items
            .len(),
        5
    );
}

fn wire_bytes(events: &[AgentsEvent]) -> usize {
    events
        .iter()
        .map(|e| serde_json::to_string(e).unwrap().len())
        .sum()
}

/// What opening a thread of ~2,000 events sends, before (`watch` from the start:
/// every event, each delta one) and after (`history`'s newest page, then `watch
/// {since: last_seq}`, which has nothing to replay), and what a page costs to
/// read in a far longer thread.
#[tokio::test]
async fn opening_a_long_thread() {
    let (_d, store) = store();
    let t = long_thread(&store, 16, 80);
    let n = t.n;
    assert!((1_900..=2_300).contains(&n), "~2,000 events: {n}");
    let filter = ReadFilter {
        thread: Some(t.id.clone()),
        without_raw: true,
    };

    let started = Instant::now();
    let before: Vec<AgentsEvent> = store
        .page(&filter, GlobalSeq(0), usize::MAX >> 1)
        .unwrap()
        .into_iter()
        .map(|e| AgentsEvent::Event { event: Box::new(e) })
        .collect();
    let before_ms = started.elapsed().as_secs_f64() * 1e3;

    let started = Instant::now();
    let page = store.history(&t.id, None, 50).unwrap();
    let tail = store.page(&filter, page.last_seq, 1_000).unwrap();
    let after_ms = started.elapsed().as_secs_f64() * 1e3;
    assert!(tail.is_empty(), "nothing between the page and the tail");
    let items = page.items.len();
    let after = vec![AgentsEvent::History(Box::new(page))];

    let (b, a) = (wire_bytes(&before), wire_bytes(&after));
    println!("opening a thread of {n} events ({} turns):", 16);
    println!(
        "  before: watch from 0  — {} messages, {b} bytes of payload, read in {before_ms:.1} ms",
        before.len()
    );
    println!("  after:  history + tail — 1 message ({items} items), {a} bytes of payload, read in {after_ms:.1} ms");
    println!("  ({}x fewer bytes, {}x fewer messages; each message also carries its own JSON-RPC envelope)", b / a.max(1), before.len());
    assert!(
        a * 5 < b,
        "the page is a fraction of the replay: {a} vs {b}"
    );

    // A reconnect that catches up from the start, settled: no finished deltas.
    let cut = store.settled(&t.id).unwrap();
    let settled = store
        .page_settled(&filter, GlobalSeq(0), usize::MAX >> 1, cut)
        .unwrap();
    let settled: Vec<AgentsEvent> = settled
        .into_iter()
        .map(|e| AgentsEvent::Event { event: Box::new(e) })
        .collect();
    println!(
        "  catch-up from 0, settled: {} messages, {} bytes",
        settled.len(),
        wire_bytes(&settled)
    );

    // A page doesn't cost the thread's length: 40x the events, about the same.
    let (_d2, big) = self::store();
    let bt = long_thread(&big, 600, 80);
    let started = Instant::now();
    let mut before = None;
    for _ in 0..5 {
        let p = big.history(&bt.id, before, 50).unwrap();
        before = p.next_before;
    }
    let per = started.elapsed().as_secs_f64() * 1e3 / 5.0;
    println!("  a thread of {} events: {per:.2} ms a page", bt.n);
    assert!(per < 25.0, "seeks, doesn't scan: {per} ms a page");
}

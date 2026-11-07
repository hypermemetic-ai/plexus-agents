//! A thread's history: what it said and did, finished, one page at a time.
//!
//! The log keeps every event, a streamed reply as each of its deltas. Opening a
//! thread by replaying that is slow for a long one, and slower from a host that
//! dialed in. [`Store::history`] answers a page of *items* instead: a prompt, a
//! finished message (its `message` event, never its deltas), a tool call joined
//! with its result, a request joined with how it resolved, a turn's start and its
//! end with how long it took, and what the runtime said. The newest page first
//! (`before` omitted); each page oldest first; `next_before` the page before it.
//! A reply still streaming on the newest page is one `partial` message: its
//! deltas so far, joined. Then `watch {since: last_seq}` follows from there.
//!
//! It reads by seeking, never by scanning: the items by `idx_events_items`
//! (`item = 1`, a partial index over the events that are items on their own), the
//! results, resolutions and turn starts by `idx_events_ref`. A page costs its
//! items, their joins, and the deltas of a reply still going, whatever the
//! thread's length.

use super::store::{Store, StoreError};
use super::{GlobalSeq, ThreadId};
use plexus_harness::{
    Actor, AgentEvent, AgentEventKind, InputOutcome, InputRequestId, SubagentId, ToolName,
    ToolUseId, TurnId, TurnOrigin, TurnTerminal,
};
use rusqlite::{params, params_from_iter, Connection};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Items in a page when the caller names no limit.
pub const DEFAULT_LIMIT: usize = 50;
/// The most items a page holds, whatever the caller asks.
pub const MAX_LIMIT: usize = 500;

/// One page of a thread's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryPage {
    pub thread: ThreadId,
    /// Oldest first.
    pub items: Vec<HistoryItem>,
    /// The `before` that reads the page older than this one. `None`: this page
    /// reaches the thread's start.
    pub next_before: Option<GlobalSeq>,
    /// The thread's newest event when this was read: `watch {since: last_seq}`
    /// carries on from here, and misses nothing.
    pub last_seq: GlobalSeq,
}

/// One thing in a thread's transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryItem {
    /// Where it sits in the log: its first event's.
    pub seq: GlobalSeq,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_turn: Option<TurnId>,
    /// A subagent's, for its start and end. `None` is the root agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<SubagentId>,
    #[serde(flatten)]
    pub body: ItemBody,
}

/// What an item is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "item", rename_all = "snake_case")]
pub enum ItemBody {
    /// A prompt, as it was sent.
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// An assistant message: finished, or `partial`, its text so far.
    Message {
        text: String,
        partial: bool,
    },
    /// A tool call, and its result once there is one.
    Tool {
        id: ToolUseId,
        name: ToolName,
        input: serde_json::Value,
        result: Option<ToolOutcome>,
    },
    /// A request for input, and how it resolved once it did.
    Input {
        id: InputRequestId,
        resolution: Option<Resolution>,
    },
    TurnStarted {
        turn: TurnId,
        origin: TurnOrigin,
    },
    /// How a turn ended, and how long it took: from its start to its end, less
    /// the time it waited on a request (`waited_ms`). `None` when its start isn't
    /// in the log.
    TurnEnded {
        terminal: TurnTerminal,
        duration_ms: Option<u64>,
        waited_ms: u64,
    },
    /// What the runtime said to the agent (a `[runtime]` prompt), a parked turn,
    /// a helper or background task starting or ending, an outcome lost to a
    /// restart, the process ending: the event itself.
    Notice {
        event: AgentEventKind,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolOutcome {
    pub seq: GlobalSeq,
    pub at_ms: u64,
    pub is_error: bool,
    pub output: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Resolution {
    pub seq: GlobalSeq,
    pub at_ms: u64,
    pub outcome: InputOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<Actor>,
}

type Row = (GlobalSeq, AgentEvent);

fn read_rows(
    c: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<Row>, StoreError> {
    let mut q = c.prepare_cached(sql)?;
    let rows = q
        .query_map(args, |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    // A body this build can't read is left out of the history, not fatal to it.
    Ok(rows
        .into_iter()
        .filter_map(|(seq, body)| {
            let event = serde_json::from_str::<AgentEvent>(&body).ok()?;
            Some((GlobalSeq(seq as u64), event))
        })
        .collect())
}

/// The first result or resolution of each `ref`, by the ref index.
fn joined(
    c: &Connection,
    thread: &ThreadId,
    refs: &[String],
) -> Result<HashMap<String, Row>, StoreError> {
    let mut found = HashMap::new();
    // SQLite's bound-variable limit is far above a page's refs; chunk anyway.
    for chunk in refs.chunks(200) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT seq, body, ref FROM events INDEXED BY idx_events_ref
             WHERE thread = ? AND ref IS NOT NULL AND ref IN ({marks})
             AND kind IN ('tool.result', 'input.resolved', 'turn.started')"
        );
        let mut args: Vec<String> = vec![thread.as_str().to_string()];
        args.extend(chunk.iter().cloned());
        let mut q = c.prepare(&sql)?;
        let rows = q
            .query_map(params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        // By the ref index, so in no order: the first of each ref is its lowest seq.
        let mut rows = rows;
        rows.sort_by_key(|r| r.0);
        for (seq, body, reference) in rows {
            if found.contains_key(&reference) {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<AgentEvent>(&body) {
                found.insert(reference, (GlobalSeq(seq as u64), event));
            }
        }
    }
    Ok(found)
}

fn tool_outcome(row: Option<&Row>) -> Option<ToolOutcome> {
    let (seq, e) = row?;
    let AgentEventKind::ToolResult {
        is_error, output, ..
    } = &e.kind
    else {
        return None;
    };
    Some(ToolOutcome {
        seq: *seq,
        at_ms: e.at_ms,
        is_error: *is_error,
        output: output.clone(),
    })
}

fn resolution(row: Option<&Row>) -> Option<Resolution> {
    let (seq, e) = row?;
    let AgentEventKind::InputResolved { outcome, by, .. } = &e.kind else {
        return None;
    };
    Some(Resolution {
        seq: *seq,
        at_ms: e.at_ms,
        outcome: *outcome,
        by: by.clone(),
    })
}

impl Store {
    /// The page of `thread`'s history before `before` (the newest when `None`):
    /// at most `limit` items, oldest first. See the module docs.
    pub fn history(
        &self,
        thread: &ThreadId,
        before: Option<GlobalSeq>,
        limit: usize,
    ) -> Result<HistoryPage, StoreError> {
        let limit = limit.clamp(1, MAX_LIMIT);
        self.with(|c| {
            let last: i64 = c
                .prepare_cached("SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread = ?")?
                .query_row([thread.as_str()], |r| r.get(0))?;
            let below = before.map_or(i64::MAX, |b| b.0 as i64);
            let mut rows = read_rows(
                c,
                "SELECT seq, body FROM events WHERE thread = ? AND item = 1 AND seq < ?
                 ORDER BY seq DESC LIMIT ?",
                params![thread.as_str(), below, (limit + 1) as i64],
            )?;
            let more = rows.len() > limit;
            rows.truncate(limit);
            rows.reverse();
            let next_before = if more {
                rows.first().map(|r| r.0)
            } else {
                None
            };

            // What each call, request and turn end joins.
            let mut refs = Vec::new();
            for (_, e) in &rows {
                match &e.kind {
                    AgentEventKind::ToolCall { id, .. } => refs.push(format!("tool:{id}")),
                    AgentEventKind::InputRequested { id } => refs.push(format!("input:{id}")),
                    AgentEventKind::TurnTerminal(t) => refs.push(format!("turn:{}", t.turn.0)),
                    _ => {}
                }
            }
            let joins = joined(c, thread, &refs)?;

            let mut items = Vec::with_capacity(rows.len() + 1);
            for (seq, e) in &rows {
                let body = match &e.kind {
                    AgentEventKind::UserMessage { text, by } => {
                        if text.trim_start().starts_with("[runtime]") {
                            ItemBody::Notice {
                                event: e.kind.clone(),
                            }
                        } else {
                            ItemBody::User {
                                text: text.clone(),
                                by: by.clone(),
                            }
                        }
                    }
                    AgentEventKind::Message { text } => ItemBody::Message {
                        text: text.clone(),
                        partial: false,
                    },
                    AgentEventKind::ToolCall { id, name, input } => ItemBody::Tool {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        result: tool_outcome(joins.get(&format!("tool:{id}"))),
                    },
                    AgentEventKind::InputRequested { id } => ItemBody::Input {
                        id: id.clone(),
                        resolution: resolution(joins.get(&format!("input:{id}"))),
                    },
                    AgentEventKind::TurnStarted { turn, origin } => ItemBody::TurnStarted {
                        turn: *turn,
                        origin: *origin,
                    },
                    AgentEventKind::TurnTerminal(t) => {
                        let start = joins.get(&format!("turn:{}", t.turn.0));
                        let waited = match start {
                            Some((from, _)) => waited(c, thread, *from, *seq, e.at_ms)?,
                            None => 0,
                        };
                        ItemBody::TurnEnded {
                            terminal: t.clone(),
                            duration_ms: start.map(|(_, s)| {
                                e.at_ms.saturating_sub(s.at_ms).saturating_sub(waited)
                            }),
                            waited_ms: waited,
                        }
                    }
                    other => ItemBody::Notice {
                        event: other.clone(),
                    },
                };
                items.push(HistoryItem {
                    seq: *seq,
                    at_ms: e.at_ms,
                    in_turn: e.in_turn,
                    agent: e.agent.clone(),
                    body,
                });
            }

            // The newest page: a reply still streaming, as its text so far.
            if before.is_none() {
                let after = rows.last().map_or(0, |r| r.0 .0 as i64);
                let deltas = read_rows(
                    c,
                    "SELECT seq, body FROM events WHERE thread = ? AND seq > ?
                     AND kind = 'text.delta' ORDER BY seq",
                    params![thread.as_str(), after],
                )?;
                let mut partial: Option<HistoryItem> = None;
                for (seq, e) in deltas {
                    let AgentEventKind::TextDelta { text } = &e.kind else {
                        continue;
                    };
                    if e.agent.is_some() {
                        continue;
                    }
                    match &mut partial {
                        Some(HistoryItem {
                            body: ItemBody::Message { text: so_far, .. },
                            ..
                        }) => so_far.push_str(text),
                        _ => {
                            partial = Some(HistoryItem {
                                seq,
                                at_ms: e.at_ms,
                                in_turn: e.in_turn,
                                agent: None,
                                body: ItemBody::Message {
                                    text: text.clone(),
                                    partial: true,
                                },
                            })
                        }
                    }
                }
                items.extend(partial);
            }

            Ok(HistoryPage {
                thread: thread.clone(),
                items,
                next_before,
                last_seq: GlobalSeq(last as u64),
            })
        })
    }
}

/// How long the turn between `from` and `to` waited on its requests: each from
/// its asking to its resolution (or the turn's end, `end_ms`, if it never did).
fn waited(
    c: &Connection,
    thread: &ThreadId,
    from: GlobalSeq,
    to: GlobalSeq,
    end_ms: u64,
) -> Result<u64, StoreError> {
    let asked = read_rows(
        c,
        "SELECT seq, body FROM events WHERE thread = ? AND item = 1 AND seq > ? AND seq < ?
         AND kind = 'input.requested' ORDER BY seq",
        params![thread.as_str(), from.0 as i64, to.0 as i64],
    )?;
    if asked.is_empty() {
        return Ok(0);
    }
    let refs: Vec<String> = asked
        .iter()
        .filter_map(|(_, e)| match &e.kind {
            AgentEventKind::InputRequested { id } => Some(format!("input:{id}")),
            _ => None,
        })
        .collect();
    let joins = joined(c, thread, &refs)?;
    let mut total = 0;
    for ((_, e), r) in asked.iter().zip(&refs) {
        let until = match joins.get(r) {
            Some((seq, done)) if *seq < to => done.at_ms,
            _ => end_ms,
        };
        total += until.saturating_sub(e.at_ms);
    }
    Ok(total)
}

//! "Allow, and don't ask again": Claude's own rule suggestions, applied.
//!
//! A `can_use_tool` request carries `permission_suggestions` (CLI 2.1.316, recorded
//! in `fixtures/recorded/agents-10-2026-11-01/approve-allow`):
//!
//! ```json
//! [{"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "touch x *"}],
//!   "behavior": "allow", "destination": "localSettings"},
//!  {"type": "addDirectories", "directories": ["/w"], "destination": "session"},
//!  {"type": "setMode", "mode": "acceptEdits", "destination": "session"}]
//! ```
//!
//! They stay on the request, whole, in [`plexus_harness::InputRequest::raw`]
//! (`raw.request.permission_suggestions`); [`suggestions`] reads them out. An answer
//! that applies some goes back as the control protocol's `updatedPermissions`, the
//! same objects, beside `behavior: allow`.
//!
//! Every update is applied with `destination: session`: a run is started with
//! `--setting-sources ""`, so a rule written to the operator's settings files would
//! never be read back, and writing them is the operator's business. What makes a rule
//! outlive the process is the thread: [`thread_rules`] become the thread spec's
//! `--allowedTools`/`--disallowedTools`, and a `setMode` becomes its mode, so a resume
//! or an unpark starts with them. `addDirectories` lasts the process.
//!
//! [`plexus_harness::InputAnswer`] has no field for updates, so the answer travels
//! as a plain `Allow` and the updates wait here, under the request's id, for the
//! driver that writes the answer ([`stash`], [`take`]).

use crate::mode::PermissionMode;
use plexus_harness::{InputRequest, InputRequestId};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The suggestions a request came with. Empty for one that has none.
pub fn suggestions(request: &InputRequest) -> Vec<Value> {
    let raw = &request.raw;
    let body = if raw["request"].is_object() {
        &raw["request"]
    } else {
        raw
    };
    body["permission_suggestions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The suggestions at `picks`, each made to last the session and no longer.
pub fn pick(request: &InputRequest, picks: &[u32]) -> Result<Vec<Value>, String> {
    let all = suggestions(request);
    let mut out = Vec::with_capacity(picks.len());
    for &i in picks {
        let mut s = all
            .get(i as usize)
            .cloned()
            .ok_or_else(|| format!("no suggestion {i}: this request has {}", all.len()))?;
        if !s.is_object() || !s["type"].is_string() {
            return Err(format!("suggestion {i} isn't one this service can apply"));
        }
        s["destination"] = Value::from("session");
        out.push(s);
    }
    Ok(out)
}

/// What a set of updates does to the thread: rules for `--allowedTools`, rules for
/// `--disallowedTools`, and a mode.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ThreadRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub mode: Option<PermissionMode>,
}

/// `Bash` with `ruleContent` `touch x *` is the rule `Bash(touch x *)`.
fn rule(r: &Value) -> Option<String> {
    let tool = r["toolName"].as_str().filter(|t| !t.is_empty())?;
    Some(match r["ruleContent"].as_str() {
        Some(content) if !content.is_empty() => format!("{tool}({content})"),
        _ => tool.to_string(),
    })
}

/// See [`ThreadRules`]. A `setMode` naming a mode this service doesn't offer is
/// left to the session.
pub fn thread_rules(updates: &[Value]) -> ThreadRules {
    let mut out = ThreadRules::default();
    for u in updates {
        match u["type"].as_str() {
            Some("addRules") => {
                let rules = u["rules"].as_array().into_iter().flatten().filter_map(rule);
                match u["behavior"].as_str() {
                    Some("allow") => out.allow.extend(rules),
                    Some("deny") => out.deny.extend(rules),
                    _ => {}
                }
            }
            Some("setMode") => {
                if let Some(m) = u["mode"].as_str().and_then(PermissionMode::parse) {
                    out.mode = Some(m);
                }
            }
            _ => {}
        }
    }
    out
}

fn stashed() -> &'static Mutex<HashMap<InputRequestId, Vec<Value>>> {
    static S: OnceLock<Mutex<HashMap<InputRequestId, Vec<Value>>>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

/// Holds `updates` for the live answer to `id`. Request ids name their run, so
/// they're unique in the process.
pub fn stash(id: &InputRequestId, updates: Vec<Value>) {
    let mut g = stashed().lock().expect("permission updates poisoned");
    g.insert(id.clone(), updates);
}

/// The updates for `id`'s answer, once.
pub fn take(id: &InputRequestId) -> Option<Vec<Value>> {
    let mut g = stashed().lock().expect("permission updates poisoned");
    g.remove(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexus_harness::{InputKind, InputState, ToolName, TurnId};
    use serde_json::json;

    fn request(raw: Value) -> InputRequest {
        InputRequest {
            id: InputRequestId::new("run:input:1"),
            turn: TurnId(1),
            agent: None,
            route: Default::default(),
            requested_at_ms: 0,
            kind: InputKind::ToolApproval {
                tool: ToolName::new("Bash"),
                input: json!({}),
                tool_use_id: None,
                description: None,
                needs_person: false,
            },
            state: InputState::Pending,
            raw,
        }
    }

    fn recorded() -> InputRequest {
        request(
            json!({"type": "control_request", "request_id": "x", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash",
            "permission_suggestions": [
                {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "touch allowed-marker *"}],
                 "behavior": "allow", "destination": "localSettings"},
                {"type": "addDirectories", "directories": ["/w"], "destination": "session"},
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}}),
        )
    }

    #[test]
    fn a_pick_lasts_the_session_whatever_was_suggested() {
        let got = pick(&recorded(), &[0]).unwrap();
        assert_eq!(got[0]["destination"], "session");
        assert_eq!(got[0]["rules"][0]["ruleContent"], "touch allowed-marker *");
        assert!(pick(&recorded(), &[3]).is_err());
        assert!(pick(&request(json!({})), &[0]).is_err());
    }

    #[test]
    fn rules_and_a_mode_go_to_the_thread() {
        let got = pick(&recorded(), &[0, 1, 2]).unwrap();
        let rules = thread_rules(&got);
        assert_eq!(
            rules.allow,
            vec!["Bash(touch allowed-marker *)".to_string()]
        );
        assert!(rules.deny.is_empty());
        assert_eq!(rules.mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn a_stash_is_taken_once() {
        let id = InputRequestId::new("stash-test:input:1");
        stash(&id, vec![json!({"type": "setMode"})]);
        assert_eq!(take(&id).unwrap().len(), 1);
        assert!(take(&id).is_none());
    }
}

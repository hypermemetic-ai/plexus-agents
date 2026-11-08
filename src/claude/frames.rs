//! Claude's stream-json frames, read and written. Shapes are from recordings.

use plexus_harness::{
    FailureKind, InputAnswer, InputKind, McpServer, McpTransport, Question, QuestionOption,
    ResolvedSecret, SubagentStatus, SubagentUsage, TokenUsage, ToolName, ToolUseId,
};
use serde_json::{json, Map, Value};

pub fn user(text: &str) -> Value {
    json!({
        "type": "user",
        "session_id": "",
        "parent_tool_use_id": null,
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    })
}

pub fn control_request(id: &str, request: Value) -> Value {
    json!({"type": "control_request", "request_id": id, "request": request})
}

pub fn interrupt(id: &str) -> Value {
    control_request(id, json!({"subtype": "interrupt"}))
}

/// `transport`'s entry with secrets in place. This value goes only to stdin.
pub fn mcp_config(transport: &McpTransport, env: &[(String, ResolvedSecret)]) -> Value {
    let values: Map<String, Value> = env
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.expose().to_string())))
        .collect();
    match transport {
        McpTransport::Stdio { command, args, .. } => {
            json!({"type": "stdio", "command": command, "args": args, "env": values})
        }
        McpTransport::Http { url, .. } => json!({"type": "http", "url": url, "headers": values}),
    }
}

pub fn mcp_set_servers(id: &str, servers: &[(&McpServer, Value)]) -> Value {
    let map: Map<String, Value> = servers
        .iter()
        .map(|(s, cfg)| (s.name.clone(), cfg.clone()))
        .collect();
    control_request(id, json!({"subtype": "mcp_set_servers", "servers": map}))
}

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn questions(input: &Value) -> Option<Vec<Question>> {
    input["questions"]
        .as_array()?
        .iter()
        .map(|q| {
            Some(Question {
                question: opt_str(&q["question"])?,
                header: opt_str(&q["header"]),
                options: q["options"]
                    .as_array()?
                    .iter()
                    .map(|o| {
                        Some(QuestionOption {
                            label: opt_str(&o["label"])?,
                            description: opt_str(&o["description"]),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
                multi_select: q["multiSelect"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

/// What a `control_request` asks. Everything real CLI 2.1.316 sent an unhooked run
/// is `can_use_tool`, with `AskUserQuestion` and `ExitPlanMode` among them.
/// `hook_callback` appears only with hooks registered, which this driver doesn't.
pub fn input_kind(request: &Value) -> InputKind {
    let subtype = request["subtype"].as_str().unwrap_or("");
    if subtype != "can_use_tool" {
        return InputKind::Other {
            name: subtype.to_string(),
        };
    }
    let tool = request["tool_name"].as_str().unwrap_or("");
    let input = request["input"].clone();
    let tool_use_id = request["tool_use_id"].as_str().map(ToolUseId::new);
    if tool == "AskUserQuestion" {
        if let Some(questions) = questions(&input) {
            return InputKind::Question {
                questions,
                tool_use_id,
            };
        }
    }
    InputKind::ToolApproval {
        tool: ToolName::new(tool),
        input,
        tool_use_id,
        description: opt_str(&request["description"]),
        needs_person: request["requires_user_interaction"]
            .as_bool()
            .unwrap_or(false),
    }
}

/// The `control_response` for `answer`. `request` is the CLI's own `request` body.
/// `updates`, on an allow, go as `updatedPermissions`: see [`crate::permissions`].
pub fn control_response(
    native_id: &str,
    request: &Value,
    answer: &InputAnswer,
    updates: Option<Vec<Value>>,
) -> Value {
    let success = |body: Value| {
        json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": native_id, "response": body}})
    };
    let input = &request["input"];
    match answer {
        InputAnswer::Allow { updated_input } => {
            let mut body = json!({
                "behavior": "allow",
                "updatedInput": updated_input.as_ref().unwrap_or(input),
            });
            if let Some(updates) = updates.filter(|u| !u.is_empty()) {
                body["updatedPermissions"] = json!(updates);
            }
            success(body)
        }
        InputAnswer::Deny { message, stop_turn } => {
            let mut body = json!({"behavior": "deny", "message": message});
            if *stop_turn {
                body["interrupt"] = json!(true);
            }
            success(body)
        }
        // The CLI reads the answers out of the tool's input, keyed by question text.
        InputAnswer::Answers { answers } => {
            let mut updated = input.clone();
            updated["answers"] = json!(answers);
            success(json!({"behavior": "allow", "updatedInput": updated}))
        }
        InputAnswer::Refuse { reason } => json!({"type": "control_response", "response": {
            "subtype": "error", "request_id": native_id, "error": reason}}),
    }
}

pub fn usage(result: &Value) -> Option<TokenUsage> {
    let u = result.get("usage")?;
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Some(TokenUsage {
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_read: n("cache_read_input_tokens"),
        cache_write: n("cache_creation_input_tokens"),
        cumulative_cost_usd: result["total_cost_usd"].as_f64(),
    })
}

pub fn subagent_usage(frame: &Value) -> SubagentUsage {
    let u = &frame["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    SubagentUsage {
        total_tokens: n("total_tokens"),
        tool_uses: n("tool_uses"),
        duration_ms: n("duration_ms"),
    }
}

pub fn subagent_status(frame: &Value) -> SubagentStatus {
    match frame["status"].as_str() {
        Some("completed") => SubagentStatus::Completed,
        Some("stopped" | "killed") => SubagentStatus::Stopped,
        Some("failed") => SubagentStatus::Failed,
        _ => SubagentStatus::Unknown,
    }
}

/// What a `result` frame's text says went wrong, where that decides the thread.
pub fn failure_of(result: &Value) -> (FailureKind, String) {
    let errors = match &result["errors"] {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("; "),
        _ => String::new(),
    };
    let text = if errors.is_empty() {
        result["result"].as_str().unwrap_or("").to_string()
    } else {
        errors
    };
    let kind = if text.contains("No conversation found") {
        FailureKind::SessionNotFound
    } else if text.contains("Not logged in") {
        FailureKind::NotLoggedIn
    } else {
        FailureKind::Agent
    };
    (kind, text)
}

/// Text from a `tool_result` block, however it's nested.
pub fn tool_results(frame: &Value) -> Vec<(String, bool, Value)> {
    frame["message"]["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "tool_result")
                .map(|b| {
                    (
                        b["tool_use_id"].as_str().unwrap_or("").to_string(),
                        b["is_error"].as_bool().unwrap_or(false),
                        b["content"].clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deny_with_stop_turn_says_interrupt() {
        let answer = InputAnswer::Deny {
            message: "no".into(),
            stop_turn: true,
        };
        let f = control_response("r1", &json!({}), &answer, None);
        assert_eq!(f["response"]["response"]["interrupt"], true);
        assert_eq!(f["response"]["request_id"], "r1");
    }

    #[test]
    fn allow_without_updates_sends_the_requests_input() {
        let answer = InputAnswer::Allow {
            updated_input: None,
        };
        let f = control_response("r", &json!({"input": {"command": "x"}}), &answer, None);
        assert_eq!(f["response"]["response"]["updatedInput"]["command"], "x");
        assert!(f["response"]["response"]
            .get("updatedPermissions")
            .is_none());
    }

    #[test]
    fn an_allow_that_applies_suggestions_sends_them_as_updated_permissions() {
        let answer = InputAnswer::Allow {
            updated_input: None,
        };
        let rule = json!({"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "x *"}],
                          "behavior": "allow", "destination": "session"});
        let request = json!({"input": {"command": "x"}});
        let f = control_response("r", &request, &answer, Some(vec![rule.clone()]));
        let body = &f["response"]["response"];
        assert_eq!(body["behavior"], "allow");
        assert_eq!(body["updatedPermissions"], json!([rule]));
        // Nothing to apply is no field at all.
        let f = control_response("r", &request, &answer, Some(vec![]));
        assert!(f["response"]["response"]
            .get("updatedPermissions")
            .is_none());
        // A deny never carries them.
        let deny = InputAnswer::Deny {
            message: "no".into(),
            stop_turn: false,
        };
        let f = control_response("r", &request, &deny, Some(vec![rule]));
        assert!(f["response"]["response"]
            .get("updatedPermissions")
            .is_none());
    }

    #[test]
    fn answers_go_inside_updated_input() {
        let answers = [("Which?".to_string(), "Blue".to_string())].into();
        let f = control_response(
            "r",
            &json!({"input": {"questions": [{"question": "Which?"}]}}),
            &InputAnswer::Answers { answers },
            None,
        );
        let updated = &f["response"]["response"]["updatedInput"];
        assert_eq!(updated["answers"]["Which?"], "Blue");
        assert!(updated["questions"].is_array());
    }

    #[test]
    fn a_refusal_is_an_error_response() {
        let f = control_response(
            "r",
            &json!({}),
            &InputAnswer::Refuse {
                reason: "no".into(),
            },
            None,
        );
        assert_eq!(f["response"]["subtype"], "error");
    }

    #[test]
    fn an_unknown_subtype_is_other() {
        let k = input_kind(&json!({"subtype": "hook_callback", "callback_id": "hook_0"}));
        assert_eq!(
            k,
            InputKind::Other {
                name: "hook_callback".into()
            }
        );
    }

    #[test]
    fn ask_user_question_is_a_question() {
        let k = input_kind(&json!({
            "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t",
            "requires_user_interaction": true,
            "input": {"questions": [{"question": "Which?", "header": "H", "multiSelect": false,
                "options": [{"label": "Red", "description": "d"}]}]},
        }));
        let InputKind::Question { questions, .. } = k else {
            panic!("{k:?}")
        };
        assert_eq!(questions[0].options[0].label, "Red");
    }

    #[test]
    fn exit_plan_mode_is_an_approval_that_needs_a_person() {
        let k = input_kind(
            &json!({"subtype": "can_use_tool", "tool_name": "ExitPlanMode",
            "input": {}, "requires_user_interaction": true}),
        );
        assert!(matches!(
            k,
            InputKind::ToolApproval {
                needs_person: true,
                ..
            }
        ));
    }

    #[test]
    fn not_logged_in_is_read_from_the_result_text() {
        let (k, _) = failure_of(&json!({"result": "Not logged in · Please run /login"}));
        assert_eq!(k, FailureKind::NotLoggedIn);
    }
}

//! Parking a Claude run (AGENTS-33): a `PreToolUse` hook returns `defer`, the CLI
//! ends the turn with `stop_reason: tool_deferred` and exits, and `--resume` with no
//! prompt fires the hook again for the same call.
//!
//! The hook is one file written for one run, under the run's state directory, and
//! reaches the CLI through `--settings`. It goes nowhere near the user's settings:
//! `--setting-sources ""` stays on, and the state directory must be outside `cwd`.
//!
//! An answer is a file in `answers/` that the hook takes by `rename`, which is
//! atomic. That rename is what consumes it: a crash mid-tool leaves nothing for a
//! later resume to apply, the hook defers again, and a person decides (AGENTS-33
//! `h_crash_kept` ran a tool twice, `h_crash_oneshot` didn't).
//!
//! A hook's `allow` applies `updatedInput`, so what runs is what was approved, not
//! what the model asked for on the retry.

use plexus_harness::{InputAnswer, InputKind, ParkAnswer, ParkedCall, PlanError, PlanFile};
use serde_json::{json, Value};

/// The tools the hook parks. Any other tool isn't deferred: if it needs approval
/// the CLI asks over stdio, which is the live route and just as safe.
pub const PARKED_TOOLS: &str = "Bash|Edit|Write|NotebookEdit|AskUserQuestion";

/// Runs under `python3 -I`, which reads no site packages or current directory.
///
/// A subagent's call passes through to the permission check: `defer` from inside a
/// subagent isn't how a run parks (AGENTS-37 `park-subagent`). So does a call a
/// preapproved rule names exactly: that rule is the CLI's to apply, and a hook that
/// stays silent changes nothing. Any failure here prints nothing, and that too
/// falls through to the CLI's own permission check.
const GATE: &str = r#"import json, os, re, sys
here = os.path.dirname(os.path.abspath(__file__))
e = json.load(sys.stdin)
tool = e.get("tool_name", "")
tid = e.get("tool_use_id", "")
if e.get("agent_id") or not re.fullmatch(r"[A-Za-z0-9_-]+", tid):
    sys.exit(0)
rules = json.load(open(f"{here}/pass.json"))
command = (e.get("tool_input") or {}).get("command")
if tool in rules or (command is not None and f"{tool}({command})" in rules):
    sys.exit(0)
path = f"{here}/answers/{tid}.json"
try:
    os.rename(path, path + ".used")
except FileNotFoundError:
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "defer"}}))
    sys.exit(0)
print(open(path + ".used").read())
"#;

fn file(state_dir: &str, name: &str, contents: String) -> PlanFile {
    PlanFile {
        path: format!("{state_dir}/{name}"),
        contents,
    }
}

/// What a parking run adds to its plan.
pub fn files(
    state_dir: &str,
    preapproved: &[String],
    permissions: Option<&Value>,
) -> Vec<PlanFile> {
    let command = format!("python3 -I {state_dir}/gate.py");
    let mut settings = json!({"hooks": {"PreToolUse": [{
        "matcher": PARKED_TOOLS,
        "hooks": [{"type": "command", "command": command}],
    }]}});
    if let Some(p) = permissions {
        settings["permissions"] = p.clone();
    }
    vec![
        file(state_dir, "gate.py", GATE.to_string()),
        file(state_dir, "settings.json", settings.to_string()),
        file(state_dir, "pass.json", json!(preapproved).to_string()),
    ]
}

/// What the hook prints for `answer`.
fn hook_output(a: &ParkAnswer) -> Result<Value, PlanError> {
    let call = &a.call;
    let allow = |input: &Value| {
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": input,
        }})
    };
    let deny = |reason: &str| {
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }})
    };
    match (&a.answer, &call.kind) {
        (InputAnswer::Allow { .. }, InputKind::ToolApproval { .. }) => {
            Ok(allow(a.approved_input()))
        }
        (InputAnswer::Answers { answers }, InputKind::Question { .. }) => {
            let mut input = call.input.clone();
            input["answers"] = json!(answers);
            Ok(allow(&input))
        }
        (InputAnswer::Deny { message, stop_turn }, _) => {
            let mut out = deny(message);
            if *stop_turn {
                out["continue"] = json!(false);
                out["stopReason"] = json!(message);
            }
            Ok(out)
        }
        (InputAnswer::Refuse { reason }, _) => Ok(deny(reason)),
        (answer, kind) => Err(PlanError::Conflict {
            what: format!("{:?} doesn't fit {kind:?}", answer.outcome()),
        }),
    }
}

/// One answer file, for the hook to take when its call comes round again.
pub fn answer_file(state_dir: &str, a: &ParkAnswer) -> Result<PlanFile, PlanError> {
    Ok(file(
        state_dir,
        &format!("answers/{}.json", a.call.tool_use_id),
        hook_output(a)?.to_string(),
    ))
}

/// The AGENTS-33 fallback text, verbatim where it can be.
pub fn message(call: &ParkedCall, answer: &InputAnswer, may_have_started: bool) -> String {
    let command = call.input["command"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| call.input.to_string());
    let said = match answer {
        InputAnswer::Allow {
            updated_input: None,
        } => format!("allow. Run the exact same command again now: {command}"),
        InputAnswer::Allow {
            updated_input: Some(edited),
        } => {
            let edited = edited["command"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| edited.to_string());
            format!("allow, with the command edited. Do not run the original. Run exactly this instead: {edited}")
        }
        InputAnswer::Deny { message, .. } => {
            format!("deny. Reason: {message}. Do not run it. Reply quoting the reason.")
        }
        InputAnswer::Refuse { reason } => {
            format!("deny. Reason: {reason}. Do not run it. Reply quoting the reason.")
        }
        InputAnswer::Answers { answers } => {
            let said: Vec<String> = answers
                .iter()
                .map(|(q, a)| format!("the person answered \"{q}\" with: {a}"))
                .collect();
            format!(
                "{}. Do not ask again. Carry on with that answer.",
                said.join("; ")
            )
        }
    };
    let ran = if may_have_started {
        ""
    } else {
        ", and it did not run"
    };
    format!(
        "[runtime] Your {} call {} was interrupted by a restart while waiting for a decision{ran}. The decision has now arrived: {said}",
        call.tool, call.tool_use_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexus_harness::{InputRequestId, ToolName, ToolUseId};

    fn call(input: Value) -> ParkedCall {
        ParkedCall {
            request: InputRequestId::new("r"),
            tool_use_id: ToolUseId::new("toolu_1"),
            tool: ToolName::new("Bash"),
            input: input.clone(),
            kind: InputKind::ToolApproval {
                tool: ToolName::new("Bash"),
                input,
                tool_use_id: None,
                description: None,
                needs_person: false,
            },
            agent: None,
        }
    }

    #[test]
    fn an_allow_prints_the_input_that_was_approved() {
        let a = ParkAnswer {
            call: call(json!({"command": "a"})),
            answer: InputAnswer::Allow {
                updated_input: Some(json!({"command": "b"})),
            },
        };
        let out = hook_output(&a).unwrap();
        assert_eq!(out["hookSpecificOutput"]["updatedInput"]["command"], "b");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn an_answer_of_the_wrong_kind_is_refused() {
        let a = ParkAnswer {
            call: call(json!({})),
            answer: InputAnswer::Answers {
                answers: Default::default(),
            },
        };
        assert!(matches!(hook_output(&a), Err(PlanError::Conflict { .. })));
    }

    #[test]
    fn the_fallback_says_the_tool_may_have_run_when_it_may_have() {
        let c = call(json!({"command": "x"}));
        let allow = InputAnswer::Allow {
            updated_input: None,
        };
        assert!(message(&c, &allow, false).contains("and it did not run"));
        assert!(!message(&c, &allow, true).contains("did not run"));
    }
}

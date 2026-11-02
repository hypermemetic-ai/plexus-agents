//! What a parked Claude run is told when it comes back.
//!
//! The CLI's permission rules decide whether a call needs a person: `allow` and
//! `deny` rules are applied inside the CLI, and only a call it would ask about
//! reaches us, as `can_use_tool`. So a run parks on that request and on nothing
//! else, and no rule is matched here. An earlier version parked from a `PreToolUse`
//! hook, which fires before the rules are read, so it parked on calls the rules
//! would have allowed and on calls they refuse (AGENTS-37's correction).
//!
//! The process is killed, not closed: a request pending when stdin closes fails, and
//! the model carries on and answers as though it were refused. The transcript keeps
//! the call with no result, `--resume` with no prompt doesn't ask again, and that
//! call can't be answered any more. So an answer goes in as a message, and the agent
//! makes the call again under a new id. That retry reaches `can_use_tool` too, and
//! the runtime answers it only by [`plexus_harness::retry_answer`]'s exact match.
//!
//! A batch needs telling what happened. The CLI asks about a message's calls one at a
//! time and writes their results together, so a kill loses the result of a call
//! that ran, and on resume the transcript says every call in that message was
//! interrupted. Left at that, the model did the finished calls again, forever: the
//! real `park-batch` asked a person five times over two calls. So the runtime's own
//! record of them goes in: [`completed`].
//!
//! The message carries the input as JSON. Told only the command, the model rewrote
//! Bash's `description` on the retry, and the match failed. Given the JSON, it kept
//! every field. Nothing depends on that but a person's time: a retry that differs
//! parks again and asks again. Approval text grants nothing.

use plexus_harness::{CompletedCall, InputAnswer, ParkedCall};

/// Most of a finished call's output to repeat.
const OUTPUT_LIMIT: usize = 4000;

const OTHERS: &str = "Any other call from that same message did not run: make it again too.";

/// What finished alongside the parked calls. Says so whatever the transcript says.
pub fn completed(calls: &[CompletedCall]) -> Option<String> {
    if calls.is_empty() {
        return None;
    }
    let said: Vec<String> = calls
        .iter()
        .map(|c| {
            let cut = (0..=OUTPUT_LIMIT.min(c.output.len()))
                .rfind(|i| c.output.is_char_boundary(*i))
                .unwrap_or(0);
            let failed = if c.is_error { " and failed" } else { "" };
            format!(
                "{} with input {} ran{failed}, and returned: {}",
                c.tool,
                c.input,
                &c.output[..cut]
            )
        })
        .collect();
    Some(format!(
        "[runtime] These calls from that message did finish, whatever the record above says. \
         Do not make them again.\n{}",
        said.join("\n")
    ))
}

fn subagent(call: &ParkedCall) -> &'static str {
    if call.agent.is_some() {
        " It was made by a subagent you started, which was stopped with it: start the subagent \
         again, and have it make the call with exactly that input."
    } else {
        ""
    }
}

/// The text that carries `answer` to the model.
pub fn message(call: &ParkedCall, answer: &InputAnswer, may_have_started: bool) -> String {
    let again = |input: &serde_json::Value| {
        format!(
            "Make the call again now, with exactly this input, every field unchanged: {input}.{}",
            subagent(call)
        )
    };
    let said = match answer {
        InputAnswer::Allow {
            updated_input: None,
        } => format!("allow. {}", again(&call.input)),
        InputAnswer::Allow {
            updated_input: Some(edited),
        } => format!(
            "allow, with the input edited. Do not make the original call. {}",
            again(edited)
        ),
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
        "[runtime] Your {} call {} was stopped while waiting for a decision{ran}. \
         The decision has now arrived: {said} {OTHERS}",
        call.tool, call.tool_use_id
    )
}

/// The text for a call with no answer to give: it comes back to a person.
pub fn again(call: &ParkedCall) -> String {
    format!(
        "[runtime] Your {} call {} was stopped, and it isn't known whether it ran. \
         Make the call again now, with exactly this input, every field unchanged, \
         and a person will decide: {}.{} {OTHERS}",
        call.tool,
        call.tool_use_id,
        call.input,
        subagent(call)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexus_harness::{InputKind, InputRequestId, SubagentId, ToolName, ToolUseId};
    use serde_json::{json, Value};

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
    fn an_allow_carries_the_whole_input_as_json() {
        let c = call(json!({"command": "a", "description": "reworded unless it's here"}));
        let text = message(
            &c,
            &InputAnswer::Allow {
                updated_input: None,
            },
            false,
        );
        assert!(text.contains(&c.input.to_string()), "{text}");
    }

    #[test]
    fn an_edit_carries_the_edit_and_not_the_original() {
        let c = call(json!({"command": "a"}));
        let edited = json!({"command": "b"});
        let text = message(
            &c,
            &InputAnswer::Allow {
                updated_input: Some(edited.clone()),
            },
            false,
        );
        assert!(text.contains(&edited.to_string()), "{text}");
        assert!(!text.contains(&c.input.to_string()), "{text}");
    }

    #[test]
    fn the_message_says_the_tool_may_have_run_when_it_may_have() {
        let c = call(json!({"command": "x"}));
        let allow = InputAnswer::Allow {
            updated_input: None,
        };
        assert!(message(&c, &allow, false).contains("and it did not run"));
        assert!(!message(&c, &allow, true).contains("did not run,"));
    }

    #[test]
    fn finished_calls_are_named_with_what_they_returned() {
        assert_eq!(completed(&[]), None);
        let done = CompletedCall {
            tool: ToolName::new("Bash"),
            input: json!({"command": "sh one"}),
            is_error: false,
            output: "é".repeat(OUTPUT_LIMIT),
        };
        let text = completed(std::slice::from_ref(&done)).unwrap();
        assert!(text.contains("Do not make them again"), "{text}");
        assert!(text.contains(&done.input.to_string()), "{text}");
        assert!(text.len() < OUTPUT_LIMIT + 300, "the output is cut");
    }

    #[test]
    fn a_subagents_call_says_the_subagent_went_with_it() {
        let mut c = call(json!({"command": "x"}));
        assert!(!again(&c).contains("subagent"));
        c.agent = Some(SubagentId::new("a"));
        assert!(again(&c).contains("subagent"));
    }
}

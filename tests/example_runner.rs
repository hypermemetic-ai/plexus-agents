//! AGENTS-31: the test harness runs through the example runner, as a real process.

use std::path::PathBuf;
use std::process::Command;

fn runner() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join("examples/agents_run")
}

/// A CLI in shell that speaks the frames `WireHarness` reads, and reports its env.
const CLI: &str = r#"
read line
echo '{"type":"system","subtype":"init","session_id":"s-1"}'
echo "{\"type\":\"probe\",\"names\":\"$(/usr/bin/env | /usr/bin/cut -d= -f1 | /usr/bin/sort | /usr/bin/tr '\n' ' ')\"}"
echo '{"type":"result","subtype":"success","is_error":false}'
read line
"#;

#[test]
fn the_wire_harness_runs_through_the_example_runner() {
    let out = Command::new(runner())
        .args([
            "--harness",
            "wire",
            "--env",
            "HOME",
            "--prompt",
            "hello",
            "--",
        ])
        .args(["/bin/sh", "-c", CLI])
        .env("AGENTS31_RUNNER_LEAK", "x")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    println!("{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
    assert!(text.contains(r#""kind":"session.started""#), "{text}");
    assert!(text.contains(r#""kind":"turn.terminal""#), "{text}");
    assert!(text.contains(r#""status":"completed""#), "{text}");
    assert!(text.contains(r#""kind":"run.ended""#), "{text}");
    assert!(
        text.contains(r#""names":"HOME"#),
        "only named env reaches it: {text}"
    );
    assert!(!text.contains("AGENTS31_RUNNER_LEAK"), "{text}");
}

#[test]
fn a_plan_that_cant_start_exits_with_the_reason() {
    let out = Command::new(runner())
        .args([
            "--harness",
            "wire",
            "--prompt",
            "hello",
            "--",
            "/no/such/program",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("/no/such/program"));
}

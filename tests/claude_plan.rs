//! AGENTS-10: what a plan says, with nothing spawned.

use plexus_agents::{ClaudeHarness, ClaudeOptions};
use plexus_harness::{
    Endpoint, EnvName, EnvValue, Harness, ModelName, Need, PlanError, Resume, RunSpec, SecretRef,
    SessionId, SessionMode, ToolName, ToolPolicy, WireFormat,
};

fn argv_has(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2).any(|w| w[0] == flag && w[1] == value)
}

#[test]
fn the_plan_is_duplex_streaming_on_the_stdio_permission_route() {
    let spec = RunSpec {
        model: Some(ModelName::new("claude-some-model-from-the-future")),
        ..RunSpec::default()
    };
    let p = ClaudeHarness::default().plan(&spec).unwrap();
    assert!(argv_has(&p.argv, "--input-format", "stream-json"));
    assert!(argv_has(&p.argv, "--output-format", "stream-json"));
    assert!(p.argv.contains(&"--include-partial-messages".to_string()));
    assert!(argv_has(&p.argv, "--permission-prompt-tool", "stdio"));
    assert!(
        argv_has(&p.argv, "--setting-sources", ""),
        "operator settings stay out"
    );
    assert!(argv_has(
        &p.argv,
        "--model",
        "claude-some-model-from-the-future"
    ));
    assert!(!p.argv.iter().any(|a| a.to_lowercase().contains("bypass")));
    assert!(!p.argv.iter().any(|a| a == "--dangerously-skip-permissions"));
}

#[test]
fn the_plan_pins_the_cli_and_its_env_is_only_the_specs() {
    let spec = RunSpec::default().with_host_env(["USER"]);
    let p = ClaudeHarness::default().plan(&spec).unwrap();
    assert_eq!(p.env.len(), 1);
    assert!(matches!(
        &p.needs[0],
        Need::Program { name, version: Some(pin) } if name == "claude" && pin.supported == ["2.1.316"]
    ));
}

#[test]
fn sessions_map_to_the_right_flags() {
    let id = SessionId::new("11111111-1111-4111-8111-111111111111");
    let h = ClaudeHarness::default();
    let spec = RunSpec {
        session: SessionMode::Assign { id: id.clone() },
        ..RunSpec::default()
    };
    assert!(argv_has(
        &h.plan(&spec).unwrap().argv,
        "--session-id",
        id.as_str()
    ));
    let resume = h.plan_resume(&RunSpec::default(), &id).unwrap();
    assert!(argv_has(&resume.argv, "--resume", id.as_str()));
    let restart = h.plan_restart(&RunSpec::default(), &id).unwrap();
    assert!(argv_has(&restart.argv, "--session-id", id.as_str()));
}

#[test]
fn tools_and_rules_are_passed_exactly() {
    let spec = RunSpec {
        tools: ToolPolicy {
            enabled: Some(vec![ToolName::new("Bash"), ToolName::new("Read")]),
            preapproved: vec!["Bash(sh slow-tool.sh)".into()],
        },
        ..RunSpec::default()
    };
    let p = ClaudeHarness::default().plan(&spec).unwrap();
    assert!(argv_has(&p.argv, "--tools", "Bash,Read"));
    assert!(argv_has(&p.argv, "--allowedTools", "Bash(sh slow-tool.sh)"));
}

fn endpoint(wire_format: WireFormat) -> Endpoint {
    Endpoint {
        base_url: "https://stub.invalid".into(),
        wire_format,
        model: ModelName::new("stub-model"),
        key: SecretRef::env("STUB_KEY"),
    }
}

#[test]
fn an_endpoint_redirects_by_env_with_the_key_as_a_reference() {
    let spec = RunSpec {
        endpoint: Some(endpoint(WireFormat::Anthropic)),
        ..RunSpec::default()
    };
    let p = ClaudeHarness::default().plan(&spec).unwrap();
    assert!(argv_has(&p.argv, "--model", "stub-model"));
    assert!(matches!(
        p.env.get(&EnvName::new("ANTHROPIC_API_KEY")),
        Some(EnvValue::Secret { .. })
    ));
    assert!(!p.argv.iter().any(|a| a.contains("STUB_KEY")));
}

#[test]
fn an_endpoint_of_another_format_is_refused() {
    let spec = RunSpec {
        endpoint: Some(endpoint(WireFormat::OpenAi)),
        ..RunSpec::default()
    };
    assert!(matches!(
        ClaudeHarness::default().plan(&spec),
        Err(PlanError::Unsupported { .. })
    ));
}

#[test]
fn a_config_dir_is_optional_and_named_when_given() {
    let p = ClaudeHarness::default().plan(&RunSpec::default()).unwrap();
    assert!(!p.env.contains_key(&EnvName::new("CLAUDE_CONFIG_DIR")));
    let h = ClaudeHarness::new(ClaudeOptions {
        config_dir: Some("/x/cfg".into()),
        ..ClaudeOptions::default()
    });
    let p = h.plan(&RunSpec::default()).unwrap();
    assert!(p.env.contains_key(&EnvName::new("CLAUDE_CONFIG_DIR")));
    assert!(p.needs.contains(&Need::Directory {
        path: "/x/cfg".into()
    }));
}

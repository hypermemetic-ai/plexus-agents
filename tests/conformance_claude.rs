//! AGENTS-10: the shared suite, against the Claude harness, replaying real sessions.

use plexus_agents::{ClaudeHarness, SUPPORTED_CLI_VERSIONS};
use plexus_harness::conformance::replay::{fixture_meta, fixtures_root, ExitTrigger, ReplayScript};
use plexus_harness::conformance::wire::agents3_script;
use plexus_harness::conformance::{run_suite, Case, Subject};
use plexus_harness::{Harness, ModelName, RunSpec};
use std::path::Path;
use std::sync::Arc;

struct Claude;

impl Subject for Claude {
    fn harness(&self) -> Arc<dyn Harness> {
        Arc::new(ClaudeHarness::default())
    }
    fn spec(&self) -> RunSpec {
        RunSpec {
            model: Some(ModelName::new("haiku")),
            ..RunSpec::default()
        }
    }
    fn script(&self, case: Case) -> Result<ReplayScript, String> {
        if case == Case::Subagents {
            // Two background subagents, three results for one prompt (AGENTS-34).
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/recorded/agents-34-2026-11-01/s2b-parallel-default");
            let exit = ReplayScript::exit_from_rc(0);
            return ReplayScript::load(&dir, "claude", exit, ExitTrigger::OnInputClose, 0);
        }
        agents3_script(case)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_claude_harness_conforms() {
    let report = run_suite(&Claude).await;
    println!("{report}");
    assert!(report.conforms(), "{report}");
    assert!(report.skipped().is_empty(), "nothing to skip: {report}");
}

#[test]
fn the_fixtures_used_are_from_a_pinned_cli() {
    let meta = fixture_meta(&fixtures_root().join("agents-3-2026-11-01")).unwrap();
    let version = meta.cli_version.split_whitespace().next().unwrap();
    assert!(SUPPORTED_CLI_VERSIONS.contains(&version), "{meta:?}");
}

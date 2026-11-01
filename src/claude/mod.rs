//! The Claude harness: one long-lived `claude -p` per run, duplex stream-json.
//!
//! Behaviour comes from recordings of the real CLI (AGENTS-3 and AGENTS-10). The
//! frame shapes are not a published contract, so versions are pinned.
//!
//! Approvals use `--permission-prompt-tool stdio`, so they arrive as
//! `control_request/can_use_tool` on stdout and no MCP server is involved. This is
//! the route AGENTS-3 case 5 tested. `--setting-sources ""` keeps the operator's
//! settings, and so their permission mode, out of a run.

mod driver;
mod frames;

use futures::future::BoxFuture;
use plexus_harness::{
    Cap, Capabilities, DriveError, Endpoint, EnvName, EnvValue, Harness, LaunchPlan, Need,
    PlanError, Process, Resume, Run, RunSpec, SessionId, SessionMode, VersionPin, WireFormat,
};
use std::sync::Arc;
use std::time::Duration;

/// The CLI versions recordings exist for. Others are refused, by the environment
/// through the plan's pin and again by the driver from `system/init`.
pub const SUPPORTED_CLI_VERSIONS: &[&str] = &["2.1.316"];

/// What's specific to this harness.
#[derive(Debug, Clone)]
pub struct ClaudeOptions {
    /// Program name or path.
    pub program: String,
    /// `CLAUDE_CONFIG_DIR`. Optional, because on macOS a config dir loses the
    /// claude.ai login: Keychain lookup goes through the dir, so even the default
    /// path says `Not logged in` when named. Such a run needs its own credentials.
    pub config_dir: Option<String>,
    /// How long an interrupt has to end the turn before a force stop.
    pub interrupt_timeout: Duration,
    /// How long a graceful stop has before a force stop.
    pub stop_timeout: Duration,
}

impl Default for ClaudeOptions {
    fn default() -> Self {
        Self {
            program: "claude".into(),
            config_dir: None,
            interrupt_timeout: Duration::from_secs(10),
            stop_timeout: Duration::from_secs(10),
        }
    }
}

/// See the module docs.
#[derive(Debug, Clone, Default)]
pub struct ClaudeHarness {
    options: ClaudeOptions,
}

impl ClaudeHarness {
    pub fn new(options: ClaudeOptions) -> Self {
        Self { options }
    }

    fn endpoint_env(
        spec: &RunSpec,
        plan: &mut LaunchPlan,
        endpoint: &Endpoint,
    ) -> Result<(), PlanError> {
        if spec.model.is_some() {
            return Err(PlanError::Conflict {
                what: "model is set both on the spec and on its endpoint".into(),
            });
        }
        if endpoint.wire_format != WireFormat::Anthropic {
            return Err(PlanError::Unsupported {
                what: format!(
                    "claude speaks the Anthropic wire format, not {:?}",
                    endpoint.wire_format
                ),
            });
        }
        plan.env.insert(
            EnvName::new("ANTHROPIC_BASE_URL"),
            EnvValue::literal(endpoint.base_url.clone()),
        );
        plan.env.insert(
            EnvName::new("ANTHROPIC_API_KEY"),
            EnvValue::secret(endpoint.key.clone()),
        );
        Ok(())
    }

    fn build(&self, spec: &RunSpec) -> Result<LaunchPlan, PlanError> {
        let mut argv: Vec<String> = [
            self.options.program.as_str(),
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
            "default",
            "--setting-sources",
            "",
            "--strict-mcp-config",
        ]
        .map(String::from)
        .into();

        let mut plan = LaunchPlan {
            argv: vec![],
            env: spec.env.clone(),
            cwd: spec.cwd.clone(),
            files: vec![],
            needs: vec![Need::Program {
                name: self.options.program.clone(),
                version: Some(VersionPin {
                    version_args: vec!["--version".into()],
                    supported: SUPPORTED_CLI_VERSIONS
                        .iter()
                        .map(|v| v.to_string())
                        .collect(),
                }),
            }],
        };

        let model = match &spec.endpoint {
            Some(endpoint) => {
                Self::endpoint_env(spec, &mut plan, endpoint)?;
                Some(&endpoint.model)
            }
            None => spec.model.as_ref(),
        };
        if let Some(model) = model {
            argv.extend(["--model".to_string(), model.to_string()]);
        }

        match &spec.session {
            SessionMode::Fresh => {}
            SessionMode::Assign { id } => argv.extend(["--session-id".to_string(), id.to_string()]),
            SessionMode::Resume { id } => argv.extend(["--resume".to_string(), id.to_string()]),
        }

        if let Some(tools) = &spec.tools.enabled {
            let list: Vec<&str> = tools.iter().map(|t| t.as_str()).collect();
            argv.extend(["--tools".to_string(), list.join(",")]);
        }
        for rule in &spec.tools.preapproved {
            argv.extend(["--allowedTools".to_string(), rule.clone()]);
        }

        if let Some(dir) = &self.options.config_dir {
            plan.env.insert(
                EnvName::new("CLAUDE_CONFIG_DIR"),
                EnvValue::literal(dir.clone()),
            );
            plan.needs.push(Need::Directory { path: dir.clone() });
        }
        plan.argv = argv;
        Ok(plan)
    }
}

impl Harness for ClaudeHarness {
    fn name(&self) -> &str {
        "claude"
    }

    fn plan(&self, spec: &RunSpec) -> Result<LaunchPlan, PlanError> {
        self.build(spec)
    }

    fn drive(
        &self,
        spec: RunSpec,
        process: Process,
    ) -> BoxFuture<'static, Result<Arc<dyn Run>, DriveError>> {
        Box::pin(driver::drive(self.options.clone(), spec, process))
    }

    fn resume(&self) -> Option<&dyn Resume> {
        Some(self)
    }

    fn capabilities(&self) -> Capabilities {
        let mut c = Capabilities::of_harness(self);
        c.interrupt = Cap::declared(true);
        c.input_queue = Cap::declared(true);
        c
    }
}

impl Resume for ClaudeHarness {
    fn plan_resume(&self, spec: &RunSpec, session: &SessionId) -> Result<LaunchPlan, PlanError> {
        let spec = RunSpec {
            session: SessionMode::Resume {
                id: session.clone(),
            },
            ..spec.clone()
        };
        self.build(&spec)
    }

    fn plan_restart(&self, spec: &RunSpec, session: &SessionId) -> Result<LaunchPlan, PlanError> {
        let spec = RunSpec {
            session: SessionMode::Assign {
                id: session.clone(),
            },
            ..spec.clone()
        };
        self.build(&spec)
    }
}

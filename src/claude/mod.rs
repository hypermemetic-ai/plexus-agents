//! The Claude harness: one long-lived `claude -p` per run, duplex stream-json.
//!
//! Behaviour comes from recordings of the real CLI (AGENTS-3 and AGENTS-10). The
//! frame shapes are not a published contract, so versions are pinned.
//!
//! Approvals use `--permission-prompt-tool stdio`, so they arrive as
//! `control_request/can_use_tool` on stdout and no MCP server is involved. This is
//! the route AGENTS-3 case 5 tested. `--setting-sources ""` keeps the operator's
//! settings, and so their permission mode and rules, out of a run: the rules a run
//! has are the spec's, as flags, and [`ClaudeOptions::permissions`].
//!
//! The permission mode is the thread's ([`crate::mode`]): `default` unless the spec
//! carries another. `bypassPermissions` also passes
//! `--allow-dangerously-skip-permissions`, which the CLI wants before it will run
//! with no checks at all.

mod driver;
mod frames;
mod park;

use futures::future::BoxFuture;
use plexus_harness::{
    Cap, Capabilities, DriveError, Endpoint, EnvName, EnvValue, Harness, LaunchPlan, Need, Park,
    ParkAnswer, ParkedCall, ParkedRun, PlanError, Process, Resume, Run, RunSpec, SessionId,
    SessionMode, Unpark, UserInput, VersionPin, WireFormat,
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
    /// A `permissions` block for this harness's runs, to carry rules that aren't
    /// flags, such as `ask`. Goes inline to `--settings`, so it's no file anywhere.
    pub permissions: Option<serde_json::Value>,
}

impl Default for ClaudeOptions {
    fn default() -> Self {
        Self {
            program: "claude".into(),
            config_dir: None,
            interrupt_timeout: Duration::from_secs(10),
            stop_timeout: Duration::from_secs(10),
            permissions: None,
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
        let mode = crate::mode::mode_of(spec);
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
            mode.as_str(),
            "--setting-sources",
            "",
            "--strict-mcp-config",
        ]
        .map(String::from)
        .into();

        if mode.is_bypass() {
            argv.push("--allow-dangerously-skip-permissions".into());
        }
        // The mode is a flag, never the agent's env.
        let mut env = spec.env.clone();
        env.remove(&EnvName::new(crate::mode::ENV_KEY));
        let mut plan = LaunchPlan {
            argv: vec![],
            env,
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
        for rule in &spec.tools.denied {
            argv.extend(["--disallowedTools".to_string(), rule.clone()]);
        }
        if let Some(permissions) = &self.options.permissions {
            let settings = serde_json::json!({ "permissions": permissions });
            argv.extend(["--settings".to_string(), settings.to_string()]);
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

    fn park(&self) -> Option<&dyn Park> {
        Some(self)
    }

    fn capabilities(&self) -> Capabilities {
        let mut c = Capabilities::of_harness(self);
        c.interrupt = Cap::declared(true);
        c.input_queue = Cap::declared(true);
        c
    }
}

impl Park for ClaudeHarness {
    fn plan_unpark(&self, parked: &ParkedRun, answers: &[ParkAnswer]) -> Result<Unpark, PlanError> {
        if !parked.spec.park {
            return Err(PlanError::Conflict {
                what: "this run was not started to park".into(),
            });
        }
        for a in answers {
            let known = parked
                .calls
                .iter()
                .any(|c| c.tool_use_id == a.call.tool_use_id);
            if !known {
                return Err(PlanError::Conflict {
                    what: format!("{} isn't pending in this run", a.call.tool_use_id),
                });
            }
            if !a.answer.fits(&a.call.kind) {
                return Err(PlanError::Conflict {
                    what: format!("{:?} doesn't fit {:?}", a.answer.outcome(), a.call.kind),
                });
            }
        }
        // Resume ignores where it's launched from, and takes the tool with it, so
        // this is the parked cwd and never a caller's.
        let spec = RunSpec {
            cwd: parked.cwd.clone(),
            session: SessionMode::Resume {
                id: parked.session.clone(),
            },
            continues: Some(parked.turn),
            turns_before: parked.turn.0.saturating_sub(1),
            ..parked.spec.clone()
        };
        let mut plan = self.build(&spec)?;
        if let Some(cwd) = &spec.cwd {
            plan.needs
                .push(Need::ExistingDirectory { path: cwd.clone() });
        }
        let mut said: Vec<String> = if answers.is_empty() {
            parked.calls.iter().map(park::again).collect()
        } else {
            answers
                .iter()
                .map(|a| park::message(&a.call, &a.answer, false))
                .collect()
        };
        said.extend(park::completed(&parked.completed));
        Ok(Unpark {
            spec,
            plan,
            input: UserInput::text(said.join("\n")),
        })
    }

    fn message(
        &self,
        call: &ParkedCall,
        answer: &plexus_harness::InputAnswer,
        may_have_started: bool,
    ) -> UserInput {
        UserInput::text(park::message(call, answer, may_have_started))
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

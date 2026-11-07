//! A thread's permission mode: Claude Code's `--permission-mode`, chosen per thread.
//!
//! `RunSpec` has no field for it (plexus-harness would need one), so the mode rides
//! in the spec's env under [`ENV_KEY`], as a literal. The spec is what a thread
//! persists and what every launch, resume and unpark is planned from, so the mode
//! goes wherever the spec goes. [`crate::ClaudeHarness`] takes it out of the
//! process's env and passes it as the flag: the CLI never sees the variable.
//!
//! `bypassPermissions` is the "dangerously allow" mode: no request ever reaches a
//! person. [`may_bypass`] says who may choose it.

use plexus_harness::{EnvName, EnvValue, RunSpec};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Where the mode lives in a [`RunSpec`]'s env.
pub const ENV_KEY: &str = "PLEXUS_AGENTS_PERMISSION_MODE";

/// Claude Code's permission modes, as `--permission-mode` spells them. CLI 2.1.316
/// lists `manual` where it used to list `default`, and still takes `default`: every
/// recorded run passes it and reports `permissionMode: default`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum PermissionMode {
    /// Asks for anything the rules don't cover.
    #[default]
    #[serde(rename = "default")]
    Default,
    /// File edits need no approval; everything else is as `default`.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// Read-only: the agent plans, and asks to leave plan mode.
    #[serde(rename = "plan")]
    Plan,
    /// Nothing is asked: every tool runs. Only a host's owner may choose it.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl PermissionMode {
    pub const ALL: [Self; 4] = [
        Self::Default,
        Self::AcceptEdits,
        Self::Plan,
        Self::BypassPermissions,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }

    pub fn is_bypass(self) -> bool {
        self == Self::BypassPermissions
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The spec's mode. A spec from before modes, or with no mode set, is `default`.
pub fn mode_of(spec: &RunSpec) -> PermissionMode {
    match spec.env.get(&EnvName::new(ENV_KEY)) {
        Some(EnvValue::Literal { value }) => PermissionMode::parse(value).unwrap_or_default(),
        _ => PermissionMode::Default,
    }
}

/// Sets the spec's mode. `default` leaves no trace, so a spec stays as it was.
pub fn set_mode(spec: &mut RunSpec, mode: PermissionMode) {
    let key = EnvName::new(ENV_KEY);
    if mode == PermissionMode::Default {
        spec.env.remove(&key);
    } else {
        spec.env.insert(key, EnvValue::literal(mode.as_str()));
    }
}

/// [`set_mode`], by value.
pub fn with_mode(mut spec: RunSpec, mode: PermissionMode) -> RunSpec {
    set_mode(&mut spec, mode);
    spec
}

/// Whether `principal` may put a thread in `bypassPermissions`: only the server a
/// host dialed, calling on the host's own hub. That call got there only past the
/// server's check that its caller owns the host (`agents.on.<host>`, `hosts.*`), so
/// on a host this is the host's owner. On the service itself nobody may: it runs
/// other people's threads.
pub fn may_bypass(principal: &str) -> bool {
    principal == crate::host::SERVER_PRINCIPAL
}

/// The refusal when [`may_bypass`] says no.
pub fn bypass_refused() -> String {
    "bypassPermissions is only for a host's owner, on their own host".into()
}

/// The refusal when the owner asks but this host wasn't started with
/// `--allow-bypass`: what to run instead.
pub fn bypass_off() -> String {
    "bypassPermissions is off on this host. To allow it, restart the host with \
     `plexus-agents host ... --allow-bypass` (or AGENTS_ALLOW_BYPASS=1)"
        .into()
}

/// Why `principal` may not put a thread in `bypassPermissions` here, if it may
/// not: it must be the host's owner ([`may_bypass`]) on a host that opted in
/// (`allowed`, `plexus-agents host --allow-bypass`).
pub fn bypass_refusal(principal: &str, allowed: bool) -> Option<String> {
    if !may_bypass(principal) {
        Some(bypass_refused())
    } else if !allowed {
        Some(bypass_off())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_as_the_cli_spells_them() {
        for m in PermissionMode::ALL {
            let json = serde_json::to_value(m).unwrap();
            assert_eq!(json, serde_json::json!(m.as_str()));
            assert_eq!(serde_json::from_value::<PermissionMode>(json).unwrap(), m);
            assert_eq!(PermissionMode::parse(m.as_str()), Some(m));
        }
        assert!(serde_json::from_value::<PermissionMode>("auto".into()).is_err());
    }

    #[test]
    fn a_spec_carries_its_mode_and_default_leaves_no_trace() {
        let spec = RunSpec::default();
        assert_eq!(mode_of(&spec), PermissionMode::Default);
        let spec = with_mode(spec, PermissionMode::Plan);
        assert_eq!(mode_of(&spec), PermissionMode::Plan);
        let back: RunSpec = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(mode_of(&back), PermissionMode::Plan);
        let spec = with_mode(back, PermissionMode::Default);
        assert!(spec.env.is_empty());
    }

    #[test]
    fn only_the_dialed_server_may_bypass() {
        assert!(may_bypass(crate::host::SERVER_PRINCIPAL));
        assert!(!may_bypass("idp:someone"));
        assert!(!may_bypass("key:abcd"));
    }

    #[test]
    fn the_owner_bypasses_only_on_a_host_that_opted_in() {
        let owner = crate::host::SERVER_PRINCIPAL;
        assert_eq!(bypass_refusal(owner, true), None);
        let off = bypass_refusal(owner, false).unwrap();
        assert!(off.contains("--allow-bypass"), "{off}");
        assert_eq!(bypass_refusal("idp:someone", true), Some(bypass_refused()));
    }
}

//! The agents service: [`LocalEnvironment`], [`ClaudeHarness`], the [`runtime`], and
//! the `agents` activation in [`service`] with who may call it in [`auth`]. Contracts
//! live in `plexus-harness`.

pub mod auth;
pub mod claude;
pub mod hosts;
mod local;
pub mod on;
pub mod peer;
pub mod record;
pub mod runner;
pub mod runtime;
pub mod service;

pub use claude::{ClaudeHarness, ClaudeOptions, SUPPORTED_CLI_VERSIONS};
pub use local::LocalEnvironment;
pub use record::RecordingEnvironment;

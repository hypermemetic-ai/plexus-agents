//! Harness implementations: [`LocalEnvironment`], [`ClaudeHarness`], and an example
//! runner's body in [`runner`]. Contracts live in `plexus-harness`.

pub mod claude;
mod local;
pub mod record;
pub mod runner;
pub mod runtime;

pub use claude::{ClaudeHarness, ClaudeOptions, SUPPORTED_CLI_VERSIONS};
pub use local::LocalEnvironment;
pub use record::RecordingEnvironment;

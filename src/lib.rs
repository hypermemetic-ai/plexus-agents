//! Harness implementations: [`LocalEnvironment`], and an example
//! runner's body in [`runner`]. Contracts live in `plexus-harness`.

mod local;
pub mod record;
pub mod runner;

pub use local::LocalEnvironment;
pub use record::RecordingEnvironment;

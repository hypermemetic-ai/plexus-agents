//! The runtime: a run belongs to the service, not to whoever started it.
//!
//! A [`Thread`](ThreadView) sits above runs, because each resume mints a new
//! `RunId`. Every event lands in a durable log, with one global sequence, before
//! anyone reads it. The [`Supervisor`] owns each process and the pending requests,
//! so an answer outlives the process that asked and the service that held it.
//!
//! This is the layer below the `agents` activation (AGENTS-13), which adds no state
//! of its own.

mod store;
mod supervisor;

pub use store::{EventLog, ReadFilter, Store, StoreError, StoredEvent, Tx};
pub use supervisor::{
    Delivery, InputRecord, Recovery, RuntimeError, Supervisor, SupervisorConfig, ThreadHome,
    ThreadOptions, ThreadStatus, ThreadView,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A conversation, across however many processes it takes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ThreadId(pub String);

impl ThreadId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A place in the whole log. This is what `since` means, across runs and threads.
/// `GlobalSeq(0)` is the start.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct GlobalSeq(pub u64);

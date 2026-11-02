//! AGENTS-30's route, compiled on published Plexus 0.7.0: an activation adapter
//! over `Arc<dyn Harness>` streams the contract's types. `service::HarnessActivation`
//! is the real one; this test keeps every contract type usable by it.
//!
//! An activation yields one event type across its methods, so the service has a
//! wire enum like `WireEvent` here. Every contract type fits inside one.

use futures::stream::{self, Stream};
use plexus_agents::ClaudeHarness;
use plexus_core::plexus::Activation;
use plexus_harness::{
    AgentEvent, AgentEventKind, Capabilities, EventSeq, Harness, InputAnswer, InputRequest,
};
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "wire", rename_all = "snake_case")]
enum WireEvent {
    Capabilities { capabilities: Capabilities },
    Agent { event: Box<AgentEvent> },
    Inputs { requests: Vec<InputRequest> },
}

#[derive(Clone)]
struct HarnessActivation {
    h: Arc<dyn Harness>,
    ns: String,
}

impl HarnessActivation {
    fn ns(&self) -> &str {
        &self.ns
    }
}

/// A coding-agent harness, served from its `Harness` impl.
#[plexus_macros::activation(
    trait = "HarnessWire",
    namespace = "harness",
    namespace_fn = "ns",
    version = "1.0.0"
)]
impl HarnessActivation {
    /// What this harness can do.
    #[plexus_macros::method(streaming)]
    pub async fn describe(&self) -> impl Stream<Item = WireEvent> + Send + 'static {
        stream::iter(vec![WireEvent::Capabilities {
            capabilities: self.h.capabilities(),
        }])
    }

    /// One run's events.
    #[plexus_macros::method(streaming)]
    pub async fn events(&self) -> impl Stream<Item = WireEvent> + Send + 'static {
        let event = AgentEvent {
            seq: EventSeq(1),
            at_ms: 0,
            in_turn: None,
            agent: None,
            kind: AgentEventKind::Native {
                name: "shape".into(),
            },
            raw: None,
        };
        stream::iter(vec![WireEvent::Agent {
            event: Box::new(event),
        }])
    }

    /// Answers a request.
    #[plexus_macros::method(streaming)]
    pub async fn answer(
        &self,
        /** the answer */
        answer: InputAnswer,
    ) -> impl Stream<Item = WireEvent> + Send + 'static {
        let _ = answer;
        stream::iter(vec![WireEvent::Inputs { requests: vec![] }])
    }
}

#[test]
fn the_contract_types_fit_the_activation_route() {
    let a = HarnessActivation {
        h: Arc::new(ClaudeHarness::default()),
        ns: "claude".into(),
    };
    assert_eq!(Activation::namespace(&a), "claude");
    let methods = Activation::methods(&a);
    for m in ["describe", "events", "answer"] {
        assert!(methods.contains(&m), "{methods:?}");
    }
}

//! A stand-in peer for `tests/on_wire.rs`: a process a test can kill, or restart with
//! another schema. It serves `agents` on a real socket, signed in with key proof.
//!
//!     on_stub --port N --key-audience URL [--added]

use async_stream::stream;
use futures::Stream;
use plexus_agents::service::{serve, SignIn};
use plexus_core::plexus::DynamicHub;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

static OPEN: AtomicUsize = AtomicUsize::new(0);

struct Open;

impl Open {
    fn new() -> Self {
        OPEN.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::SeqCst);
    }
}

fn ticks() -> impl Stream<Item = u64> + Send + 'static {
    stream! {
        let _open = Open::new();
        for n in 0.. {
            yield n;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn open() -> impl Stream<Item = u64> + Send + 'static {
    futures::stream::iter([OPEN.load(Ordering::SeqCst) as u64])
}

#[derive(Clone)]
struct FirstActivation;

/// The peer as first published.
#[plexus_macros::activation(namespace = "agents", version = "0.1.0", auth = "none")]
impl FirstActivation {
    /// Counts until dropped.
    #[plexus_macros::method(streaming)]
    pub async fn ticks(&self) -> impl Stream<Item = u64> + Send + 'static {
        ticks()
    }

    /// How many `ticks` are open.
    #[plexus_macros::method(streaming)]
    pub async fn open(&self) -> impl Stream<Item = u64> + Send + 'static {
        open()
    }
}

#[derive(Clone)]
struct SecondActivation;

/// The peer after an upgrade.
#[plexus_macros::activation(namespace = "agents", version = "0.2.0", auth = "none")]
impl SecondActivation {
    /// Counts until dropped.
    #[plexus_macros::method(streaming)]
    pub async fn ticks(&self) -> impl Stream<Item = u64> + Send + 'static {
        ticks()
    }

    /// How many `ticks` are open.
    #[plexus_macros::method(streaming)]
    pub async fn open(&self) -> impl Stream<Item = u64> + Send + 'static {
        open()
    }

    /// New in this version.
    #[plexus_macros::method(streaming)]
    pub async fn added(&self) -> impl Stream<Item = u64> + Send + 'static {
        futures::stream::iter([1])
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        let at = args.iter().position(|a| a == flag);
        at.map(|i| args[i + 1].clone())
            .ok_or(anyhow::anyhow!("{flag}"))
    };
    let port: u16 = value("--port")?.parse()?;
    let hub = DynamicHub::new("plexus-agents");
    let hub = if args.iter().any(|a| a == "--added") {
        hub.register(SecondActivation)
    } else {
        hub.register(FirstActivation)
    };
    let sign_in = SignIn {
        tokens: None,
        key_audiences: vec![value("--key-audience")?],
    };
    serve(
        Arc::new(hub),
        sign_in.validator()?,
        [127, 0, 0, 1].into(),
        port,
    )
    .await
}

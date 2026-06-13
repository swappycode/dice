//! dice-event-bus: the service→gateway event fabric.
//!
//! One trait ([`EventBus`]), two backends selected at runtime ([`BusConfig`],
//! per docs/design/workspace-and-protocol.md §6 — no cargo features):
//!
//! - **Local** (dev-lite / monolith): a single in-process
//!   `tokio::sync::broadcast` channel; subscriptions filter by exact subject.
//! - **NATS** (full profile): core pub/sub — at-most-once is correct here
//!   because client gap-recovery is the gateway resume buffer plus REST
//!   history backfill. A capture-only JetStream stream `DICE_EVT` is ensured
//!   at connect time so future durable consumers can attach without a
//!   protocol change; nothing consumes it in M1.
//!
//! Payloads are [`BusEvent`] (`dice.internal.v1`, see docs/protocol.md §9)
//! on both backends, so services cannot tell which bus they are on. Lagged or
//! undecodable events are counted ([`DROPPED_EVENTS`], [`DECODE_FAILURES`])
//! and skipped — they never error or terminate a subscription stream.

mod local;
mod nats;
pub mod rpc;
mod subject;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::task::{Context, Poll};

pub use dice_protocol::internal::v1::BusEvent;

pub use crate::subject::{Subject, SubjectParseError};

/// Events dropped because a local subscriber lagged behind the broadcast
/// buffer. At-most-once loss the resume machinery already tolerates; exported
/// to metrics as `dice_bus_dropped_events_total` (M1: a bare counter).
pub static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Bus payloads that failed `BusEvent` decoding (corrupt or foreign
/// publisher). Counted and skipped, never surfaced as a stream error.
pub static DECODE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Default capacity of the local broadcast channel.
pub const DEFAULT_LOCAL_CAPACITY: usize = 4096;

/// Name of the capture-only JetStream stream ensured at connect time.
pub const JETSTREAM_STREAM: &str = "DICE_EVT";

/// Runtime backend selection (`DICE_BUS=local|nats` is parsed by callers).
#[derive(Debug, Clone)]
pub enum BusConfig {
    /// In-process broadcast bus (dev-lite, monolith, tests).
    Local { capacity: usize },
    /// Core NATS pub/sub, e.g. `nats://127.0.0.1:4222`.
    Nats { url: String },
}

impl Default for BusConfig {
    fn default() -> Self {
        Self::Local {
            capacity: DEFAULT_LOCAL_CAPACITY,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("bus connect failed: {0}")]
    Connect(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("bus publish failed: {0}")]
    Publish(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("bus subscribe failed: {0}")]
    Subscribe(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("bus event encode failed: {0}")]
    Encode(#[from] dice_protocol::prost::EncodeError),
    #[error("bus event decode failed: {0}")]
    Decode(#[from] dice_protocol::prost::DecodeError),
}

/// Connect the configured backend. Returns `Arc<dyn EventBus>` so the
/// monolith and the split services share one wiring shape.
pub async fn connect(cfg: BusConfig) -> Result<Arc<dyn EventBus>, BusError> {
    match cfg {
        BusConfig::Local { capacity } => Ok(Arc::new(local::LocalBus::new(capacity))),
        BusConfig::Nats { url } => Ok(Arc::new(nats::NatsBus::connect(&url).await?)),
    }
}

#[async_trait::async_trait]
pub trait EventBus: Send + Sync {
    /// Publish one event to one subject. Fire-and-forget fan-out:
    /// "no subscribers" is success, not an error.
    async fn publish(&self, subject: Subject, event: BusEvent) -> Result<(), BusError>;

    /// Exact-subject subscription (the gateway's interest map subscribes per
    /// subject; wildcard subscriptions are deliberately not exposed).
    async fn subscribe(&self, subject: Subject) -> Result<BusSubscription, BusError>;
}

/// A live subscription: a `Stream<Item = BusEvent>` (also [`recv`]) that ends
/// (`None`) only when the bus itself shuts down. Lag and decode failures are
/// counted and skipped, never errors.
///
/// [`recv`]: BusSubscription::recv
pub struct BusSubscription {
    inner: SubInner,
}

enum SubInner {
    Local(local::LocalStream),
    Nats(nats::NatsStream),
}

impl BusSubscription {
    pub(crate) fn from_local(stream: local::LocalStream) -> Self {
        Self {
            inner: SubInner::Local(stream),
        }
    }

    pub(crate) fn from_nats(stream: nats::NatsStream) -> Self {
        Self {
            inner: SubInner::Nats(stream),
        }
    }

    /// Next event; `None` once the bus has shut down.
    pub async fn recv(&mut self) -> Option<BusEvent> {
        futures_util::StreamExt::next(self).await
    }
}

impl futures_util::Stream for BusSubscription {
    type Item = BusEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.get_mut().inner {
            SubInner::Local(s) => Pin::new(s).poll_next(cx),
            SubInner::Nats(s) => Pin::new(s).poll_next(cx),
        }
    }
}

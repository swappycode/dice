//! NATS bus: core (non-JetStream) pub/sub of prost-encoded [`BusEvent`]s.
//!
//! At-most-once is correct for gateway fan-out — client gap-recovery is the
//! resume buffer plus REST history backfill, and per-user durable consumers
//! would sink the connection-count memory budget.
//!
//! Connect-time side effect: ensure the capture-only JetStream stream
//! `DICE_EVT` exists (`dice.evt.guild.*.msg` + `dice.evt.dm.*.msg`,
//! limits-based retention, max_age 10 min). It has ZERO consumers in M1;
//! it exists so future durable consumers (notification/search) can attach
//! without a protocol change.

use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use async_nats::jetstream;
use dice_protocol::prost::Message as _;
use futures_util::Stream;

use crate::{
    BusError, BusEvent, BusSubscription, DECODE_FAILURES, EventBus, JETSTREAM_STREAM, Subject,
};

/// Subjects captured by the `DICE_EVT` stream (docs/protocol.md §9).
const JETSTREAM_SUBJECTS: [&str; 2] = ["dice.evt.guild.*.msg", "dice.evt.dm.*.msg"];

pub(crate) struct NatsBus {
    client: async_nats::Client,
}

impl NatsBus {
    pub(crate) async fn connect(url: &str) -> Result<Self, BusError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| BusError::Connect(Box::new(e)))?;

        let js = jetstream::new(client.clone());
        js.get_or_create_stream(jetstream::stream::Config {
            name: JETSTREAM_STREAM.to_owned(),
            subjects: JETSTREAM_SUBJECTS.iter().map(|s| (*s).to_owned()).collect(),
            retention: jetstream::stream::RetentionPolicy::Limits,
            max_age: Duration::from_secs(10 * 60),
            ..Default::default()
        })
        .await
        .map_err(|e| BusError::Connect(Box::new(e)))?;

        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl EventBus for NatsBus {
    async fn publish(&self, subject: Subject, event: BusEvent) -> Result<(), BusError> {
        let payload = event.encode_to_vec();
        self.client
            .publish(subject.to_string(), payload.into())
            .await
            .map_err(|e| BusError::Publish(Box::new(e)))?;
        Ok(())
    }

    async fn subscribe(&self, subject: Subject) -> Result<BusSubscription, BusError> {
        let sub = self
            .client
            .subscribe(subject.to_string())
            .await
            .map_err(|e| BusError::Subscribe(Box::new(e)))?;
        Ok(BusSubscription::from_nats(NatsStream { sub }))
    }
}

pub(crate) struct NatsStream {
    sub: async_nats::Subscriber,
}

impl Stream for NatsStream {
    type Item = BusEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match ready!(Pin::new(&mut this.sub).poll_next(cx)) {
                Some(msg) => match BusEvent::decode(msg.payload) {
                    Ok(event) => return Poll::Ready(Some(event)),
                    // Corrupt/foreign publisher: count, skip, keep the stream alive.
                    Err(_) => {
                        DECODE_FAILURES.fetch_add(1, Ordering::Relaxed);
                    }
                },
                None => return Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use dice_common::id::GuildId;
    use tokio::time::timeout;

    use crate::{BusConfig, BusEvent, Subject, connect};

    /// Needs live infra: run `just infra-up` (NATS with JetStream on
    /// nats://127.0.0.1:4222), then `cargo test -p dice-event-bus -- --ignored`.
    #[tokio::test]
    #[ignore = "needs live NATS — run `just infra-up` first"]
    async fn nats_pub_sub_round_trip() {
        let bus = connect(BusConfig::Nats {
            url: "nats://127.0.0.1:4222".into(),
        })
        .await
        .unwrap();
        let subject = Subject::GuildMsg(GuildId::from_raw(1));
        let mut sub = bus.subscribe(subject).await.unwrap();

        let event = BusEvent {
            event_id: 7,
            origin: "itest".into(),
            ..Default::default()
        };
        bus.publish(subject, event).await.unwrap();

        let got = timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("recv timed out")
            .expect("stream ended");
        assert_eq!(got.event_id, 7);
        assert_eq!(got.origin, "itest");
    }
}

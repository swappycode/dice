//! In-process bus: ONE `tokio::sync::broadcast` channel carrying
//! `(subject, event)`; each subscription filters for its exact subject.
//!
//! On `Lagged(n)` the subscription bumps [`DROPPED_EVENTS`] and keeps
//! receiving — the same at-most-once loss the resume machinery tolerates.
//! Dropped events never error or end the stream.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, ready};

use futures_util::Stream;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::ReusableBoxFuture;

use crate::{BusError, BusEvent, BusSubscription, DROPPED_EVENTS, EventBus, Subject};

/// `Arc`s keep the per-subscriber fan-out clone shallow: only the subscriber
/// whose subject matches deep-clones the event out.
type Item = (Arc<str>, Arc<BusEvent>);

pub(crate) struct LocalBus {
    tx: broadcast::Sender<Item>,
}

impl LocalBus {
    pub(crate) fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }
}

#[async_trait::async_trait]
impl EventBus for LocalBus {
    async fn publish(&self, subject: Subject, event: BusEvent) -> Result<(), BusError> {
        // A send error only means "no subscribers right now" — for pub/sub
        // fan-out that is success, not an error.
        let _ = self
            .tx
            .send((Arc::from(subject.to_string()), Arc::new(event)));
        Ok(())
    }

    async fn subscribe(&self, subject: Subject) -> Result<BusSubscription, BusError> {
        let stream = LocalStream::new(self.tx.subscribe(), Arc::from(subject.to_string()));
        Ok(BusSubscription::from_local(stream))
    }
}

/// `broadcast::Receiver::recv` borrows the receiver, so the reusable future
/// owns it and hands it back with each result (the `BroadcastStream` trick).
async fn recv_owned(
    mut rx: broadcast::Receiver<Item>,
) -> (Result<Item, RecvError>, broadcast::Receiver<Item>) {
    let res = rx.recv().await;
    (res, rx)
}

pub(crate) struct LocalStream {
    subject: Arc<str>,
    fut: ReusableBoxFuture<'static, (Result<Item, RecvError>, broadcast::Receiver<Item>)>,
}

impl LocalStream {
    fn new(rx: broadcast::Receiver<Item>, subject: Arc<str>) -> Self {
        Self {
            subject,
            fut: ReusableBoxFuture::new(recv_owned(rx)),
        }
    }
}

impl Stream for LocalStream {
    type Item = BusEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            let (res, rx) = ready!(this.fut.poll(cx));
            this.fut.set(recv_owned(rx));
            match res {
                Ok((subject, event)) if subject == this.subject => {
                    return Poll::Ready(Some(BusEvent::clone(&event)));
                }
                // Some other subject on the shared channel: filter, keep polling.
                Ok(_) => {}
                // Fell behind the ring buffer: count and continue (at-most-once).
                Err(RecvError::Lagged(n)) => {
                    DROPPED_EVENTS.fetch_add(n, Ordering::Relaxed);
                }
                Err(RecvError::Closed) => return Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use dice_common::id::{ChannelId, GuildId, UserId};
    use tokio::time::timeout;

    use crate::{BusConfig, BusEvent, BusSubscription, DROPPED_EVENTS, Subject, connect};
    use std::sync::atomic::Ordering;

    fn ev(id: u64) -> BusEvent {
        BusEvent {
            event_id: id,
            origin: "test".into(),
            ..Default::default()
        }
    }

    async fn recv(sub: &mut BusSubscription) -> Option<BusEvent> {
        timeout(Duration::from_secs(2), sub.recv())
            .await
            .expect("recv timed out")
    }

    #[tokio::test]
    async fn delivers_only_the_subscribed_subject() {
        let bus = connect(BusConfig::default()).await.unwrap();
        let mut sub = bus
            .subscribe(Subject::GuildMsg(GuildId::from_raw(1)))
            .await
            .unwrap();

        bus.publish(Subject::GuildMsg(GuildId::from_raw(2)), ev(2))
            .await
            .unwrap();
        bus.publish(Subject::User(UserId::from_raw(1)), ev(3))
            .await
            .unwrap();
        bus.publish(Subject::GuildTyping(GuildId::from_raw(1)), ev(4))
            .await
            .unwrap();
        bus.publish(Subject::GuildMsg(GuildId::from_raw(1)), ev(1))
            .await
            .unwrap();
        bus.publish(Subject::GuildMsg(GuildId::from_raw(1)), ev(5))
            .await
            .unwrap();

        // The non-matching publishes are filtered out, in order.
        assert_eq!(recv(&mut sub).await.unwrap().event_id, 1);
        assert_eq!(recv(&mut sub).await.unwrap().event_id, 5);
    }

    #[tokio::test]
    async fn two_subscribers_both_receive() {
        let bus = connect(BusConfig::default()).await.unwrap();
        let subject = Subject::DmMsg(ChannelId::from_raw(3));
        let mut a = bus.subscribe(subject).await.unwrap();
        let mut b = bus.subscribe(subject).await.unwrap();

        bus.publish(subject, ev(11)).await.unwrap();

        assert_eq!(recv(&mut a).await.unwrap().event_id, 11);
        assert_eq!(recv(&mut b).await.unwrap().event_id, 11);
    }

    #[tokio::test]
    async fn lagged_subscriber_drops_but_keeps_receiving() {
        let bus = connect(BusConfig::Local { capacity: 4 }).await.unwrap();
        let subject = Subject::GuildMsg(GuildId::from_raw(9));
        let mut sub = bus.subscribe(subject).await.unwrap();

        let dropped_before = DROPPED_EVENTS.load(Ordering::Relaxed);
        for i in 1..=20 {
            bus.publish(subject, ev(i)).await.unwrap();
        }

        // Oldest events were overwritten; the stream did NOT error.
        let first = recv(&mut sub).await.unwrap();
        assert!(
            first.event_id > 1,
            "expected the head of the batch to be dropped"
        );
        assert!(
            DROPPED_EVENTS.load(Ordering::Relaxed) > dropped_before,
            "lag must be counted"
        );

        // The tail is contiguous through the last published event.
        let mut last = first.event_id;
        while last < 20 {
            let next = recv(&mut sub).await.unwrap();
            assert_eq!(next.event_id, last + 1);
            last = next.event_id;
        }

        // Still alive: later publishes keep arriving.
        bus.publish(subject, ev(21)).await.unwrap();
        assert_eq!(recv(&mut sub).await.unwrap().event_id, 21);
    }

    #[tokio::test]
    async fn stream_ends_only_when_bus_is_dropped() {
        let bus = connect(BusConfig::default()).await.unwrap();
        let subject = Subject::DmPresence(ChannelId::from_raw(5));
        let mut sub = bus.subscribe(subject).await.unwrap();

        bus.publish(subject, ev(1)).await.unwrap();
        drop(bus);

        // Buffered events drain first, then the stream terminates.
        assert_eq!(recv(&mut sub).await.unwrap().event_id, 1);
        assert!(recv(&mut sub).await.is_none());
    }

    #[tokio::test]
    async fn subscription_is_a_futures_stream() {
        use futures_util::StreamExt;

        let bus = connect(BusConfig::default()).await.unwrap();
        let subject = Subject::User(UserId::from_raw(8));
        let mut sub = bus.subscribe(subject).await.unwrap();

        bus.publish(subject, ev(7)).await.unwrap();

        let got = timeout(Duration::from_secs(2), sub.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.event_id, 7);
    }
}

//! Transactional-outbox relay durability test (M4).
//!
//! Proves the reconciliation guarantee: a committed message event whose inline
//! publish was DROPPED (process crash / bus outage) — i.e. an unpublished
//! `event_outbox` row — is delivered to the bus by the relay, exactly once, and
//! the row is then stamped published so it is never re-sent. See
//! docs/adr/0006-transactional-outbox.md.
//!
//! Needs live Postgres (`just infra-up`, DATABASE_URL) + the in-process Local
//! bus. Robust to concurrent `just check` outbox writers: the subscription uses
//! a unique guild subject, so only this test's event can reach it, and the
//! assertions never depend on global table counts.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use dice_common::{GuildId, SnowflakeGenerator};
use dice_event_bus::{BusConfig, BusEvent, Subject};
use dice_protocol::internal::v1::bus_event;
use dice_protocol::prost::Message as _;
use dice_protocol::v1;
use rand::Rng;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::time::timeout;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dice:dice_dev@localhost:5433/dice".to_owned());
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("live Postgres required (just infra-up)")
}

/// A message-create [`BusEvent`] exactly like `ChatService` would record.
fn sample_event(event_id: u64, guild: u64) -> BusEvent {
    let message = v1::Message {
        id: event_id,
        channel_id: 1,
        author_id: 2,
        content: "outbox-reconciled".to_owned(),
        edited_at_ms: 0,
        reply_to_id: 0,
        reactions: Vec::new(),
        attachments: Vec::new(),
    };
    let payload = v1::frame::Payload::MessageCreate(v1::MessageCreate {
        message: Some(message),
        nonce: 0,
    });
    BusEvent {
        event_id,
        emitted_at_ms: 1,
        origin: "outbox-test".to_owned(),
        guild_id: guild,
        recipient_user_ids: Vec::new(),
        ephemeral: false,
        payload: Some(bus_event::Payload::Frame(v1::Frame::dispatch(payload))),
    }
}

#[tokio::test]
async fn relay_reconciles_a_dropped_publish_exactly_once() {
    let pool = pool().await;
    let bus = dice_event_bus::connect(BusConfig::default()).await.unwrap();

    // A unique guild subject so no sibling test's events can reach our sub.
    let ids = SnowflakeGenerator::new(rand::rng().random_range(0..1024u16)).unwrap();
    let guild = ids.generate().0;
    let subject = Subject::GuildMsg(GuildId::from_raw(guild));
    let mut sub = bus.subscribe(subject).await.unwrap();

    // Simulate a COMMITTED message whose inline publish never landed: an
    // unpublished outbox row, exactly as send_message writes it in-tx.
    let event_id = ids.generate().0;
    let event = sample_event(event_id, guild);
    sqlx::query!(
        "INSERT INTO event_outbox (event_id, subject, payload) VALUES ($1, $2, $3)",
        event_id as i64,
        subject.to_string(),
        event.encode_to_vec(),
    )
    .execute(&pool)
    .await
    .unwrap();

    // One relay pass (grace 0 → immediately eligible) republishes it.
    let relayed = chat_service::relay::drain(&pool, &bus, 0.0).await.unwrap();
    assert!(relayed >= 1, "relay should reconcile the dropped event");

    let got = timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("relay must deliver within 2s")
        .expect("bus open");
    assert_eq!(got.event_id, event_id, "reconciled the right event");

    // The row is now stamped published, so a second pass never re-sends it.
    let published = sqlx::query_scalar!(
        "SELECT published_at FROM event_outbox WHERE event_id = $1",
        event_id as i64
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(published.is_some(), "row marked published after relay");

    let _ = chat_service::relay::drain(&pool, &bus, 0.0).await.unwrap();
    assert!(
        timeout(Duration::from_millis(300), sub.recv())
            .await
            .is_err(),
        "no duplicate delivery once published"
    );

    // Best-effort cleanup (matches the live-test convention).
    let _ = sqlx::query!(
        "DELETE FROM event_outbox WHERE event_id = $1",
        event_id as i64
    )
    .execute(&pool)
    .await;
}

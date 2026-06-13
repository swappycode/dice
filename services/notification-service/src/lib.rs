//! notification-service: a DURABLE JetStream consumer on the `DICE_EVT` stream
//! that turns the message firehose into per-user unread counts.
//!
//! `DICE_EVT` captures `dice.evt.{guild,dm}.*.msg` (ensured at bus connect).
//! For every `MessageCreate` we bump an [`UnreadStore`] counter for each
//! channel member except the author; the read-marker path clears it. The
//! gateway serves the counts (`GET /v1/unread`) so badges survive a reconnect.
//!
//! Durability lives in JetStream (full profile only — `dev-lite` has the
//! in-process Local bus with no stream, so the monolith skips this there).
//! The decode→resolve→bump CORE ([`handle_event`]) is bus-agnostic and tested
//! against Postgres + an in-memory cache; [`run`] is the thin JetStream glue.

use async_nats::jetstream::{self, consumer::pull};
use dice_cache::UnreadStore;
use dice_common::id::{ChannelId, UserId};
use dice_common::shutdown::CancellationToken;
use dice_event_bus::{BusEvent, JETSTREAM_STREAM};
use dice_protocol::internal::v1::bus_event::Payload as BusPayload;
use dice_protocol::prost::Message as _;
use dice_protocol::v1::frame::Payload as FramePayload;
use futures_util::StreamExt as _;
use sqlx::PgPool;

/// Durable consumer name on `DICE_EVT`. Stable so progress survives restarts.
const DURABLE: &str = "notifications";
const KIND_DM: i16 = dice_protocol::v1::ChannelKind::Dm as i16;

/// Run the durable consumer until `shutdown` fires. Full profile only — needs
/// the JetStream stream that the NATS bus ensures at connect time.
pub async fn run(
    nats_url: &str,
    pool: PgPool,
    unread: UnreadStore,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let client = async_nats::connect(nats_url).await?;
    let js = jetstream::new(client);
    let stream = js.get_stream(JETSTREAM_STREAM).await?;
    let consumer = stream
        .get_or_create_consumer(
            DURABLE,
            pull::Config {
                durable_name: Some(DURABLE.to_owned()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await?;
    let mut messages = consumer.messages().await?;
    tracing::info!(stream = JETSTREAM_STREAM, "notification-service consuming");
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            next = messages.next() => {
                let Some(next) = next else { break };
                let msg = match next {
                    Ok(m) => m,
                    Err(error) => {
                        tracing::warn!(%error, "jetstream message error");
                        continue;
                    }
                };
                match BusEvent::decode(msg.payload.as_ref()) {
                    Ok(event) => {
                        if let Err(error) = handle_event(&pool, &unread, &event).await {
                            tracing::warn!(%error, "notification handling failed");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "undecodable bus event"),
                }
                // Ack regardless: a poison message must not wedge the consumer
                // (the count is best-effort; clients heal on next mark-read).
                if let Err(error) = msg.ack().await {
                    tracing::warn!(%error, "jetstream ack failed");
                }
            }
        }
    }
    tracing::info!("notification-service stopped");
    Ok(())
}

/// Process one decoded bus event: a `MessageCreate` bumps the unread counter
/// for every channel member except the author. All other payloads are ignored.
/// Bus-agnostic and unit-tested (no NATS required).
pub async fn handle_event(
    pool: &PgPool,
    unread: &UnreadStore,
    event: &BusEvent,
) -> Result<(), sqlx::Error> {
    let Some(BusPayload::Frame(frame)) = &event.payload else {
        return Ok(());
    };
    let Some(FramePayload::MessageCreate(mc)) = &frame.payload else {
        return Ok(());
    };
    let Some(message) = &mc.message else {
        return Ok(());
    };
    let channel = ChannelId::from_raw(message.channel_id);
    let author = message.author_id;
    for uid in recipients(pool, channel).await? {
        if uid == author {
            continue;
        }
        // A cache hiccup must not drop the whole batch — log and continue.
        if let Err(error) = unread.bump(UserId::from_raw(uid), channel).await {
            tracing::warn!(%error, user = uid, "unread bump failed");
        }
    }
    Ok(())
}

/// Everyone who should see a message in `channel` (guild members or DM
/// recipients), as raw user ids. Empty for an unknown channel.
async fn recipients(pool: &PgPool, channel: ChannelId) -> Result<Vec<u64>, sqlx::Error> {
    let Some(row) = sqlx::query!(
        "SELECT channel_type, guild_id FROM channels WHERE id = $1",
        channel.as_i64()
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(Vec::new());
    };
    let ids: Vec<i64> = if row.channel_type == KIND_DM {
        sqlx::query_scalar!(
            "SELECT user_id FROM channel_recipients WHERE channel_id = $1",
            channel.as_i64()
        )
        .fetch_all(pool)
        .await?
    } else if let Some(guild_id) = row.guild_id {
        sqlx::query_scalar!(
            "SELECT user_id FROM guild_members WHERE guild_id = $1",
            guild_id
        )
        .fetch_all(pool)
        .await?
    } else {
        Vec::new()
    };
    Ok(ids.into_iter().map(|v| v as u64).collect())
}

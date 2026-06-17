//! Transactional-outbox relay (M4).
//!
//! The message path records every dispatch [`BusEvent`] in `event_outbox` inside
//! the write transaction and publishes it inline, stamping `published_at` on
//! success (see [`ChatService`](crate::ChatService)). This relay is the
//! reconciliation backstop: it republishes any row whose inline publish never
//! landed — a process crash between commit and publish, or a bus outage. Delivery
//! is therefore at-least-once; consumers/clients dedup by `event_id` / message id.
//!
//! It is OFF the live path (the inline publish carries the happy path), so a
//! simple poll is enough — no `LISTEN/NOTIFY`. A `FOR UPDATE SKIP LOCKED` claim
//! makes concurrent relay workers safe (they never publish the same row twice),
//! though M4 assumes exactly one relay per Postgres cluster. A grace window keeps
//! the relay from racing the inline publish on fresh rows; a slow sweep reclaims
//! published rows.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use dice_event_bus::{BusEvent, EventBus, Subject};
use dice_protocol::prost::Message as _;
use sqlx::PgPool;

/// How often the relay scans for unpublished rows. The inline publish owns live
/// latency, so this only bounds crash/outage recovery time.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Don't relay a row until it is at least this old — the inline publish gets
/// first crack at stamping `published_at`, so the happy path never double-sends.
const GRACE_SECS: f64 = 10.0;

/// Max rows reclaimed per transaction (bounds the lock-hold + publish batch).
const BATCH: i64 = 256;

/// Run the retention sweep every N poll ticks (≈1 h at a 2 s poll).
const SWEEP_EVERY_TICKS: u32 = 1800;

/// Delete published rows older than this so the table can't grow unbounded.
const RETENTION: &str = "24 hours";

/// Run the relay until the runtime stops (the caller wraps this in its drain
/// `select!`; the chat-service bin spawns it detached). Loops: drain, then sweep
/// on a slow cadence. The first interval tick fires immediately, so a relay that
/// starts after a crash drains the backlog right away.
pub async fn run(pool: PgPool, bus: Arc<dyn EventBus>) {
    tracing::info!(poll = ?POLL_INTERVAL, grace_s = GRACE_SECS, "outbox relay started");
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ticks: u32 = 0;
    loop {
        poll.tick().await;
        match drain(&pool, &bus, GRACE_SECS).await {
            Ok(0) => {}
            Ok(n) => {
                dice_metrics::counter!("dice_outbox_relayed_total").increment(n as u64);
                tracing::debug!(relayed = n, "outbox relay reconciled events");
            }
            Err(error) => tracing::warn!(%error, "outbox relay drain failed; will retry"),
        }
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(SWEEP_EVERY_TICKS)
            && let Err(error) = sweep(&pool).await
        {
            tracing::warn!(%error, "outbox retention sweep failed");
        }
    }
}

/// One drain pass: reclaim and republish every unpublished row older than
/// `grace_secs`, oldest first, in `BATCH`-sized transactions. Returns the number
/// of rows marked published. Public so the durability test can drive a single
/// pass (with `grace_secs = 0.0`) deterministically.
pub async fn drain(
    pool: &PgPool,
    bus: &Arc<dyn EventBus>,
    grace_secs: f64,
) -> Result<usize, sqlx::Error> {
    let mut relayed = 0usize;
    loop {
        let mut tx = pool.begin().await?;
        // Claim a batch of due rows; SKIP LOCKED lets parallel workers coexist.
        let rows = sqlx::query!(
            "SELECT event_id, subject, payload FROM event_outbox \
             WHERE published_at IS NULL AND created_at <= now() - make_interval(secs => $1) \
             ORDER BY created_at, event_id \
             FOR UPDATE SKIP LOCKED \
             LIMIT $2",
            grace_secs,
            BATCH
        )
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            break;
        }
        let batch_len = rows.len();

        // Publish each; only stamp the rows that delivered (or are unrecoverable
        // poison). A transient publish failure leaves the row for the next pass.
        let mut done: Vec<i64> = Vec::with_capacity(batch_len);
        for row in rows {
            match publish_row(bus, &row.subject, &row.payload).await {
                Outcome::Delivered | Outcome::Poison => done.push(row.event_id),
                Outcome::Retry => {}
            }
        }
        if !done.is_empty() {
            relayed += done.len();
            sqlx::query!(
                "UPDATE event_outbox SET published_at = now() WHERE event_id = ANY($1)",
                &done
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        // A short batch means we've caught up; stop until the next tick.
        if (batch_len as i64) < BATCH {
            break;
        }
    }
    Ok(relayed)
}

/// Outcome of relaying one row.
enum Outcome {
    /// Published to the bus — stamp it.
    Delivered,
    /// Undecodable / bad subject — can never be delivered; stamp it so it can't
    /// wedge the loop (counted as a decode failure).
    Poison,
    /// Transient bus error — leave unpublished for the next pass.
    Retry,
}

async fn publish_row(bus: &Arc<dyn EventBus>, subject: &str, payload: &[u8]) -> Outcome {
    let event = match BusEvent::decode(payload) {
        Ok(event) => event,
        Err(error) => {
            tracing::error!(%error, "outbox relay: undecodable payload; skipping");
            dice_event_bus::DECODE_FAILURES.fetch_add(1, Ordering::Relaxed);
            return Outcome::Poison;
        }
    };
    let subject = match subject.parse::<Subject>() {
        Ok(subject) => subject,
        Err(error) => {
            tracing::error!(%error, "outbox relay: unparseable subject; skipping");
            dice_event_bus::DECODE_FAILURES.fetch_add(1, Ordering::Relaxed);
            return Outcome::Poison;
        }
    };
    match bus.publish(subject, event).await {
        Ok(()) => Outcome::Delivered,
        Err(error) => {
            tracing::warn!(%error, %subject, "outbox relay: publish failed; will retry");
            Outcome::Retry
        }
    }
}

/// Reclaim published rows older than [`RETENTION`]; returns the number deleted.
async fn sweep(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query(&format!(
        "DELETE FROM event_outbox \
         WHERE published_at IS NOT NULL AND published_at < now() - interval '{RETENTION}'"
    ))
    .execute(pool)
    .await?
    .rows_affected();
    if deleted > 0 {
        tracing::info!(deleted, "outbox retention sweep");
    }
    Ok(deleted)
}

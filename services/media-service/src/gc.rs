//! Orphaned-media garbage collection (M4 follow-up): a poll-based sweep that
//! reaps `media` blobs no message or avatar references any more — the residue of
//! deleted messages/attachments, replaced avatars, and uploads whose send never
//! landed. Same shape as the transactional-outbox relay (`chat-service::relay`):
//! a slow interval loop claiming a bounded batch `FOR UPDATE SKIP LOCKED` so
//! concurrent sweepers (multi-node) coexist safely.
//!
//! A media row is **referenced** iff it is in `message_attachments` OR is some
//! user's `avatar_media_id`. The grace window keeps the sweep off freshly
//! uploaded-but-not-yet-attached media (upload writes the row, the send/avatar
//! update links it a beat later). Deletion order is **blob then row**, both
//! idempotent, so a crash mid-sweep self-heals: a surviving row is re-found next
//! pass and `MediaStore::delete` is a no-op on the already-gone blob.

use std::sync::Arc;
use std::time::Duration;

use dice_database::PgPool;

use crate::MediaStore;

/// How often the sweep runs. Reclaiming disk is not latency-sensitive (unlike
/// the outbox's live-delivery backstop), so this is minutes, not seconds.
const POLL_INTERVAL: Duration = Duration::from_secs(300);

/// Don't reap media younger than this — the upload→attach (or →avatar) link
/// lands seconds after the row, so an hour's grace can never race it.
const GRACE_SECS: f64 = 3600.0;

/// Max rows reaped per transaction (bounds the lock hold + the blob-delete batch).
const BATCH: i64 = 256;

/// Run the media GC sweep forever (spawned by the monolith; media is never
/// split, so it always runs in-process there).
pub async fn run(pool: PgPool, store: Arc<dyn MediaStore>) {
    tracing::info!(poll = ?POLL_INTERVAL, grace_s = GRACE_SECS, "media GC sweep started");
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        poll.tick().await;
        match sweep(&pool, &store, GRACE_SECS).await {
            Ok(0) => {}
            Ok(n) => {
                dice_metrics::counter!("dice_media_gc_reaped_total").increment(n as u64);
                tracing::debug!(reaped = n, "media GC reaped orphaned blobs");
            }
            Err(error) => tracing::warn!(%error, "media GC sweep failed; will retry"),
        }
    }
}

/// One full sweep: reap every unreferenced media row older than `grace_secs`,
/// deleting its blob then its row. Returns the number reaped. Drains in bounded
/// batches so one pass clears a large backlog without an unbounded lock hold.
pub async fn sweep(
    pool: &PgPool,
    store: &Arc<dyn MediaStore>,
    grace_secs: f64,
) -> Result<usize, sqlx::Error> {
    let mut reaped = 0usize;
    loop {
        let mut tx = pool.begin().await?;
        // Claim a batch of orphans (in neither junction); SKIP LOCKED lets a
        // sibling sweeper take a different batch instead of blocking.
        let rows = sqlx::query!(
            "SELECT id FROM media m \
             WHERE NOT EXISTS (SELECT 1 FROM message_attachments ma WHERE ma.media_id = m.id) \
               AND NOT EXISTS (SELECT 1 FROM users u WHERE u.avatar_media_id = m.id) \
               AND m.created_at < now() - make_interval(secs => $1) \
             ORDER BY m.created_at, m.id \
             FOR UPDATE SKIP LOCKED \
             LIMIT $2",
            grace_secs,
            BATCH,
        )
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            break;
        }
        let batch_len = rows.len();

        // Delete the blob first; only the rows whose blob is gone get their row
        // removed. A transient store error leaves the row for the next pass.
        let mut deleted: Vec<i64> = Vec::with_capacity(batch_len);
        for row in rows {
            match store.delete(&row.id.to_string()).await {
                Ok(()) => deleted.push(row.id),
                Err(error) => {
                    tracing::warn!(%error, media_id = row.id, "media GC: blob delete failed");
                }
            }
        }
        if !deleted.is_empty() {
            reaped += deleted.len();
            sqlx::query!("DELETE FROM media WHERE id = ANY($1)", &deleted)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;

        // A short batch means the backlog is cleared; wait for the next tick.
        if (batch_len as i64) < BATCH {
            break;
        }
    }
    Ok(reaped)
}

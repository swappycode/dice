//! Durable cross-node resume (ADR-0007 phase 1/2b): a detached session's
//! identity + next seq + replay ring, serialized into the shared [`Cache`] so a
//! *different* gateway node can re-host the session after the origin node is
//! gone. The in-memory ring (`resume.rs`) is the live, hot-path store; this is
//! the cold snapshot taken on detach + the single-winner claim that fences
//! re-host so two nodes never host the same session.
//!
//! Encoding is a compact, hand-rolled, length-prefixed binary blob (no schema
//! churn): `user | auth_session | next_seq | resume_token[32] | frame_count |
//! (len ‖ Frame bytes)*`, all integers little-endian. Decoding is total — a
//! malformed blob reads as `None` and the resume degrades to a fresh Identify.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dice_cache::{Cache, CacheError, keys};
use dice_protocol::v1::Frame;
use prost::Message as _;

use crate::resume::{MAX_BUFFERED_FRAMES, RESUME_TOKEN_LEN};

/// A detached session's durable resume state, enough for any node to validate a
/// `Resume` and re-host the session with full replay + seq continuity.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResumeSnapshot {
    pub(crate) user: u64,
    pub(crate) auth_session: u64,
    pub(crate) resume_token: [u8; RESUME_TOKEN_LEN],
    /// Next seq the session would assign — re-host continues from here.
    pub(crate) next_seq: u64,
    /// Highest seq no longer in the ring (acked/evicted); a `Resume` from a
    /// `last_seq` below this cannot be healed (mirrors `LocalReplayBuffer`).
    pub(crate) trimmed_to: u64,
    /// The replay ring (sequenced frames still buffered), in seq order.
    pub(crate) frames: Vec<Frame>,
}

impl ResumeSnapshot {
    /// Serialize to the on-cache blob (see the module docs for the layout).
    fn encode(&self) -> Bytes {
        let mut buf = Vec::with_capacity(68 + self.frames.len() * 64);
        buf.extend_from_slice(&self.user.to_le_bytes());
        buf.extend_from_slice(&self.auth_session.to_le_bytes());
        buf.extend_from_slice(&self.next_seq.to_le_bytes());
        buf.extend_from_slice(&self.trimmed_to.to_le_bytes());
        buf.extend_from_slice(&self.resume_token);
        buf.extend_from_slice(&(self.frames.len() as u32).to_le_bytes());
        for frame in &self.frames {
            let bytes = frame.encode_to_vec();
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&bytes);
        }
        Bytes::from(buf)
    }

    /// Parse a blob written by [`Self::encode`]. Total: any truncation, bad
    /// length prefix, or undecodable frame yields `None`.
    fn decode(bytes: &[u8]) -> Option<Self> {
        fn take<'a>(cur: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            if cur.len() < n {
                return None;
            }
            let (head, tail) = cur.split_at(n);
            *cur = tail;
            Some(head)
        }
        let mut cur = bytes;
        let user = u64::from_le_bytes(take(&mut cur, 8)?.try_into().ok()?);
        let auth_session = u64::from_le_bytes(take(&mut cur, 8)?.try_into().ok()?);
        let next_seq = u64::from_le_bytes(take(&mut cur, 8)?.try_into().ok()?);
        let trimmed_to = u64::from_le_bytes(take(&mut cur, 8)?.try_into().ok()?);
        let resume_token: [u8; RESUME_TOKEN_LEN] =
            take(&mut cur, RESUME_TOKEN_LEN)?.try_into().ok()?;
        // Cap BOTH the allocation and the loop at the ring bound (protocol §5):
        // a valid snapshot never exceeds it, and a hostile count can't drive an
        // unbounded loop / memory spike (the surplus is simply ignored).
        let count = (u32::from_le_bytes(take(&mut cur, 4)?.try_into().ok()?) as usize)
            .min(MAX_BUFFERED_FRAMES);
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u32::from_le_bytes(take(&mut cur, 4)?.try_into().ok()?) as usize;
            let frame_bytes = take(&mut cur, len)?;
            frames.push(Frame::decode(frame_bytes).ok()?);
        }
        Some(Self {
            user,
            auth_session,
            resume_token,
            next_seq,
            trimmed_to,
            frames,
        })
    }
}

/// The durable resume store over the shared [`Cache`] (ADR-0007 phase 2b).
/// Genuine cross-node only with the Redis backend; the in-memory backend is
/// per-process (so a single-node deployment never re-hosts, harmlessly).
#[derive(Clone)]
pub(crate) struct DurableResume {
    cache: Arc<dyn Cache>,
}

impl DurableResume {
    pub(crate) fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache }
    }

    /// Persist (or refresh) the snapshot, expiring after `ttl` (the resume
    /// window) so it lives exactly as long as the session is resumable.
    pub(crate) async fn save(
        &self,
        session_id: u64,
        snapshot: &ResumeSnapshot,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        self.cache
            .set(
                &keys::resume_snapshot(session_id),
                snapshot.encode(),
                Some(ttl),
            )
            .await
    }

    /// Load the snapshot for `session_id`, if one is still within its window.
    pub(crate) async fn load(&self, session_id: u64) -> Result<Option<ResumeSnapshot>, CacheError> {
        Ok(self
            .cache
            .get(&keys::resume_snapshot(session_id))
            .await?
            .and_then(|b| ResumeSnapshot::decode(b.as_ref())))
    }

    /// Drop the snapshot + its claim — the session resumed (anywhere), was torn
    /// down, or the window expired. Idempotent.
    pub(crate) async fn clear(&self, session_id: u64) -> Result<(), CacheError> {
        self.cache
            .delete(&keys::resume_snapshot(session_id))
            .await?;
        self.cache.delete(&keys::resume_claim(session_id)).await
    }

    /// Release ONLY the takeover claim, leaving the snapshot intact — a re-host
    /// that won the claim then aborted (presence/router setup failed, or the
    /// snapshot vanished under it) frees the fence so another node can re-host
    /// the still-valid snapshot, instead of blocking it for the claim's TTL.
    pub(crate) async fn release_claim(&self, session_id: u64) -> Result<(), CacheError> {
        self.cache.delete(&keys::resume_claim(session_id)).await
    }

    /// Atomically claim the right to re-host `session_id`. Returns `true` for
    /// EXACTLY ONE caller — the one that reads back `1` from the fixed-window
    /// claim counter — so racing reconnects on different nodes can't both
    /// re-host (the seq-monotonicity fence). The claim expires after `ttl`.
    pub(crate) async fn try_claim(
        &self,
        session_id: u64,
        ttl: Duration,
    ) -> Result<bool, CacheError> {
        Ok(self
            .cache
            .incr_expire(&keys::resume_claim(session_id), ttl)
            .await?
            == 1)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use dice_cache::{CacheConfig, connect};
    use dice_protocol::v1::{self, frame::Payload};

    fn msg_frame(seq: u64) -> Frame {
        Frame {
            seq,
            nonce: 0,
            payload: Some(Payload::MessageCreate(v1::MessageCreate {
                message: Some(v1::Message {
                    id: seq,
                    channel_id: 7,
                    author_id: 9,
                    content: format!("frame {seq}"),
                    ..Default::default()
                }),
                nonce: 0,
            })),
        }
    }

    fn sample() -> ResumeSnapshot {
        ResumeSnapshot {
            user: 4242,
            auth_session: 99,
            resume_token: [7u8; RESUME_TOKEN_LEN],
            next_seq: 11,
            trimmed_to: 4,
            frames: (5..=10).map(msg_frame).collect(),
        }
    }

    #[test]
    fn snapshot_round_trips_through_the_blob() {
        let snap = sample();
        let decoded = ResumeSnapshot::decode(snap.encode().as_ref()).unwrap();
        assert_eq!(
            decoded, snap,
            "identity, next_seq, token and frames survive"
        );
    }

    #[test]
    fn empty_ring_round_trips() {
        let snap = ResumeSnapshot {
            frames: Vec::new(),
            ..sample()
        };
        assert_eq!(
            ResumeSnapshot::decode(snap.encode().as_ref()).unwrap(),
            snap
        );
    }

    #[test]
    fn truncated_and_garbage_blobs_decode_to_none() {
        let blob = sample().encode();
        assert!(ResumeSnapshot::decode(&blob[..blob.len() - 1]).is_none());
        assert!(ResumeSnapshot::decode(b"").is_none());
        assert!(ResumeSnapshot::decode(b"not a snapshot").is_none());
    }

    #[tokio::test]
    async fn save_load_clear_round_trip() {
        let store = DurableResume::new(connect(CacheConfig::Memory).await.unwrap());
        let sid = 123u64;
        assert!(store.load(sid).await.unwrap().is_none(), "fresh = none");
        store
            .save(sid, &sample(), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(store.load(sid).await.unwrap().unwrap(), sample());
        store.clear(sid).await.unwrap();
        assert!(store.load(sid).await.unwrap().is_none(), "cleared");
    }

    #[tokio::test]
    async fn try_claim_admits_exactly_one_winner() {
        let store = DurableResume::new(connect(CacheConfig::Memory).await.unwrap());
        let sid = 777u64;
        let ttl = Duration::from_secs(60);
        assert!(store.try_claim(sid, ttl).await.unwrap(), "first wins");
        assert!(!store.try_claim(sid, ttl).await.unwrap(), "second loses");
        assert!(!store.try_claim(sid, ttl).await.unwrap(), "third loses");
    }
}

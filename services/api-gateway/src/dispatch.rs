//! Client→server dispatch for a Ready session: heartbeat, send-message,
//! typing, presence updates — plus the inbound command token bucket
//! (20 commands / 10 s; typing exempt with its own 1 / 3 s per channel gate).

use std::time::Duration;

use chat_service::ChatError;
use dice_common::id::{ChannelId, MediaId, MessageId};
use dice_network_core::server::FramedTransport;
use dice_protocol::v1::frame::Payload;
use dice_protocol::v1::{self, ErrorCode, Frame};
use presence_service::PresenceError;
use tokio::time::Instant;

use crate::Gateway;
use crate::session::{LoopEnd, SessionState, error_frame, rate_limited_frame};

/// Inbound command budget (backend-services.md §3.4).
pub(crate) const COMMANDS_PER_WINDOW: u32 = 20;
pub(crate) const COMMAND_WINDOW_MS: u64 = 10_000;

/// Typing gate: 1 / 3 s per (session, channel) (protocol §6), enforced via
/// the shared `RateLimiter` (`rl:gw.typing:{session}:{channel}`).
const TYPING_SCOPE: &str = "gw.typing";
const TYPING_WINDOW: Duration = Duration::from_secs(3);

const MICRO: u64 = 1_000_000;

/// In-process token bucket (critique #25: the per-session bucket stays
/// hand-rolled; the cache-backed limiter is for cross-cutting scopes).
/// Tokens are tracked in millionths to keep the refill math integral.
pub(crate) struct TokenBucket {
    capacity_micro: u64,
    window_ms: u64,
    tokens_micro: u64,
    last_refill: Instant,
}

impl Default for TokenBucket {
    fn default() -> Self {
        Self::new(COMMANDS_PER_WINDOW, COMMAND_WINDOW_MS)
    }
}

impl TokenBucket {
    pub(crate) fn new(capacity: u32, window_ms: u64) -> Self {
        let capacity_micro = u64::from(capacity) * MICRO;
        Self {
            capacity_micro,
            window_ms: window_ms.max(1),
            tokens_micro: capacity_micro,
            last_refill: Instant::now(),
        }
    }

    /// Take one token. `Err(retry_after_ms)` when empty.
    pub(crate) fn try_take(&mut self, now: Instant) -> Result<(), u32> {
        let elapsed_ms = now.saturating_duration_since(self.last_refill).as_millis() as u64;
        if elapsed_ms > 0 {
            let refill = elapsed_ms.saturating_mul(self.capacity_micro) / self.window_ms;
            self.tokens_micro = (self.tokens_micro.saturating_add(refill)).min(self.capacity_micro);
            self.last_refill = now;
        }
        if self.tokens_micro >= MICRO {
            self.tokens_micro -= MICRO;
            Ok(())
        } else {
            let deficit = MICRO - self.tokens_micro;
            let retry_ms = deficit
                .saturating_mul(self.window_ms)
                .div_ceil(self.capacity_micro.max(1));
            Err(u32::try_from(retry_ms).unwrap_or(u32::MAX))
        }
    }
}

/// Map a [`ChatError`] to its wire code (same table for socket and REST).
pub(crate) fn chat_error_code(error: &ChatError) -> ErrorCode {
    match error {
        ChatError::NotFound | ChatError::InvalidInvite => ErrorCode::NotFound,
        ChatError::NotAMember | ChatError::PermissionDenied(_) | ChatError::Forbidden(_) => {
            ErrorCode::PermissionDenied
        }
        ChatError::InvalidArgument(_) => ErrorCode::InvalidArgument,
        ChatError::Internal(_) => ErrorCode::Internal,
    }
}

fn chat_error_frame(nonce: u64, error: &ChatError) -> Frame {
    if matches!(error, ChatError::Internal(_)) {
        tracing::error!(%error, "chat call failed");
    }
    error_frame(nonce, chat_error_code(error), &error.to_string())
}

async fn send_or_detach(transport: &mut dyn FramedTransport, frame: &Frame) -> Option<LoopEnd> {
    if transport.send(frame).await.is_err() {
        Some(LoopEnd::Detach)
    } else {
        None
    }
}

/// Handle one inbound frame on a Ready session. `Some(end)` terminates this
/// life of the session; `None` continues the loop.
pub(crate) async fn handle(
    gw: &Gateway,
    st: &mut SessionState,
    transport: &mut dyn FramedTransport,
    frame: Frame,
) -> Option<LoopEnd> {
    let nonce = frame.nonce;
    match frame.payload {
        Some(Payload::Heartbeat(hb)) => {
            st.last_heartbeat = Instant::now();
            // Cumulative ack trims the replay ring (protocol §4).
            st.replay.ack(hb.last_seq);
            let ack = Frame::control(Payload::HeartbeatAck(v1::HeartbeatAck {
                client_time_ms: hb.client_time_ms,
            }));
            if let Some(end) = send_or_detach(transport, &ack).await {
                return Some(end);
            }
            if let Err(error) = gw
                .deps
                .presence
                .heartbeat(st.user, st.gateway_session())
                .await
            {
                tracing::debug!(%error, user = %st.user, "presence heartbeat failed");
            }
            None
        }

        Some(Payload::SendMessage(req)) => {
            if let Err(retry_after_ms) = st.bucket.try_take(Instant::now()) {
                return send_or_detach(transport, &rate_limited_frame(nonce, retry_after_ms)).await;
            }
            let reply_to = (req.reply_to_id != 0).then(|| MessageId::from_raw(req.reply_to_id));
            let attachments: Vec<MediaId> = req
                .attachment_ids
                .iter()
                .map(|id| MediaId::from_raw(*id))
                .collect();
            let result = gw
                .deps
                .chat
                .send_message(
                    st.user,
                    ChannelId::from_raw(req.channel_id),
                    req.content,
                    reply_to,
                    attachments,
                    nonce,
                )
                .await;
            let reply = match result {
                Ok(message) => Frame::with_nonce(
                    nonce,
                    Payload::SendMessageAck(v1::SendMessageAck {
                        message: Some(message),
                    }),
                ),
                Err(error) => chat_error_frame(nonce, &error),
            };
            send_or_detach(transport, &reply).await
        }

        Some(Payload::EditMessage(req)) => {
            if let Err(retry_after_ms) = st.bucket.try_take(Instant::now()) {
                return send_or_detach(transport, &rate_limited_frame(nonce, retry_after_ms)).await;
            }
            // Success is confirmed by the broadcast MessageUpdate dispatch (the
            // editor is subscribed to the channel subject), so reply only on error.
            if let Err(error) = gw
                .deps
                .chat
                .edit_message(
                    st.user,
                    ChannelId::from_raw(req.channel_id),
                    MessageId::from_raw(req.message_id),
                    req.content,
                )
                .await
            {
                return send_or_detach(transport, &chat_error_frame(nonce, &error)).await;
            }
            None
        }

        Some(Payload::DeleteMessage(req)) => {
            if let Err(retry_after_ms) = st.bucket.try_take(Instant::now()) {
                return send_or_detach(transport, &rate_limited_frame(nonce, retry_after_ms)).await;
            }
            // Confirmed by the broadcast MessageDelete dispatch; reply only on error.
            if let Err(error) = gw
                .deps
                .chat
                .delete_message(
                    st.user,
                    ChannelId::from_raw(req.channel_id),
                    MessageId::from_raw(req.message_id),
                )
                .await
            {
                return send_or_detach(transport, &chat_error_frame(nonce, &error)).await;
            }
            None
        }

        Some(Payload::AddReaction(req)) => {
            if let Err(retry_after_ms) = st.bucket.try_take(Instant::now()) {
                return send_or_detach(transport, &rate_limited_frame(nonce, retry_after_ms)).await;
            }
            if let Err(error) = gw
                .deps
                .chat
                .add_reaction(
                    st.user,
                    ChannelId::from_raw(req.channel_id),
                    MessageId::from_raw(req.message_id),
                    req.emoji,
                )
                .await
            {
                return send_or_detach(transport, &chat_error_frame(nonce, &error)).await;
            }
            None
        }

        Some(Payload::RemoveReaction(req)) => {
            if let Err(retry_after_ms) = st.bucket.try_take(Instant::now()) {
                return send_or_detach(transport, &rate_limited_frame(nonce, retry_after_ms)).await;
            }
            if let Err(error) = gw
                .deps
                .chat
                .remove_reaction(
                    st.user,
                    ChannelId::from_raw(req.channel_id),
                    MessageId::from_raw(req.message_id),
                    req.emoji,
                )
                .await
            {
                return send_or_detach(transport, &chat_error_frame(nonce, &error)).await;
            }
            None
        }

        Some(Payload::StartTyping(req)) => {
            let channel = ChannelId::from_raw(req.channel_id);
            match gw
                .deps
                .rate
                .check(
                    TYPING_SCOPE,
                    &format!("{}:{}", st.session_id, channel),
                    1,
                    TYPING_WINDOW,
                )
                .await
            {
                Ok(decision) if !decision.allowed => return None, // lossy class B: drop
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "typing rate-limit check failed; allowing")
                }
            }
            if let Err(error) = gw.deps.chat.typing(st.user, channel).await {
                if nonce != 0 {
                    return send_or_detach(transport, &chat_error_frame(nonce, &error)).await;
                }
                tracing::debug!(%error, user = %st.user, "typing rejected");
            }
            None
        }

        Some(Payload::UpdatePresence(req)) => {
            if let Err(retry_after_ms) = st.bucket.try_take(Instant::now()) {
                return send_or_detach(transport, &rate_limited_frame(nonce, retry_after_ms)).await;
            }
            // INVISIBLE (raw 5) is reserved: the closed prost enum rejects it
            // at conversion time (protocol §11).
            let Ok(status) = v1::PresenceStatus::try_from(req.status) else {
                return send_or_detach(
                    transport,
                    &error_frame(
                        nonce,
                        ErrorCode::InvalidArgument,
                        "unsupported presence status",
                    ),
                )
                .await;
            };
            match gw
                .deps
                .presence
                .set_status(st.user, st.gateway_session(), status)
                .await
            {
                Ok(()) => None,
                Err(PresenceError::InvisibleNotSupported) => {
                    send_or_detach(
                        transport,
                        &error_frame(
                            nonce,
                            ErrorCode::InvalidArgument,
                            "INVISIBLE/OFFLINE are not settable in M1",
                        ),
                    )
                    .await
                }
                Err(PresenceError::UnknownSession) => {
                    send_or_detach(
                        transport,
                        &error_frame(nonce, ErrorCode::InvalidSession, "unknown presence session"),
                    )
                    .await
                }
                Err(error @ PresenceError::Internal(_)) => {
                    tracing::error!(%error, "presence set_status failed");
                    send_or_detach(
                        transport,
                        &error_frame(nonce, ErrorCode::Internal, "presence update failed"),
                    )
                    .await
                }
            }
        }

        // A client-initiated Close frame = clean goodbye (no resume window).
        Some(Payload::Close(_)) => Some(LoopEnd::CleanClose),

        // Unknown payload (newer client) or server-only frames: per protocol
        // §2, reply UNKNOWN_OPCODE iff it carried a nonce, else ignore.
        Some(_) | None => {
            if nonce != 0 {
                return send_or_detach(
                    transport,
                    &error_frame(nonce, ErrorCode::UnknownOpcode, "unexpected payload"),
                )
                .await;
            }
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bucket_allows_capacity_then_denies() {
        let mut bucket = TokenBucket::new(20, 10_000);
        let now = Instant::now();
        for _ in 0..20 {
            assert!(bucket.try_take(now).is_ok());
        }
        let retry = bucket.try_take(now).unwrap_err();
        // One token refills every 500 ms at 20/10 s.
        assert!(retry > 0 && retry <= 500, "retry_after_ms = {retry}");
    }

    #[tokio::test]
    async fn bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(20, 10_000);
        let start = Instant::now();
        for _ in 0..20 {
            assert!(bucket.try_take(start).is_ok());
        }
        assert!(bucket.try_take(start).is_err());
        // After one full window the bucket is full again.
        let later = start + Duration::from_millis(10_000);
        for _ in 0..20 {
            assert!(bucket.try_take(later).is_ok());
        }
        assert!(bucket.try_take(later).is_err());
        // Partial refill: 500 ms buys exactly one token.
        let one_more = later + Duration::from_millis(500);
        assert!(bucket.try_take(one_more).is_ok());
        assert!(bucket.try_take(one_more).is_err());
    }

    #[test]
    fn chat_error_codes_match_the_spec_table() {
        assert_eq!(chat_error_code(&ChatError::NotFound), ErrorCode::NotFound);
        assert_eq!(
            chat_error_code(&ChatError::InvalidInvite),
            ErrorCode::NotFound
        );
        assert_eq!(
            chat_error_code(&ChatError::NotAMember),
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            chat_error_code(&ChatError::InvalidArgument("x".into())),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            chat_error_code(&ChatError::Internal("x".into())),
            ErrorCode::Internal
        );
    }
}

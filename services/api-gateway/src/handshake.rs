//! Identify handling: local JWT verification (no auth-service round trip),
//! state sync, presence connect, interest registration, `Ready` assembly
//! (docs/protocol.md §3).

use std::sync::Arc;

use bytes::Bytes;
use dice_auth_core::token::verify_access;
use dice_common::id::{ChannelId, GuildId, SessionId};
use dice_network_core::server::FramedTransport;
use dice_protocol::v1::frame::Payload;
use dice_protocol::v1::{self, ErrorCode, Frame, PresenceStatus};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::Gateway;
use crate::dispatch::TokenBucket;
use crate::durable::ResumeSnapshot;
use crate::resume::{LocalReplayBuffer, RESUME_TOKEN_LEN, ReplayBuffer};
use crate::session::{
    KillSwitch, OUTBOUND_QUEUE, SessionSender, SessionState, close_with, resume_replay, run_ready,
};

/// Full Identify path. Owns the transport: on success it runs the session to
/// completion, on failure it closes and returns.
pub(crate) async fn identify(
    gw: Arc<Gateway>,
    mut transport: Box<dyn FramedTransport>,
    identify: v1::Identify,
) {
    // 1. Verify the access JWT locally with the public key (protocol §12).
    let claims = match verify_access(&gw.deps.jwt, &identify.access_token) {
        Ok(claims) => claims,
        Err(_) => {
            close_with(
                &mut *transport,
                ErrorCode::Unauthenticated,
                "invalid access token",
            )
            .await;
            return;
        }
    };
    let (Some(user), Some(auth_session)) = (claims.user_id(), claims.session_id()) else {
        close_with(
            &mut *transport,
            ErrorCode::Unauthenticated,
            "malformed token claims",
        )
        .await;
        return;
    };

    // 2. Snapshot the user's world for Ready.
    let mut sync = match gw.deps.chat.sync_user_state(user).await {
        Ok(sync) => sync,
        Err(error) => {
            tracing::error!(%error, %user, "sync_user_state failed");
            close_with(&mut *transport, ErrorCode::Internal, "state sync failed").await;
            return;
        }
    };

    // For CAP_LAZY_MEMBERS clients, trim the Ready user dictionary to the inlined
    // set — self + the (≤100) inlined guild members + DM recipients. Authors
    // beyond that are resolved on demand via RequestUsers, so this is the Ready
    // bandwidth win lazy member loading was designed for. The presence snapshot
    // below then covers only the trimmed set (exactly what the client displays).
    if identify.capabilities & dice_protocol::CAP_LAZY_MEMBERS != 0 {
        retain_inlined_users(&mut sync, user.raw());
    }
    // 3. Mint the gateway session identity.
    let session_id = gw.deps.ids.generate().0;
    let mut resume_token = [0u8; RESUME_TOKEN_LEN];
    getrandom::fill(&mut resume_token).expect("operating-system RNG failure");

    let guild_ids: Vec<GuildId> = sync
        .guilds
        .iter()
        .map(|g| GuildId::from_raw(g.id))
        .collect();
    let dm_ids: Vec<ChannelId> = sync
        .dm_channels
        .iter()
        .map(|c| ChannelId::from_raw(c.id))
        .collect();

    // 4. Presence: the session starts ONLINE (no status in Identify).
    //    Connect BEFORE the snapshot so the user's own dot is already live
    //    in their Ready.presences.
    if let Err(error) = gw
        .deps
        .presence
        .connect(
            user,
            SessionId::from_raw(session_id),
            &guild_ids,
            &dm_ids,
            PresenceStatus::Online,
        )
        .await
    {
        tracing::error!(%error, %user, "presence connect failed");
        close_with(
            &mut *transport,
            ErrorCode::Internal,
            "presence connect failed",
        )
        .await;
        return;
    }

    // Visible users = the deduped dictionary (guild members ∪ DM recipients
    // ∪ self) — exactly the presence snapshot set.
    let visible: Vec<_> = sync
        .users
        .iter()
        .map(|u| dice_common::id::UserId::from_raw(u.id))
        .collect();
    let presences = match gw.deps.presence.snapshot(&visible).await {
        Ok(presences) => presences,
        Err(error) => {
            tracing::error!(%error, %user, "presence snapshot failed");
            if let Err(error) = gw
                .deps
                .presence
                .disconnect(user, SessionId::from_raw(session_id))
                .await
            {
                tracing::debug!(%error, "presence disconnect after failed snapshot");
            }
            close_with(
                &mut *transport,
                ErrorCode::Internal,
                "presence snapshot failed",
            )
            .await;
            return;
        }
    };

    // 5. Register interest BEFORE sending Ready so nothing slips between the
    //    snapshot and the first dispatch (events queue behind Ready).
    let (tx, outbound) = mpsc::channel(OUTBOUND_QUEUE);
    let kill = KillSwitch::new();
    let sender = SessionSender::new(session_id, user, auth_session, tx, kill.clone());
    if let Err(error) = gw
        .router
        .register_session(&sender, &guild_ids, &dm_ids)
        .await
    {
        tracing::error!(%error, %user, "interest registration failed");
        gw.router.unregister_session(session_id);
        if let Err(error) = gw
            .deps
            .presence
            .disconnect(user, SessionId::from_raw(session_id))
            .await
        {
            tracing::debug!(%error, "presence disconnect after failed registration");
        }
        close_with(&mut *transport, ErrorCode::Internal, "subscription failed").await;
        return;
    }

    // 6. Ready.
    let ready = Frame::control(Payload::Ready(v1::Ready {
        gateway_session_id: session_id,
        resume_token: Bytes::copy_from_slice(&resume_token),
        user: sync.users.iter().find(|u| u.id == user.raw()).cloned(),
        guilds: sync.guilds,
        dm_channels: sync.dm_channels,
        presences,
        users: sync.users,
    }));

    let st = SessionState {
        user,
        auth_session,
        session_id,
        resume_token,
        outbound,
        kill,
        next_seq: 1,
        replay: Box::new(LocalReplayBuffer::new()),
        last_heartbeat: Instant::now(),
        bucket: TokenBucket::default(),
    };

    if transport.send(&ready).await.is_err() {
        // The client never saw the resume token: nothing to resume.
        gw.router.unregister_session(session_id);
        if let Err(error) = gw
            .deps
            .presence
            .disconnect(user, SessionId::from_raw(session_id))
            .await
        {
            tracing::debug!(%error, "presence disconnect after failed Ready send");
        }
        return;
    }

    // Voice-datagram capability is advertised here (negotiation seam); the
    // functional enable is QUIC-transport presence (see voice_dg), so voice
    // survives a resume even though Resume carries no capabilities field.
    let voice_capable = identify.capabilities & dice_protocol::CAP_VOICE_DATAGRAMS != 0;
    tracing::debug!(
        %user,
        session_id,
        auth_session = %st.auth_session,
        kind = ?transport.kind(),
        voice_capable,
        "session ready"
    );
    run_ready(gw, st, transport).await;
}

/// Re-host a detached session from its durable snapshot on a DIFFERENT node
/// after the origin is gone (cross-node resume phase 2b, ADR-0007). Mirrors
/// [`identify`] — re-derive the user's world, reconnect presence, register
/// interest — but seeded with the EXISTING `session_id`, `resume_token`, next
/// seq and the rehydrated replay ring, and sends `Resumed` + replay instead of
/// `Ready`. The caller has already validated the resume token + ring coverage
/// and won the single-takeover claim; on success it owns the transport for the
/// rest of the session's life. `last_seq` is the client's cumulative ack.
pub(crate) async fn rehost(
    gw: Arc<Gateway>,
    mut transport: Box<dyn FramedTransport>,
    session_id: u64,
    snapshot: ResumeSnapshot,
    last_seq: u64,
) {
    let user = dice_common::id::UserId::from_raw(snapshot.user);
    let auth_session = SessionId::from_raw(snapshot.auth_session);

    // Re-derive the user's current subscription set — membership may have moved
    // since the snapshot; we re-subscribe to the world as it is NOW.
    let sync = match gw.deps.chat.sync_user_state(user).await {
        Ok(sync) => sync,
        Err(error) => {
            tracing::error!(%error, %user, session_id, "re-host: sync_user_state failed");
            // Release the takeover claim (won by try_rehost) but KEEP the snapshot
            // so another node — or a retry — can still re-host this valid session.
            let _ = gw.durable.release_claim(session_id).await;
            close_with(&mut *transport, ErrorCode::Internal, "state sync failed").await;
            return;
        }
    };
    let guild_ids: Vec<GuildId> = sync
        .guilds
        .iter()
        .map(|g| GuildId::from_raw(g.id))
        .collect();
    let dm_ids: Vec<ChannelId> = sync
        .dm_channels
        .iter()
        .map(|c| ChannelId::from_raw(c.id))
        .collect();

    // Reconnect presence on THIS node (the origin's entry TTLs out).
    if let Err(error) = gw
        .deps
        .presence
        .connect(
            user,
            SessionId::from_raw(session_id),
            &guild_ids,
            &dm_ids,
            PresenceStatus::Online,
        )
        .await
    {
        tracing::error!(%error, %user, session_id, "re-host: presence connect failed");
        let _ = gw.durable.release_claim(session_id).await; // free the fence, keep the snapshot
        close_with(
            &mut *transport,
            ErrorCode::Internal,
            "presence connect failed",
        )
        .await;
        return;
    }

    // Register interest BEFORE Resumed so nothing slips in between.
    let (tx, outbound) = mpsc::channel(OUTBOUND_QUEUE);
    let kill = KillSwitch::new();
    let sender = SessionSender::new(session_id, user, auth_session, tx, kill.clone());
    if let Err(error) = gw
        .router
        .register_session(&sender, &guild_ids, &dm_ids)
        .await
    {
        tracing::error!(%error, %user, session_id, "re-host: interest registration failed");
        gw.router.unregister_session(session_id);
        if let Err(error) = gw
            .deps
            .presence
            .disconnect(user, SessionId::from_raw(session_id))
            .await
        {
            tracing::debug!(%error, "presence disconnect after failed re-host registration");
        }
        let _ = gw.durable.release_claim(session_id).await; // free the fence, keep the snapshot
        close_with(&mut *transport, ErrorCode::Internal, "subscription failed").await;
        return;
    }

    // Rehydrate the ring (coverage restored via trimmed_to) and continue seq
    // from the snapshot. `ring_next` guards against a stale next_seq: the highest
    // buffered seq + 1 is a hard lower bound, so a re-host never reuses a seq the
    // client already saw.
    let ring_next = snapshot
        .frames
        .last()
        .map_or(snapshot.next_seq, |f| f.seq + 1);
    let next_seq = snapshot.next_seq.max(ring_next);
    let mut replay = LocalReplayBuffer::from_snapshot(snapshot.frames, snapshot.trimmed_to);
    replay.ack(last_seq); // drop what the client already has, as on the origin

    let mut st = SessionState {
        user,
        auth_session,
        session_id,
        resume_token: snapshot.resume_token,
        outbound,
        kill,
        next_seq,
        replay: Box::new(replay),
        last_heartbeat: Instant::now(),
        bucket: TokenBucket::default(),
    };

    tracing::info!(%user, session_id, next_seq, this_node = gw.node_id, "re-hosting session");
    // `Resumed` + replay of the rehydrated, acked ring (original seqs).
    resume_replay(&mut st, &mut *transport).await;
    run_ready(gw, st, transport).await;
}

/// Trim the Ready user dictionary to the inlined set for CAP_LAZY_MEMBERS
/// clients: self + inlined guild members + DM recipients. Authors beyond that
/// are resolved on demand via `RequestUsers`. DM recipients are kept on purpose
/// — a DM partner you share no guild with cannot be re-fetched via the
/// shared-guild-gated `get_users`.
fn retain_inlined_users(sync: &mut chat_service::UserSyncState, self_id: u64) {
    let mut keep: std::collections::HashSet<u64> = std::collections::HashSet::new();
    keep.insert(self_id);
    for guild in &sync.guilds {
        keep.extend(guild.members.iter().map(|m| m.user_id));
    }
    for channel in &sync.dm_channels {
        keep.extend(channel.recipient_ids.iter().copied());
    }
    sync.users.retain(|u| keep.contains(&u.id));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::retain_inlined_users;
    use chat_service::UserSyncState;
    use dice_protocol::v1;

    #[test]
    fn trim_keeps_self_inlined_members_and_dm_recipients() {
        let mut sync = UserSyncState {
            guilds: vec![v1::Guild {
                members: vec![v1::Member {
                    user_id: 2,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            dm_channels: vec![v1::Channel {
                recipient_ids: vec![3],
                ..Default::default()
            }],
            users: vec![
                v1::User {
                    id: 1,
                    ..Default::default()
                }, // self
                v1::User {
                    id: 2,
                    ..Default::default()
                }, // inlined guild member
                v1::User {
                    id: 3,
                    ..Default::default()
                }, // DM recipient
                v1::User {
                    id: 99,
                    ..Default::default()
                }, // non-inlined member → trimmed
            ],
        };
        retain_inlined_users(&mut sync, 1);
        let kept: Vec<u64> = sync.users.iter().map(|u| u.id).collect();
        assert_eq!(
            kept,
            vec![1, 2, 3],
            "the non-inlined member (99) is trimmed"
        );
    }
}

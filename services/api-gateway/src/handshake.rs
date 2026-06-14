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
use crate::resume::{RESUME_TOKEN_LEN, ReplayBuffer};
use crate::session::{
    KillSwitch, OUTBOUND_QUEUE, SessionSender, SessionState, close_with, run_ready,
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
    let sync = match gw.deps.chat.sync_user_state(user).await {
        Ok(sync) => sync,
        Err(error) => {
            tracing::error!(%error, %user, "sync_user_state failed");
            close_with(&mut *transport, ErrorCode::Internal, "state sync failed").await;
            return;
        }
    };
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
        replay: ReplayBuffer::new(),
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

//! Split-mode proof: a [`Presence`] impl served over NATS round-trips through
//! [`PresenceNatsClient`] exactly like a direct trait call — unit returns, the
//! typed-error mapping, and a non-trivial response (snapshot). Uses a mock
//! `Presence` so it needs only live NATS (no Postgres). Skips cleanly if NATS
//! is down.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use dice_common::{ChannelId, GuildId, SessionId, UserId};
use dice_event_bus::rpc::RpcClient;
use dice_protocol::v1::{PresenceStatus, PresenceUpdate};
use presence_service::rpc::{PresenceNatsClient, serve};
use presence_service::{Presence, PresenceError};

/// Matches infrastructure/docker/docker-compose.yml + .env.example.
const DEV_NATS: &str = "nats://localhost:4222";

/// A canned [`Presence`]: session 404 is "unknown"; snapshot echoes the first
/// requested user as ONLINE. Enough to exercise every RPC code path.
struct MockPresence;

#[async_trait::async_trait]
impl Presence for MockPresence {
    async fn connect(
        &self,
        _user: UserId,
        _session: SessionId,
        _guild_ids: &[GuildId],
        _dm_channel_ids: &[ChannelId],
        _status: PresenceStatus,
    ) -> Result<(), PresenceError> {
        Ok(())
    }

    async fn heartbeat(&self, _user: UserId, session: SessionId) -> Result<(), PresenceError> {
        if session == SessionId::from_raw(404) {
            Err(PresenceError::UnknownSession)
        } else {
            Ok(())
        }
    }

    async fn set_status(
        &self,
        _user: UserId,
        _session: SessionId,
        status: PresenceStatus,
    ) -> Result<(), PresenceError> {
        if status == PresenceStatus::Unspecified {
            Err(PresenceError::InvisibleNotSupported)
        } else {
            Ok(())
        }
    }

    async fn disconnect(&self, _user: UserId, _session: SessionId) -> Result<(), PresenceError> {
        Ok(())
    }

    async fn detach(&self, _user: UserId, _session: SessionId) -> Result<(), PresenceError> {
        Ok(())
    }

    async fn add_interest(
        &self,
        _user: UserId,
        _session: SessionId,
        _guild_ids: &[GuildId],
        _dm_channel_ids: &[ChannelId],
    ) -> Result<(), PresenceError> {
        Ok(())
    }

    async fn snapshot(&self, users: &[UserId]) -> Result<Vec<PresenceUpdate>, PresenceError> {
        Ok(users
            .iter()
            .map(|&u| PresenceUpdate {
                user_id: u.raw(),
                status: PresenceStatus::Online as i32,
                since_ms: 1_700,
            })
            .collect())
    }
}

#[tokio::test]
async fn presence_round_trips_over_nats() {
    let url = std::env::var("DICE_NATS_URL").unwrap_or_else(|_| DEV_NATS.to_owned());
    let Ok(server) = RpcClient::connect(&url).await else {
        eprintln!("skipping: live NATS required (just infra-up)");
        return;
    };
    let task = tokio::spawn(serve(server, Arc::new(MockPresence)));
    // Let the queue subscription register before the first request.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = PresenceNatsClient::new(RpcClient::connect(&url).await.unwrap());
    let user = UserId::from_raw(7);

    // Unit return round-trips Ok.
    client
        .connect(
            user,
            SessionId::from_raw(1),
            &[GuildId::from_raw(10)],
            &[ChannelId::from_raw(20)],
            PresenceStatus::Online,
        )
        .await
        .unwrap();
    client
        .heartbeat(user, SessionId::from_raw(1))
        .await
        .unwrap();

    // Typed errors map back across the wire.
    assert!(matches!(
        client.heartbeat(user, SessionId::from_raw(404)).await,
        Err(PresenceError::UnknownSession)
    ));
    assert!(matches!(
        client
            .set_status(user, SessionId::from_raw(1), PresenceStatus::Unspecified)
            .await,
        Err(PresenceError::InvisibleNotSupported)
    ));

    // A non-trivial response decodes correctly.
    let snap = client.snapshot(&[user, UserId::from_raw(9)]).await.unwrap();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].user_id, 7);
    assert_eq!(snap[0].status, PresenceStatus::Online as i32);
    assert_eq!(snap[1].user_id, 9);

    task.abort();
}

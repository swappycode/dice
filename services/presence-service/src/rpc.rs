//! Split-mode NATS RPC for presence (docs/design/backend-services.md). The
//! monolith calls [`PresenceService`](crate::PresenceService) directly; a split
//! deployment puts [`PresenceNatsClient`] behind the same `Arc<dyn Presence>`
//! seam in the gateway and runs [`serve`] in the presence-service bin. Both
//! sides use the generic envelope/transport in `dice_event_bus::rpc`; only the
//! per-method payloads (rpc.proto) and the error mapping live here.

use std::sync::Arc;

use dice_common::{ChannelId, GuildId, SessionId, UserId};
use dice_event_bus::rpc::{RpcClient, RpcError, RpcFault};
use dice_protocol::internal::v1 as rpc;
use dice_protocol::prost::Message as _;
use dice_protocol::v1::PresenceStatus;

use crate::{Presence, PresenceError};

/// RPC service name (subject segment + queue group): `dice.rpc.presence.*`.
pub const SERVICE: &str = "presence";

// Fault codes carried over the wire so the client can rebuild the typed error.
const CODE_INTERNAL: u32 = 0;
const CODE_UNKNOWN_SESSION: u32 = 1;
const CODE_INVISIBLE: u32 = 2;

fn raw<T: Copy>(ids: &[T], to_u64: impl Fn(T) -> u64) -> Vec<u64> {
    ids.iter().map(|&id| to_u64(id)).collect()
}

fn guilds(ids: &[u64]) -> Vec<GuildId> {
    ids.iter().map(|&r| GuildId::from_raw(r)).collect()
}

fn channels(ids: &[u64]) -> Vec<ChannelId> {
    ids.iter().map(|&r| ChannelId::from_raw(r)).collect()
}

fn status_from(raw: i32) -> PresenceStatus {
    PresenceStatus::try_from(raw).unwrap_or(PresenceStatus::Unspecified)
}

// ---- server: PresenceError -> RpcFault ----

fn to_fault(e: PresenceError) -> RpcFault {
    match e {
        PresenceError::UnknownSession => RpcFault {
            code: CODE_UNKNOWN_SESSION,
            message: e.to_string(),
        },
        PresenceError::InvisibleNotSupported => RpcFault {
            code: CODE_INVISIBLE,
            message: e.to_string(),
        },
        PresenceError::Internal(_) => RpcFault {
            code: CODE_INTERNAL,
            message: "internal presence error".to_owned(),
        },
    }
}

fn decode_fault(e: dice_protocol::prost::DecodeError) -> RpcFault {
    RpcFault::internal(format!("malformed request: {e}"))
}

// ---- client: RpcError -> PresenceError ----

fn to_err(e: RpcError) -> PresenceError {
    match e {
        RpcError::Fault {
            code: CODE_UNKNOWN_SESSION,
            ..
        } => PresenceError::UnknownSession,
        RpcError::Fault {
            code: CODE_INVISIBLE,
            ..
        } => PresenceError::InvisibleNotSupported,
        other => PresenceError::Internal(other.to_string().into()),
    }
}

/// Run the presence RPC responder until the future is dropped/aborted (the
/// presence-service bin spawns this). Decodes each `dice.rpc.presence.{method}`,
/// calls `presence`, and replies with the encoded response or a mapped fault.
/// Takes the [`RpcClient`] by value so it can be `tokio::spawn`ed (clone first
/// if the bin also makes outbound calls).
pub async fn serve(client: RpcClient, presence: Arc<dyn Presence>) -> Result<(), RpcError> {
    client
        .serve(SERVICE, move |method, body| {
            let presence = Arc::clone(&presence);
            async move {
                match method.as_str() {
                    "connect" => {
                        let r = rpc::PresenceConnectReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        presence
                            .connect(
                                UserId::from_raw(r.user),
                                SessionId::from_raw(r.session),
                                &guilds(&r.guild_ids),
                                &channels(&r.dm_channel_ids),
                                status_from(r.status),
                            )
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "heartbeat" => {
                        let r = rpc::PresenceHeartbeatReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        presence
                            .heartbeat(UserId::from_raw(r.user), SessionId::from_raw(r.session))
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "set_status" => {
                        let r = rpc::PresenceSetStatusReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        presence
                            .set_status(
                                UserId::from_raw(r.user),
                                SessionId::from_raw(r.session),
                                status_from(r.status),
                            )
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "disconnect" => {
                        let r = rpc::PresenceDisconnectReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        presence
                            .disconnect(UserId::from_raw(r.user), SessionId::from_raw(r.session))
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "detach" => {
                        // Reuses the {user, session} disconnect request shape.
                        let r = rpc::PresenceDisconnectReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        presence
                            .detach(UserId::from_raw(r.user), SessionId::from_raw(r.session))
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "add_interest" => {
                        let r = rpc::PresenceInterestReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        presence
                            .add_interest(
                                UserId::from_raw(r.user),
                                SessionId::from_raw(r.session),
                                &guilds(&r.guild_ids),
                                &channels(&r.dm_channel_ids),
                            )
                            .await
                            .map(|()| Vec::new())
                            .map_err(to_fault)
                    }
                    "snapshot" => {
                        let r = rpc::PresenceSnapshotReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        let users: Vec<UserId> =
                            r.users.iter().map(|&u| UserId::from_raw(u)).collect();
                        let updates = presence.snapshot(&users).await.map_err(to_fault)?;
                        Ok(rpc::PresenceSnapshotResp { updates }.encode_to_vec())
                    }
                    other => Err(RpcFault::internal(format!("unknown method {other}"))),
                }
            }
        })
        .await
}

/// Gateway-side stub: speaks the [`Presence`] trait by issuing NATS RPC, so it
/// drops into `GatewayDeps.presence` unchanged in a split deployment.
pub struct PresenceNatsClient {
    rpc: RpcClient,
}

impl PresenceNatsClient {
    #[must_use]
    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc }
    }

    async fn unit_call(&self, method: &str, req: Vec<u8>) -> Result<(), PresenceError> {
        self.rpc.call(SERVICE, method, req).await.map_err(to_err)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Presence for PresenceNatsClient {
    async fn connect(
        &self,
        user: UserId,
        session: SessionId,
        guild_ids: &[GuildId],
        dm_channel_ids: &[ChannelId],
        status: PresenceStatus,
    ) -> Result<(), PresenceError> {
        let req = rpc::PresenceConnectReq {
            user: user.raw(),
            session: session.raw(),
            guild_ids: raw(guild_ids, GuildId::raw),
            dm_channel_ids: raw(dm_channel_ids, ChannelId::raw),
            status: status as i32,
        };
        self.unit_call("connect", req.encode_to_vec()).await
    }

    async fn heartbeat(&self, user: UserId, session: SessionId) -> Result<(), PresenceError> {
        let req = rpc::PresenceHeartbeatReq {
            user: user.raw(),
            session: session.raw(),
        };
        self.unit_call("heartbeat", req.encode_to_vec()).await
    }

    async fn set_status(
        &self,
        user: UserId,
        session: SessionId,
        status: PresenceStatus,
    ) -> Result<(), PresenceError> {
        let req = rpc::PresenceSetStatusReq {
            user: user.raw(),
            session: session.raw(),
            status: status as i32,
        };
        self.unit_call("set_status", req.encode_to_vec()).await
    }

    async fn disconnect(&self, user: UserId, session: SessionId) -> Result<(), PresenceError> {
        let req = rpc::PresenceDisconnectReq {
            user: user.raw(),
            session: session.raw(),
        };
        self.unit_call("disconnect", req.encode_to_vec()).await
    }

    async fn detach(&self, user: UserId, session: SessionId) -> Result<(), PresenceError> {
        let req = rpc::PresenceDisconnectReq {
            user: user.raw(),
            session: session.raw(),
        };
        self.unit_call("detach", req.encode_to_vec()).await
    }

    async fn add_interest(
        &self,
        user: UserId,
        session: SessionId,
        guild_ids: &[GuildId],
        dm_channel_ids: &[ChannelId],
    ) -> Result<(), PresenceError> {
        let req = rpc::PresenceInterestReq {
            user: user.raw(),
            session: session.raw(),
            guild_ids: raw(guild_ids, GuildId::raw),
            dm_channel_ids: raw(dm_channel_ids, ChannelId::raw),
        };
        self.unit_call("add_interest", req.encode_to_vec()).await
    }

    async fn snapshot(
        &self,
        users: &[UserId],
    ) -> Result<Vec<dice_protocol::v1::PresenceUpdate>, PresenceError> {
        let req = rpc::PresenceSnapshotReq {
            users: raw(users, UserId::raw),
        };
        let bytes = self
            .rpc
            .call(SERVICE, "snapshot", req.encode_to_vec())
            .await
            .map_err(to_err)?;
        let resp = rpc::PresenceSnapshotResp::decode(bytes.as_slice())
            .map_err(|e| PresenceError::Internal(e.to_string().into()))?;
        Ok(resp.updates)
    }
}

//! Split-mode NATS RPC for chat (mirror of `presence::rpc` / `auth::rpc`). The
//! monolith calls [`ChatService`](crate::ChatService) directly; a split
//! deployment puts [`ChatNatsClient`] behind the same `Arc<dyn Chat>` seam in
//! the gateway and runs [`serve`] in the chat-service bin. Only the per-method
//! payloads and the error mapping live here; the envelope/transport is generic
//! (`dice_event_bus::rpc`).

use std::sync::Arc;

use dice_common::{ChannelId, GuildId, MediaId, MessageId, UserId};
use dice_event_bus::rpc::{RpcClient, RpcError, RpcFault};
use dice_permissions::{MissingPermissions, Permissions};
use dice_protocol::internal::v1 as rpc;
use dice_protocol::prost::{self, Message as _};
use dice_protocol::v1;

use crate::{Chat, ChatError, HistoryCursor, MemberPage, UserSyncState};

/// RPC service name (subject segment + queue group): `dice.rpc.chat.*`.
pub const SERVICE: &str = "chat";

// Fault codes carried over the wire so the client can rebuild the typed error.
const CODE_INTERNAL: u32 = 0;
const CODE_NOT_FOUND: u32 = 1;
const CODE_NOT_A_MEMBER: u32 = 2;
const CODE_PERMISSION_DENIED: u32 = 3;
const CODE_FORBIDDEN: u32 = 4;
const CODE_INVALID_ARGUMENT: u32 = 5;
const CODE_INVALID_INVITE: u32 = 6;

fn internal(e: impl std::fmt::Display) -> ChatError {
    ChatError::Internal(e.to_string().into())
}

/// `0` ⇒ `None`, else the id.
fn opt_message(id: u64) -> Option<MessageId> {
    (id != 0).then(|| MessageId::from_raw(id))
}
fn opt_media(id: u64) -> Option<MediaId> {
    (id != 0).then(|| MediaId::from_raw(id))
}

fn cursor_to_wire(cursor: HistoryCursor) -> (u32, u64) {
    match cursor {
        HistoryCursor::Latest => (0, 0),
        HistoryCursor::Before(id) => (1, id.raw()),
        HistoryCursor::After(id) => (2, id.raw()),
    }
}
fn cursor_from_wire(kind: u32, id: u64) -> HistoryCursor {
    match kind {
        1 => HistoryCursor::Before(MessageId::from_raw(id)),
        2 => HistoryCursor::After(MessageId::from_raw(id)),
        _ => HistoryCursor::Latest,
    }
}

// ---- server: ChatError -> RpcFault ----

fn to_fault(e: ChatError) -> RpcFault {
    let code = match e {
        ChatError::NotFound => CODE_NOT_FOUND,
        ChatError::NotAMember => CODE_NOT_A_MEMBER,
        ChatError::PermissionDenied(_) => CODE_PERMISSION_DENIED,
        ChatError::Forbidden(_) => CODE_FORBIDDEN,
        ChatError::InvalidArgument(_) => CODE_INVALID_ARGUMENT,
        ChatError::InvalidInvite => CODE_INVALID_INVITE,
        ChatError::Internal(_) => CODE_INTERNAL,
    };
    // `message` carries the only data that doesn't fit in `code`: the missing
    // permission bits (as digits), or the Forbidden/InvalidArgument detail.
    let message = match e {
        ChatError::PermissionDenied(p) => p.missing.bits().to_string(),
        ChatError::Forbidden(m) | ChatError::InvalidArgument(m) => m,
        ChatError::Internal(_) => "internal chat error".to_owned(),
        other => other.to_string(),
    };
    RpcFault { code, message }
}

fn decode_fault(e: prost::DecodeError) -> RpcFault {
    RpcFault::internal(format!("malformed request: {e}"))
}

// ---- client: RpcError -> ChatError ----

fn to_err(e: RpcError) -> ChatError {
    match e {
        RpcError::Fault {
            code: CODE_NOT_FOUND,
            ..
        } => ChatError::NotFound,
        RpcError::Fault {
            code: CODE_NOT_A_MEMBER,
            ..
        } => ChatError::NotAMember,
        RpcError::Fault {
            code: CODE_PERMISSION_DENIED,
            message,
        } => ChatError::PermissionDenied(MissingPermissions {
            missing: Permissions::from_bits_truncate(message.parse().unwrap_or(0)),
        }),
        RpcError::Fault {
            code: CODE_FORBIDDEN,
            message,
        } => ChatError::Forbidden(message),
        RpcError::Fault {
            code: CODE_INVALID_ARGUMENT,
            message,
        } => ChatError::InvalidArgument(message),
        RpcError::Fault {
            code: CODE_INVALID_INVITE,
            ..
        } => ChatError::InvalidInvite,
        other => ChatError::Internal(other.to_string().into()),
    }
}

/// Run the chat RPC responder until dropped/aborted (the chat-service bin spawns
/// this). Decodes each `dice.rpc.chat.{method}`, calls `chat`, and replies with
/// the encoded response or a mapped fault.
pub async fn serve(client: RpcClient, chat: Arc<dyn Chat>) -> Result<(), RpcError> {
    client
        .serve(SERVICE, move |method, body| {
            let chat = Arc::clone(&chat);
            async move {
                match method.as_str() {
                    "sync_user_state" => {
                        let r = rpc::ChatSyncReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let s = chat
                            .sync_user_state(UserId::from_raw(r.user))
                            .await
                            .map_err(to_fault)?;
                        Ok(rpc::ChatSyncResp {
                            guilds: s.guilds,
                            dm_channels: s.dm_channels,
                            users: s.users,
                        }
                        .encode_to_vec())
                    }
                    "send_message" => {
                        let r = rpc::ChatSendMessageReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        let attachments =
                            r.attachments.into_iter().map(MediaId::from_raw).collect();
                        let m = chat
                            .send_message(
                                UserId::from_raw(r.actor),
                                ChannelId::from_raw(r.channel),
                                r.content,
                                opt_message(r.reply_to),
                                attachments,
                                r.nonce,
                            )
                            .await
                            .map_err(to_fault)?;
                        Ok(m.encode_to_vec())
                    }
                    "get_messages" => {
                        let r = rpc::ChatGetMessagesReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        let messages = chat
                            .get_messages(
                                UserId::from_raw(r.actor),
                                ChannelId::from_raw(r.channel),
                                cursor_from_wire(r.cursor_kind, r.cursor_id),
                                r.limit as u8,
                            )
                            .await
                            .map_err(to_fault)?;
                        Ok(rpc::ChatMessagesResp { messages }.encode_to_vec())
                    }
                    "request_members" => {
                        let r = rpc::ChatRequestMembersReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        let page = chat
                            .request_members(
                                UserId::from_raw(r.actor),
                                GuildId::from_raw(r.guild),
                                r.after,
                                r.limit as u8,
                            )
                            .await
                            .map_err(to_fault)?;
                        Ok(rpc::ChatRequestMembersResp {
                            members: page.members,
                            users: page.users,
                            has_more: page.has_more,
                        }
                        .encode_to_vec())
                    }
                    "edit_message" => {
                        let r = rpc::ChatEditMessageReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        let m = chat
                            .edit_message(
                                UserId::from_raw(r.actor),
                                ChannelId::from_raw(r.channel),
                                MessageId::from_raw(r.message),
                                r.content,
                            )
                            .await
                            .map_err(to_fault)?;
                        Ok(m.encode_to_vec())
                    }
                    "delete_message" => {
                        let r = rpc::ChatMessageRefReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        chat.delete_message(
                            UserId::from_raw(r.actor),
                            ChannelId::from_raw(r.channel),
                            MessageId::from_raw(r.message),
                        )
                        .await
                        .map(|()| Vec::new())
                        .map_err(to_fault)
                    }
                    "add_reaction" | "remove_reaction" => {
                        let r =
                            rpc::ChatReactionReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let (actor, channel, message) = (
                            UserId::from_raw(r.actor),
                            ChannelId::from_raw(r.channel),
                            MessageId::from_raw(r.message),
                        );
                        let result = if method == "add_reaction" {
                            chat.add_reaction(actor, channel, message, r.emoji).await
                        } else {
                            chat.remove_reaction(actor, channel, message, r.emoji).await
                        };
                        result.map(|()| Vec::new()).map_err(to_fault)
                    }
                    "create_guild" => {
                        let r = rpc::ChatCreateGuildReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        let g = chat
                            .create_guild(UserId::from_raw(r.actor), r.name)
                            .await
                            .map_err(to_fault)?;
                        Ok(g.encode_to_vec())
                    }
                    "join_guild" => {
                        let r =
                            rpc::ChatJoinGuildReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let g = chat
                            .join_guild(UserId::from_raw(r.actor), &r.code)
                            .await
                            .map_err(to_fault)?;
                        Ok(g.encode_to_vec())
                    }
                    "create_channel" => {
                        let r = rpc::ChatCreateChannelReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        let kind = v1::ChannelKind::try_from(r.kind)
                            .unwrap_or(v1::ChannelKind::Unspecified);
                        let c = chat
                            .create_channel(
                                UserId::from_raw(r.actor),
                                GuildId::from_raw(r.guild),
                                r.name,
                                kind,
                            )
                            .await
                            .map_err(to_fault)?;
                        Ok(c.encode_to_vec())
                    }
                    "open_dm" => {
                        let r =
                            rpc::ChatOpenDmReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let c = chat
                            .open_dm(UserId::from_raw(r.actor), UserId::from_raw(r.other))
                            .await
                            .map_err(to_fault)?;
                        Ok(c.encode_to_vec())
                    }
                    "typing" | "mark_read" => {
                        let r = rpc::ChatChannelRefReq::decode(body.as_slice())
                            .map_err(decode_fault)?;
                        let (actor, channel) =
                            (UserId::from_raw(r.actor), ChannelId::from_raw(r.channel));
                        let result = if method == "typing" {
                            chat.typing(actor, channel).await
                        } else {
                            chat.mark_read(actor, channel).await
                        };
                        result.map(|()| Vec::new()).map_err(to_fault)
                    }
                    "set_avatar" => {
                        let r =
                            rpc::ChatSetAvatarReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let u = chat
                            .set_avatar(UserId::from_raw(r.actor), opt_media(r.media))
                            .await
                            .map_err(to_fault)?;
                        Ok(u.encode_to_vec())
                    }
                    "list_friends" => {
                        let r = rpc::ChatActorReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let list = chat
                            .list_friends(UserId::from_raw(r.actor))
                            .await
                            .map_err(to_fault)?;
                        Ok(list.encode_to_vec())
                    }
                    "add_friend" => {
                        let r =
                            rpc::ChatAddFriendReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let f = chat
                            .add_friend(UserId::from_raw(r.actor), &r.username)
                            .await
                            .map_err(to_fault)?;
                        Ok(f.encode_to_vec())
                    }
                    "accept_friend" => {
                        let r =
                            rpc::ChatFriendRefReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let f = chat
                            .accept_friend(UserId::from_raw(r.actor), UserId::from_raw(r.other))
                            .await
                            .map_err(to_fault)?;
                        Ok(f.encode_to_vec())
                    }
                    "decline_friend" | "remove_friend" => {
                        let r =
                            rpc::ChatFriendRefReq::decode(body.as_slice()).map_err(decode_fault)?;
                        let (actor, other) = (UserId::from_raw(r.actor), UserId::from_raw(r.other));
                        let result = if method == "decline_friend" {
                            chat.decline_friend(actor, other).await
                        } else {
                            chat.remove_friend(actor, other).await
                        };
                        result.map(|()| Vec::new()).map_err(to_fault)
                    }
                    other => Err(RpcFault::internal(format!("unknown method {other}"))),
                }
            }
        })
        .await
}

/// Gateway-side stub: speaks the [`Chat`] trait by issuing NATS RPC, so it drops
/// into the gateway's `Arc<dyn Chat>` seam unchanged in a split deployment.
pub struct ChatNatsClient {
    rpc: RpcClient,
}

impl ChatNatsClient {
    #[must_use]
    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc }
    }

    async fn unit_call(&self, method: &str, req: Vec<u8>) -> Result<(), ChatError> {
        self.rpc.call(SERVICE, method, req).await.map_err(to_err)?;
        Ok(())
    }

    /// Call + decode a single protobuf response object.
    async fn one<T: prost::Message + Default>(
        &self,
        method: &str,
        req: Vec<u8>,
    ) -> Result<T, ChatError> {
        let bytes = self.rpc.call(SERVICE, method, req).await.map_err(to_err)?;
        T::decode(bytes.as_slice()).map_err(internal)
    }
}

#[async_trait::async_trait]
impl Chat for ChatNatsClient {
    async fn sync_user_state(&self, user: UserId) -> Result<UserSyncState, ChatError> {
        let req = rpc::ChatSyncReq { user: user.raw() };
        let bytes = self
            .rpc
            .call(SERVICE, "sync_user_state", req.encode_to_vec())
            .await
            .map_err(to_err)?;
        let r = rpc::ChatSyncResp::decode(bytes.as_slice()).map_err(internal)?;
        Ok(UserSyncState {
            guilds: r.guilds,
            dm_channels: r.dm_channels,
            users: r.users,
        })
    }

    async fn send_message(
        &self,
        actor: UserId,
        channel: ChannelId,
        content: String,
        reply_to: Option<MessageId>,
        attachments: Vec<MediaId>,
        nonce: u64,
    ) -> Result<v1::Message, ChatError> {
        let req = rpc::ChatSendMessageReq {
            actor: actor.raw(),
            channel: channel.raw(),
            content,
            reply_to: reply_to.map_or(0, MessageId::raw),
            attachments: attachments.iter().map(|m| m.raw()).collect(),
            nonce,
        };
        self.one("send_message", req.encode_to_vec()).await
    }

    async fn get_messages(
        &self,
        actor: UserId,
        channel: ChannelId,
        cursor: HistoryCursor,
        limit: u8,
    ) -> Result<Vec<v1::Message>, ChatError> {
        let (cursor_kind, cursor_id) = cursor_to_wire(cursor);
        let req = rpc::ChatGetMessagesReq {
            actor: actor.raw(),
            channel: channel.raw(),
            cursor_kind,
            cursor_id,
            limit: u32::from(limit),
        };
        let bytes = self
            .rpc
            .call(SERVICE, "get_messages", req.encode_to_vec())
            .await
            .map_err(to_err)?;
        Ok(rpc::ChatMessagesResp::decode(bytes.as_slice())
            .map_err(internal)?
            .messages)
    }

    async fn request_members(
        &self,
        actor: UserId,
        guild: GuildId,
        after: u64,
        limit: u8,
    ) -> Result<MemberPage, ChatError> {
        let req = rpc::ChatRequestMembersReq {
            actor: actor.raw(),
            guild: guild.raw(),
            after,
            limit: u32::from(limit),
        };
        let bytes = self
            .rpc
            .call(SERVICE, "request_members", req.encode_to_vec())
            .await
            .map_err(to_err)?;
        let r = rpc::ChatRequestMembersResp::decode(bytes.as_slice()).map_err(internal)?;
        Ok(MemberPage {
            members: r.members,
            users: r.users,
            has_more: r.has_more,
        })
    }

    async fn edit_message(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
        content: String,
    ) -> Result<v1::Message, ChatError> {
        let req = rpc::ChatEditMessageReq {
            actor: actor.raw(),
            channel: channel.raw(),
            message: message.raw(),
            content,
        };
        self.one("edit_message", req.encode_to_vec()).await
    }

    async fn delete_message(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
    ) -> Result<(), ChatError> {
        let req = rpc::ChatMessageRefReq {
            actor: actor.raw(),
            channel: channel.raw(),
            message: message.raw(),
        };
        self.unit_call("delete_message", req.encode_to_vec()).await
    }

    async fn add_reaction(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
        emoji: String,
    ) -> Result<(), ChatError> {
        let req = rpc::ChatReactionReq {
            actor: actor.raw(),
            channel: channel.raw(),
            message: message.raw(),
            emoji,
        };
        self.unit_call("add_reaction", req.encode_to_vec()).await
    }

    async fn remove_reaction(
        &self,
        actor: UserId,
        channel: ChannelId,
        message: MessageId,
        emoji: String,
    ) -> Result<(), ChatError> {
        let req = rpc::ChatReactionReq {
            actor: actor.raw(),
            channel: channel.raw(),
            message: message.raw(),
            emoji,
        };
        self.unit_call("remove_reaction", req.encode_to_vec()).await
    }

    async fn create_guild(&self, actor: UserId, name: String) -> Result<v1::Guild, ChatError> {
        let req = rpc::ChatCreateGuildReq {
            actor: actor.raw(),
            name,
        };
        self.one("create_guild", req.encode_to_vec()).await
    }

    async fn join_guild(&self, actor: UserId, code: &str) -> Result<v1::Guild, ChatError> {
        let req = rpc::ChatJoinGuildReq {
            actor: actor.raw(),
            code: code.to_owned(),
        };
        self.one("join_guild", req.encode_to_vec()).await
    }

    async fn create_channel(
        &self,
        actor: UserId,
        guild: GuildId,
        name: String,
        kind: v1::ChannelKind,
    ) -> Result<v1::Channel, ChatError> {
        let req = rpc::ChatCreateChannelReq {
            actor: actor.raw(),
            guild: guild.raw(),
            name,
            kind: kind as i32,
        };
        self.one("create_channel", req.encode_to_vec()).await
    }

    async fn open_dm(&self, actor: UserId, other: UserId) -> Result<v1::Channel, ChatError> {
        let req = rpc::ChatOpenDmReq {
            actor: actor.raw(),
            other: other.raw(),
        };
        self.one("open_dm", req.encode_to_vec()).await
    }

    async fn typing(&self, actor: UserId, channel: ChannelId) -> Result<(), ChatError> {
        let req = rpc::ChatChannelRefReq {
            actor: actor.raw(),
            channel: channel.raw(),
        };
        self.unit_call("typing", req.encode_to_vec()).await
    }

    async fn mark_read(&self, actor: UserId, channel: ChannelId) -> Result<(), ChatError> {
        let req = rpc::ChatChannelRefReq {
            actor: actor.raw(),
            channel: channel.raw(),
        };
        self.unit_call("mark_read", req.encode_to_vec()).await
    }

    async fn set_avatar(
        &self,
        actor: UserId,
        media: Option<MediaId>,
    ) -> Result<v1::User, ChatError> {
        let req = rpc::ChatSetAvatarReq {
            actor: actor.raw(),
            media: media.map_or(0, MediaId::raw),
        };
        self.one("set_avatar", req.encode_to_vec()).await
    }

    async fn list_friends(&self, actor: UserId) -> Result<v1::FriendList, ChatError> {
        let req = rpc::ChatActorReq { actor: actor.raw() };
        self.one("list_friends", req.encode_to_vec()).await
    }

    async fn add_friend(&self, actor: UserId, username: &str) -> Result<v1::Friend, ChatError> {
        let req = rpc::ChatAddFriendReq {
            actor: actor.raw(),
            username: username.to_owned(),
        };
        self.one("add_friend", req.encode_to_vec()).await
    }

    async fn accept_friend(&self, actor: UserId, other: UserId) -> Result<v1::Friend, ChatError> {
        let req = rpc::ChatFriendRefReq {
            actor: actor.raw(),
            other: other.raw(),
        };
        self.one("accept_friend", req.encode_to_vec()).await
    }

    async fn decline_friend(&self, actor: UserId, other: UserId) -> Result<(), ChatError> {
        let req = rpc::ChatFriendRefReq {
            actor: actor.raw(),
            other: other.raw(),
        };
        self.unit_call("decline_friend", req.encode_to_vec()).await
    }

    async fn remove_friend(&self, actor: UserId, other: UserId) -> Result<(), ChatError> {
        let req = rpc::ChatFriendRefReq {
            actor: actor.raw(),
            other: other.raw(),
        };
        self.unit_call("remove_friend", req.encode_to_vec()).await
    }
}

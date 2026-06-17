//! Split-mode proof: a [`Chat`] impl served over NATS round-trips through
//! [`ChatNatsClient`] exactly like a direct trait call — the sync/list wrappers,
//! `HistoryCursor` + `ChannelKind` passthrough, a unit return, and the typed
//! errors incl. `PermissionDenied` carrying the missing-permission bits. Uses a
//! mock `Chat` so it needs only live NATS (no Postgres). Skips if NATS is down.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use chat_service::rpc::{ChatNatsClient, serve};
use chat_service::{Chat, ChatError, HistoryCursor, MemberPage, UserSyncState};
use dice_common::{ChannelId, GuildId, MediaId, MessageId, UserId};
use dice_event_bus::rpc::RpcClient;
use dice_permissions::{MissingPermissions, Permissions};
use dice_protocol::v1;

const DEV_NATS: &str = "nats://localhost:4222";

/// A canned [`Chat`]: sentinel ids/strings trigger the typed errors, the rest
/// echo their inputs so the wire mapping is checkable.
struct MockChat;

#[async_trait::async_trait]
impl Chat for MockChat {
    async fn sync_user_state(&self, user: UserId) -> Result<UserSyncState, ChatError> {
        Ok(UserSyncState {
            guilds: vec![v1::Guild {
                name: "g".to_owned(),
                ..Default::default()
            }],
            dm_channels: vec![v1::Channel {
                id: 5,
                ..Default::default()
            }],
            users: vec![v1::User {
                id: user.raw(),
                ..Default::default()
            }],
        })
    }

    async fn send_message(
        &self,
        _actor: UserId,
        channel: ChannelId,
        content: String,
        reply_to: Option<MessageId>,
        _attachments: Vec<MediaId>,
        nonce: u64,
    ) -> Result<v1::Message, ChatError> {
        if channel == ChannelId::from_raw(403) {
            return Err(ChatError::PermissionDenied(MissingPermissions {
                missing: Permissions::VIEW_CHANNEL,
            }));
        }
        Ok(v1::Message {
            id: nonce,
            content,
            reply_to_id: reply_to.map_or(0, MessageId::raw),
            ..Default::default()
        })
    }

    async fn get_messages(
        &self,
        _actor: UserId,
        _channel: ChannelId,
        cursor: HistoryCursor,
        limit: u8,
    ) -> Result<Vec<v1::Message>, ChatError> {
        // Echo the cursor target id so the test can confirm it crossed the wire.
        let id = match cursor {
            HistoryCursor::Before(m) | HistoryCursor::After(m) => m.raw(),
            HistoryCursor::Latest => 0,
        };
        Ok((0..limit)
            .map(|_| v1::Message {
                id,
                ..Default::default()
            })
            .collect())
    }

    async fn request_members(
        &self,
        _actor: UserId,
        guild: GuildId,
        after: u64,
        limit: u8,
    ) -> Result<MemberPage, ChatError> {
        // Echo cursor + guild + limit so the test confirms they crossed the wire.
        Ok(MemberPage {
            members: vec![v1::Member {
                user_id: after + 1,
                guild_id: guild.raw(),
                permissions: u64::from(limit),
                ..Default::default()
            }],
            users: vec![v1::User {
                id: after + 1,
                ..Default::default()
            }],
            has_more: limit > 1,
        })
    }

    async fn get_users(
        &self,
        _actor: UserId,
        user_ids: Vec<UserId>,
    ) -> Result<Vec<v1::User>, ChatError> {
        // Echo the requested ids as user records so the test confirms they crossed.
        Ok(user_ids
            .into_iter()
            .map(|u| v1::User {
                id: u.raw(),
                ..Default::default()
            })
            .collect())
    }

    async fn edit_message(
        &self,
        _a: UserId,
        _c: ChannelId,
        _m: MessageId,
        content: String,
    ) -> Result<v1::Message, ChatError> {
        Ok(v1::Message {
            content,
            ..Default::default()
        })
    }

    async fn delete_message(
        &self,
        _a: UserId,
        _c: ChannelId,
        message: MessageId,
    ) -> Result<(), ChatError> {
        if message == MessageId::from_raw(404) {
            return Err(ChatError::NotFound);
        }
        Ok(())
    }

    async fn add_reaction(
        &self,
        _a: UserId,
        _c: ChannelId,
        _m: MessageId,
        _e: String,
    ) -> Result<(), ChatError> {
        Ok(())
    }
    async fn remove_reaction(
        &self,
        _a: UserId,
        _c: ChannelId,
        _m: MessageId,
        _e: String,
    ) -> Result<(), ChatError> {
        Ok(())
    }

    async fn create_guild(&self, _a: UserId, name: String) -> Result<v1::Guild, ChatError> {
        Ok(v1::Guild {
            name,
            ..Default::default()
        })
    }

    async fn join_guild(&self, _a: UserId, code: &str) -> Result<v1::Guild, ChatError> {
        if code == "bad" {
            return Err(ChatError::InvalidInvite);
        }
        Ok(v1::Guild::default())
    }

    async fn create_channel(
        &self,
        _a: UserId,
        _g: GuildId,
        _name: String,
        kind: v1::ChannelKind,
    ) -> Result<v1::Channel, ChatError> {
        // Echo the kind in `id` so the test confirms the enum crossed the wire.
        Ok(v1::Channel {
            id: kind as u64,
            ..Default::default()
        })
    }

    async fn open_dm(&self, _a: UserId, _o: UserId) -> Result<v1::Channel, ChatError> {
        Ok(v1::Channel::default())
    }

    async fn typing(&self, _a: UserId, _c: ChannelId) -> Result<(), ChatError> {
        Ok(())
    }
    async fn mark_read(&self, _a: UserId, _c: ChannelId) -> Result<(), ChatError> {
        Ok(())
    }

    async fn set_avatar(
        &self,
        actor: UserId,
        _media: Option<MediaId>,
    ) -> Result<v1::User, ChatError> {
        Ok(v1::User {
            id: actor.raw(),
            ..Default::default()
        })
    }

    async fn list_friends(&self, _a: UserId) -> Result<v1::FriendList, ChatError> {
        Ok(v1::FriendList::default())
    }
    async fn add_friend(&self, _a: UserId, username: &str) -> Result<v1::Friend, ChatError> {
        if username == "forbidden" {
            return Err(ChatError::Forbidden("blocked".to_owned()));
        }
        Ok(v1::Friend::default())
    }
    async fn accept_friend(&self, _a: UserId, _o: UserId) -> Result<v1::Friend, ChatError> {
        Ok(v1::Friend::default())
    }
    async fn decline_friend(&self, _a: UserId, _o: UserId) -> Result<(), ChatError> {
        Ok(())
    }
    async fn remove_friend(&self, _a: UserId, _o: UserId) -> Result<(), ChatError> {
        Ok(())
    }
}

#[tokio::test]
async fn chat_round_trips_over_nats() {
    let url = std::env::var("DICE_NATS_URL").unwrap_or_else(|_| DEV_NATS.to_owned());
    let Ok(server) = RpcClient::connect(&url).await else {
        eprintln!("skipping: live NATS required (just infra-up)");
        return;
    };
    let task = tokio::spawn(serve(server, Arc::new(MockChat)));
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = ChatNatsClient::new(RpcClient::connect(&url).await.unwrap());
    let actor = UserId::from_raw(7);

    // The sync wrapper (repeated Guild/Channel/User) round-trips.
    let sync = client.sync_user_state(actor).await.unwrap();
    assert_eq!(sync.guilds.len(), 1);
    assert_eq!(sync.guilds[0].name, "g");
    assert_eq!(sync.dm_channels[0].id, 5);
    assert_eq!(sync.users[0].id, 7);

    // A single-object response + the reply_to/nonce passthrough.
    let msg = client
        .send_message(
            actor,
            ChannelId::from_raw(10),
            "hi".to_owned(),
            Some(MessageId::from_raw(42)),
            vec![MediaId::from_raw(1)],
            99,
        )
        .await
        .unwrap();
    assert_eq!(msg.id, 99);
    assert_eq!(msg.content, "hi");
    assert_eq!(msg.reply_to_id, 42);

    // PermissionDenied carries the missing-permission bits across the wire.
    match client
        .send_message(
            actor,
            ChannelId::from_raw(403),
            "x".to_owned(),
            None,
            vec![],
            1,
        )
        .await
    {
        Err(ChatError::PermissionDenied(p)) => assert_eq!(p.missing, Permissions::VIEW_CHANNEL),
        other => panic!("expected PermissionDenied, got {other:?}"),
    }

    // The list wrapper + HistoryCursor passthrough.
    let msgs = client
        .get_messages(
            actor,
            ChannelId::from_raw(10),
            HistoryCursor::Before(MessageId::from_raw(55)),
            3,
        )
        .await
        .unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].id, 55, "cursor id crossed the wire");

    // get_users round-trips the requested ids (the mock echoes them as records).
    let users = client
        .get_users(actor, vec![UserId::from_raw(11), UserId::from_raw(22)])
        .await
        .unwrap();
    let user_ids: std::collections::HashSet<u64> = users.iter().map(|u| u.id).collect();
    assert!(
        user_ids.contains(&11) && user_ids.contains(&22),
        "user ids crossed the wire"
    );

    // ChannelKind enum passthrough.
    let chan = client
        .create_channel(
            actor,
            GuildId::from_raw(1),
            "voice".to_owned(),
            v1::ChannelKind::Voice,
        )
        .await
        .unwrap();
    assert_eq!(chan.id, v1::ChannelKind::Voice as u64);

    // More typed errors + a unit method.
    assert!(matches!(
        client.join_guild(actor, "bad").await,
        Err(ChatError::InvalidInvite)
    ));
    assert!(matches!(
        client
            .delete_message(actor, ChannelId::from_raw(1), MessageId::from_raw(404))
            .await,
        Err(ChatError::NotFound)
    ));
    match client.add_friend(actor, "forbidden").await {
        Err(ChatError::Forbidden(m)) => assert_eq!(m, "blocked"),
        other => panic!("expected Forbidden, got {other:?}"),
    }
    client.typing(actor, ChannelId::from_raw(10)).await.unwrap();

    // Lazy member page: cursor + guild + has_more round-trip through the wire.
    let page = client
        .request_members(actor, GuildId::from_raw(9), 100, 50)
        .await
        .unwrap();
    assert_eq!(page.members.len(), 1);
    assert_eq!(page.members[0].user_id, 101);
    assert_eq!(page.members[0].guild_id, 9);
    assert_eq!(page.users[0].id, 101);
    assert!(page.has_more);

    task.abort();
}

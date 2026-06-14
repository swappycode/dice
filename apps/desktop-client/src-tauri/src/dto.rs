//! Bridge DTOs: the exact shapes `apps/desktop-client/src/lib/types.ts`
//! expects over Tauri IPC. HARD CONVENTION: every id crosses IPC as a
//! `String` (u64 snowflakes overflow JS numbers); the host back-parses.

use std::collections::BTreeMap;

use dice_common::Snowflake;
use dice_protocol::v1;
use serde::{Deserialize, Serialize};

/// The single event channel the frontend listens on (`lib/ipc.ts`).
pub const EVENT_CHANNEL: &str = "dice://event";
/// Raw signal after a post-invalidation re-Ready (cache fully re-synced).
pub const RESYNC_CHANNEL: &str = "cache://resynced";

// ------------------------------------------------------------- conversions

pub fn id_str(id: u64) -> String {
    id.to_string()
}

/// Pending cache rows use negative ids; everything else is a u64 snowflake.
pub fn db_id_str(id: i64) -> String {
    id.to_string()
}

pub fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok()
}

/// Creation time derived from the snowflake (docs/protocol.md §11).
pub fn snowflake_ms(id: u64) -> u64 {
    Snowflake(id).timestamp_ms()
}

/// `dice.v1.PresenceStatus` → frontend string.
pub fn presence_str(status: i32) -> &'static str {
    match v1::PresenceStatus::try_from(status) {
        Ok(v1::PresenceStatus::Online) => "online",
        Ok(v1::PresenceStatus::Idle) => "idle",
        Ok(v1::PresenceStatus::Dnd) => "dnd",
        _ => "offline",
    }
}

pub fn parse_presence(status: &str) -> i32 {
    match status {
        "online" => v1::PresenceStatus::Online as i32,
        "idle" => v1::PresenceStatus::Idle as i32,
        "dnd" => v1::PresenceStatus::Dnd as i32,
        _ => v1::PresenceStatus::Offline as i32,
    }
}

fn kind_str(kind: i32) -> &'static str {
    match v1::ChannelKind::try_from(kind) {
        Ok(v1::ChannelKind::Dm) => "dm",
        Ok(v1::ChannelKind::Voice) => "voice",
        _ => "guild_text",
    }
}

/// `dice.v1.FriendStatus` → frontend string. (Unspecified only appears on
/// removals, where the client ignores the status, so it maps to "accepted".)
pub fn friend_status_str(status: i32) -> &'static str {
    match v1::FriendStatus::try_from(status) {
        Ok(v1::FriendStatus::PendingIncoming) => "incoming",
        Ok(v1::FriendStatus::PendingOutgoing) => "outgoing",
        _ => "accepted",
    }
}

// -------------------------------------------------------------------- DTOs

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserDto {
    pub id: String,
    pub username: String,
    pub display_name: String,
    /// Avatar media id (fetch bytes via the same path as attachments); None =
    /// no avatar (render initials).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_id: Option<String>,
}

impl From<&v1::User> for UserDto {
    fn from(u: &v1::User) -> Self {
        Self {
            id: id_str(u.id),
            username: u.username.clone(),
            display_name: if u.display_name.is_empty() {
                u.username.clone()
            } else {
                u.display_name.clone()
            },
            avatar_id: (u.avatar_id != 0).then(|| id_str(u.avatar_id)),
        }
    }
}

/// One row of the Friends page: the other user + the relationship from the
/// caller's side (`"incoming" | "outgoing" | "accepted"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendDto {
    pub user: UserDto,
    pub status: String,
}

impl From<&v1::Friend> for FriendDto {
    fn from(f: &v1::Friend) -> Self {
        Self {
            user: f
                .user
                .as_ref()
                .map(UserDto::from)
                .unwrap_or_else(|| UserDto {
                    id: "0".to_owned(),
                    username: String::new(),
                    display_name: String::new(),
                    avatar_id: None,
                }),
            status: friend_status_str(f.status).to_owned(),
        }
    }
}

/// One participant in a voice channel (signaling state; audio is separate).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceMemberDto {
    pub user_id: String,
    pub channel_id: String,
    pub guild_id: String,
    pub ssrc: u32,
    pub muted: bool,
    pub deafened: bool,
    pub speaking: bool,
}

impl From<&v1::VoiceMember> for VoiceMemberDto {
    fn from(m: &v1::VoiceMember) -> Self {
        Self {
            user_id: id_str(m.user_id),
            channel_id: id_str(m.channel_id),
            guild_id: id_str(m.guild_id),
            ssrc: m.ssrc,
            muted: m.muted,
            deafened: m.deafened,
            speaking: m.speaking,
        }
    }
}

/// A voice channel's current roster + a warm user dictionary for its members.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceRosterDto {
    pub channel_id: String,
    pub members: Vec<VoiceMemberDto>,
    pub users: Vec<UserDto>,
}

impl From<&v1::VoiceRoster> for VoiceRosterDto {
    fn from(r: &v1::VoiceRoster) -> Self {
        Self {
            channel_id: id_str(r.channel_id),
            members: r.members.iter().map(VoiceMemberDto::from).collect(),
            users: r.users.iter().map(UserDto::from).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelDto {
    pub id: String,
    pub guild_id: Option<String>,
    pub kind: String, // "guild_text" | "dm" | "voice"
    pub name: String,
    pub position: u32,
    pub last_message_id: Option<String>,
    pub recipient_ids: Vec<String>,
}

impl From<&v1::Channel> for ChannelDto {
    fn from(c: &v1::Channel) -> Self {
        Self {
            id: id_str(c.id),
            guild_id: (c.guild_id != 0).then(|| id_str(c.guild_id)),
            kind: kind_str(c.kind).to_owned(),
            name: c.name.clone(),
            position: c.position,
            last_message_id: (c.last_message_id != 0).then(|| id_str(c.last_message_id)),
            recipient_ids: c.recipient_ids.iter().map(|&r| id_str(r)).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberDto {
    pub user_id: String,
    pub guild_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildDto {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub invite_code: String,
    pub members: Vec<MemberDto>,
}

impl From<&v1::Guild> for GuildDto {
    fn from(g: &v1::Guild) -> Self {
        Self {
            id: id_str(g.id),
            name: g.name.clone(),
            owner_id: id_str(g.owner_id),
            invite_code: g.invite_code.clone(),
            members: g
                .members
                .iter()
                .map(|m| MemberDto {
                    user_id: id_str(m.user_id),
                    guild_id: id_str(g.id),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReactionDto {
    pub emoji: String,
    pub count: u32,
    pub me: bool,
}

impl From<&v1::Reaction> for ReactionDto {
    fn from(r: &v1::Reaction) -> Self {
        Self {
            emoji: r.emoji.clone(),
            count: r.count,
            me: r.me,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDto {
    pub id: String,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: u64,
    /// 0 for non-images (the frontend uses these to reserve layout space).
    pub width: u32,
    pub height: u32,
}

impl From<&v1::Attachment> for AttachmentDto {
    fn from(a: &v1::Attachment) -> Self {
        Self {
            id: id_str(a.id),
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            size_bytes: a.size_bytes,
            width: a.width,
            height: a.height,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDto {
    pub id: String,
    pub channel_id: String,
    pub author_id: String,
    pub content: String,
    pub created_at_ms: u64,
    pub edited_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<ReactionDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
}

impl MessageDto {
    pub fn from_wire(m: &v1::Message, nonce: Option<String>) -> Self {
        Self {
            id: id_str(m.id),
            channel_id: id_str(m.channel_id),
            author_id: id_str(m.author_id),
            content: m.content.clone(),
            created_at_ms: snowflake_ms(m.id),
            edited_at_ms: (m.edited_at_ms != 0).then_some(m.edited_at_ms),
            reply_to_id: (m.reply_to_id != 0).then(|| id_str(m.reply_to_id)),
            reactions: m.reactions.iter().map(ReactionDto::from).collect(),
            attachments: m.attachments.iter().map(AttachmentDto::from).collect(),
            nonce,
            pending: None,
            failed: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreadDto {
    pub channel_id: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDto {
    pub user: UserDto,
}

/// `login` result: exactly one field is set — a `session` (fully authenticated)
/// or a `totpTicket` (the account has 2FA; answer via `completeTotpLogin`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResultDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totp_ticket: Option<String>,
}

/// A fresh 2FA enrollment for the settings UI: the secret (manual entry) + the
/// `otpauth://` URI (rendered as a QR).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotpEnrollDto {
    pub secret: String,
    pub otpauth_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapDto {
    pub user: UserDto,
    pub guilds: Vec<GuildDto>,
    pub channels: Vec<ChannelDto>,
    pub dms: Vec<ChannelDto>,
    pub users: Vec<UserDto>,
    pub presence: BTreeMap<String, String>,
    pub last_channel_id: Option<String>,
}

/// The `DiceEvent` union of `lib/types.ts` (serde-tagged on `type`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DiceEvent {
    #[serde(rename_all = "camelCase")]
    MessageCreate {
        message: MessageDto,
        #[serde(skip_serializing_if = "Option::is_none")]
        nonce: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    MessageUpdate { message: MessageDto },
    #[serde(rename_all = "camelCase")]
    MessageDelete {
        channel_id: String,
        message_id: String,
    },
    #[serde(rename_all = "camelCase")]
    ReactionUpdate {
        channel_id: String,
        message_id: String,
        emoji: String,
        user_id: String,
        added: bool,
    },
    #[serde(rename_all = "camelCase")]
    TypingStart { channel_id: String, user_id: String },
    #[serde(rename_all = "camelCase")]
    PresenceUpdate { user_id: String, status: String },
    #[serde(rename_all = "camelCase")]
    UserUpdate { user: UserDto },
    #[serde(rename_all = "camelCase")]
    ReadMarkerUpdate {
        channel_id: String,
        last_read_message_id: String,
    },
    /// A friendship changed for the caller. `removed` ⇒ drop it by `friend.user.id`;
    /// otherwise upsert the friend by id with its status.
    #[serde(rename_all = "camelCase")]
    FriendUpdate { friend: FriendDto, removed: bool },
    /// A user joined a voice channel; `user` carries their record (warm dict).
    #[serde(rename_all = "camelCase")]
    VoiceJoin {
        member: VoiceMemberDto,
        #[serde(skip_serializing_if = "Option::is_none")]
        user: Option<UserDto>,
    },
    /// A user left a voice channel.
    #[serde(rename_all = "camelCase")]
    VoiceLeave {
        channel_id: String,
        user_id: String,
        guild_id: String,
    },
    /// A voice member's mute / deafen / speaking state changed.
    #[serde(rename_all = "camelCase")]
    VoiceState { member: VoiceMemberDto },
    #[serde(rename_all = "camelCase")]
    GuildCreate {
        guild: GuildDto,
        channels: Vec<ChannelDto>,
    },
    #[serde(rename_all = "camelCase")]
    DmChannelCreate {
        channel: ChannelDto,
        users: Vec<UserDto>,
    },
    #[serde(rename_all = "camelCase")]
    ConnState {
        state: String,
        /// Active transport ("quic"/"wss"); only set while connected.
        #[serde(skip_serializing_if = "Option::is_none")]
        transport: Option<String>,
    },
    /// The stored session was rejected by the server (terminal). The host has
    /// already cleared credentials + cache; the webview must route to login.
    #[serde(rename_all = "camelCase")]
    SessionExpired,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn dice_event_serializes_to_the_ts_shape() {
        let ev = DiceEvent::TypingStart {
            channel_id: "42".into(),
            user_id: "7".into(),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "typingStart");
        assert_eq!(json["channelId"], "42");
        assert_eq!(json["userId"], "7");

        let conn = serde_json::to_value(DiceEvent::ConnState {
            state: "connected".into(),
            transport: Some("quic".into()),
        })
        .unwrap();
        assert_eq!(conn["type"], "connState");
        assert_eq!(conn["state"], "connected");
        assert_eq!(conn["transport"], "quic");
        let idle = serde_json::to_value(DiceEvent::ConnState {
            state: "idle".into(),
            transport: None,
        })
        .unwrap();
        assert!(idle.get("transport").is_none());

        let expired = serde_json::to_value(DiceEvent::SessionExpired).unwrap();
        assert_eq!(expired["type"], "sessionExpired");
    }

    #[test]
    fn message_dto_derives_created_at_and_hides_empty_optionals() {
        let id = (1234u64 << 22) | 5;
        let m = v1::Message {
            id,
            channel_id: 9,
            author_id: 3,
            content: "hi".into(),
            edited_at_ms: 0,
            reply_to_id: 0,
            reactions: Vec::new(),
            attachments: Vec::new(),
        };
        let dto = MessageDto::from_wire(&m, None);
        assert_eq!(dto.created_at_ms, 1234 + dice_common::time::DICE_EPOCH_MS);
        let json = serde_json::to_value(&dto).unwrap();
        assert!(json.get("nonce").is_none());
        assert!(json.get("pending").is_none());
        assert_eq!(json["editedAtMs"], serde_json::Value::Null);
        assert_eq!(json["channelId"], "9");
    }

    #[test]
    fn presence_round_trips() {
        for s in ["online", "idle", "dnd", "offline"] {
            assert_eq!(presence_str(parse_presence(s)), s);
        }
        assert_eq!(presence_str(0), "offline", "unspecified maps to offline");
    }

    #[test]
    fn channel_dto_maps_dm_and_guild_shapes() {
        let dm = v1::Channel {
            id: 5,
            guild_id: 0,
            kind: v1::ChannelKind::Dm as i32,
            name: String::new(),
            position: 0,
            last_message_id: 0,
            recipient_ids: vec![1, 2],
        };
        let dto = ChannelDto::from(&dm);
        assert_eq!(dto.kind, "dm");
        assert!(dto.guild_id.is_none());
        assert!(dto.last_message_id.is_none());
        assert_eq!(dto.recipient_ids, vec!["1", "2"]);
    }
}

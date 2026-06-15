/**
 * Bridge DTOs for the UI layer.
 *
 * HARD CONVENTION: every id is a `string`. Snowflakes are u64 and overflow
 * JS numbers; the Tauri bridge stringifies ids on the way in and the host
 * back-parses them. The mock follows the same contract.
 *
 * Shapes mirror docs/protocol.md §10–11 (dice.v1 entities), camelCased.
 */

export type PresenceStatus = "online" | "idle" | "dnd" | "offline";

export type ChannelKind = "guild_text" | "dm" | "voice";

/** Available capture/playback devices + the system defaults (voice settings). */
export interface AudioDevices {
  inputs: string[];
  outputs: string[];
  defaultInput: string | null;
  defaultOutput: string | null;
}

export interface User {
  id: string;
  username: string;
  displayName: string;
  avatarId?: string | null; // media id; fetch via ipc.attachmentSrc(id). null = initials
}

export interface Channel {
  id: string;
  guildId: string | null; // null ⇒ DM channel
  kind: ChannelKind;
  name: string; // empty for DM channels
  position: number;
  lastMessageId: string | null;
  recipientIds: string[]; // DM only
}

export interface Member {
  userId: string;
  guildId: string;
}

export interface Guild {
  id: string;
  name: string;
  ownerId: string;
  inviteCode: string;
  members: Member[];
}

export interface Reaction {
  emoji: string;
  count: number;
  me: boolean; // this user reacted with this emoji
}

export interface Attachment {
  id: string; // media id; bytes fetched via ipc.attachmentSrc(id)
  filename: string;
  contentType: string; // MIME
  sizeBytes: number;
  width: number; // 0 for non-images (used to reserve layout space)
  height: number;
}

export interface Message {
  id: string;
  channelId: string;
  authorId: string;
  content: string;
  createdAtMs: number; // derived from the snowflake by the bridge/mock
  editedAtMs: number | null;
  replyToId?: string | null; // parent message id (may be uncached/deleted)
  reactions?: Reaction[];
  attachments?: Attachment[];
  nonce?: string; // present on optimistic pending rows + their echoes
  pending?: boolean; // optimistic row not yet acked
  failed?: boolean;
}

export interface Session {
  user: User;
}

/** A friendship from the caller's point of view. */
export type FriendStatus = "incoming" | "outgoing" | "accepted";
export interface Friend {
  user: User;
  status: FriendStatus;
}

/** A participant in a voice channel (signaling state; audio is separate). */
export interface VoiceMember {
  userId: string;
  channelId: string;
  guildId: string;
  ssrc: number;
  muted: boolean;
  deafened: boolean;
  speaking: boolean;
}

/** A voice channel's current roster + user records for its members. */
export interface VoiceRoster {
  channelId: string;
  members: VoiceMember[];
  users: User[];
}

/** Result of `ipc.login`: either a `session` (no 2FA) or a `totpTicket` to
 *  answer the 2FA challenge with `ipc.completeTotpLogin`. Exactly one is set. */
export interface LoginResult {
  session?: Session;
  totpTicket?: string;
}

/** A fresh 2FA enrollment for the settings UI. */
export interface TotpEnroll {
  secret: string; // base32, for manual entry
  otpauthUri: string; // otpauth://totp/... rendered as a QR
}

export interface Bootstrap {
  user: User;
  guilds: Guild[];
  channels: Channel[]; // guild channels, all guilds
  dms: Channel[];
  users: User[]; // everyone referenced by members/DMs/messages
  presence: Record<string, PresenceStatus>;
  lastChannelId: string | null;
}

export type ConnState =
  | "idle"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "offline";

/* ---- Event stream (gateway → UI), the seam the Tauri bridge fills later ---- */

export type DiceEvent =
  | { type: "messageCreate"; message: Message; nonce?: string }
  | { type: "messageUpdate"; message: Message }
  | { type: "messageDelete"; channelId: string; messageId: string }
  | {
      type: "reactionUpdate";
      channelId: string;
      messageId: string;
      emoji: string;
      userId: string;
      added: boolean;
    }
  | { type: "typingStart"; channelId: string; userId: string }
  | { type: "presenceUpdate"; userId: string; status: PresenceStatus }
  | { type: "userUpdate"; user: User }
  | { type: "readMarkerUpdate"; channelId: string; lastReadMessageId: string }
  | { type: "guildCreate"; guild: Guild; channels: Channel[] }
  | { type: "channelCreate"; channel: Channel }
  | { type: "dmChannelCreate"; channel: Channel; users: User[] }
  | { type: "friendUpdate"; friend: Friend; removed: boolean }
  | { type: "voiceJoin"; member: VoiceMember; user?: User }
  | { type: "voiceLeave"; channelId: string; userId: string; guildId: string }
  | { type: "voiceState"; member: VoiceMember }
  | { type: "connState"; state: ConnState; transport?: "quic" | "wss" | null }
  | { type: "sessionExpired" };

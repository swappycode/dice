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

export type ChannelKind = "guild_text" | "dm";

export interface User {
  id: string;
  username: string;
  displayName: string;
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

export interface Message {
  id: string;
  channelId: string;
  authorId: string;
  content: string;
  createdAtMs: number; // derived from the snowflake by the bridge/mock
  editedAtMs: number | null;
  replyToId?: string | null; // parent message id (may be uncached/deleted)
  reactions?: Reaction[];
  nonce?: string; // present on optimistic pending rows + their echoes
  pending?: boolean; // optimistic row not yet acked
  failed?: boolean;
}

export interface Session {
  user: User;
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
  | { type: "guildCreate"; guild: Guild; channels: Channel[] }
  | { type: "dmChannelCreate"; channel: Channel; users: User[] }
  | { type: "connState"; state: ConnState; transport?: "quic" | "wss" | null }
  | { type: "sessionExpired" };

import { createStore, produce } from "solid-js/store";
import { ipc } from "../lib/ipc";
import { directory } from "./guilds";

/** Per-channel unread counts that drive badges. Seeded from the server on
 *  boot (`ipc.fetchUnread`), bumped live by the dispatcher for messages in
 *  non-active channels, cleared when a channel is opened. */
const [unread, setUnread] = createStore<Record<string, number>>({});

/** Per-channel last-read message id (snowflake string), for the unread divider.
 *  Seeded from the bootstrap (`Ready.read_markers`) and advanced by
 *  `ReadMarkerUpdate` dispatches (this device or another). */
const [readMarkers, setReadMarkers] = createStore<Record<string, string>>({});

export function unreadCount(channelId: string): number {
  return unread[channelId] ?? 0;
}

/** The user's last-read message id in `channelId`, or null if never read. */
export function lastReadMessageId(channelId: string): string | null {
  return readMarkers[channelId] ?? null;
}

/** Replace the whole read-marker map (boot / resync). */
export function setAllReadMarkers(map: Record<string, string>): void {
  setReadMarkers(
    produce((s) => {
      for (const k of Object.keys(s)) delete s[k];
      Object.assign(s, map);
    }),
  );
}

/** Advance one channel's last-read pointer (from a ReadMarkerUpdate). */
export function setReadMarker(channelId: string, messageId: string): void {
  setReadMarkers(produce((s) => (s[channelId] = messageId)));
}

/** Replace the whole map (boot / resync). */
export function setAllUnread(map: Record<string, number>): void {
  setUnread(
    produce((s) => {
      for (const k of Object.keys(s)) delete s[k];
      Object.assign(s, map);
    }),
  );
}

export function bumpUnread(channelId: string): void {
  setUnread(produce((s) => (s[channelId] = (s[channelId] ?? 0) + 1)));
}

export function clearUnread(channelId: string): void {
  setUnread(produce((s) => void delete s[channelId]));
}

/** Open/read a channel: clear the badge locally and on the server. */
export function markChannelRead(channelId: string): void {
  clearUnread(channelId);
  void ipc.markRead(channelId).catch(() => {});
}

/** True when any channel in the guild has unread messages (rail dot). */
export function guildHasUnread(guildId: string): boolean {
  return (directory.channelsByGuild[guildId] ?? []).some((c) => (unread[c.id] ?? 0) > 0);
}

export function resetUnread(): void {
  setUnread(produce((s) => void Object.keys(s).forEach((k) => delete s[k])));
  setReadMarkers(produce((s) => void Object.keys(s).forEach((k) => delete s[k])));
}

export { unread };

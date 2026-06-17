/**
 * Single onEvent() registration → fine-grained store updates.
 * This is the only place gateway events touch state; the Tauri bridge
 * will feed the exact same DiceEvent stream later.
 */

import { playChime } from "../lib/chime";
import { ipc } from "../lib/ipc";
import type { DiceEvent, Message } from "../lib/types";
import { setConnState, setTransport } from "../stores/connection";
import {
  addChannel,
  addDm,
  addGuild,
  applyBootstrap,
  applyMemberChunk,
  applyUserUpdate,
  displayName,
  resetDirectory,
  selectedChannelId,
} from "../stores/guilds";
import {
  applyMessageCreate,
  applyMessageDelete,
  applyMessageUpdate,
  applyReactionDelta,
  resetMessages,
} from "../stores/messages";
import { setHomeTab } from "../components/home/homeTab";
import { applyFriendUpdate, loadFriends, resetFriends } from "../stores/friends";
import { applyVoiceJoin, applyVoiceLeave, applyVoiceState, resetVoice } from "../stores/voice";
import { loadPresence, resetPresence, setPresenceLocal } from "../stores/presence";
import {
  bumpUnread,
  clearUnread,
  markChannelRead,
  resetUnread,
  setAllUnread,
} from "../stores/unread";
import { currentUser, session, setLoginNotice, setSession } from "../stores/session";
import { clearTyping, noteTyping } from "../stores/typing";

/** Collapse whitespace + cap length for an OS-toast body. */
function snippet(s: string): string {
  const one = s.replace(/\s+/g, " ").trim();
  return one.length > 120 ? `${one.slice(0, 117)}…` : one;
}

/** Chime + (background-only) OS toast for an incoming message. */
function notifyNewMessage(m: Message): void {
  playChime();
  // Don't toast over an app you're actively looking at — the chime + badge
  // already covered that. Toast only when the window is in the background.
  if (typeof document !== "undefined" && document.hasFocus()) return;
  void ipc.notify(displayName(m.authorId), snippet(m.content) || "Sent an attachment");
}

function dispatch(ev: DiceEvent): void {
  switch (ev.type) {
    case "messageCreate": {
      applyMessageCreate(ev.message, ev.nonce);
      clearTyping(ev.message.channelId, ev.message.authorId);
      // The author never notifies themselves.
      if (ev.message.authorId !== currentUser()?.id) {
        const elsewhere = ev.message.channelId !== selectedChannelId();
        // Badge any non-active channel.
        if (elsewhere) bumpUnread(ev.message.channelId);
        // Chime + toast unless it's the channel you're actively viewing.
        if (elsewhere || !document.hasFocus()) notifyNewMessage(ev.message);
      }
      break;
    }
    case "messageUpdate":
      applyMessageUpdate(ev.message);
      break;
    case "messageDelete":
      applyMessageDelete(ev.channelId, ev.messageId);
      break;
    case "reactionUpdate":
      applyReactionDelta(
        ev.channelId,
        ev.messageId,
        ev.emoji,
        ev.added,
        ev.userId === currentUser()?.id,
      );
      break;
    case "typingStart":
      if (ev.userId !== currentUser()?.id) noteTyping(ev.channelId, ev.userId);
      break;
    case "presenceUpdate":
      setPresenceLocal(ev.userId, ev.status);
      break;
    case "readMarkerUpdate":
      // Another of this user's devices read the channel — clear its badge here.
      clearUnread(ev.channelId);
      break;
    case "userUpdate":
      applyUserUpdate(ev.user);
      // Keep the session's own user (SelfStrip reads it) in sync too.
      if (ev.user.id === currentUser()?.id) {
        const cur = session();
        if (cur) setSession({ ...cur, user: { ...cur.user, ...ev.user } });
      }
      break;
    case "guildCreate":
      addGuild(ev.guild, ev.channels);
      break;
    case "guildMembers": {
      applyMemberChunk(ev.guildId, ev.members, ev.users);
      // Page the rest by user_id until the server reports no more.
      const last = ev.members[ev.members.length - 1];
      if (ev.hasMore && last) {
        void ipc.requestGuildMembers(ev.guildId, last.userId, 100);
      }
      break;
    }
    case "channelCreate":
      addChannel(ev.channel);
      break;
    case "dmChannelCreate":
      addDm(ev.channel, ev.users);
      break;
    case "friendUpdate":
      applyFriendUpdate(ev.friend, ev.removed);
      break;
    case "voiceJoin":
      applyVoiceJoin(ev.member, ev.user);
      break;
    case "voiceLeave":
      applyVoiceLeave(ev.channelId, ev.userId, ev.userId === currentUser()?.id);
      break;
    case "voiceState":
      applyVoiceState(ev.member);
      break;
    case "connState":
      setConnState(ev.state);
      setTransport(ev.state === "connected" ? (ev.transport ?? null) : null);
      break;
    case "sessionExpired":
      // The host already cleared credentials + cache; drop to login cleanly
      // instead of stranding the user on an "Offline" shell (Issue 1).
      resetClientState();
      setConnState("idle");
      setTransport(null);
      setLoginNotice("Your session expired. Please log in again.");
      setSession(null);
      break;
  }
}

/** Wipe all per-account client stores. The ONE place both logout paths (the
 *  SelfStrip "Log off" button and the sessionExpired dispatch) clear state, so
 *  a newly-added store can't be forgotten in one path and leak across accounts. */
export function resetClientState(): void {
  resetMessages();
  resetPresence();
  resetDirectory();
  resetUnread();
  resetFriends();
  resetVoice();
  setHomeTab("messages");
}

export function installDispatcher(): () => void {
  return ipc.onEvent(dispatch);
}

/** Hydrate the directory stores after login / session resume. */
export async function runBootstrap(): Promise<void> {
  const b = await ipc.getBootstrap();
  applyBootstrap(b);
  loadPresence(b.presence);
  try {
    setAllUnread(await ipc.fetchUnread());
    // The channel you're already viewing isn't unread.
    const sel = selectedChannelId();
    if (sel) markChannelRead(sel);
  } catch {
    /* offline / no server: badges accrue live */
  }
  try {
    await loadFriends();
  } catch {
    /* offline / no server: the Friends page reloads on open */
  }
}

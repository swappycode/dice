/**
 * Single onEvent() registration → fine-grained store updates.
 * This is the only place gateway events touch state; the Tauri bridge
 * will feed the exact same DiceEvent stream later.
 */

import { ipc } from "../lib/ipc";
import type { DiceEvent } from "../lib/types";
import { setConnState, setTransport } from "../stores/connection";
import { addDm, addGuild, applyBootstrap, resetDirectory } from "../stores/guilds";
import {
  applyMessageCreate,
  applyMessageDelete,
  applyMessageUpdate,
  applyReactionDelta,
  resetMessages,
} from "../stores/messages";
import { loadPresence, resetPresence, setPresenceLocal } from "../stores/presence";
import { currentUser, setLoginNotice, setSession } from "../stores/session";
import { clearTyping, noteTyping } from "../stores/typing";

function dispatch(ev: DiceEvent): void {
  switch (ev.type) {
    case "messageCreate":
      applyMessageCreate(ev.message, ev.nonce);
      clearTyping(ev.message.channelId, ev.message.authorId);
      break;
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
    case "guildCreate":
      addGuild(ev.guild, ev.channels);
      break;
    case "dmChannelCreate":
      addDm(ev.channel, ev.users);
      break;
    case "connState":
      setConnState(ev.state);
      setTransport(ev.state === "connected" ? (ev.transport ?? null) : null);
      break;
    case "sessionExpired":
      // The host already cleared credentials + cache; drop to login cleanly
      // instead of stranding the user on an "Offline" shell (Issue 1).
      resetMessages();
      resetPresence();
      resetDirectory();
      setConnState("idle");
      setTransport(null);
      setLoginNotice("Your session expired. Please log in again.");
      setSession(null);
      break;
  }
}

export function installDispatcher(): () => void {
  return ipc.onEvent(dispatch);
}

/** Hydrate the directory stores after login / session resume. */
export async function runBootstrap(): Promise<void> {
  const b = await ipc.getBootstrap();
  applyBootstrap(b);
  loadPresence(b.presence);
}

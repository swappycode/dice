/**
 * Single onEvent() registration → fine-grained store updates.
 * This is the only place gateway events touch state; the Tauri bridge
 * will feed the exact same DiceEvent stream later.
 */

import { ipc } from "../lib/ipc";
import type { DiceEvent } from "../lib/types";
import { setConnState, setTransport } from "../stores/connection";
import { addDm, addGuild, applyBootstrap } from "../stores/guilds";
import { applyMessageCreate } from "../stores/messages";
import { loadPresence, setPresenceLocal } from "../stores/presence";
import { currentUser } from "../stores/session";
import { clearTyping, noteTyping } from "../stores/typing";

function dispatch(ev: DiceEvent): void {
  switch (ev.type) {
    case "messageCreate":
      applyMessageCreate(ev.message, ev.nonce);
      clearTyping(ev.message.channelId, ev.message.authorId);
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

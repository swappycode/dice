/**
 * Friends store: the caller's friends + pending requests, keyed by the other
 * user's id. Hydrated by `loadFriends()` at bootstrap; kept live by the
 * `friendUpdate` dispatch. Accepted friends are grouped by presence for the
 * Discord-home-style list (presenceOf is reactive, so the grouping re-renders
 * as friends come and go online).
 */

import { createStore, produce, reconcile } from "solid-js/store";
import { ipc } from "../lib/ipc";
import type { Friend, PresenceStatus } from "../lib/types";
import { presenceOf } from "./presence";

const [friends, setFriends] = createStore<Record<string, Friend>>({});

/** Replace the whole set from the server (bootstrap / resync). */
export async function loadFriends(): Promise<void> {
  const list = await ipc.listFriends();
  const map: Record<string, Friend> = {};
  for (const f of list) map[f.user.id] = f;
  setFriends(reconcile(map));
}

/** Apply a live `friendUpdate`: drop on removal, else upsert by user id. */
export function applyFriendUpdate(friend: Friend, removed: boolean): void {
  setFriends(
    produce((s) => {
      if (removed) delete s[friend.user.id];
      else s[friend.user.id] = friend;
    }),
  );
}

export function resetFriends(): void {
  setFriends(reconcile({}));
}

export function incomingRequests(): Friend[] {
  return Object.values(friends).filter((f) => f.status === "incoming");
}

export function outgoingRequests(): Friend[] {
  return Object.values(friends).filter((f) => f.status === "outgoing");
}

/** Number of incoming requests — drives the Friends-tab badge. */
export function incomingCount(): number {
  return incomingRequests().length;
}

const PRESENCE_ORDER: PresenceStatus[] = ["online", "idle", "dnd", "offline"];

/** Accepted friends grouped Online → Idle → DND → Offline (empty groups dropped). */
export function acceptedByPresence(): Array<{ status: PresenceStatus; friends: Friend[] }> {
  const accepted = Object.values(friends).filter((f) => f.status === "accepted");
  return PRESENCE_ORDER.map((status) => ({
    status,
    friends: accepted
      .filter((f) => presenceOf(f.user.id)() === status)
      .sort((a, b) => a.user.displayName.localeCompare(b.user.displayName)),
  })).filter((group) => group.friends.length > 0);
}

export { friends };

import { createSignal, type Accessor, type Setter } from "solid-js";
import type { PresenceStatus } from "../lib/types";

/**
 * Map<userId, Signal> — fine-grained per-user presence. A presenceUpdate
 * touches exactly one signal, so only that user's orb re-renders.
 * Presence is ephemeral (never persisted).
 */
const signals = new Map<string, [Accessor<PresenceStatus>, Setter<PresenceStatus>]>();

function entry(userId: string): [Accessor<PresenceStatus>, Setter<PresenceStatus>] {
  let s = signals.get(userId);
  if (!s) {
    s = createSignal<PresenceStatus>("offline");
    signals.set(userId, s);
  }
  return s;
}

/** Reactive accessor for one user's status (defaults to offline). */
export function presenceOf(userId: string): Accessor<PresenceStatus> {
  return entry(userId)[0];
}

export function setPresenceLocal(userId: string, status: PresenceStatus): void {
  entry(userId)[1](status);
}

export function loadPresence(initial: Record<string, PresenceStatus>): void {
  for (const [userId, status] of Object.entries(initial)) setPresenceLocal(userId, status);
}

export function resetPresence(): void {
  for (const [, set] of signals.values()) set("offline");
}

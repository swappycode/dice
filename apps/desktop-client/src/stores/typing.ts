import { createEffect, createRoot, createSignal } from "solid-js";
import { selectedChannelId } from "./guilds";

/**
 * Per-channel Map<userId, expiresAt> with a 10 s TTL (protocol.md §6).
 * The expiry sweep runs ONLY while the active channel has live typers —
 * zero timers when nobody is typing.
 */
const TTL_MS = 10_000;
const SWEEP_MS = 1_000;

const channelTypers = new Map<string, Map<string, number>>();

const [typingUserIds, setTypingUserIds] = createSignal<string[]>([]);

let sweepTimer: ReturnType<typeof setInterval> | null = null;

function recompute(): void {
  const active = selectedChannelId();
  const map = active ? channelTypers.get(active) : undefined;
  const nowMs = Date.now();
  const live: string[] = [];
  if (map) {
    for (const [userId, expires] of map) {
      if (expires > nowMs) live.push(userId);
      else map.delete(userId);
    }
  }
  setTypingUserIds((prev) =>
    prev.length === live.length && prev.every((id, i) => id === live[i]) ? prev : live,
  );
  if (live.length && sweepTimer === null) {
    sweepTimer = setInterval(recompute, SWEEP_MS);
  } else if (!live.length && sweepTimer !== null) {
    clearInterval(sweepTimer);
    sweepTimer = null;
  }
}

export function noteTyping(channelId: string, userId: string): void {
  let map = channelTypers.get(channelId);
  if (!map) {
    map = new Map();
    channelTypers.set(channelId, map);
  }
  map.set(userId, Date.now() + TTL_MS);
  if (channelId === selectedChannelId()) recompute();
}

/** A message from a typer ends their indicator immediately. */
export function clearTyping(channelId: string, userId: string): void {
  const map = channelTypers.get(channelId);
  if (map?.delete(userId) && channelId === selectedChannelId()) recompute();
}

/** Re-evaluate when the active channel changes (call once from main). */
export function installTypingSweep(): void {
  createRoot(() => {
    createEffect(() => {
      selectedChannelId();
      recompute();
    });
  });
}

export { typingUserIds };

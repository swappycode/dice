import { createSignal } from "solid-js";
import { createStore, produce } from "solid-js/store";
import type { Message } from "../lib/types";

/** Per-channel ascending message arrays, capped at 200 in memory. */
const MAX_PER_CHANNEL = 200;

const [byChannel, setByChannel] = createStore<Record<string, Message[]>>({});

/** The message the composer is currently replying to (null = not replying). */
const [replyTarget, setReplyTarget] = createSignal<Message | null>(null);
export { replyTarget, setReplyTarget };

/** Look up a (cached) message by id within a channel — for reply previews. */
export function messageById(channelId: string, messageId: string): Message | undefined {
  return byChannel[channelId]?.find((m) => m.id === messageId);
}

/** Channels whose newest page has been fetched (non-reactive bookkeeping). */
const fetchedChannels = new Set<string>();

export function messagesFor(channelId: string): Message[] {
  return byChannel[channelId] ?? [];
}

export function isFetched(channelId: string): boolean {
  return fetchedChannels.has(channelId);
}

/** Insert an optimistic pending row (caller already generated the nonce). */
export function addPending(m: Message): void {
  setByChannel(
    produce((s) => {
      (s[m.channelId] ??= []).push(m);
    }),
  );
}

/**
 * Apply a messageCreate event. If a nonce matches a pending row, the echo
 * replaces it in place (reconcile-by-nonce); otherwise append + cap, deduped
 * by id (ack/dispatch can both arrive — protocol.md §7).
 */
export function applyMessageCreate(m: Message, nonce?: string): void {
  setByChannel(
    produce((s) => {
      const arr = (s[m.channelId] ??= []);
      if (nonce) {
        const i = arr.findIndex((x) => x.pending && x.nonce === nonce);
        if (i >= 0) {
          arr[i] = m;
          return;
        }
      }
      if (arr.some((x) => x.id === m.id)) return;
      arr.push(m);
      if (arr.length > MAX_PER_CHANNEL) arr.splice(0, arr.length - MAX_PER_CHANNEL);
    }),
  );
}

/** Merge the newest fetched page under any rows that arrived live meanwhile. */
export function applyFetchedPage(channelId: string, page: Message[]): void {
  fetchedChannels.add(channelId);
  setByChannel(
    produce((s) => {
      const existing = s[channelId] ?? [];
      const seen = new Set(existing.map((m) => m.id));
      const merged = [...page.filter((m) => !seen.has(m.id)), ...existing];
      merged.sort((a, b) => a.createdAtMs - b.createdAtMs);
      s[channelId] = merged.slice(-MAX_PER_CHANNEL);
    }),
  );
}

/** Prepend an older history page ("Load older"). Returns rows actually added. */
export function prependOlder(channelId: string, page: Message[]): number {
  let added = 0;
  setByChannel(
    produce((s) => {
      const existing = s[channelId] ?? [];
      const seen = new Set(existing.map((m) => m.id));
      const fresh = page.filter((m) => !seen.has(m.id));
      added = fresh.length;
      if (!fresh.length) return;
      const merged = [...fresh, ...existing];
      merged.sort((a, b) => a.createdAtMs - b.createdAtMs);
      s[channelId] = merged.slice(-MAX_PER_CHANNEL);
    }),
  );
  return added;
}

export function oldestMessageId(channelId: string): string | null {
  const arr = byChannel[channelId];
  const first = arr?.find((m) => !m.pending);
  return first ? first.id : null;
}

/** Apply a messageUpdate (edit): MERGE content + editedAtMs, PRESERVING the
 *  cached replyToId and reactions (the edit broadcast doesn't carry them). */
export function applyMessageUpdate(m: Message): void {
  setByChannel(
    produce((s) => {
      const arr = s[m.channelId];
      const i = arr?.findIndex((x) => x.id === m.id) ?? -1;
      if (arr && i >= 0) {
        arr[i] = { ...arr[i]!, content: m.content, editedAtMs: m.editedAtMs };
      }
    }),
  );
}

/** Apply a reactionUpdate delta: adjust the message's per-emoji aggregate. */
export function applyReactionDelta(
  channelId: string,
  messageId: string,
  emoji: string,
  added: boolean,
  isSelf: boolean,
): void {
  setByChannel(
    produce((s) => {
      const m = s[channelId]?.find((x) => x.id === messageId);
      if (!m) return;
      const list = (m.reactions ??= []);
      const idx = list.findIndex((r) => r.emoji === emoji);
      if (added) {
        if (idx >= 0) {
          list[idx]!.count += 1;
          if (isSelf) list[idx]!.me = true;
        } else {
          list.push({ emoji, count: 1, me: isSelf });
        }
      } else if (idx >= 0) {
        list[idx]!.count -= 1;
        if (isSelf) list[idx]!.me = false;
        if (list[idx]!.count <= 0) list.splice(idx, 1);
      }
    }),
  );
}

/** Apply a messageDelete: drop the row from its channel. */
export function applyMessageDelete(channelId: string, messageId: string): void {
  setByChannel(
    produce((s) => {
      const arr = s[channelId];
      const i = arr?.findIndex((x) => x.id === messageId) ?? -1;
      if (arr && i >= 0) arr.splice(i, 1);
    }),
  );
}

export function markFailed(channelId: string, nonce: string): void {
  setByChannel(
    produce((s) => {
      const arr = s[channelId];
      const row = arr?.find((x) => x.pending && x.nonce === nonce);
      if (row) {
        row.pending = false;
        row.failed = true;
      }
    }),
  );
}

export function resetMessages(): void {
  fetchedChannels.clear();
  setByChannel(produce((s) => {
    for (const k of Object.keys(s)) delete s[k];
  }));
}

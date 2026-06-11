import { createEffect, createSignal, For, Show, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import type { Message } from "../../lib/types";
import { crossesDay, dayLabel, formatTime } from "../../lib/time";
import { displayName, selectedChannelId } from "../../stores/guilds";
import { messagesFor, oldestMessageId, prependOlder } from "../../stores/messages";
import styles from "./MessageList.module.css";

const GROUP_WINDOW_MS = 5 * 60_000;
const PIN_THRESHOLD_PX = 48;

/**
 * Render-last-100 + "Load older" button — the design doc's pre-approved
 * no-virtualizer M1 shape. Bottom-anchored; sticks to the bottom unless the
 * user has scrolled up.
 */
export const MessageList: Component = () => {
  let scroller: HTMLDivElement | undefined;
  const [exhausted, setExhausted] = createSignal<Record<string, boolean>>({});
  const [loading, setLoading] = createSignal(false);

  const channelId = () => selectedChannelId() ?? "";
  const messages = () => messagesFor(channelId());

  function pinnedToBottom(): boolean {
    if (!scroller) return true;
    return scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < PIN_THRESHOLD_PX;
  }

  // Stick to the bottom on new messages (and snap on channel switch).
  let lastChannel = "";
  createEffect(() => {
    const id = channelId();
    messages().length; // track
    const switched = id !== lastChannel;
    lastChannel = id;
    if (!scroller) return;
    if (switched || pinnedToBottom()) {
      requestAnimationFrame(() => {
        if (scroller) scroller.scrollTop = scroller.scrollHeight;
      });
    }
  });

  async function loadOlder(): Promise<void> {
    const id = channelId();
    const before = oldestMessageId(id);
    if (!id || !before || loading()) return;
    setLoading(true);
    const prevHeight = scroller?.scrollHeight ?? 0;
    const prevTop = scroller?.scrollTop ?? 0;
    try {
      const page = await ipc.fetchMessages(id, before, 50);
      const added = prependOlder(id, page);
      if (added === 0) setExhausted((e) => ({ ...e, [id]: true }));
      // keep the viewport anchored on the rows the user was reading
      requestAnimationFrame(() => {
        if (scroller) scroller.scrollTop = prevTop + (scroller.scrollHeight - prevHeight);
      });
    } finally {
      setLoading(false);
    }
  }

  const showHeader = (m: Message, prev: Message | undefined): boolean =>
    !prev ||
    prev.authorId !== m.authorId ||
    m.createdAtMs - prev.createdAtMs > GROUP_WINDOW_MS ||
    crossesDay(prev.createdAtMs, m.createdAtMs);

  return (
    <div class={`bevel-sunken ${styles.well}`}>
      <div class={styles.scroller} ref={scroller}>
        <Show when={!exhausted()[channelId()] && messages().length > 0}>
          <div class={styles.loadOlderRow}>
            <button
              type="button"
              class={`bevel-raised ${styles.loadOlder}`}
              disabled={loading()}
              onClick={() => void loadOlder()}
            >
              {loading() ? "Loading…" : "Load older messages"}
            </button>
          </div>
        </Show>
        <For each={messages()}>
          {(m, i) => {
            const prev = () => messages()[i() - 1];
            return (
              <>
                <Show when={!prev() || crossesDay(prev()!.createdAtMs, m.createdAtMs)}>
                  <div class={styles.day}>
                    <span class={styles.dayLabel}>{dayLabel(m.createdAtMs)}</span>
                  </div>
                </Show>
                <div
                  class={styles.row}
                  classList={{
                    [styles.grouped!]: !showHeader(m, prev()),
                    [styles.pending!]: !!m.pending,
                    [styles.failed!]: !!m.failed,
                  }}
                >
                  <Show when={showHeader(m, prev())}>
                    <div class={styles.meta}>
                      <span class={styles.author}>{displayName(m.authorId)}</span>
                      <span class={styles.time}>{formatTime(m.createdAtMs)}</span>
                    </div>
                  </Show>
                  <div class={`selectable ${styles.content}`}>
                    {m.content}
                    <Show when={m.failed}>
                      <span class={styles.failedNote}> — failed to send</span>
                    </Show>
                  </div>
                </div>
              </>
            );
          }}
        </For>
      </div>
    </div>
  );
};

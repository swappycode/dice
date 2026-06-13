import {
  createEffect,
  createResource,
  createSignal,
  For,
  Show,
  type Component,
} from "solid-js";
import { ipc } from "../../lib/ipc";
import type { Attachment, Message } from "../../lib/types";
import { crossesDay, dayLabel, formatTime } from "../../lib/time";
import { displayName, selectedChannelId } from "../../stores/guilds";
import {
  messageById,
  messagesFor,
  oldestMessageId,
  prependOlder,
  setReplyTarget,
} from "../../stores/messages";
import { currentUser } from "../../stores/session";
import styles from "./MessageList.module.css";

const GROUP_WINDOW_MS = 5 * 60_000;
const PIN_THRESHOLD_PX = 48;
/** Fixed retro reaction palette (system emoji, no image assets). */
const REACT_EMOJIS = ["👍", "❤️", "😂", "🎉", "😮", "😢"];

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** One attachment: an inline image, or a download chip for other files. Bytes
 *  are fetched lazily from the host (`ipc.attachmentSrc`, cached per id). */
const AttachmentView: Component<{ attachment: Attachment }> = (props) => {
  const a = props.attachment;
  const isImage = a.contentType.startsWith("image/");
  const [src] = createResource(() => ipc.attachmentSrc(a.id));
  return (
    <Show
      when={isImage}
      fallback={
        <a
          class={`bevel-raised ${styles.fileChip}`}
          href={src() || undefined}
          download={a.filename}
          target="_blank"
          rel="noreferrer"
        >
          <span class={styles.fileIcon}>📄</span>
          <span class={styles.fileName}>{a.filename}</span>
          <span class={styles.fileSize}>{formatSize(a.sizeBytes)}</span>
        </a>
      }
    >
      <a class={styles.imageLink} href={src() || undefined} target="_blank" rel="noreferrer">
        <img
          class={styles.image}
          src={src() || ""}
          alt={a.filename}
          width={a.width || undefined}
          height={a.height || undefined}
          loading="lazy"
        />
      </a>
    </Show>
  );
};

/**
 * Render-last-100 + "Load older" button — the design doc's pre-approved
 * no-virtualizer M1 shape. Bottom-anchored; sticks to the bottom unless the
 * user has scrolled up.
 */
export const MessageList: Component = () => {
  let scroller: HTMLDivElement | undefined;
  const [exhausted, setExhausted] = createSignal<Record<string, boolean>>({});
  const [loading, setLoading] = createSignal(false);
  // Which message (by id) is being edited inline, and its draft text.
  const [editingId, setEditingId] = createSignal<string | null>(null);
  const [editText, setEditText] = createSignal("");
  // Which message's reaction picker is open (id or null).
  const [pickerFor, setPickerFor] = createSignal<string | null>(null);

  const channelId = () => selectedChannelId() ?? "";
  const messages = () => messagesFor(channelId());

  const isOwn = (m: Message): boolean => m.authorId === currentUser()?.id;

  function beginEdit(m: Message): void {
    setEditText(m.content);
    setEditingId(m.id);
  }
  function commitEdit(m: Message): void {
    const next = editText().trim();
    setEditingId(null);
    // Unchanged or empty = no-op (an empty edit would be rejected anyway).
    if (next && next !== m.content) {
      void ipc.editMessage(m.channelId, m.id, next).catch(() => {});
    }
  }
  function removeMessage(m: Message): void {
    void ipc.deleteMessage(m.channelId, m.id).catch(() => {});
  }
  function toggleReaction(m: Message, emoji: string): void {
    setPickerFor(null);
    const mine = m.reactions?.find((r) => r.emoji === emoji)?.me ?? false;
    void ipc.react(m.channelId, m.id, emoji, !mine).catch(() => {});
  }

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
                  <Show when={m.replyToId}>
                    {(rid) => {
                      const parent = () => messageById(m.channelId, rid());
                      return (
                        <div class={styles.replyRef}>
                          ↪{" "}
                          <Show when={parent()} fallback={<i>original message</i>}>
                            <span class={styles.replyAuthor}>
                              {displayName(parent()!.authorId)}
                            </span>
                            <span class={styles.replySnippet}>{parent()!.content}</span>
                          </Show>
                        </div>
                      );
                    }}
                  </Show>
                  <Show when={showHeader(m, prev())}>
                    <div class={styles.meta}>
                      <span class={styles.author}>{displayName(m.authorId)}</span>
                      <span class={styles.time}>{formatTime(m.createdAtMs)}</span>
                    </div>
                  </Show>
                  <Show
                    when={editingId() === m.id}
                    fallback={
                      <div class={`selectable ${styles.content}`}>
                        {m.content}
                        <Show when={m.editedAtMs}>
                          <span class={styles.edited}> (edited)</span>
                        </Show>
                        <Show when={m.failed}>
                          <span class={styles.failedNote}> — failed to send</span>
                        </Show>
                      </div>
                    }
                  >
                    <div class={styles.editor}>
                      <textarea
                        class={styles.editArea}
                        value={editText()}
                        onInput={(e) => setEditText(e.currentTarget.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Escape") {
                            e.preventDefault();
                            setEditingId(null);
                          } else if (e.key === "Enter" && !e.shiftKey) {
                            e.preventDefault();
                            commitEdit(m);
                          }
                        }}
                        ref={(el) => requestAnimationFrame(() => el.focus())}
                      />
                      <div class={styles.editHint}>
                        <button type="button" class="bevel-raised" onClick={() => commitEdit(m)}>
                          Save
                        </button>
                        <button type="button" class="bevel-raised" onClick={() => setEditingId(null)}>
                          Cancel
                        </button>
                        <span>Enter to save · Esc to cancel</span>
                      </div>
                    </div>
                  </Show>
                  {/* Attachments (images inline, other files as chips) */}
                  <Show when={m.attachments && m.attachments.length > 0}>
                    <div class={styles.attachments}>
                      <For each={m.attachments}>
                        {(a) => <AttachmentView attachment={a} />}
                      </For>
                    </div>
                  </Show>
                  {/* Reaction pills */}
                  <Show when={m.reactions && m.reactions.length > 0}>
                    <div class={styles.reactions}>
                      <For each={m.reactions}>
                        {(r) => (
                          <button
                            type="button"
                            class={styles.pill}
                            classList={{ [styles.pillMine!]: r.me }}
                            onClick={() => toggleReaction(m, r.emoji)}
                          >
                            <span>{r.emoji}</span>
                            <span class={styles.pillCount}>{r.count}</span>
                          </button>
                        )}
                      </For>
                    </div>
                  </Show>
                  {/* Hover actions: React + Reply for anyone; Edit/Delete for own */}
                  <Show when={!m.pending && !m.failed && editingId() !== m.id}>
                    <div class={styles.actions}>
                      <button
                        type="button"
                        class={styles.action}
                        onClick={() => setPickerFor(pickerFor() === m.id ? null : m.id)}
                      >
                        React
                      </button>
                      <button
                        type="button"
                        class={styles.action}
                        onClick={() => setReplyTarget(m)}
                      >
                        Reply
                      </button>
                      <Show when={isOwn(m)}>
                        <button type="button" class={styles.action} onClick={() => beginEdit(m)}>
                          Edit
                        </button>
                        <button
                          type="button"
                          class={styles.action}
                          onClick={() => removeMessage(m)}
                        >
                          Delete
                        </button>
                      </Show>
                    </div>
                  </Show>
                  {/* Emoji picker popover */}
                  <Show when={pickerFor() === m.id}>
                    <div class={styles.picker}>
                      <For each={REACT_EMOJIS}>
                        {(e) => (
                          <button
                            type="button"
                            class={styles.pickerEmoji}
                            onClick={() => toggleReaction(m, e)}
                          >
                            {e}
                          </button>
                        )}
                      </For>
                    </div>
                  </Show>
                </div>
              </>
            );
          }}
        </For>
      </div>
    </div>
  );
};

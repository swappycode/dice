import { createEffect, createSignal, For, Show, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import type { Attachment, Message } from "../../lib/types";
import { displayName, dmPartnerId, selectedChannel } from "../../stores/guilds";
import { addPending, markFailed, replyTarget, setReplyTarget } from "../../stores/messages";
import { currentUser } from "../../stores/session";
import styles from "./Composer.module.css";

const TYPING_THROTTLE_MS = 8_000; // protocol.md §6: ≤ 1 per 8 s per channel
const MAX_ROWS = 5;
const LINE_PX = 18;
const MAX_ATTACHMENTS = 10; // mirrors the server cap

export const Composer: Component = () => {
  let textarea: HTMLTextAreaElement | undefined;
  let fileInput: HTMLInputElement | undefined;
  const [pendingAttachments, setPendingAttachments] = createSignal<Attachment[]>([]);
  const [uploading, setUploading] = createSignal(false);
  const lastTypingSent = new Map<string, number>();

  const channel = () => selectedChannel();

  const placeholder = () => {
    const ch = channel();
    if (!ch) return "Message";
    if (ch.kind === "dm") {
      const partner = dmPartnerId(ch, currentUser()?.id);
      return partner ? `Message @${displayName(partner)}` : "Message";
    }
    return `Message #${ch.name}`;
  };

  // reset draft + reply target + staged attachments when switching channels
  createEffect(() => {
    channel();
    setReplyTarget(null);
    setPendingAttachments([]);
    if (textarea) {
      textarea.value = "";
      autosize();
    }
  });

  async function onFilesPicked(e: Event): Promise<void> {
    const input = e.currentTarget as HTMLInputElement;
    const files = Array.from(input.files ?? []);
    input.value = ""; // allow re-picking the same file
    if (!files.length) return;
    setUploading(true);
    try {
      for (const file of files) {
        if (pendingAttachments().length >= MAX_ATTACHMENTS) break;
        try {
          const att = await ipc.uploadAttachment(file);
          setPendingAttachments((prev) => [...prev, att]);
        } catch {
          /* a failed upload is skipped; the rest continue */
        }
      }
    } finally {
      setUploading(false);
    }
  }

  function removeAttachment(id: string): void {
    setPendingAttachments((prev) => prev.filter((a) => a.id !== id));
  }

  function autosize(): void {
    if (!textarea) return;
    textarea.style.height = "auto";
    const max = MAX_ROWS * LINE_PX + 10;
    textarea.style.height = `${Math.min(textarea.scrollHeight, max)}px`;
  }

  function onInput(): void {
    autosize();
    const ch = channel();
    if (!ch || !textarea?.value) return;
    const nowMs = Date.now();
    if (nowMs - (lastTypingSent.get(ch.id) ?? 0) >= TYPING_THROTTLE_MS) {
      lastTypingSent.set(ch.id, nowMs);
      void ipc.startTyping(ch.id);
    }
  }

  function send(): void {
    const ch = channel();
    const me = currentUser();
    if (!ch || !me || !textarea) return;
    const content = textarea.value.trim();
    if (content.length > 4000) return;
    const attachments = pendingAttachments();
    // A message needs content OR at least one attachment (matches the server).
    if (!content && attachments.length === 0) return;

    const replyId = replyTarget()?.id;
    const nonce = crypto.randomUUID();
    const pending: Message = {
      id: `pending-${nonce}`,
      channelId: ch.id,
      authorId: me.id,
      content,
      createdAtMs: Date.now(),
      editedAtMs: null,
      replyToId: replyId ?? null,
      attachments,
      nonce,
      pending: true,
    };
    addPending(pending); // optimistic row; echo reconciles by nonce
    textarea.value = "";
    autosize();
    setReplyTarget(null);
    setPendingAttachments([]);
    ipc
      .sendMessage(ch.id, content, nonce, replyId, attachments.map((a) => a.id))
      .catch(() => markFailed(ch.id, nonce));
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  return (
    <div class={styles.composer}>
      <Show when={replyTarget()}>
        {(target) => (
          <div class={styles.replyBar}>
            <span class={styles.replyText}>
              Replying to <b>{displayName(target().authorId)}</b>
            </span>
            <button
              type="button"
              class={styles.replyCancel}
              title="Cancel reply"
              onClick={() => setReplyTarget(null)}
            >
              ✕
            </button>
          </div>
        )}
      </Show>
      <Show when={pendingAttachments().length > 0}>
        <div class={styles.attachments}>
          <For each={pendingAttachments()}>
            {(a) => (
              <div class={`bevel-raised ${styles.attachChip}`} title={a.filename}>
                <span class={styles.attachName}>{a.filename}</span>
                <button
                  type="button"
                  class={styles.attachRemove}
                  title="Remove attachment"
                  onClick={() => removeAttachment(a.id)}
                >
                  ✕
                </button>
              </div>
            )}
          </For>
        </div>
      </Show>
      <input
        ref={fileInput}
        type="file"
        multiple
        class={styles.fileInput}
        onChange={(e) => void onFilesPicked(e)}
      />
      <button
        type="button"
        class={`bevel-raised ${styles.attach}`}
        title="Attach files"
        disabled={uploading() || pendingAttachments().length >= MAX_ATTACHMENTS}
        onClick={() => fileInput?.click()}
      >
        {uploading() ? "…" : "📎"}
      </button>
      <textarea
        ref={textarea}
        class={`bevel-sunken ${styles.input}`}
        rows="1"
        placeholder={placeholder()}
        maxlength="4000"
        onInput={onInput}
        onKeyDown={onKeyDown}
      />
      <button type="button" class={`bevel-raised btn-default ${styles.send}`} onClick={send}>
        Send
      </button>
    </div>
  );
};

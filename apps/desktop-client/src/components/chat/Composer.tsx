import { createEffect, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import type { Message } from "../../lib/types";
import { displayName, dmPartnerId, selectedChannel } from "../../stores/guilds";
import { addPending, markFailed } from "../../stores/messages";
import { currentUser } from "../../stores/session";
import styles from "./Composer.module.css";

const TYPING_THROTTLE_MS = 8_000; // protocol.md §6: ≤ 1 per 8 s per channel
const MAX_ROWS = 5;
const LINE_PX = 18;

export const Composer: Component = () => {
  let textarea: HTMLTextAreaElement | undefined;
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

  // reset draft when switching channels
  createEffect(() => {
    channel();
    if (textarea) {
      textarea.value = "";
      autosize();
    }
  });

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
    if (!content || content.length > 4000) return;

    const nonce = crypto.randomUUID();
    const pending: Message = {
      id: `pending-${nonce}`,
      channelId: ch.id,
      authorId: me.id,
      content,
      createdAtMs: Date.now(),
      editedAtMs: null,
      nonce,
      pending: true,
    };
    addPending(pending); // optimistic row; echo reconciles by nonce
    textarea.value = "";
    autosize();
    ipc.sendMessage(ch.id, content, nonce).catch(() => markFailed(ch.id, nonce));
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  return (
    <div class={styles.composer}>
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

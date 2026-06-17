import { createEffect, createSignal, Show, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import {
  displayName,
  dmPartnerId,
  selectedChannel,
  selectedGuild,
  unknownUserIds,
} from "../../stores/guilds";
import { applyFetchedPage, isFetched } from "../../stores/messages";
import { presenceOf } from "../../stores/presence";
import { currentUser } from "../../stores/session";
import { Composer } from "./Composer";
import { MessageList } from "./MessageList";
import styles from "./ChatView.module.css";

export const ChatView: Component = () => {
  // Fetch the newest page once per channel (cache-first later via the host).
  createEffect(() => {
    const ch = selectedChannel();
    if (!ch || isFetched(ch.id)) return;
    const id = ch.id;
    void ipc
      .fetchMessages(id, undefined, 100)
      .then((page) => {
        applyFetchedPage(id, page);
        const unknown = unknownUserIds(page.map((m) => m.authorId));
        if (unknown.length) void ipc.requestUsers(unknown);
      })
      .catch(() => applyFetchedPage(id, []));
  });

  const dmPartner = () => {
    const ch = selectedChannel();
    return ch?.kind === "dm" ? dmPartnerId(ch, currentUser()?.id) : null;
  };

  const [copied, setCopied] = createSignal(false);
  const copyInvite = async () => {
    const code = selectedGuild()?.inviteCode;
    if (!code) return;
    try {
      await navigator.clipboard.writeText(code);
    } catch {
      // Webview clipboard denied: old-school fallback.
      const ta = document.createElement("textarea");
      ta.value = code;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  return (
    <section class={styles.chat}>
      <Show
        when={selectedChannel()}
        fallback={<div class={styles.empty}>Select a channel to start chatting.</div>}
      >
        {(ch) => (
          <>
            <header class={styles.header}>
              <Show
                when={ch().kind === "dm"}
                fallback={
                  <>
                    <span class={styles.title}>#{ch().name}</span>
                    <span class={styles.etch} />
                    <span class={styles.topic}>{selectedGuild()?.name}</span>
                    <Show when={selectedGuild()?.inviteCode}>
                      <button
                        type="button"
                        class={`bevel-raised ${styles.invite}`}
                        title="Copy the invite code — friends join via [+] > Join with an invite code"
                        onClick={copyInvite}
                      >
                        {copied() ? "Copied!" : `Invite: ${selectedGuild()!.inviteCode}`}
                      </button>
                    </Show>
                  </>
                }
              >
                <span class={styles.title}>@{dmPartner() ? displayName(dmPartner()!) : "?"}</span>
                <span class={styles.etch} />
                <span class={styles.topic}>{dmPartner() ? presenceOf(dmPartner()!)() : ""}</span>
              </Show>
            </header>
            <MessageList />
            <Composer />
          </>
        )}
      </Show>
    </section>
  );
};

import { createEffect, Show, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import {
  displayName,
  dmPartnerId,
  selectedChannel,
  selectedGuild,
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
      .then((page) => applyFetchedPage(id, page))
      .catch(() => applyFetchedPage(id, []));
  });

  const dmPartner = () => {
    const ch = selectedChannel();
    return ch?.kind === "dm" ? dmPartnerId(ch, currentUser()?.id) : null;
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

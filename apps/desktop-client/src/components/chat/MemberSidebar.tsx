import { createEffect, createMemo, For, Show, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import {
  addDm,
  displayName,
  selectDm,
  selectedGuild,
  userById,
} from "../../stores/guilds";
import { presenceOf } from "../../stores/presence";
import { currentUser } from "../../stores/session";
import { Avatar } from "../common/Avatar";
import { PresenceOrb } from "../common/PresenceOrb";
import styles from "./MemberSidebar.module.css";

const ORDER = { online: 0, idle: 1, dnd: 2, disconnected: 3, offline: 4 } as const;

export const MemberSidebar: Component = () => {
  const members = createMemo(() => {
    const g = selectedGuild();
    if (!g) return [];
    return [...g.members].sort((a, b) => {
      const pa = ORDER[presenceOf(a.userId)()];
      const pb = ORDER[presenceOf(b.userId)()];
      if (pa !== pb) return pa - pb;
      return displayName(a.userId).localeCompare(displayName(b.userId));
    });
  });

  // Lazy member loading: Ready inlines only ~100 members, so for a guild at
  // that cap fetch the rest on open (idempotent per guild; further pages arrive
  // via the guildMembers dispatch, which re-requests until exhausted).
  const requested = new Set<string>();
  createEffect(() => {
    const g = selectedGuild();
    if (g && g.members.length >= 100 && !requested.has(g.id)) {
      requested.add(g.id);
      void ipc.requestGuildMembers(g.id, "", 100);
    }
  });

  async function openDm(userId: string): Promise<void> {
    if (userId === currentUser()?.id) return;
    const ch = await ipc.openDm(userId);
    const u = userById(userId);
    addDm(ch, u ? [u] : []); // idempotent; event covers the fresh-channel case
    selectDm(ch.id);
  }

  return (
    <aside class={styles.panel} aria-label="Members">
      <div class={styles.header}>MEMBERS — {members().length}</div>
      <ul class={styles.scroll}>
        <For each={members()}>
          {(m) => (
            <li>
              <button
                type="button"
                class={styles.row}
                title={m.userId === currentUser()?.id ? "That's you" : `Message ${displayName(m.userId)}`}
                onClick={() => void openDm(m.userId)}
              >
                <Avatar
                  name={displayName(m.userId)}
                  avatarId={userById(m.userId)?.avatarId}
                  size="sm"
                />
                <span class={styles.name}>{displayName(m.userId)}</span>
                <PresenceOrb status={presenceOf(m.userId)()} />
                <Show when={m.userId === currentUser()?.id}>
                  <span class={styles.you}>(you)</span>
                </Show>
              </button>
            </li>
          )}
        </For>
      </ul>
    </aside>
  );
};

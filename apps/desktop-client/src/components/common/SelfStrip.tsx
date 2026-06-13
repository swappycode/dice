import { createSignal, Show, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import type { PresenceStatus } from "../../lib/types";
import { resetDirectory } from "../../stores/guilds";
import { resetMessages } from "../../stores/messages";
import { presenceOf, resetPresence } from "../../stores/presence";
import { currentUser, setSession } from "../../stores/session";
import { resetUnread } from "../../stores/unread";
import { SecurityDialog } from "../dialogs/SecurityDialog";
import { Avatar } from "./Avatar";
import { PresenceOrb } from "./PresenceOrb";
import styles from "./SelfStrip.module.css";

const PRESENCE_CYCLE: PresenceStatus[] = ["online", "idle", "dnd", "offline"];

/** Sidebar footer: own avatar (click = change) + orb (click = cycle status) +
 *  username + log off. */
export const SelfStrip: Component = () => {
  let avatarInput: HTMLInputElement | undefined;
  const [securityOpen, setSecurityOpen] = createSignal(false);

  function cyclePresence(): void {
    const me = currentUser();
    if (!me) return;
    const cur = presenceOf(me.id)();
    const next = PRESENCE_CYCLE[(PRESENCE_CYCLE.indexOf(cur) + 1) % PRESENCE_CYCLE.length]!;
    void ipc.setPresence(next); // store updates via the presenceUpdate echo
  }

  async function onAvatarPicked(e: Event): Promise<void> {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = "";
    if (!file) return;
    try {
      const att = await ipc.uploadAttachment(file);
      await ipc.setAvatar(att.id); // UI updates via the userUpdate echo
    } catch {
      /* upload/set failed; leave the current avatar in place */
    }
  }

  async function logOff(): Promise<void> {
    await ipc.logout();
    resetMessages();
    resetPresence();
    resetDirectory();
    resetUnread();
    setSession(null);
  }

  return (
    <Show when={currentUser()}>
      {(me) => (
        <footer class={styles.self}>
          <input
            ref={avatarInput}
            type="file"
            accept="image/*"
            style={{ display: "none" }}
            onChange={(e) => void onAvatarPicked(e)}
          />
          <button
            type="button"
            class={styles.avatarBtn}
            title="Change avatar"
            onClick={() => avatarInput?.click()}
          >
            <Avatar name={me().displayName} avatarId={me().avatarId} size="sm" />
          </button>
          <button type="button" class={styles.selfOrb} onClick={cyclePresence} title="Change status">
            <PresenceOrb status={presenceOf(me().id)()} />
          </button>
          <span class={styles.selfName}>{me().displayName}</span>
          <button
            type="button"
            class={styles.security}
            title="Security & two-factor"
            aria-label="Security and two-factor authentication"
            onClick={() => setSecurityOpen(true)}
          >
            🔒
          </button>
          <button type="button" class={styles.logOff} onClick={() => void logOff()}>
            Log off
          </button>
          <Show when={securityOpen()}>
            <SecurityDialog onClose={() => setSecurityOpen(false)} />
          </Show>
        </footer>
      )}
    </Show>
  );
};

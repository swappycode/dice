import { createSignal, For, onMount, Show, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import type { PresenceStatus, User } from "../../lib/types";
import {
  acceptedByPresence,
  incomingRequests,
  loadFriends,
  outgoingRequests,
} from "../../stores/friends";
import { addDm, selectDm } from "../../stores/guilds";
import { presenceOf } from "../../stores/presence";
import { Avatar } from "../common/Avatar";
import { PresenceOrb } from "../common/PresenceOrb";
import { setHomeTab } from "../home/homeTab";
import styles from "./FriendsList.module.css";

const GROUP_LABEL: Record<PresenceStatus, string> = {
  online: "Online",
  idle: "Idle",
  dnd: "Do Not Disturb",
  offline: "Offline",
};

/** Friends list body — add-by-username, incoming/outgoing requests, and
 *  accepted friends grouped by presence (Discord-home style). Mutations rely on
 *  the `friendUpdate` dispatch to reconcile the store. */
export const FriendsList: Component = () => {
  const [name, setName] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");

  // Refresh against the server whenever the page opens (bootstrap also loads it).
  onMount(() => {
    void loadFriends().catch(() => {});
  });

  /** Run a friend mutation, surfacing any error inline. */
  function guard(action: Promise<unknown>): void {
    action.catch((err) =>
      setError(err instanceof Error ? err.message : "Something went wrong. Try again."),
    );
  }

  function submitAdd(e: Event): void {
    e.preventDefault();
    const username = name().trim();
    if (!username || busy()) return;
    setBusy(true);
    setError("");
    ipc
      .addFriend(username)
      .then(() => setName(""))
      .catch((err) =>
        setError(err instanceof Error ? err.message : "Could not send the request."),
      )
      .finally(() => setBusy(false));
  }

  /** Open (or focus) the DM with a friend and switch to the Messages tab. */
  function message(user: User): void {
    ipc
      .openDm(user.id)
      .then((channel) => {
        addDm(channel, [user]);
        selectDm(channel.id);
        setHomeTab("messages");
      })
      .catch(() => setError("Could not open the conversation."));
  }

  const isEmpty = (): boolean =>
    !incomingRequests().length && !outgoingRequests().length && !acceptedByPresence().length;

  return (
    <div class={styles.scroll}>
      <form class={styles.addForm} onSubmit={submitAdd}>
        <input
          class={`bevel-sunken ${styles.addInput}`}
          type="text"
          placeholder="Add a friend by username"
          autocomplete="off"
          value={name()}
          onInput={(e) => setName(e.currentTarget.value)}
        />
        <button type="submit" class={`bevel-raised ${styles.addBtn}`} disabled={busy()}>
          Add
        </button>
      </form>
      <Show when={error()}>
        <p class={styles.error} role="alert">
          {error()}
        </p>
      </Show>

      <Show when={incomingRequests().length}>
        <div class={styles.section}>Incoming — {incomingRequests().length}</div>
        <For each={incomingRequests()}>
          {(f) => (
            <div class={styles.rowWrap}>
              <span class={styles.info}>
                <PresenceOrb status={presenceOf(f.user.id)()} />
                <Avatar name={f.user.displayName} avatarId={f.user.avatarId} size="sm" />
                <span class={styles.name}>{f.user.displayName}</span>
              </span>
              <button
                type="button"
                class={`bevel-raised btn-default ${styles.act}`}
                onClick={() => guard(ipc.acceptFriend(f.user.id))}
              >
                Accept
              </button>
              <button
                type="button"
                class={styles.linkBtn}
                onClick={() => guard(ipc.declineFriend(f.user.id))}
              >
                Decline
              </button>
            </div>
          )}
        </For>
      </Show>

      <Show when={outgoingRequests().length}>
        <div class={styles.section}>Outgoing — {outgoingRequests().length}</div>
        <For each={outgoingRequests()}>
          {(f) => (
            <div class={styles.rowWrap}>
              <span class={styles.info}>
                <PresenceOrb status={presenceOf(f.user.id)()} />
                <Avatar name={f.user.displayName} avatarId={f.user.avatarId} size="sm" />
                <span class={styles.name}>{f.user.displayName}</span>
              </span>
              <span class={styles.pending}>Pending</span>
              <button
                type="button"
                class={styles.linkBtn}
                onClick={() => guard(ipc.declineFriend(f.user.id))}
              >
                Cancel
              </button>
            </div>
          )}
        </For>
      </Show>

      <For each={acceptedByPresence()}>
        {(group) => (
          <>
            <div class={styles.section}>
              {GROUP_LABEL[group.status]} — {group.friends.length}
            </div>
            <For each={group.friends}>
              {(f) => (
                <div class={styles.rowWrap}>
                  <button
                    type="button"
                    class={styles.rowMain}
                    title={`Message ${f.user.displayName}`}
                    onClick={() => message(f.user)}
                  >
                    <PresenceOrb status={group.status} />
                    <Avatar name={f.user.displayName} avatarId={f.user.avatarId} size="sm" />
                    <span class={styles.name}>{f.user.displayName}</span>
                  </button>
                  <button
                    type="button"
                    class={styles.iconBtn}
                    aria-label={`Remove ${f.user.displayName}`}
                    title="Remove friend"
                    onClick={() => guard(ipc.removeFriend(f.user.id))}
                  >
                    ×
                  </button>
                </div>
              )}
            </For>
          </>
        )}
      </For>

      <Show when={isEmpty()}>
        <p class={styles.empty}>No friends yet — add someone by username above.</p>
      </Show>
    </div>
  );
};

import { For, Show, type Component } from "solid-js";
import {
  directory,
  displayName,
  dmPartnerId,
  selectDm,
  selectedChannelId,
} from "../../stores/guilds";
import { presenceOf } from "../../stores/presence";
import { currentUser } from "../../stores/session";
import { markChannelRead, unreadCount } from "../../stores/unread";
import { PresenceOrb } from "../common/PresenceOrb";
import { SelfStrip } from "../common/SelfStrip";
import styles from "./DmList.module.css";

/** DM home panel — same chrome as the channel tree, rows = orb + name. */
export const DmList: Component = () => (
  <aside class={styles.panel} aria-label="Direct messages">
    <div class={styles.header}>Direct Messages</div>
    <ul class={styles.scroll}>
      <For each={directory.dms}>
        {(dm) => {
          const partner = () => dmPartnerId(dm, currentUser()?.id);
          return (
            <li>
              <button
                type="button"
                class={`${styles.row} ${selectedChannelId() === dm.id ? styles.selected : ""}`}
                onClick={() => {
                  selectDm(dm.id);
                  markChannelRead(dm.id);
                }}
              >
                <PresenceOrb status={presenceOf(partner() ?? "")()} />
                <span class={styles.name}>{partner() ? displayName(partner()!) : "Unknown"}</span>
                <Show when={unreadCount(dm.id) > 0}>
                  <span class={styles.badge}>{unreadCount(dm.id)}</span>
                </Show>
              </button>
            </li>
          );
        }}
      </For>
    </ul>
    <SelfStrip />
  </aside>
);

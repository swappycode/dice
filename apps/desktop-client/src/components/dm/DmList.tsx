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
import styles from "./DmList.module.css";

/** DM list body — rows = orb + name + unread badge. The surrounding panel
 *  chrome (+ SelfStrip) is owned by HomePane, which tabs between this and the
 *  Friends list. */
export const DmList: Component = () => (
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
);

import { createSignal, For, Show, type Component } from "solid-js";
import {
  directory,
  selectChannel,
  selectedChannelId,
  selectedGuild,
  selectedGuildId,
} from "../../stores/guilds";
import { markChannelRead, unreadCount } from "../../stores/unread";
import { SelfStrip } from "../common/SelfStrip";
import styles from "./ChannelTree.module.css";

/** Luna Explorer-tree channel list (Aero swaps to nav-pane styling in CSS). */
export const ChannelTree: Component = () => {
  const [collapsed, setCollapsed] = createSignal(false);

  const channels = () => {
    const gid = selectedGuildId();
    return gid ? (directory.channelsByGuild[gid] ?? []) : [];
  };

  return (
    <aside class={styles.panel} aria-label="Channels">
      <div class={styles.guildName}>{selectedGuild()?.name ?? ""}</div>
      <div class={styles.scroll}>
        <button type="button" class={styles.section} onClick={() => setCollapsed(!collapsed())}>
          <span class={styles.boxToggle} aria-hidden="true">
            {collapsed() ? "+" : "−"}
          </span>
          <span
            class={styles.chevToggle}
            classList={{ [styles.chevOpen!]: !collapsed() }}
            aria-hidden="true"
          />
          <span class={styles.sectionLabel}>TEXT CHANNELS</span>
        </button>
        <Show when={!collapsed()}>
          <ul class={styles.tree}>
            <For each={channels()}>
              {(ch) => (
                <li class={styles.node}>
                  <button
                    type="button"
                    class={`${styles.row} ${selectedChannelId() === ch.id ? styles.selected : ""}`}
                    onClick={() => {
                      selectChannel(ch.id);
                      markChannelRead(ch.id);
                    }}
                  >
                    <span class={styles.hash} aria-hidden="true">
                      #
                    </span>
                    <span class={styles.name}>{ch.name}</span>
                    <Show when={unreadCount(ch.id) > 0}>
                      <span class={styles.badge}>{unreadCount(ch.id)}</span>
                    </Show>
                  </button>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </div>
      <SelfStrip />
    </aside>
  );
};

import { createSignal, For, Show, type Component } from "solid-js";
import {
  directory,
  displayName,
  selectChannel,
  selectedChannelId,
  selectedGuild,
  selectedGuildId,
} from "../../stores/guilds";
import { markChannelRead, unreadCount } from "../../stores/unread";
import {
  activeVoiceChannel,
  joinVoice,
  leaveVoice,
  voiceMembers,
  voiceUser,
} from "../../stores/voice";
import { SelfStrip } from "../common/SelfStrip";
import styles from "./ChannelTree.module.css";

/** Luna Explorer-tree channel list (Aero swaps to nav-pane styling in CSS). */
export const ChannelTree: Component = () => {
  const [collapsed, setCollapsed] = createSignal(false);
  const [voiceCollapsed, setVoiceCollapsed] = createSignal(false);

  const channels = () => {
    const gid = selectedGuildId();
    return gid ? (directory.channelsByGuild[gid] ?? []) : [];
  };
  const textChannels = () => channels().filter((c) => c.kind !== "voice");
  const voiceChannels = () => channels().filter((c) => c.kind === "voice");

  const memberName = (userId: string) => voiceUser(userId)?.displayName ?? displayName(userId);

  // Join/leave are best-effort signaling — no audio yet (on-hardware phase).
  const toggleVoice = (channelId: string) => {
    if (activeVoiceChannel() === channelId) void leaveVoice().catch(() => {});
    else void joinVoice(channelId).catch(() => {});
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
            <For each={textChannels()}>
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

        <Show when={voiceChannels().length > 0}>
          <button
            type="button"
            class={styles.section}
            onClick={() => setVoiceCollapsed(!voiceCollapsed())}
          >
            <span class={styles.boxToggle} aria-hidden="true">
              {voiceCollapsed() ? "+" : "−"}
            </span>
            <span
              class={styles.chevToggle}
              classList={{ [styles.chevOpen!]: !voiceCollapsed() }}
              aria-hidden="true"
            />
            <span class={styles.sectionLabel}>VOICE CHANNELS</span>
          </button>
          <Show when={!voiceCollapsed()}>
            <ul class={styles.tree}>
              <For each={voiceChannels()}>
                {(ch) => (
                  <li class={styles.node}>
                    <button
                      type="button"
                      class={`${styles.row} ${activeVoiceChannel() === ch.id ? styles.selected : ""}`}
                      onClick={() => toggleVoice(ch.id)}
                      title={activeVoiceChannel() === ch.id ? "Leave voice" : "Join voice"}
                    >
                      <span class={styles.hash} aria-hidden="true">
                        🔊
                      </span>
                      <span class={styles.name}>{ch.name}</span>
                      <Show when={activeVoiceChannel() === ch.id}>
                        <span class={styles.voiceTag}>leave</span>
                      </Show>
                    </button>
                    <Show when={voiceMembers(ch.id).length > 0}>
                      <ul class={styles.voiceRoster}>
                        <For each={voiceMembers(ch.id)}>
                          {(m) => (
                            <li class={styles.voiceMember}>
                              <span
                                class={styles.voiceMemberName}
                                classList={{ [styles.speaking!]: m.speaking }}
                              >
                                {memberName(m.userId)}
                              </span>
                              <Show when={m.deafened || m.muted}>
                                <span class={styles.voiceTag}>{m.deafened ? "deaf" : "muted"}</span>
                              </Show>
                            </li>
                          )}
                        </For>
                      </ul>
                    </Show>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </Show>
      </div>
      <SelfStrip />
    </aside>
  );
};

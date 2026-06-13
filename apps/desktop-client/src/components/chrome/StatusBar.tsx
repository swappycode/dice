import { type Component, createMemo, Show } from "solid-js";
import { perfMode, setPerfMode } from "../../lib/perfMode";
import { setTheme, theme, type Theme } from "../../lib/theme";
import { connLabel, connState } from "../../stores/connection";
import { directory, displayName } from "../../stores/guilds";
import { presenceOf } from "../../stores/presence";
import { typingUserIds } from "../../stores/typing";
import { PresenceOrb } from "../common/PresenceOrb";
import styles from "./StatusBar.module.css";

export const StatusBar: Component = () => {
  const connOrb = createMemo(() =>
    connState() === "connected" ? "online" : connState() === "connecting" || connState() === "reconnecting" ? "idle" : "offline",
  );

  const onlineCount = createMemo(
    () => Object.keys(directory.usersById).filter((id) => presenceOf(id)() !== "offline").length,
  );

  const typingLabel = createMemo(() => {
    const ids = typingUserIds();
    if (ids.length === 0) return "";
    if (ids.length === 1) return `${displayName(ids[0]!)} is typing`;
    if (ids.length === 2) return `${displayName(ids[0]!)} and ${displayName(ids[1]!)} are typing`;
    return `${ids.length} people are typing`;
  });

  return (
    <footer class={styles.bar}>
      <div class={styles.cell}>
        <PresenceOrb status={connOrb()} title={connLabel()} />
        <span>{connLabel()}</span>
      </div>
      {/* typing cell + its steps() animation exist ONLY while someone types */}
      <Show when={typingLabel()}>
        <div class={`${styles.cell} ${styles.typingCell}`}>
          <span class="typing-dots">{typingLabel()}</span>
        </div>
      </Show>
      <div class={styles.spring} />
      <div class={styles.cell}>{onlineCount()} online</div>
      <div class={styles.cell}>
        <label class={styles.themeLabel}>
          <input
            type="checkbox"
            class={styles.perfCheck}
            checked={perfMode()}
            onChange={(e) => setPerfMode(e.currentTarget.checked)}
            title="Disable glass blur and decorative overlays to save GPU/RAM"
          />
          Perf
        </label>
      </div>
      <div class={styles.cell}>
        <label class={styles.themeLabel} for="theme-select">
          Theme
        </label>
        <select
          id="theme-select"
          class={styles.themeSelect}
          value={theme()}
          onChange={(e) => setTheme(e.currentTarget.value as Theme)}
        >
          <option value="luna">Luna</option>
          <option value="aero">Aero</option>
        </select>
      </div>
    </footer>
  );
};

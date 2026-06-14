import { type Component, createMemo, createSignal, For, lazy, Show, Suspense } from "solid-js";
import { perfMode, setPerfMode } from "../../lib/perfMode";
import { setTheme, theme, THEMES, type ThemeChoice } from "../../lib/theme";
import { connLabel, connState } from "../../stores/connection";
import { directory, displayName } from "../../stores/guilds";
import { presenceOf } from "../../stores/presence";
import { typingUserIds } from "../../stores/typing";
import { PresenceOrb } from "../common/PresenceOrb";
import styles from "./StatusBar.module.css";

// Lazy: the builder + its color math stay out of the initial/login bundle.
const ThemeBuilderDialog = lazy(() => import("../dialogs/ThemeBuilderDialog"));

export const StatusBar: Component = () => {
  const [builderOpen, setBuilderOpen] = createSignal(false);

  const onThemePick = (v: string): void => {
    if (v === "custom") setBuilderOpen(true);
    else setTheme(v as ThemeChoice);
  };

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
          onChange={(e) => onThemePick(e.currentTarget.value)}
        >
          <For each={THEMES}>{(t) => <option value={t.id}>{t.label}</option>}</For>
          <option value="custom">Custom…</option>
        </select>
        <Show when={theme() === "custom"}>
          <button
            type="button"
            class={styles.editTheme}
            title="Edit custom theme"
            aria-label="Edit custom theme"
            onClick={() => setBuilderOpen(true)}
          >
            ✎
          </button>
        </Show>
      </div>
      <Show when={builderOpen()}>
        <Suspense>
          <ThemeBuilderDialog onClose={() => setBuilderOpen(false)} />
        </Suspense>
      </Show>
    </footer>
  );
};

import { type Component, createMemo } from "solid-js";
import { hasTauri } from "../../lib/ipc";
import {
  dmPartnerId,
  displayName,
  selectedChannel,
  selectedGuild,
} from "../../stores/guilds";
import { currentUser } from "../../stores/session";
import styles from "./TitleBar.module.css";

/**
 * Custom titlebar — VISUAL ONLY in this phase. Drag-region attributes are
 * already in place for the Tauri host; the caption buttons feature-detect
 * window.__TAURI__ and no-op gracefully in a plain browser.
 * (Windows caveats from the design doc — maximize/restore glyph swap, snap
 * layouts, resize-border padding — land together with src-tauri.)
 */

type WinAction = "minimize" | "maximize" | "close";

async function winCmd(action: WinAction): Promise<void> {
  if (!hasTauri) return; // standalone dev: nothing to control
  try {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    const w = getCurrentWindow();
    if (action === "minimize") await w.minimize();
    else if (action === "maximize") await w.toggleMaximize();
    else await w.close();
  } catch {
    /* host not ready — stay graceful */
  }
}

export const TitleBar: Component = () => {
  const title = createMemo(() => {
    const ch = selectedChannel();
    if (!ch) return "Dice";
    if (ch.kind === "dm") {
      const partner = dmPartnerId(ch, currentUser()?.id);
      return partner ? `Dice — @${displayName(partner)}` : "Dice";
    }
    const g = selectedGuild();
    return g ? `Dice — ${g.name} / #${ch.name}` : `Dice — #${ch.name}`;
  });

  return (
    <header class={styles.bar} data-tauri-drag-region>
      <svg class={styles.icon} width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
        <rect x="1.5" y="1.5" width="13" height="13" rx="3" fill="currentColor" opacity="0.9" />
        <circle cx="5.4" cy="5.4" r="1.4" fill="var(--c-window-frame)" />
        <circle cx="10.6" cy="10.6" r="1.4" fill="var(--c-window-frame)" />
        <circle cx="10.6" cy="5.4" r="1.4" fill="var(--c-window-frame)" />
        <circle cx="5.4" cy="10.6" r="1.4" fill="var(--c-window-frame)" />
      </svg>
      <span class={styles.title} data-tauri-drag-region>
        {title()}
      </span>
      <div class={styles.captions}>
        <button
          type="button"
          class={styles.btn}
          aria-label="Minimize"
          onClick={() => void winCmd("minimize")}
        >
          <svg width="9" height="9" viewBox="0 0 9 9" aria-hidden="true">
            <path d="M1 7.5h7" stroke="currentColor" stroke-width="2" />
          </svg>
        </button>
        <button
          type="button"
          class={styles.btn}
          aria-label="Maximize"
          onClick={() => void winCmd("maximize")}
        >
          <svg width="9" height="9" viewBox="0 0 9 9" aria-hidden="true">
            <path d="M1.5 2.5h6v5h-6z" fill="none" stroke="currentColor" stroke-width="1.5" />
          </svg>
        </button>
        <button
          type="button"
          class={`${styles.btn} ${styles.btnClose}`}
          aria-label="Close"
          onClick={() => void winCmd("close")}
        >
          <svg width="9" height="9" viewBox="0 0 9 9" aria-hidden="true">
            <path d="M1.5 1.5l6 6m0-6l-6 6" stroke="currentColor" stroke-width="1.7" />
          </svg>
        </button>
      </div>
    </header>
  );
};

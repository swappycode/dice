import { For, type Component } from "solid-js";
import {
  PTT_KEY_LABELS,
  PTT_KEYS,
  pttEnabled,
  pttKey,
  setPttEnabled,
  setPttKey,
  type PttKey,
} from "../../stores/voiceSettings";
import styles from "./VoiceSettingsDialog.module.css";

/** Voice settings: push-to-talk on/off + its global key. The host binds the key
 *  OS-wide and gates the mic; this dialog just edits the persisted preference. */
export const VoiceSettingsDialog: Component<{ onClose: () => void }> = (props) => {
  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === "Escape") props.onClose();
  }

  return (
    <div class={styles.scrim} onClick={() => props.onClose()}>
      <div
        class={styles.dialog}
        role="dialog"
        aria-modal="true"
        aria-label="Voice settings"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <header class={styles.titlebar}>
          <span class={styles.titleText}>Voice settings</span>
          <button
            type="button"
            class={styles.closeBtn}
            aria-label="Close"
            onClick={() => props.onClose()}
          >
            ×
          </button>
        </header>
        <div class={styles.body}>
          <label class={styles.row}>
            <input
              type="checkbox"
              checked={pttEnabled()}
              onChange={(e) => setPttEnabled(e.currentTarget.checked)}
            />
            <span>Push-to-talk — transmit only while a key is held</span>
          </label>
          <label class={styles.row} classList={{ [styles.disabled!]: !pttEnabled() }}>
            <span>Push-to-talk key</span>
            <select
              value={pttKey()}
              disabled={!pttEnabled()}
              onChange={(e) => setPttKey(e.currentTarget.value as PttKey)}
            >
              <For each={PTT_KEYS}>{(k) => <option value={k}>{PTT_KEY_LABELS[k]}</option>}</For>
            </select>
          </label>
          <p class={styles.hint}>
            The key works globally (even when Dice isn't focused). Off = open mic. Use headphones —
            there's no echo cancellation yet.
          </p>
        </div>
      </div>
    </div>
  );
};

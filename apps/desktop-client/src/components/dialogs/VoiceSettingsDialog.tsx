import { createSignal, For, onMount, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import type { AudioDevices } from "../../lib/types";
import {
  inputDevice,
  outputDevice,
  PTT_KEY_LABELS,
  PTT_KEYS,
  pttEnabled,
  pttKey,
  setInputDevice,
  setOutputDevice,
  setPttEnabled,
  setPttKey,
  type PttKey,
} from "../../stores/voiceSettings";
import styles from "./VoiceSettingsDialog.module.css";

/** Voice settings: push-to-talk on/off + its global key, and the capture /
 *  playback device. The host binds the key OS-wide and gates the mic; device
 *  changes apply on the next voice join. */
export const VoiceSettingsDialog: Component<{ onClose: () => void }> = (props) => {
  const [devices, setDevices] = createSignal<AudioDevices>({
    inputs: [],
    outputs: [],
    defaultInput: null,
    defaultOutput: null,
  });

  onMount(async () => {
    try {
      setDevices(await ipc.listAudioDevices());
    } catch {
      /* leave the lists empty → only "System default" is offered */
    }
  });

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === "Escape") props.onClose();
  }

  const defaultLabel = (name: string | null): string =>
    name ? `System default (${name})` : "System default";

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
            <span>Input (microphone)</span>
            <select
              value={inputDevice() ?? ""}
              onChange={(e) => setInputDevice(e.currentTarget.value || null)}
            >
              <option value="">{defaultLabel(devices().defaultInput)}</option>
              <For each={devices().inputs}>{(d) => <option value={d}>{d}</option>}</For>
            </select>
          </label>
          <label class={styles.row}>
            <span>Output (speakers)</span>
            <select
              value={outputDevice() ?? ""}
              onChange={(e) => setOutputDevice(e.currentTarget.value || null)}
            >
              <option value="">{defaultLabel(devices().defaultOutput)}</option>
              <For each={devices().outputs}>{(d) => <option value={d}>{d}</option>}</For>
            </select>
          </label>
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
            Device changes apply the next time you join voice. PTT works globally (even when Dice
            isn't focused). Use headphones — there's no echo cancellation yet.
          </p>
        </div>
      </div>
    </div>
  );
};

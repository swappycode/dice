/**
 * Voice settings persisted locally: push-to-talk on/off + which key. The host
 * owns the OS-wide key binding and the audio gate; this store owns the user's
 * preference and pushes it to the host (on change + once at startup, so PTT is
 * re-bound if it was enabled last session).
 */

import { createSignal } from "solid-js";
import { ipc } from "../lib/ipc";
import { scopedKey } from "../lib/profileScope";

const STORAGE_KEY = scopedKey("dice.voiceSettings");

/** Keys the host's PTT binder accepts (must match `ptt.rs` `shortcut_for`). */
export const PTT_KEYS = ["Backquote", "CapsLock", "Insert", "F8", "F9", "F10"] as const;
export type PttKey = (typeof PTT_KEYS)[number];

/** Friendly labels for the dropdown. */
export const PTT_KEY_LABELS: Record<PttKey, string> = {
  Backquote: "` (backtick)",
  CapsLock: "Caps Lock",
  Insert: "Insert",
  F8: "F8",
  F9: "F9",
  F10: "F10",
};

interface Persisted {
  pttEnabled: boolean;
  pttKey: PttKey;
  /** Chosen device NAMES; null = system default. */
  inputDevice: string | null;
  outputDevice: string | null;
}

function load(): Persisted {
  const fallback: Persisted = {
    pttEnabled: false,
    pttKey: "Backquote",
    inputDevice: null,
    outputDevice: null,
  };
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const p = JSON.parse(raw) as Partial<Persisted>;
      return {
        pttEnabled: Boolean(p.pttEnabled),
        pttKey: PTT_KEYS.includes(p.pttKey as PttKey) ? (p.pttKey as PttKey) : "Backquote",
        inputDevice: typeof p.inputDevice === "string" ? p.inputDevice : null,
        outputDevice: typeof p.outputDevice === "string" ? p.outputDevice : null,
      };
    }
  } catch {
    /* corrupt / unavailable storage → defaults */
  }
  return fallback;
}

const initial = load();
const [pttEnabled, setPttEnabledSig] = createSignal(initial.pttEnabled);
const [pttKey, setPttKeySig] = createSignal<PttKey>(initial.pttKey);
const [inputDevice, setInputDeviceSig] = createSignal<string | null>(initial.inputDevice);
const [outputDevice, setOutputDeviceSig] = createSignal<string | null>(initial.outputDevice);

export { pttEnabled, pttKey, inputDevice, outputDevice };

function persist(): void {
  try {
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        pttEnabled: pttEnabled(),
        pttKey: pttKey(),
        inputDevice: inputDevice(),
        outputDevice: outputDevice(),
      }),
    );
  } catch {
    /* ignore storage failures */
  }
}

function pushPtt(): void {
  void ipc.setPtt(pttEnabled(), pttKey()).catch(() => {});
}

function pushDevices(): void {
  void ipc.setAudioDevices(inputDevice(), outputDevice()).catch(() => {});
}

export function setPttEnabled(enabled: boolean): void {
  setPttEnabledSig(enabled);
  persist();
  pushPtt();
}

export function setPttKey(key: PttKey): void {
  setPttKeySig(key);
  persist();
  pushPtt();
}

export function setInputDevice(name: string | null): void {
  setInputDeviceSig(name);
  persist();
  pushDevices();
}

export function setOutputDevice(name: string | null): void {
  setOutputDeviceSig(name);
  persist();
  pushDevices();
}

/** Push the persisted settings to the host once at startup. */
export function syncVoiceSettings(): void {
  pushPtt();
  pushDevices();
}

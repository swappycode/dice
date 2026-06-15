/**
 * Voice settings persisted locally: push-to-talk on/off + which key. The host
 * owns the OS-wide key binding and the audio gate; this store owns the user's
 * preference and pushes it to the host (on change + once at startup, so PTT is
 * re-bound if it was enabled last session).
 */

import { createSignal } from "solid-js";
import { ipc } from "../lib/ipc";

const STORAGE_KEY = "dice.voiceSettings";

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
}

function load(): Persisted {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const p = JSON.parse(raw) as Partial<Persisted>;
      const key = PTT_KEYS.includes(p.pttKey as PttKey) ? (p.pttKey as PttKey) : "Backquote";
      return { pttEnabled: Boolean(p.pttEnabled), pttKey: key };
    }
  } catch {
    /* corrupt / unavailable storage → defaults */
  }
  return { pttEnabled: false, pttKey: "Backquote" };
}

const initial = load();
const [pttEnabled, setPttEnabledSig] = createSignal(initial.pttEnabled);
const [pttKey, setPttKeySig] = createSignal<PttKey>(initial.pttKey);

export { pttEnabled, pttKey };

function persistAndPush(): void {
  const enabled = pttEnabled();
  const key = pttKey();
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ pttEnabled: enabled, pttKey: key }));
  } catch {
    /* ignore storage failures */
  }
  void ipc.setPtt(enabled, key).catch(() => {});
}

export function setPttEnabled(enabled: boolean): void {
  setPttEnabledSig(enabled);
  persistAndPush();
}

export function setPttKey(key: PttKey): void {
  setPttKeySig(key);
  persistAndPush();
}

/** Push the persisted settings to the host once at startup. */
export function syncVoiceSettings(): void {
  void ipc.setPtt(pttEnabled(), pttKey()).catch(() => {});
}

/**
 * Per-profile localStorage scoping.
 *
 * Two side-by-side `client-as alice` / `client-as bob` instances each get their
 * own host cache + keyring + WebView2 user-data-folder, but the user-data-folder
 * does NOT reliably isolate browser `localStorage` for the main window — so
 * per-account browser prefs (theme, perf mode, custom theme, voice device
 * selection) would otherwise leak between the two profiles.
 *
 * The host injects the active profile name synchronously, before any page
 * script, as `window.__DICE_PROFILE__` (see `src-tauri/src/lib.rs`), so it's
 * readable at module-load time (when the theme/perf/voice stores first read
 * localStorage). We suffix every per-account localStorage key with it.
 *
 * The DEFAULT app (and the browser mock, which has no host) has no profile →
 * bare keys, so an existing install keeps its saved settings.
 */

function activeProfile(): string {
  const p = (globalThis as { __DICE_PROFILE__?: unknown }).__DICE_PROFILE__;
  return typeof p === "string" ? p : "";
}

/** Namespace a localStorage key by the active host profile (`base@<profile>`);
 *  the default profile keeps the bare `base`. Must match the index.html
 *  pre-paint script's `sk()` so the pre-paint theme read stays consistent. */
export function scopedKey(base: string): string {
  const p = activeProfile();
  return p ? `${base}@${p}` : base;
}

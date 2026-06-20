import { createSignal } from "solid-js";
import type { Theme } from "./theme";
import { scopedKey } from "./profileScope";

/* ============================================================
   Custom theme = a base built-in theme + five high-level COLOR controls.
   Everything else (bevels, button gloss, dim text, frame, rings, selection)
   is *derived* from those five via color-mix(), so a custom theme stays
   coherent on any surface — light or dark — regardless of the base. Colors
   only, no images (keeps the no-raster perf guardrail). Persisted as a small
   JSON map in localStorage, exactly like the theme/perf prefs; applied as
   inline CSS-var overrides on :root (inline beats the base [data-theme] rule),
   so runtime cost is ~zero. The picker dialog is lazy-loaded.
   ============================================================ */

/** The five knobs the builder exposes; the rest is derived. */
export type CustomControls = {
  accent: string;
  surface: string;
  text: string;
  backdrop: string;
  titlebar: string;
};

export type CustomTheme = {
  base: Theme;
  controls: CustomControls;
};

const STORAGE_KEY = scopedKey("dice.customTheme");

/** Display order + copy for the builder rows. */
export const CONTROL_FIELDS: ReadonlyArray<{ key: keyof CustomControls; label: string; hint: string }> = [
  { key: "accent", label: "Accent", hint: "Buttons, links, selection, the wordmark" },
  { key: "surface", label: "Surface", hint: "Panels, inputs, the chat area" },
  { key: "text", label: "Text", hint: "Body text — a dimmed shade is derived" },
  { key: "backdrop", label: "Backdrop", hint: "The login / empty-state background" },
  { key: "titlebar", label: "Titlebar", hint: "Window + dialog title bars" },
];

/** Last-resort config if nothing is stored and we can't read the DOM yet. */
const FALLBACK: CustomTheme = {
  base: "midnight",
  controls: {
    accent: "#4fd6ff",
    surface: "#131a26",
    text: "#e7eef7",
    backdrop: "#0b1a2b",
    titlebar: "#1a2638",
  },
};

function load(): CustomTheme {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const j = JSON.parse(raw) as Partial<CustomTheme>;
      if (j && j.base && j.controls) return j as CustomTheme;
    }
  } catch {
    /* corrupt storage — fall through */
  }
  return FALLBACK;
}

const [customTheme, setCustomTheme] = createSignal<CustomTheme>(load());

/** Persist (Save in the builder); live edits only call setCustomTheme. The
 *  derived `overrides` map is embedded so index.html's plain-JS pre-paint can
 *  apply it before first paint without re-running deriveOverrides(). */
export function saveCustomTheme(c: CustomTheme): void {
  setCustomTheme(c);
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ ...c, overrides: deriveOverrides(c) }));
  } catch {
    /* storage full / unavailable — preview still works */
  }
}

/** Has the user ever saved a custom theme? (Decides seed-from-current vs load-saved.) */
export function hasSavedCustomTheme(): boolean {
  try {
    return !!localStorage.getItem(STORAGE_KEY);
  } catch {
    return false;
  }
}

export { customTheme, setCustomTheme };

/* ---- color math (pure) ---- */

function clamp255(n: number): number {
  return Math.max(0, Math.min(255, n));
}

/** Normalize #rgb / #rrggbb / rgb()/rgba() to a 6-digit hex (for <input type=color>). */
export function toHex6(input: string): string {
  const s = (input || "").trim();
  if (s.startsWith("#")) {
    if (s.length === 4) {
      const r = s[1]!;
      const g = s[2]!;
      const b = s[3]!;
      return `#${r}${r}${g}${g}${b}${b}`.toLowerCase();
    }
    if (s.length >= 7) return s.slice(0, 7).toLowerCase();
  }
  const m = s.match(/rgba?\(([^)]+)\)/i);
  if (m) {
    const [r, g, b] = m[1]!.split(",").map((x) => parseFloat(x));
    const h = (n: number): string => clamp255(Math.round(n || 0)).toString(16).padStart(2, "0");
    return `#${h(r!)}${h(g!)}${h(b!)}`;
  }
  return "#000000";
}

function channelLin(c8: number): number {
  const c = c8 / 255;
  return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
}

/** WCAG relative luminance of a color string (hex or rgb()). */
export function relLuminance(color: string): number {
  const h = toHex6(color);
  const r = parseInt(h.slice(1, 3), 16);
  const g = parseInt(h.slice(3, 5), 16);
  const b = parseInt(h.slice(5, 7), 16);
  return 0.2126 * channelLin(r) + 0.7152 * channelLin(g) + 0.0722 * channelLin(b);
}

/** WCAG contrast ratio between two color strings (1..21). */
export function contrastRatio(a: string, b: string): number {
  const la = relLuminance(a);
  const lb = relLuminance(b);
  const hi = Math.max(la, lb);
  const lo = Math.min(la, lb);
  return (hi + 0.05) / (lo + 0.05);
}

/** Pick whichever of near-black / near-white ink has MORE contrast on bg.
 *  (A fixed luminance threshold mispicks across the ~0.18–0.42 mid band, where
 *  it would keep white ink that reads as little as ~2:1.) */
function readableOn(bg: string): string {
  return contrastRatio("#101010", bg) >= contrastRatio("#f5f5f5", bg) ? "#101010" : "#f5f5f5";
}

const lighten = (c: string, keepPct: number): string => `color-mix(in srgb, ${c} ${keepPct}%, white)`;
const darken = (c: string, keepPct: number): string => `color-mix(in srgb, ${c} ${keepPct}%, black)`;
const blend = (a: string, b: string, aPct: number): string => `color-mix(in srgb, ${a} ${aPct}%, ${b})`;

/**
 * Expand the five controls + base into the full inline-override token map.
 * `color-mix` toward white/black is direction-agnostic, so bevels/buttons read
 * whether the surface is light or dark — the base only supplies geometry, fonts,
 * orbs, glass, scrollbars, and tooltip styling.
 */
export function deriveOverrides(c: CustomTheme): Record<string, string> {
  const { accent, surface, text, backdrop, titlebar } = c.controls;
  return {
    // accent family
    "--c-accent": accent,
    "--c-accent-border": accent,
    "--c-accent-soft": blend(accent, surface, 22),
    "--c-link": accent,
    "--c-brand-ink": accent,
    "--c-unread": accent,
    "--c-default-ring": accent,
    "--c-active-ring": accent,
    "--c-hover-ring": `color-mix(in srgb, ${accent} 55%, transparent)`,
    "--c-select-bg": accent,
    "--c-select-text": readableOn(accent),
    "--c-text-invert": readableOn(accent),
    // surface family
    "--c-window-face": surface,
    "--c-content": surface,
    "--c-bevel-hi": lighten(surface, 72),
    "--c-bevel-lo": darken(surface, 70),
    "--c-bevel-dark": darken(surface, 52),
    "--c-window-frame": blend(surface, text, 64),
    "--grad-button": `linear-gradient(180deg, ${lighten(surface, 82)} 0%, ${surface} 52%, ${darken(surface, 88)} 100%)`,
    "--grad-button-pressed": `linear-gradient(180deg, ${darken(surface, 88)} 0%, ${surface} 50%, ${lighten(surface, 90)} 100%)`,
    "--grad-rail": `linear-gradient(160deg, ${lighten(surface, 90)}, ${darken(surface, 90)})`,
    "--grad-selection": `linear-gradient(180deg, ${lighten(accent, 84)}, ${darken(accent, 88)})`,
    "--grad-start": `linear-gradient(180deg, ${lighten(accent, 86)}, ${darken(accent, 90)})`,
    // text
    "--c-text": text,
    "--c-text-dim": blend(text, surface, 60),
    // backdrop
    "--grad-page": backdrop,
    "--c-page-ink": readableOn(backdrop),
    // titlebar
    "--grad-titlebar": titlebar,
    "--grad-caption": titlebar,
    "--c-titlebar-text": readableOn(titlebar),
    "--c-caption-glyph": readableOn(titlebar),
  };
}

/** All token keys the builder may write — used to clear stale inline props. */
export const OVERRIDE_KEYS: ReadonlyArray<string> = Object.keys(
  deriveOverrides(FALLBACK),
);

/**
 * Read a built-in theme's accent/surface/text off a detached probe element
 * (its own [data-theme] rule wins over any inline overrides on :root), then
 * seed backdrop/titlebar from them. Used to start (or reset) a custom theme as
 * a faithful copy of a base.
 */
export function seedFromTheme(base: Theme): CustomControls {
  const probe = document.createElement("div");
  probe.dataset.theme = base;
  probe.style.display = "none";
  document.body.appendChild(probe);
  const cs = getComputedStyle(probe);
  const accent = toHex6(cs.getPropertyValue("--c-accent"));
  const surface = toHex6(cs.getPropertyValue("--c-window-face"));
  const text = toHex6(cs.getPropertyValue("--c-text"));
  document.body.removeChild(probe);
  return { accent, surface, text, backdrop: surface, titlebar: accent };
}

import { createSignal, createEffect } from "solid-js";
import { customTheme, deriveOverrides, OVERRIDE_KEYS } from "./customTheme";

export type Theme = "luna" | "aero" | "midnight" | "nocturne" | "vantablack" | "bubble" | "phosphor";
/** A selectable choice = a built-in theme OR the user's "custom" theme. */
export type ThemeChoice = Theme | "custom";

/** All selectable BUILT-IN themes + their display labels (order = dropdown order). */
export const THEMES: ReadonlyArray<{ id: Theme; label: string }> = [
  { id: "luna", label: "Luna" },
  { id: "aero", label: "Aero" },
  { id: "midnight", label: "Midnight" },
  { id: "nocturne", label: "Nocturne" },
  { id: "vantablack", label: "Vantablack" },
  { id: "bubble", label: "Bubble" },
  { id: "phosphor", label: "Phosphor" },
];

const CHOICES: ReadonlyArray<ThemeChoice> = [...THEMES.map((t) => t.id), "custom"];

function isChoice(v: string | null): v is ThemeChoice {
  return v != null && CHOICES.includes(v as ThemeChoice);
}

const stored = localStorage.getItem("dice.theme");

const [theme, setTheme] = createSignal<ThemeChoice>(isChoice(stored) ? stored : "luna");

/** Inline override keys currently set on :root, so a removed one never lingers. */
let appliedKeys: ReadonlyArray<string> = [];

function applyOverrides(map: Record<string, string>): void {
  const root = document.documentElement;
  for (const k of appliedKeys) if (!(k in map)) root.style.removeProperty(k);
  for (const k of Object.keys(map)) root.style.setProperty(k, map[k]!);
  appliedKeys = Object.keys(map);
}

function clearOverrides(): void {
  const root = document.documentElement;
  // clear both whatever we last set and the full known set (covers a pre-paint
  // apply that this module didn't track).
  for (const k of appliedKeys) root.style.removeProperty(k);
  for (const k of OVERRIDE_KEYS) root.style.removeProperty(k);
  appliedKeys = [];
}

/**
 * Theme switch = one dataset write (+ inline var overrides for "custom"); the
 * index.html pre-paint script mirrors this to avoid a flash on load.
 */
export function installThemeEffect(): void {
  createEffect(() => {
    const t = theme();
    if (t === "custom") {
      const c = customTheme();
      document.documentElement.dataset.theme = c.base;
      applyOverrides(deriveOverrides(c));
    } else {
      clearOverrides();
      document.documentElement.dataset.theme = t;
    }
    localStorage.setItem("dice.theme", t);
  });
}

export { theme, setTheme };

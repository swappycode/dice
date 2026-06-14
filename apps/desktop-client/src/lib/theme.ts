import { createSignal, createEffect } from "solid-js";

export type Theme = "luna" | "aero" | "midnight" | "nocturne" | "vantablack" | "bubble" | "phosphor";

/** All selectable themes + their display labels (order = dropdown order). */
export const THEMES: ReadonlyArray<{ id: Theme; label: string }> = [
  { id: "luna", label: "Luna" },
  { id: "aero", label: "Aero" },
  { id: "midnight", label: "Midnight" },
  { id: "nocturne", label: "Nocturne" },
  { id: "vantablack", label: "Vantablack" },
  { id: "bubble", label: "Bubble" },
  { id: "phosphor", label: "Phosphor" },
];

function isTheme(v: string | null): v is Theme {
  return THEMES.some((t) => t.id === v);
}

const stored = localStorage.getItem("dice.theme");

const [theme, setTheme] = createSignal<Theme>(isTheme(stored) ? stored : "luna");

/** Theme switch = one dataset write; index.html pre-paint script avoids FOUC. */
export function installThemeEffect(): void {
  createEffect(() => {
    document.documentElement.dataset.theme = theme();
    localStorage.setItem("dice.theme", theme());
  });
}

export { theme, setTheme };

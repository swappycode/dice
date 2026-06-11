import { createSignal, createEffect } from "solid-js";

export type Theme = "luna" | "aero";

const stored = localStorage.getItem("dice.theme");

const [theme, setTheme] = createSignal<Theme>(stored === "aero" ? "aero" : "luna");

/** Theme switch = one dataset write; index.html pre-paint script avoids FOUC. */
export function installThemeEffect(): void {
  createEffect(() => {
    document.documentElement.dataset.theme = theme();
    localStorage.setItem("dice.theme", theme());
  });
}

export { theme, setTheme };

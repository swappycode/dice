import { createSignal, createEffect } from "solid-js";

// Perf mode = the performance escape hatch: forces `--glass-blur: 0` (no
// backdrop-filter) and disables decorative overlays (e.g. the CRT scanline
// veil) regardless of theme. Persisted like the theme; the index.html
// pre-paint script applies the class before first paint to avoid a flash.

const stored = localStorage.getItem("dice.perfMode");

const [perfMode, setPerfMode] = createSignal<boolean>(stored === "1");

/** Perf mode toggle = one class write; index.html pre-paint avoids FOUC. */
export function installPerfModeEffect(): void {
  createEffect(() => {
    document.documentElement.classList.toggle("perf-mode", perfMode());
    localStorage.setItem("dice.perfMode", perfMode() ? "1" : "0");
  });
}

export { perfMode, setPerfMode };

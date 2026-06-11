import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// Port 1420 + strictPort: the Tauri dev loop (later phase) expects the dev
// server at a fixed address (build.devUrl in tauri.conf.json).
export default defineConfig({
  plugins: [solid()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "esnext",
    sourcemap: false,
  },
});

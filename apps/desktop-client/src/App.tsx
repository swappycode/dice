import { createSignal, onMount, Show, type Component } from "solid-js";
import { LoginCard } from "./components/auth/LoginCard";
import { AppShell } from "./components/shell/AppShell";
import { runBootstrap } from "./gateway/dispatcher";
import { ipc } from "./lib/ipc";
import { installPerfModeEffect } from "./lib/perfMode";
import { installThemeEffect } from "./lib/theme";
import { session, setSession } from "./stores/session";
import { syncVoiceSettings } from "./stores/voiceSettings";

const App: Component = () => {
  installThemeEffect();
  installPerfModeEffect();

  const [booting, setBooting] = createSignal(true);

  onMount(async () => {
    // Re-bind global push-to-talk if it was enabled last session.
    syncVoiceSettings();
    try {
      const s = await ipc.getSession();
      if (s) {
        setSession(s);
        await runBootstrap();
      }
    } finally {
      setBooting(false);
    }
  });

  return (
    <>
      {/* Decorative full-screen overlay; only the Phosphor theme renders it
          (CSS-gated), so it's inert under every other theme. */}
      <div class="crt-veil" aria-hidden="true" />
      <Show when={!booting()}>
        <Show when={session()} fallback={<LoginCard />}>
          <AppShell />
        </Show>
      </Show>
    </>
  );
};

export default App;

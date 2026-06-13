import { createSignal, onMount, Show, type Component } from "solid-js";
import { LoginCard } from "./components/auth/LoginCard";
import { AppShell } from "./components/shell/AppShell";
import { runBootstrap } from "./gateway/dispatcher";
import { ipc } from "./lib/ipc";
import { installPerfModeEffect } from "./lib/perfMode";
import { installThemeEffect } from "./lib/theme";
import { session, setSession } from "./stores/session";

const App: Component = () => {
  installThemeEffect();
  installPerfModeEffect();

  const [booting, setBooting] = createSignal(true);

  onMount(async () => {
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
    <Show when={!booting()}>
      <Show when={session()} fallback={<LoginCard />}>
        <AppShell />
      </Show>
    </Show>
  );
};

export default App;

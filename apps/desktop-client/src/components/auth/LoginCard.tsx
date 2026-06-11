import { createSignal, Show, type Component } from "solid-js";
import { runBootstrap } from "../../gateway/dispatcher";
import { ipc } from "../../lib/ipc";
import { setSession } from "../../stores/session";
import styles from "./LoginCard.module.css";

/** XP Welcome-screen style login/register card on a full-bleed gradient. */
export const LoginCard: Component = () => {
  const [mode, setMode] = createSignal<"login" | "register">("login");
  const [email, setEmail] = createSignal("");
  const [username, setUsername] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");

  const isRegister = () => mode() === "register";

  async function submit(e: Event): Promise<void> {
    e.preventDefault();
    if (busy()) return;
    setBusy(true);
    setError("");
    try {
      const s = isRegister()
        ? await ipc.register(email(), username(), password())
        : await ipc.login(email(), password());
      setSession(s);
      await runBootstrap();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Something went wrong. Try again.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div class={styles.page}>
      <div class={styles.rule} />
      <main class={styles.card}>
        <section class={styles.brand}>
          <svg width="56" height="56" viewBox="0 0 16 16" aria-hidden="true" class={styles.logo}>
            <rect x="1.5" y="1.5" width="13" height="13" rx="3" fill="currentColor" opacity="0.95" />
            <circle cx="5.4" cy="5.4" r="1.5" fill="var(--c-accent)" />
            <circle cx="10.6" cy="5.4" r="1.5" fill="var(--c-accent)" />
            <circle cx="5.4" cy="10.6" r="1.5" fill="var(--c-accent)" />
            <circle cx="10.6" cy="10.6" r="1.5" fill="var(--c-accent)" />
          </svg>
          <h1 class={styles.wordmark}>D I C E</h1>
          <p class={styles.tagline}>
            {isRegister()
              ? "Pick a name, set a password, and roll in."
              : "To begin, enter your account details and press Log in."}
          </p>
        </section>
        <section class={styles.formPane}>
          <h2 class={styles.heading}>{isRegister() ? "Create your account" : "Log in to Dice"}</h2>
          <form class={styles.form} onSubmit={(e) => void submit(e)}>
            <label class={styles.label} for="auth-email">
              E-mail
            </label>
            <input
              id="auth-email"
              class={`bevel-sunken ${styles.input}`}
              type="email"
              autocomplete="email"
              required
              value={email()}
              onInput={(e) => setEmail(e.currentTarget.value)}
            />
            <Show when={isRegister()}>
              <label class={styles.label} for="auth-username">
                Username
              </label>
              <input
                id="auth-username"
                class={`bevel-sunken ${styles.input}`}
                type="text"
                autocomplete="username"
                required
                value={username()}
                onInput={(e) => setUsername(e.currentTarget.value)}
              />
            </Show>
            <label class={styles.label} for="auth-password">
              Password
            </label>
            <input
              id="auth-password"
              class={`bevel-sunken ${styles.input}`}
              type="password"
              autocomplete={isRegister() ? "new-password" : "current-password"}
              required
              value={password()}
              onInput={(e) => setPassword(e.currentTarget.value)}
            />
            <Show when={error()}>
              <p class={styles.error} role="alert">
                {error()}
              </p>
            </Show>
            <button type="submit" class={`bevel-raised btn-default ${styles.submit}`} disabled={busy()}>
              <span class={styles.submitArrow} aria-hidden="true">
                →
              </span>
              {busy() ? "One moment…" : isRegister() ? "Register" : "Log in"}
            </button>
            <button
              type="button"
              class={styles.switchLink}
              onClick={() => {
                setError("");
                setMode(isRegister() ? "login" : "register");
              }}
            >
              {isRegister() ? "Have an account? Log in" : "New here? Create an account"}
            </button>
          </form>
        </section>
      </main>
      <div class={styles.rule} />
      <footer class={styles.foot}>
        <span>Dice — retro chat for the comeback era</span>
        <span>v0.1.0 (mock mode)</span>
      </footer>
    </div>
  );
};

import { createSignal, Show, type Component } from "solid-js";
import { runBootstrap } from "../../gateway/dispatcher";
import { ipc, MOCK_IPC } from "../../lib/ipc";
import { loginNotice, setLoginNotice, setSession } from "../../stores/session";
import styles from "./LoginCard.module.css";

/** XP Welcome-screen style login/register card on a full-bleed gradient. */
export const LoginCard: Component = () => {
  const [mode, setMode] = createSignal<"login" | "register">("login");
  const [email, setEmail] = createSignal("");
  const [username, setUsername] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  // Set once the password step returns a 2FA challenge; switches to the code step.
  const [totpTicket, setTotpTicket] = createSignal<string | null>(null);
  const [totpCode, setTotpCode] = createSignal("");

  const isRegister = () => mode() === "register";

  async function finishSession(s: Awaited<ReturnType<typeof ipc.completeTotpLogin>>): Promise<void> {
    setSession(s);
    await runBootstrap();
  }

  async function submit(e: Event): Promise<void> {
    e.preventDefault();
    if (busy()) return;
    setBusy(true);
    setError("");
    setLoginNotice("");
    try {
      if (isRegister()) {
        await finishSession(await ipc.register(email(), username(), password()));
        return;
      }
      const res = await ipc.login(email(), password());
      if (res.totpTicket) {
        setTotpTicket(res.totpTicket);
        setTotpCode("");
      } else if (res.session) {
        await finishSession(res.session);
      } else {
        throw new Error("Unexpected login response. Try again.");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Something went wrong. Try again.");
    } finally {
      setBusy(false);
    }
  }

  async function submitTotp(e: Event): Promise<void> {
    e.preventDefault();
    if (busy()) return;
    const ticket = totpTicket();
    if (!ticket) return;
    setBusy(true);
    setError("");
    try {
      await finishSession(await ipc.completeTotpLogin(ticket, totpCode()));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Something went wrong. Try again.");
    } finally {
      setBusy(false);
    }
  }

  function cancelTotp(): void {
    setTotpTicket(null);
    setTotpCode("");
    setError("");
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
            {totpTicket()
              ? "Almost there — confirm it's you with your authenticator."
              : isRegister()
                ? "Pick a name, set a password, and roll in."
                : "To begin, enter your account details and press Log in."}
          </p>
        </section>
        <section class={styles.formPane}>
          <Show
            when={totpTicket()}
            fallback={
              <>
                <h2 class={styles.heading}>
                  {isRegister() ? "Create your account" : "Log in to Dice"}
                </h2>
                <Show when={loginNotice()}>
                  <p class={styles.notice} role="status">
                    {loginNotice()}
                  </p>
                </Show>
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
                  <button
                    type="submit"
                    class={`bevel-raised btn-default ${styles.submit}`}
                    disabled={busy()}
                  >
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
              </>
            }
          >
            <h2 class={styles.heading}>Two-step verification</h2>
            <form class={styles.form} onSubmit={(e) => void submitTotp(e)}>
              <label class={styles.label} for="auth-totp">
                Authenticator code
              </label>
              <input
                id="auth-totp"
                class={`bevel-sunken ${styles.input}`}
                type="text"
                inputmode="numeric"
                autocomplete="one-time-code"
                autofocus
                placeholder="123 456"
                required
                value={totpCode()}
                onInput={(e) => setTotpCode(e.currentTarget.value)}
              />
              <p class={styles.hint}>Enter the 6-digit code, or one of your recovery codes.</p>
              <Show when={error()}>
                <p class={styles.error} role="alert">
                  {error()}
                </p>
              </Show>
              <button
                type="submit"
                class={`bevel-raised btn-default ${styles.submit}`}
                disabled={busy()}
              >
                <span class={styles.submitArrow} aria-hidden="true">
                  →
                </span>
                {busy() ? "Verifying…" : "Verify"}
              </button>
              <button type="button" class={styles.switchLink} onClick={cancelTotp}>
                ← Back to login
              </button>
            </form>
          </Show>
        </section>
      </main>
      <div class={styles.rule} />
      <footer class={styles.foot}>
        <span>Dice — retro chat for the comeback era</span>
        <span>v0.1.0{MOCK_IPC ? " (mock mode)" : ""}</span>
      </footer>
    </div>
  );
};

import { createSignal, Show, type Component } from "solid-js";
import { runBootstrap } from "../../gateway/dispatcher";
import { ipc, MOCK_IPC } from "../../lib/ipc";
import { loginNotice, setLoginNotice, setSession } from "../../stores/session";
import styles from "./LoginCard.module.css";

type View = "login" | "register" | "totp" | "reset-request" | "reset-confirm";

/** XP Welcome-screen style login/register card on a full-bleed gradient. */
export const LoginCard: Component = () => {
  const [mode, setMode] = createSignal<"login" | "register">("login");
  const [email, setEmail] = createSignal("");
  const [username, setUsername] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  // Set once the password step returns a 2FA challenge.
  const [totpTicket, setTotpTicket] = createSignal<string | null>(null);
  const [totpCode, setTotpCode] = createSignal("");
  // Password-reset sub-flow.
  const [resetting, setResetting] = createSignal(false);
  const [resetSent, setResetSent] = createSignal(false);
  const [resetToken, setResetToken] = createSignal("");

  const view = (): View => {
    if (totpTicket()) return "totp";
    if (resetting()) return resetSent() ? "reset-confirm" : "reset-request";
    return mode();
  };

  async function finishSession(s: Awaited<ReturnType<typeof ipc.completeTotpLogin>>): Promise<void> {
    setSession(s);
    await runBootstrap();
  }

  /** Run an async action with the shared busy/error guard. */
  async function guard(fn: () => Promise<void>): Promise<void> {
    if (busy()) return;
    setBusy(true);
    setError("");
    try {
      await fn();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Something went wrong. Try again.");
    } finally {
      setBusy(false);
    }
  }

  function submit(e: Event): void {
    e.preventDefault();
    setLoginNotice("");
    void guard(async () => {
      if (mode() === "register") {
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
    });
  }

  function submitTotp(e: Event): void {
    e.preventDefault();
    const ticket = totpTicket();
    if (!ticket) return;
    void guard(() => ipc.completeTotpLogin(ticket, totpCode()).then(finishSession));
  }

  function submitResetRequest(e: Event): void {
    e.preventDefault();
    void guard(async () => {
      await ipc.requestPasswordReset(email());
      setResetSent(true);
      setResetToken("");
      setPassword("");
    });
  }

  function submitResetConfirm(e: Event): void {
    e.preventDefault();
    void guard(async () => {
      await ipc.resetPassword(resetToken(), password());
      backToLogin();
      setLoginNotice("Password changed. Log in with your new password.");
    });
  }

  function startReset(): void {
    setError("");
    setLoginNotice("");
    setResetting(true);
    setResetSent(false);
  }

  function backToLogin(): void {
    setTotpTicket(null);
    setTotpCode("");
    setResetting(false);
    setResetSent(false);
    setResetToken("");
    setPassword("");
    setError("");
  }

  const tagline = (): string => {
    switch (view()) {
      case "totp":
        return "Almost there — confirm it's you with your authenticator.";
      case "reset-request":
        return "Forgot your password? We'll email you a reset code.";
      case "reset-confirm":
        return "Enter the code from your email and a new password.";
      case "register":
        return "Pick a name, set a password, and roll in.";
      default:
        return "To begin, enter your account details and press Log in.";
    }
  };

  const heading = (): string => {
    switch (view()) {
      case "totp":
        return "Two-step verification";
      case "reset-request":
      case "reset-confirm":
        return "Reset your password";
      case "register":
        return "Create your account";
      default:
        return "Log in to Dice";
    }
  };

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
          <p class={styles.tagline}>{tagline()}</p>
        </section>
        <section class={styles.formPane}>
          <h2 class={styles.heading}>{heading()}</h2>
          <Show when={loginNotice() && view() !== "totp"}>
            <p class={styles.notice} role="status">
              {loginNotice()}
            </p>
          </Show>

          {/* ---- login / register ---- */}
          <Show when={view() === "login" || view() === "register"}>
            <form class={styles.form} onSubmit={submit}>
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
              <Show when={mode() === "register"}>
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
                autocomplete={mode() === "register" ? "new-password" : "current-password"}
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
                {busy() ? "One moment…" : mode() === "register" ? "Register" : "Log in"}
              </button>
              <Show when={mode() === "login"}>
                <button type="button" class={styles.switchLink} onClick={startReset}>
                  Forgot your password?
                </button>
              </Show>
              <button
                type="button"
                class={styles.switchLink}
                onClick={() => {
                  setError("");
                  setMode(mode() === "register" ? "login" : "register");
                }}
              >
                {mode() === "register" ? "Have an account? Log in" : "New here? Create an account"}
              </button>
            </form>
          </Show>

          {/* ---- 2FA challenge ---- */}
          <Show when={view() === "totp"}>
            <form class={styles.form} onSubmit={submitTotp}>
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
              <button type="submit" class={`bevel-raised btn-default ${styles.submit}`} disabled={busy()}>
                <span class={styles.submitArrow} aria-hidden="true">
                  →
                </span>
                {busy() ? "Verifying…" : "Verify"}
              </button>
              <button type="button" class={styles.switchLink} onClick={backToLogin}>
                ← Back to login
              </button>
            </form>
          </Show>

          {/* ---- reset: request a code ---- */}
          <Show when={view() === "reset-request"}>
            <form class={styles.form} onSubmit={submitResetRequest}>
              <label class={styles.label} for="reset-email">
                E-mail
              </label>
              <input
                id="reset-email"
                class={`bevel-sunken ${styles.input}`}
                type="email"
                autocomplete="email"
                required
                value={email()}
                onInput={(e) => setEmail(e.currentTarget.value)}
              />
              <p class={styles.hint}>If the address has an account, a reset code is on its way.</p>
              <Show when={error()}>
                <p class={styles.error} role="alert">
                  {error()}
                </p>
              </Show>
              <button type="submit" class={`bevel-raised btn-default ${styles.submit}`} disabled={busy()}>
                <span class={styles.submitArrow} aria-hidden="true">
                  →
                </span>
                {busy() ? "Sending…" : "Send reset code"}
              </button>
              <button type="button" class={styles.switchLink} onClick={backToLogin}>
                ← Back to login
              </button>
            </form>
          </Show>

          {/* ---- reset: enter code + new password ---- */}
          <Show when={view() === "reset-confirm"}>
            <form class={styles.form} onSubmit={submitResetConfirm}>
              <label class={styles.label} for="reset-token">
                Reset code
              </label>
              <input
                id="reset-token"
                class={`bevel-sunken ${styles.input}`}
                type="text"
                autocomplete="one-time-code"
                required
                value={resetToken()}
                onInput={(e) => setResetToken(e.currentTarget.value)}
              />
              <label class={styles.label} for="reset-password">
                New password
              </label>
              <input
                id="reset-password"
                class={`bevel-sunken ${styles.input}`}
                type="password"
                autocomplete="new-password"
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
                {busy() ? "Resetting…" : "Reset password"}
              </button>
              <button type="button" class={styles.switchLink} onClick={backToLogin}>
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

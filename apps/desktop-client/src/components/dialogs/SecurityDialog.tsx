import { createSignal, For, Show, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import type { TotpEnroll } from "../../lib/types";
import styles from "./SecurityDialog.module.css";

type Mode = "menu" | "verify" | "enroll" | "recovery" | "disable";

/** Account security: verify your email, and set up / turn off two-factor. The
 *  server is the source of truth — invalid operations (e.g. enrolling when
 *  already on) surface as an error here. */
export const SecurityDialog: Component<{ onClose: () => void }> = (props) => {
  const [mode, setMode] = createSignal<Mode>("menu");
  const [enroll, setEnroll] = createSignal<TotpEnroll | null>(null);
  const [code, setCode] = createSignal("");
  const [recovery, setRecovery] = createSignal<string[]>([]);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  const [notice, setNotice] = createSignal("");

  async function beginEnroll(): Promise<void> {
    if (busy()) return;
    setBusy(true);
    setError("");
    try {
      setEnroll(await ipc.totpEnroll());
      setCode("");
      setMode("enroll");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not start setup.");
    } finally {
      setBusy(false);
    }
  }

  async function confirmEnroll(): Promise<void> {
    if (busy()) return;
    setBusy(true);
    setError("");
    try {
      setRecovery(await ipc.totpConfirm(code()));
      setCode("");
      setMode("recovery");
    } catch (err) {
      setError(err instanceof Error ? err.message : "That code didn't match.");
    } finally {
      setBusy(false);
    }
  }

  async function disable(): Promise<void> {
    if (busy()) return;
    setBusy(true);
    setError("");
    try {
      await ipc.totpDisable(code());
      props.onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : "That code didn't match.");
    } finally {
      setBusy(false);
    }
  }

  async function resendVerification(): Promise<void> {
    if (busy()) return;
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await ipc.resendVerification();
      setNotice("Verification email sent — check your inbox for the code.");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not send the email.");
    } finally {
      setBusy(false);
    }
  }

  async function confirmVerify(): Promise<void> {
    if (busy()) return;
    setBusy(true);
    setError("");
    try {
      await ipc.verifyEmail(code());
      setNotice("Email verified ✓");
      setError("");
    } catch (err) {
      setError(err instanceof Error ? err.message : "That code didn't match.");
    } finally {
      setBusy(false);
    }
  }

  function goto(next: Mode): void {
    setError("");
    setNotice("");
    setCode("");
    setMode(next);
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === "Escape") props.onClose();
  }

  return (
    <div class={styles.scrim} onClick={() => props.onClose()}>
      <div
        class={styles.dialog}
        role="dialog"
        aria-modal="true"
        aria-label="Two-factor authentication"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <header class={styles.titlebar}>
          <span class={styles.titleText}>Account security</span>
          <button
            type="button"
            class={styles.closeBtn}
            aria-label="Close"
            onClick={() => props.onClose()}
          >
            ×
          </button>
        </header>

        <div class={styles.body}>
          <Show when={mode() === "menu"}>
            <p class={styles.lead}>Confirm your email address.</p>
            <div class={styles.menuRow}>
              <button
                type="button"
                class={`bevel-raised btn-default ${styles.btn}`}
                onClick={() => goto("verify")}
              >
                Verify email
              </button>
            </div>
            <p class={styles.lead}>
              Add a second step to your login using an authenticator app (Aegis,
              Google Authenticator, 1Password, …).
            </p>
            <div class={styles.menuRow}>
              <button
                type="button"
                class={`bevel-raised btn-default ${styles.btn}`}
                disabled={busy()}
                onClick={() => void beginEnroll()}
              >
                Set up 2FA
              </button>
              <button type="button" class={`bevel-raised ${styles.btn}`} onClick={() => goto("disable")}>
                Turn off 2FA
              </button>
            </div>
          </Show>

          <Show when={mode() === "verify"}>
            <p class={styles.lead}>
              We email a code at sign-up. Resend it, then paste the code here.
            </p>
            <button
              type="button"
              class={`bevel-raised ${styles.btn}`}
              disabled={busy()}
              onClick={() => void resendVerification()}
            >
              Resend email
            </button>
            <div class={styles.field}>
              <label class={styles.fieldLabel} for="email-verify">
                Verification code
              </label>
              <input
                id="email-verify"
                class={`bevel-sunken ${styles.input}`}
                type="text"
                value={code()}
                onInput={(e) => setCode(e.currentTarget.value)}
              />
            </div>
          </Show>

          <Show when={mode() === "enroll"}>
            <p class={styles.lead}>
              Add this secret to your authenticator, then enter the 6-digit code
              it shows.
            </p>
            <div class={styles.field}>
              <span class={styles.fieldLabel}>Setup key</span>
              <code class={`bevel-sunken ${styles.secret}`}>{enroll()?.secret}</code>
            </div>
            <a class={styles.uriLink} href={enroll()?.otpauthUri} rel="noreferrer">
              Open in authenticator
            </a>
            <div class={styles.field}>
              <label class={styles.fieldLabel} for="totp-confirm">
                Code from the app
              </label>
              <input
                id="totp-confirm"
                class={`bevel-sunken ${styles.input}`}
                type="text"
                inputmode="numeric"
                autocomplete="one-time-code"
                placeholder="123 456"
                value={code()}
                onInput={(e) => setCode(e.currentTarget.value)}
              />
            </div>
          </Show>

          <Show when={mode() === "recovery"}>
            <p class={styles.lead}>
              Two-factor is on. Save these <strong>recovery codes</strong> somewhere
              safe — each works once if you lose your authenticator.
            </p>
            <ul class={`bevel-sunken ${styles.recoveryGrid}`}>
              <For each={recovery()}>{(c) => <li class={styles.recoveryCode}>{c}</li>}</For>
            </ul>
          </Show>

          <Show when={mode() === "disable"}>
            <p class={styles.lead}>
              Enter a current authenticator code (or a recovery code) to turn off
              two-factor.
            </p>
            <div class={styles.field}>
              <label class={styles.fieldLabel} for="totp-disable">
                Code
              </label>
              <input
                id="totp-disable"
                class={`bevel-sunken ${styles.input}`}
                type="text"
                inputmode="numeric"
                autocomplete="one-time-code"
                placeholder="123 456"
                value={code()}
                onInput={(e) => setCode(e.currentTarget.value)}
              />
            </div>
          </Show>

          <Show when={notice()}>
            <p class={styles.notice} role="status">
              {notice()}
            </p>
          </Show>
          <Show when={error()}>
            <p class={styles.error} role="alert">
              {error()}
            </p>
          </Show>
        </div>

        <footer class={styles.buttons}>
          <Show when={mode() === "verify"}>
            <button
              type="button"
              class={`bevel-raised btn-default ${styles.btn}`}
              disabled={busy()}
              onClick={() => void confirmVerify()}
            >
              Verify
            </button>
          </Show>
          <Show when={mode() === "enroll"}>
            <button
              type="button"
              class={`bevel-raised btn-default ${styles.btn}`}
              disabled={busy()}
              onClick={() => void confirmEnroll()}
            >
              Verify
            </button>
          </Show>
          <Show when={mode() === "disable"}>
            <button
              type="button"
              class={`bevel-raised btn-default ${styles.btn}`}
              disabled={busy()}
              onClick={() => void disable()}
            >
              Turn off
            </button>
          </Show>
          <button type="button" class={`bevel-raised ${styles.btn}`} onClick={() => props.onClose()}>
            {mode() === "recovery" ? "Done" : "Cancel"}
          </button>
        </footer>
      </div>
    </div>
  );
};

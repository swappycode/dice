import { createSignal, Show, type Component } from "solid-js";
import { ipc } from "../../lib/ipc";
import { selectGuild } from "../../stores/guilds";
import styles from "./GuildDialog.module.css";

/** Classic Windows dialog: titlebar strip, radio mode toggle, OK/Cancel. */
export const GuildDialog: Component<{ onClose: () => void }> = (props) => {
  const [mode, setMode] = createSignal<"create" | "join">("create");
  const [name, setName] = createSignal("");
  const [code, setCode] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");

  async function ok(): Promise<void> {
    if (busy()) return;
    setBusy(true);
    setError("");
    try {
      const guild =
        mode() === "create" ? await ipc.createGuild(name()) : await ipc.joinGuild(code());
      // guildCreate event already added it to the store; just focus it
      selectGuild(guild.id);
      props.onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : "That didn't work. Try again.");
    } finally {
      setBusy(false);
    }
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === "Escape") props.onClose();
    if (e.key === "Enter") void ok();
  }

  return (
    <div class={styles.scrim} onClick={() => props.onClose()}>
      <div
        class={styles.dialog}
        role="dialog"
        aria-modal="true"
        aria-label="Add a guild"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <header class={styles.titlebar}>
          <span class={styles.titleText}>Add a guild</span>
          <button type="button" class={styles.closeBtn} aria-label="Close" onClick={() => props.onClose()}>
            ×
          </button>
        </header>
        <div class={styles.body}>
          <label class={styles.option}>
            <input
              type="radio"
              name="guild-mode"
              checked={mode() === "create"}
              onChange={() => setMode("create")}
            />
            <span>Create a new guild</span>
          </label>
          <Show when={mode() === "create"}>
            <div class={styles.field}>
              <label class={styles.fieldLabel} for="guild-name">
                Guild name
              </label>
              <input
                id="guild-name"
                class={`bevel-sunken ${styles.input}`}
                type="text"
                maxlength="64"
                value={name()}
                onInput={(e) => setName(e.currentTarget.value)}
              />
            </div>
          </Show>
          <label class={styles.option}>
            <input
              type="radio"
              name="guild-mode"
              checked={mode() === "join"}
              onChange={() => setMode("join")}
            />
            <span>Join with an invite code</span>
          </label>
          <Show when={mode() === "join"}>
            <div class={styles.field}>
              <label class={styles.fieldLabel} for="guild-code">
                Invite code
              </label>
              <input
                id="guild-code"
                class={`bevel-sunken ${styles.input}`}
                type="text"
                maxlength="32"
                value={code()}
                onInput={(e) => setCode(e.currentTarget.value)}
              />
            </div>
          </Show>
          <Show when={error()}>
            <p class={styles.error} role="alert">
              {error()}
            </p>
          </Show>
        </div>
        <footer class={styles.buttons}>
          <button
            type="button"
            class={`bevel-raised btn-default ${styles.btn}`}
            disabled={busy()}
            onClick={() => void ok()}
          >
            OK
          </button>
          <button type="button" class={`bevel-raised ${styles.btn}`} onClick={() => props.onClose()}>
            Cancel
          </button>
        </footer>
      </div>
    </div>
  );
};

import { type Component, For, onMount } from "solid-js";
import {
  CONTROL_FIELDS,
  contrastRatio,
  type CustomControls,
  customTheme,
  hasSavedCustomTheme,
  saveCustomTheme,
  seedFromTheme,
  setCustomTheme,
} from "../../lib/customTheme";
import { setTheme, theme, THEMES, type Theme } from "../../lib/theme";
import styles from "./ThemeBuilderDialog.module.css";

/**
 * The in-app theme builder. "Custom" = a base built-in + five color knobs;
 * everything else is derived (see lib/customTheme). Edits preview live (the
 * theme effect re-applies on every customTheme() change); Save persists,
 * Cancel reverts to whatever was active when the dialog opened. Lazy-loaded so
 * none of this is in the initial/login bundle.
 */
const ThemeBuilderDialog: Component<{ onClose: () => void }> = (props) => {
  // Snapshot the state at open so Cancel can restore it (read once, untracked).
  const prevTheme = theme();
  const prevCustom = customTheme();
  let dialogRef: HTMLDivElement | undefined;

  onMount(() => {
    // First time from a built-in: load a previously-saved custom if one exists,
    // otherwise start as a faithful copy of the theme we opened from. Then preview.
    if (prevTheme !== "custom") {
      if (!hasSavedCustomTheme()) {
        setCustomTheme({ base: prevTheme, controls: seedFromTheme(prevTheme) });
      }
      setTheme("custom");
    }
    // Move focus into the dialog so Escape works immediately and aria-modal is honest.
    dialogRef?.focus();
  });

  const controls = (): CustomControls => customTheme().controls;

  const updateControl = (key: keyof CustomControls, value: string): void => {
    const c = customTheme();
    setCustomTheme({ ...c, controls: { ...c.controls, [key]: value } });
  };

  const changeBase = (base: Theme): void => {
    // A new base resets the five colors to that base's palette as a starting point.
    setCustomTheme({ base, controls: seedFromTheme(base) });
  };

  const save = (): void => {
    saveCustomTheme(customTheme());
    props.onClose();
  };

  const cancel = (): void => {
    setCustomTheme(prevCustom);
    setTheme(prevTheme);
    props.onClose();
  };

  const reset = (): void => changeBase(customTheme().base);

  const textContrast = (): number => contrastRatio(controls().text, controls().surface);
  const accentContrast = (): number => contrastRatio(controls().accent, controls().surface);
  const fmt = (n: number): string => `${n.toFixed(1)}:1`;

  const onKeyDown = (e: KeyboardEvent): void => {
    if (e.key === "Escape") cancel();
  };

  return (
    <div class={styles.scrim} onClick={cancel}>
      <div
        ref={dialogRef}
        class={styles.dialog}
        role="dialog"
        aria-modal="true"
        aria-label="Customize theme"
        tabindex={-1}
        onClick={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <header class={styles.titlebar}>
          <span class={styles.titleText}>Customize theme</span>
          <button type="button" class={styles.closeBtn} aria-label="Close" onClick={cancel}>
            ×
          </button>
        </header>

        <div class={styles.body}>
          <label class={styles.baseRow}>
            <span class={styles.baseLabel}>Start from</span>
            <select
              class={styles.baseSelect}
              value={customTheme().base}
              onChange={(e) => changeBase(e.currentTarget.value as Theme)}
            >
              <For each={THEMES}>{(t) => <option value={t.id}>{t.label}</option>}</For>
            </select>
          </label>
          <p class={styles.lead}>
            Pick five colors — bevels, buttons, dim text and the rest are derived to match. Start from a
            base with a similar light/dark feel for the most cohesive result.
          </p>

          <For each={CONTROL_FIELDS}>
            {(field) => (
              <div class={styles.row}>
                <input
                  type="color"
                  class={styles.swatch}
                  aria-label={field.label}
                  value={controls()[field.key]}
                  onInput={(e) => updateControl(field.key, e.currentTarget.value)}
                />
                <div class={styles.rowText}>
                  <span class={styles.rowLabel}>{field.label}</span>
                  <span class={styles.rowHint}>{field.hint}</span>
                </div>
                <code class={styles.hex}>{controls()[field.key]}</code>
              </div>
            )}
          </For>

          <div class={styles.contrast}>
            <span class={styles.contrastTitle}>Readability</span>
            <span class={textContrast() >= 4.5 ? styles.ok : styles.warn}>
              Text {fmt(textContrast())} {textContrast() >= 4.5 ? "✓" : "⚠ low"}
            </span>
            <span class={accentContrast() >= 3 ? styles.ok : styles.warn}>
              Accent {fmt(accentContrast())} {accentContrast() >= 3 ? "✓" : "⚠ low"}
            </span>
          </div>
        </div>

        <footer class={styles.buttons}>
          <button type="button" class={`bevel-raised ${styles.btn}`} onClick={reset}>
            Reset
          </button>
          <span class={styles.spring} />
          <button type="button" class={`bevel-raised ${styles.btn}`} onClick={cancel}>
            Cancel
          </button>
          <button type="button" class={`bevel-raised btn-default ${styles.btn}`} onClick={save}>
            Save
          </button>
        </footer>
      </div>
    </div>
  );
};

export default ThemeBuilderDialog;

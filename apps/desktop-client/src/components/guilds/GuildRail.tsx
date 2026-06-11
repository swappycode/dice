import { For, type Component } from "solid-js";
import { directory, selectDmHome, selectGuild, selectedGuildId } from "../../stores/guilds";
import { initialsOf } from "../common/Avatar";
import styles from "./GuildRail.module.css";

/** Vertical XP taskbar: green Start-style DM pill, quick-launch guild tiles. */
export const GuildRail: Component<{ onAddGuild: () => void }> = (props) => (
  <nav class={styles.rail} aria-label="Guilds">
    <div class={styles.tileWrap}>
      <button
        type="button"
        class={`${styles.startPill} ${selectedGuildId() === null ? styles.startActive : ""}`}
        onClick={selectDmHome}
      >
        DM
      </button>
      <span class={`tooltip-classic ${styles.tip}`}>Direct messages</span>
    </div>
    <div class={styles.sep} />
    <For each={directory.guilds}>
      {(g) => (
        <div class={styles.tileWrap}>
          <button
            type="button"
            class={`${styles.tile} ${selectedGuildId() === g.id ? styles.active : ""}`}
            onClick={() => selectGuild(g.id)}
          >
            {initialsOf(g.name)}
          </button>
          <span class={`tooltip-classic ${styles.tip}`}>{g.name}</span>
        </div>
      )}
    </For>
    <div class={styles.spring} />
    <div class={styles.tileWrap}>
      <button
        type="button"
        class={`${styles.tile} ${styles.addTile}`}
        onClick={() => props.onAddGuild()}
        aria-label="Add or join a guild"
      >
        +
      </button>
      <span class={`tooltip-classic ${styles.tip}`}>Add or join a guild</span>
    </div>
  </nav>
);

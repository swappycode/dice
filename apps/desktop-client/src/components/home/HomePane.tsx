import { Show, type Component } from "solid-js";
import { incomingCount } from "../../stores/friends";
import { SelfStrip } from "../common/SelfStrip";
import { DmList } from "../dm/DmList";
import { FriendsList } from "../friends/FriendsList";
import { homeTab, setHomeTab } from "./homeTab";
import styles from "./HomePane.module.css";

/** The no-guild home column: a Messages/Friends tab strip over the active list,
 *  with one shared SelfStrip at the bottom. */
export const HomePane: Component = () => (
  <aside class={styles.panel} aria-label="Home">
    <div class={styles.tabs} role="tablist">
      <button
        type="button"
        role="tab"
        aria-selected={homeTab() === "messages"}
        class={`${styles.tab} ${homeTab() === "messages" ? styles.active : ""}`}
        onClick={() => setHomeTab("messages")}
      >
        Messages
      </button>
      <button
        type="button"
        role="tab"
        aria-selected={homeTab() === "friends"}
        class={`${styles.tab} ${homeTab() === "friends" ? styles.active : ""}`}
        onClick={() => setHomeTab("friends")}
      >
        Friends
        <Show when={incomingCount() > 0}>
          <span class={styles.badge}>{incomingCount()}</span>
        </Show>
      </button>
    </div>
    <Show when={homeTab() === "friends"} fallback={<DmList />}>
      <FriendsList />
    </Show>
    <SelfStrip />
  </aside>
);

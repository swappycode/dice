import { createSignal } from "solid-js";

/** Which home pane is showing when no guild is selected. Lifted out of HomePane
 *  so FriendsList can switch back to "messages" after opening a DM. */
export type HomeTab = "messages" | "friends";

const [homeTab, setHomeTab] = createSignal<HomeTab>("messages");

export { homeTab, setHomeTab };

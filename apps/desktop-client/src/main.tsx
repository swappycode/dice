/* @refresh reload */
import { render } from "solid-js/web";

import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/recipes.css";
import "./styles/scrollbars.css";
import "./themes/luna.css";
import "./themes/aero.css";
import "./themes/midnight.css";
import "./themes/nocturne.css";
import "./themes/bubble.css";
import "./themes/phosphor.css";

import App from "./App";
import { installDispatcher } from "./gateway/dispatcher";
import { installTypingSweep } from "./stores/typing";

// Pause all CSS animations while the window is hidden/blurred (idle-CPU rule).
function syncIdleClass(): void {
  const idle = document.hidden || !document.hasFocus();
  document.documentElement.classList.toggle("app-idle", idle);
}
document.addEventListener("visibilitychange", syncIdleClass);
window.addEventListener("blur", syncIdleClass);
window.addEventListener("focus", syncIdleClass);
syncIdleClass();

installDispatcher();
installTypingSweep();

render(() => <App />, document.getElementById("root")!);

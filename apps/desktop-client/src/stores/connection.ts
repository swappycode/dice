import { createSignal } from "solid-js";
import type { ConnState } from "../lib/types";

const [connState, setConnState] = createSignal<ConnState>("idle");

const CONN_LABEL: Record<ConnState, string> = {
  idle: "Offline",
  connecting: "Connecting…",
  connected: "Connected",
  reconnecting: "Reconnecting…",
  offline: "Offline",
};

function connLabel(): string {
  return CONN_LABEL[connState()];
}

export { connState, setConnState, connLabel };

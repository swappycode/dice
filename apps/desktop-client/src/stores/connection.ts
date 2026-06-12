import { createSignal } from "solid-js";
import type { ConnState } from "../lib/types";

const [connState, setConnState] = createSignal<ConnState>("idle");
/** Active transport while connected ("quic" | "wss"); null otherwise. */
const [transport, setTransport] = createSignal<"quic" | "wss" | null>(null);

const CONN_LABEL: Record<ConnState, string> = {
  idle: "Offline",
  connecting: "Connecting…",
  connected: "Connected",
  reconnecting: "Reconnecting…",
  offline: "Offline",
};

function connLabel(): string {
  const base = CONN_LABEL[connState()];
  const t = transport();
  return connState() === "connected" && t ? `${base} (${t.toUpperCase()})` : base;
}

export { connState, setConnState, transport, setTransport, connLabel };

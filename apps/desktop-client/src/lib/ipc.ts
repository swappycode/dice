/**
 * The IPC seam. The UI talks ONLY to this interface; in a later phase the
 * Tauri host (src-tauri) implements it over invoke/listen, today the mock
 * (ipc.mock.ts) implements it with in-memory fixtures.
 *
 * Selection: VITE_MOCK_IPC (default "true") — and we always fall back to
 * the mock when window.__TAURI__ is absent, so `npm run dev` in a plain
 * browser can never hit a dead bridge.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { createMockIpc } from "./ipc.mock";
import type {
  Bootstrap,
  Channel,
  DiceEvent,
  Guild,
  Message,
  PresenceStatus,
  Session,
} from "./types";

export interface DiceIpc {
  /** Resume an existing session if the host has one (keyring later, localStorage in mock). */
  getSession(): Promise<Session | null>;
  login(email: string, password: string): Promise<Session>;
  register(email: string, username: string, password: string): Promise<Session>;
  logout(): Promise<void>;

  getBootstrap(): Promise<Bootstrap>;

  /** Optimistic send: caller generates the nonce, renders a pending row, and
      reconciles on the `messageCreate` event echoing the same nonce. */
  sendMessage(channelId: string, content: string, nonce: string): Promise<void>;
  fetchMessages(channelId: string, before?: string, limit?: number): Promise<Message[]>;

  startTyping(channelId: string): Promise<void>;
  setPresence(status: PresenceStatus): Promise<void>;

  createGuild(name: string): Promise<Guild>;
  joinGuild(code: string): Promise<Guild>;
  openDm(recipientId: string): Promise<Channel>;

  /** Subscribe to the gateway event stream. Returns an unsubscribe fn. */
  onEvent(cb: (ev: DiceEvent) => void): () => void;
}

declare global {
  interface Window {
    __TAURI__?: unknown;
  }
}

export const hasTauri = typeof window !== "undefined" && "__TAURI__" in window;

export const MOCK_IPC: boolean = import.meta.env.VITE_MOCK_IPC !== "false" || !hasTauri;

/* ---- Tauri-backed implementation (wired up when src-tauri lands) ---- */

const EVENT_CHANNEL = "dice://event";

const tauriIpc: DiceIpc = {
  getSession: () => invoke<Session | null>("session_status"),
  login: (email, password) => invoke<Session>("login", { email, password }),
  register: (email, username, password) =>
    invoke<Session>("register", { email, username, password }),
  logout: () => invoke<void>("logout"),
  getBootstrap: () => invoke<Bootstrap>("get_bootstrap"),
  sendMessage: (channelId, content, nonce) =>
    invoke<void>("send_message", { channelId, content, nonce }),
  fetchMessages: (channelId, before, limit) =>
    invoke<Message[]>("fetch_messages", { channelId, before, limit }),
  startTyping: (channelId) => invoke<void>("start_typing", { channelId }),
  setPresence: (status) => invoke<void>("set_presence", { status }),
  createGuild: (name) => invoke<Guild>("create_guild", { name }),
  joinGuild: (code) => invoke<Guild>("join_guild", { code }),
  openDm: (recipientId) => invoke<Channel>("open_dm", { recipientId }),
  onEvent: (cb) => {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    void listen<DiceEvent>(EVENT_CHANNEL, (e) => cb(e.payload)).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  },
};

export const ipc: DiceIpc = MOCK_IPC ? createMockIpc() : tauriIpc;

/**
 * The REAL DiceIpc: Tauri 2 invoke/listen against the src-tauri host.
 *
 * Contract notes (must mirror src-tauri/src/commands.rs):
 * - command names are snake_case; argument keys are camelCase (Tauri 2 maps
 *   them onto the snake_case Rust parameters by default);
 * - every id crosses IPC as a string;
 * - command failures reject with a plain user-presentable string — we wrap
 *   them in `Error` so callers can keep `err instanceof Error` (the same
 *   shape the mock throws).
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { DiceIpc } from "./ipc";
import type {
  Bootstrap,
  Channel,
  DiceEvent,
  Guild,
  Message,
  Session,
} from "./types";

/** The single host→webview event stream (src-tauri/src/dto.rs EVENT_CHANNEL). */
const EVENT_CHANNEL = "dice://event";

function toError(e: unknown): Error {
  if (e instanceof Error) return e;
  if (typeof e === "string") return new Error(e);
  return new Error(JSON.stringify(e));
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw toError(e);
  }
}

export function createTauriIpc(): DiceIpc {
  return {
    getSession: () => call<Session | null>("session_status"),
    login: (email, password) => call<Session>("login", { email, password }),
    register: (email, username, password) =>
      call<Session>("register", { email, username, password }),
    logout: () => call<void>("logout"),
    getBootstrap: () => call<Bootstrap>("get_bootstrap"),
    // The host returns the pending message row; the DiceIpc seam only needs
    // the promise (the UI renders its own optimistic row keyed by nonce).
    sendMessage: (channelId, content, nonce) =>
      call<void>("send_message", { channelId, content, nonce }),
    fetchMessages: (channelId, before, limit) =>
      call<Message[]>("fetch_messages", { channelId, before, limit }),
    startTyping: (channelId) => call<void>("start_typing", { channelId }),
    setPresence: (status) => call<void>("set_presence", { status }),
    createGuild: (name) => call<Guild>("create_guild", { name }),
    joinGuild: (code) => call<Guild>("join_guild", { code }),
    openDm: (recipientId) => call<Channel>("open_dm", { recipientId }),
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
}

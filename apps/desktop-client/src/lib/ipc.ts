/**
 * The IPC seam. The UI talks ONLY to this interface; inside the Tauri shell
 * the real host implementation (ipc.real.ts, invoke/listen) is selected,
 * everywhere else the in-memory mock (ipc.mock.ts) serves the demo fixtures.
 *
 * Selection: real when running inside Tauri (detected via the always-present
 * `__TAURI_INTERNALS__`; `__TAURI__` only exists with `withGlobalTauri`).
 * `VITE_MOCK_IPC=true` forces the mock even inside Tauri (UI demos against
 * fixtures); a plain browser ALWAYS gets the mock so `npm run dev` can never
 * hit a dead bridge.
 */

import { createMockIpc } from "./ipc.mock";
import { createTauriIpc } from "./ipc.real";
import type {
  Attachment,
  Bootstrap,
  Channel,
  DiceEvent,
  Guild,
  Message,
  PresenceStatus,
  Session,
} from "./types";

export interface DiceIpc {
  /** Resume an existing session if the host has one (keyring; localStorage in mock). */
  getSession(): Promise<Session | null>;
  login(email: string, password: string): Promise<Session>;
  register(email: string, username: string, password: string): Promise<Session>;
  logout(): Promise<void>;

  getBootstrap(): Promise<Bootstrap>;

  /** Optimistic send: caller generates the nonce, renders a pending row, and
      reconciles on the `messageCreate` event echoing the same nonce.
      `attachmentIds` are media ids returned by prior `uploadAttachment` calls. */
  sendMessage(
    channelId: string,
    content: string,
    nonce: string,
    replyToId?: string,
    attachmentIds?: string[],
  ): Promise<void>;
  /** Upload one file ahead of a send; returns its stored metadata (the `id`
      is then passed to `sendMessage` in `attachmentIds`). */
  uploadAttachment(file: File): Promise<Attachment>;
  /** Resolve an attachment's bytes to a URL the webview can render directly
      (`<img src>` / download link). Cached per id by the implementation.
      Avatars are media too, so the UI resolves them through this same call. */
  attachmentSrc(mediaId: string): Promise<string>;
  /** Set (mediaId) or clear (null) the current user's avatar; the change comes
      back as a `userUpdate` event. */
  setAvatar(mediaId: string | null): Promise<void>;
  /** Edit (author-only); the UI reconciles on the `messageUpdate` event. */
  editMessage(channelId: string, messageId: string, content: string): Promise<void>;
  /** Delete (author, or MANAGE_MESSAGES); reconciles on `messageDelete`. */
  deleteMessage(channelId: string, messageId: string): Promise<void>;
  /** Toggle a reaction; reconciles on the `reactionUpdate` delta. */
  react(channelId: string, messageId: string, emoji: string, add: boolean): Promise<void>;
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
    __TAURI_INTERNALS__?: unknown;
  }
}

export const hasTauri =
  typeof window !== "undefined" &&
  ("__TAURI_INTERNALS__" in window || "__TAURI__" in window);

export const MOCK_IPC: boolean = !hasTauri || import.meta.env.VITE_MOCK_IPC === "true";

export const ipc: DiceIpc = MOCK_IPC ? createMockIpc() : createTauriIpc();

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
  Attachment,
  Bootstrap,
  Channel,
  DiceEvent,
  Friend,
  Guild,
  LoginResult,
  Message,
  Session,
  TotpEnroll,
  VoiceRoster,
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

/** Read a File as `{ contentType, dataBase64 }` (FileReader gives a
 *  `data:<mime>;base64,<b64>` URL; we split off the base64 payload). */
function readFileBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error("could not read the selected file"));
    reader.onload = () => {
      const result = String(reader.result);
      const comma = result.indexOf(",");
      resolve(comma >= 0 ? result.slice(comma + 1) : result);
    };
    reader.readAsDataURL(file);
  });
}

/** Resolved attachment data: URLs, deduped per media id (bytes fetched once). */
const attachmentSrcCache = new Map<string, Promise<string>>();

export function createTauriIpc(): DiceIpc {
  return {
    getSession: () => call<Session | null>("session_status"),
    login: (email, password) => call<LoginResult>("login", { email, password }),
    completeTotpLogin: (ticket, code) =>
      call<Session>("complete_totp_login", { ticket, code }),
    register: (email, username, password) =>
      call<Session>("register", { email, username, password }),
    logout: () => call<void>("logout"),
    totpEnroll: () => call<TotpEnroll>("totp_enroll"),
    totpConfirm: (code) => call<string[]>("totp_confirm", { code }),
    totpDisable: (code) => call<void>("totp_disable", { code }),
    verifyEmail: (token) => call<void>("verify_email", { token }),
    resendVerification: () => call<void>("resend_verification"),
    requestPasswordReset: (email) => call<void>("request_password_reset", { email }),
    resetPassword: (token, newPassword) => call<void>("reset_password", { token, newPassword }),
    getBootstrap: () => call<Bootstrap>("get_bootstrap"),
    // The host returns the pending message row; the DiceIpc seam only needs
    // the promise (the UI renders its own optimistic row keyed by nonce).
    sendMessage: (channelId, content, nonce, replyToId, attachmentIds) =>
      call<void>("send_message", {
        channelId,
        content,
        nonce,
        replyToId: replyToId ?? null,
        attachmentIds: attachmentIds ?? [],
      }),
    uploadAttachment: async (file) => {
      const dataBase64 = await readFileBase64(file);
      return call<Attachment>("upload_attachment", {
        filename: file.name,
        contentType: file.type || "application/octet-stream",
        dataBase64,
      });
    },
    attachmentSrc: (mediaId) => {
      let pending = attachmentSrcCache.get(mediaId);
      if (!pending) {
        pending = call<string>("fetch_attachment", { mediaId });
        attachmentSrcCache.set(mediaId, pending);
      }
      return pending;
    },
    setAvatar: (mediaId) => call<void>("set_avatar", { mediaId }),
    editMessage: (channelId, messageId, content) =>
      call<void>("edit_message", { channelId, messageId, content }),
    deleteMessage: (channelId, messageId) =>
      call<void>("delete_message", { channelId, messageId }),
    react: (channelId, messageId, emoji, add) =>
      call<void>("react", { channelId, messageId, emoji, add }),
    fetchMessages: (channelId, before, limit) =>
      call<Message[]>("fetch_messages", { channelId, before, limit }),
    startTyping: (channelId) => call<void>("start_typing", { channelId }),
    setPresence: (status) => call<void>("set_presence", { status }),
    createGuild: (name) => call<Guild>("create_guild", { name }),
    joinGuild: (code) => call<Guild>("join_guild", { code }),
    openDm: (recipientId) => call<Channel>("open_dm", { recipientId }),
    createChannel: (guildId, name, kind) =>
      call<Channel>("create_channel", { guildId, name, kind }),
    listFriends: () => call<Friend[]>("list_friends"),
    addFriend: (username) => call<Friend>("add_friend", { username }),
    acceptFriend: (userId) => call<Friend>("accept_friend", { userId }),
    declineFriend: (userId) => call<void>("decline_friend", { userId }),
    removeFriend: (userId) => call<void>("remove_friend", { userId }),
    voiceJoin: (channelId, muted, deafened) =>
      call<VoiceRoster>("voice_join", { channelId, muted, deafened }),
    voiceLeave: (channelId) => call<void>("voice_leave", { channelId }),
    voiceState: (channelId, muted, deafened, speaking) =>
      call<void>("voice_state", { channelId, muted, deafened, speaking }),
    voiceRoster: (channelId) => call<VoiceRoster>("voice_roster", { channelId }),
    fetchUnread: async () => {
      const list = await call<{ channelId: string; count: number }[]>("fetch_unread");
      const map: Record<string, number> = {};
      for (const e of list) map[e.channelId] = e.count;
      return map;
    },
    markRead: (channelId) => call<void>("mark_read", { channelId }),
    notify: (title, body) => call<void>("notify", { title, body }),
    setPtt: (enabled, key) => call<void>("set_ptt", { enabled, key }),
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

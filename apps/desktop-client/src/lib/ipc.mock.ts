/**
 * In-memory mock of the DiceIpc interface (no network, no Tauri).
 * Fixtures: 2 guilds / 4 guild channels / 2 DMs / ~30 messages / 4 other
 * users with varied presence. Sends echo back after 150 ms with a real id
 * and the caller's nonce; a fake incoming message + typing burst fires
 * every ~20 s while the page is visible.
 */

import { DICE_EPOCH_MS } from "./time";
import type { DiceIpc } from "./ipc";
import type {
  Attachment,
  Bootstrap,
  Channel,
  DiceEvent,
  Guild,
  Message,
  PresenceStatus,
  Session,
  User,
} from "./types";

/* ---- snowflake-ish id generator (string, BigInt-safe) ---- */

let seq = 0;
function genId(ms: number): string {
  seq = (seq + 1) & 0xfff;
  return ((BigInt(Math.max(ms - DICE_EPOCH_MS, 1)) << 22n) | BigInt(seq)).toString();
}

/* ---- fixtures ---- */

const now = Date.now();
const min = 60_000;

const SELF: User = { id: genId(now - 400 * 24 * 60 * min), username: "sooru", displayName: "Sooru" };
const AYAAN: User = { id: genId(now - 390 * 24 * 60 * min), username: "ayaan_xp", displayName: "Ayaan_xp" };
const PRIYA: User = { id: genId(now - 380 * 24 * 60 * min), username: "priya7", displayName: "Priya7" };
const MOSS: User = { id: genId(now - 370 * 24 * 60 * min), username: "mossdog", displayName: "MossDog" };
const GLITCH: User = { id: genId(now - 360 * 24 * 60 * min), username: "glitch", displayName: "Glitch" };

const users: User[] = [SELF, AYAAN, PRIYA, MOSS, GLITCH];

const presence: Record<string, PresenceStatus> = {
  [SELF.id]: "online",
  [AYAAN.id]: "online",
  [PRIYA.id]: "idle",
  [MOSS.id]: "dnd",
  [GLITCH.id]: "offline",
};

function mkChannel(guildId: string | null, name: string, position: number): Channel {
  return {
    id: genId(now - 300 * 24 * 60 * min + position * min),
    guildId,
    kind: guildId ? "guild_text" : "dm",
    name,
    position,
    lastMessageId: null,
    recipientIds: [],
  };
}

const guildHq: Guild = {
  id: genId(now - 350 * 24 * 60 * min),
  name: "Dice HQ",
  ownerId: SELF.id,
  inviteCode: "DICE-HQ-01",
  members: [] as Guild["members"],
};
guildHq.members = users.map((u) => ({ userId: u.id, guildId: guildHq.id }));

const guildRetro: Guild = {
  id: genId(now - 340 * 24 * 60 * min),
  name: "Retro Computing",
  ownerId: AYAAN.id,
  inviteCode: "RETRO-99",
  members: [] as Guild["members"],
};
guildRetro.members = [SELF, AYAAN, GLITCH].map((u) => ({ userId: u.id, guildId: guildRetro.id }));

const guilds: Guild[] = [guildHq, guildRetro];

const chGeneral = mkChannel(guildHq.id, "general", 0);
const chDev = mkChannel(guildHq.id, "dev", 1);
const chOffTopic = mkChannel(guildHq.id, "off-topic", 2);
const chLounge = mkChannel(guildRetro.id, "lounge", 0);
const channels: Channel[] = [chGeneral, chDev, chOffTopic, chLounge];

const dmPriya = mkChannel(null, "", 0);
dmPriya.recipientIds = [SELF.id, PRIYA.id];
const dmMoss = mkChannel(null, "", 1);
dmMoss.recipientIds = [SELF.id, MOSS.id];
const dms: Channel[] = [dmPriya, dmMoss];

/** Canonical per-channel message logs (ascending by time). */
const logs = new Map<string, Message[]>();
/** Remaining "Load older" filler pages per channel. */
const fillerPages = new Map<string, number>();

function seed(channel: Channel, author: User, content: string, minutesAgo: number): void {
  const ms = now - minutesAgo * min;
  const m: Message = {
    id: genId(ms),
    channelId: channel.id,
    authorId: author.id,
    content,
    createdAtMs: ms,
    editedAtMs: null,
  };
  const log = logs.get(channel.id) ?? [];
  log.push(m);
  logs.set(channel.id, log);
  channel.lastMessageId = m.id;
}

// ~30 seeded messages: #general gets a two-day conversation, the rest a sprinkle.
const Y = 24 * 60; // minutes in a day â†’ yesterday
seed(chGeneral, AYAAN, "ok the retro UI direction is locked in", Y + 95);
seed(chGeneral, AYAAN, "Luna AND Aero, switchable live", Y + 94);
seed(chGeneral, PRIYA, "the orange hover ring is so 2003, I love it", Y + 90);
seed(chGeneral, MOSS, "as long as the scrollbars have arrow buttons I'm in", Y + 82);
seed(chGeneral, SELF, "they will. beveled, with the little triangles", Y + 80);
seed(chGeneral, PRIYA, "shipping a statusbar in 2026 is a power move", Y + 41);
seed(chGeneral, AYAAN, "did you try the new build?", 132);
seed(chGeneral, AYAAN, "cold start is under 2s now", 131);
seed(chGeneral, PRIYA, "LETS GOOO", 128);
seed(chGeneral, MOSS, "ram floor is basically the webview, host is tiny", 120);
seed(chGeneral, SELF, "pushing the mock fixtures today, then the tauri side", 95);
seed(chGeneral, AYAAN, "presence orbs look properly glossy now btw", 64);
seed(chGeneral, PRIYA, "the offline hollow ring is a nice touch", 61);
seed(chGeneral, GLITCH, "back online later, dentist", 58);
seed(chGeneral, SELF, "tooltip yellow is #ffffe1 or it doesn't count", 33);
seed(chGeneral, AYAAN, "obviously", 32);
seed(chGeneral, MOSS, "someone test aero with the glass blur on a potato gpu", 18);
seed(chGeneral, PRIYA, "my laptop IS the potato gpu, it's fine", 12);
seed(chGeneral, AYAAN, "ok shipping it", 6);

seed(chDev, SELF, "tsc is clean, vite build is clean", 200);
seed(chDev, AYAAN, "css budget check passes? we said <100KB raw", 185);
seed(chDev, SELF, "way under, both themes always loaded", 180);
seed(chDev, MOSS, "remember: no hardcoded hex in component css", 90);

seed(chOffTopic, PRIYA, "found my old mp3 player in a drawer", 300);
seed(chOffTopic, MOSS, "does it still hold charge??", 295);
seed(chOffTopic, PRIYA, "11 hours of battery. they don't make em like that", 290);

seed(chLounge, AYAAN, "anyone restored a CRT lately", 400);
seed(chLounge, GLITCH, "got a 17 inch trinitron last week, it weighs a ton", 395);

seed(dmPriya, PRIYA, "hey, did the build finish?", 75);
seed(dmPriya, SELF, "yep â€” try the aero theme, it's glossy", 71);
seed(dmPriya, PRIYA, "ooh the titlebar glow is perfect", 68);
seed(dmMoss, MOSS, "ping me when the member sidebar is clickable", 150);

for (const c of [...channels, ...dms]) fillerPages.set(c.id, 2);

/* ---- mock impl ---- */

const SESSION_KEY = "dice.mock.session";

const AMBIENT_LINES: Array<[User, Channel, string]> = [
  [AYAAN, chGeneral, "anyone else getting nostalgia hits from the statusbar"],
  [PRIYA, chGeneral, "I keep toggling luna/aero for fun"],
  [MOSS, chDev, "pushed a tweak to the bevel recipe"],
  [PRIYA, dmPriya, "lunch later?"],
  [AYAAN, chGeneral, "the start pill should pulse... kidding, no infinite animations"],
  [MOSS, chGeneral, "load older works, pagination is smooth"],
];

export function createMockIpc(): DiceIpc {
  const subscribers = new Set<(ev: DiceEvent) => void>();
  // Uploaded attachments live as object URLs (browser-only; no backend).
  const uploads = new Map<string, { attachment: Attachment; url: string }>();
  let session: Session | null = localStorage.getItem(SESSION_KEY) ? { user: SELF } : null;
  let ambientTimer: ReturnType<typeof setInterval> | null = null;
  let ambientIdx = 0;
  // Mock 2FA state (browser demo of the enroll/challenge/disable flow).
  let totpSecret: string | null = null; // set on enroll (pending until confirm)
  let totpEnabled = false;
  let recoveryCodes: string[] = [];
  const norm = (s: string): string => s.replace(/[^a-z0-9]/gi, "").toUpperCase();

  function emit(ev: DiceEvent): void {
    for (const cb of subscribers) cb(ev);
  }

  function delay(ms: number): Promise<void> {
    return new Promise((r) => setTimeout(r, ms));
  }

  function appendToLog(m: Message): void {
    const log = logs.get(m.channelId) ?? [];
    log.push(m);
    logs.set(m.channelId, log);
    const ch = [...channels, ...dms].find((c) => c.id === m.channelId);
    if (ch) ch.lastMessageId = m.id;
  }

  function ambientTick(): void {
    if (document.visibilityState !== "visible" || !session) return;
    const entry = AMBIENT_LINES[ambientIdx % AMBIENT_LINES.length];
    ambientIdx++;
    if (!entry) return;
    const [author, channel, content] = entry;
    emit({ type: "typingStart", channelId: channel.id, userId: author.id });
    setTimeout(() => {
      if (document.visibilityState !== "visible" || !session) return;
      const ms = Date.now();
      const m: Message = {
        id: genId(ms),
        channelId: channel.id,
        authorId: author.id,
        content,
        createdAtMs: ms,
        editedAtMs: null,
      };
      appendToLog(m);
      emit({ type: "messageCreate", message: m });
    }, 2500);
  }

  function startAmbient(): void {
    ambientTimer ??= setInterval(ambientTick, 20_000);
  }

  function connectFlow(): void {
    emit({ type: "connState", state: "connecting" });
    setTimeout(() => emit({ type: "connState", state: "connected", transport: "wss" }), 700);
  }

  return {
    async getSession() {
      await delay(50);
      if (session) setTimeout(connectFlow, 0);
      return session;
    },

    async login(email, _password) {
      await delay(300);
      if (!email.includes("@")) throw new Error("Enter a valid e-mail address.");
      if (totpEnabled) return { totpTicket: "mock-ticket" };
      session = { user: SELF };
      localStorage.setItem(SESSION_KEY, "1");
      connectFlow();
      return { session };
    },

    async completeTotpLogin(ticket, code) {
      await delay(200);
      if (ticket !== "mock-ticket") {
        throw new Error("Your verification session expired. Log in again.");
      }
      const isTotp = /^\d{6}$/.test(code.trim());
      const recovery = recoveryCodes.find((c) => norm(c) === norm(code));
      if (!isTotp && !recovery) throw new Error("Invalid code. Try again.");
      if (recovery) recoveryCodes = recoveryCodes.filter((c) => c !== recovery);
      session = { user: SELF };
      localStorage.setItem(SESSION_KEY, "1");
      connectFlow();
      return session;
    },

    async totpEnroll() {
      await delay(120);
      const a = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"; // RFC 4648 base32
      totpSecret = Array.from({ length: 32 }, () => a[Math.floor(Math.random() * a.length)]).join("");
      return {
        secret: totpSecret,
        otpauthUri: `otpauth://totp/Dice:${SELF.username}?secret=${totpSecret}&issuer=Dice&algorithm=SHA1&digits=6&period=30`,
      };
    },

    async totpConfirm(code) {
      await delay(120);
      if (!totpSecret) throw new Error("Start enrollment first.");
      if (!/^\d{6}$/.test(code.trim())) {
        throw new Error("Enter the 6-digit code from your authenticator.");
      }
      totpEnabled = true;
      const a = "ABCDEFGHJKMNPQRSTVWXYZ23456789";
      recoveryCodes = Array.from({ length: 10 }, () => {
        const body = Array.from({ length: 10 }, () => a[Math.floor(Math.random() * a.length)]).join("");
        return `${body.slice(0, 5)}-${body.slice(5)}`;
      });
      return [...recoveryCodes];
    },

    async totpDisable(code) {
      await delay(120);
      if (!totpEnabled) throw new Error("Two-factor is not enabled.");
      const ok = /^\d{6}$/.test(code.trim()) || recoveryCodes.some((c) => norm(c) === norm(code));
      if (!ok) throw new Error("Invalid code.");
      totpEnabled = false;
      totpSecret = null;
      recoveryCodes = [];
    },

    async verifyEmail(token) {
      await delay(120);
      if (!token.trim().startsWith("dvt_")) throw new Error("That code doesn't look right.");
      // mock: any well-formed token verifies
    },

    async resendVerification() {
      await delay(120);
      // mock: pretend a fresh mail went out
    },

    async requestPasswordReset(email) {
      await delay(200);
      if (!email.includes("@")) throw new Error("Enter a valid e-mail address.");
      // mock: always "sends" (no enumeration)
    },

    async resetPassword(token, newPassword) {
      await delay(200);
      if (!token.trim().startsWith("drst_")) throw new Error("That reset code doesn't look right.");
      if (newPassword.length < 8) throw new Error("Password must be at least 8 characters.");
      // mock: accepts and resolves
    },

    async register(email, username, _password) {
      await delay(300);
      if (!email.includes("@")) throw new Error("Enter a valid e-mail address.");
      if (username.trim().length < 2) throw new Error("Username must be at least 2 characters.");
      session = { user: SELF };
      localStorage.setItem(SESSION_KEY, "1");
      connectFlow();
      return session;
    },

    async logout() {
      await delay(100);
      session = null;
      localStorage.removeItem(SESSION_KEY);
      emit({ type: "connState", state: "idle" });
    },

    async getBootstrap(): Promise<Bootstrap> {
      await delay(80);
      return {
        user: SELF,
        guilds: guilds.map((g) => ({ ...g, members: [...g.members] })),
        channels: channels.map((c) => ({ ...c })),
        dms: dms.map((c) => ({ ...c, recipientIds: [...c.recipientIds] })),
        users: users.map((u) => ({ ...u })),
        presence: { ...presence },
        lastChannelId: chGeneral.id,
      };
    },

    async sendMessage(channelId, content, nonce, replyToId, attachmentIds) {
      const attachments = (attachmentIds ?? [])
        .map((id) => uploads.get(id)?.attachment)
        .filter((a): a is Attachment => !!a);
      // echo after 150 ms with a real id + the caller's nonce (reconcile path)
      setTimeout(() => {
        const ms = Date.now();
        const m: Message = {
          id: genId(ms),
          channelId,
          authorId: SELF.id,
          content,
          createdAtMs: ms,
          editedAtMs: null,
          replyToId: replyToId ?? null,
          attachments,
        };
        appendToLog(m);
        emit({ type: "messageCreate", message: m, nonce });
      }, 150);
    },

    async uploadAttachment(file) {
      await delay(80);
      const id = genId(Date.now());
      const url = URL.createObjectURL(file);
      const attachment: Attachment = {
        id,
        filename: file.name,
        contentType: file.type || "application/octet-stream",
        sizeBytes: file.size,
        width: 0,
        height: 0,
      };
      uploads.set(id, { attachment, url });
      return attachment;
    },

    async attachmentSrc(mediaId) {
      return uploads.get(mediaId)?.url ?? "";
    },

    async setAvatar(mediaId) {
      await delay(60);
      emit({ type: "userUpdate", user: { ...SELF, avatarId: mediaId } });
    },

    async react(channelId, messageId, emoji, add) {
      setTimeout(
        () =>
          emit({
            type: "reactionUpdate",
            channelId,
            messageId,
            emoji,
            userId: SELF.id,
            added: add,
          }),
        60,
      );
    },

    async editMessage(channelId, messageId, content) {
      const trimmed = content.trim();
      if (!trimmed) throw new Error("Message cannot be empty.");
      const log = logs.get(channelId) ?? [];
      const m = log.find((x) => x.id === messageId);
      if (!m || m.authorId !== SELF.id) throw new Error("You can only edit your own messages.");
      const ms = Date.now();
      m.content = trimmed;
      m.editedAtMs = ms;
      setTimeout(() => emit({ type: "messageUpdate", message: { ...m } }), 80);
    },

    async deleteMessage(channelId, messageId) {
      const log = logs.get(channelId) ?? [];
      const i = log.findIndex((x) => x.id === messageId);
      if (i < 0 || log[i]!.authorId !== SELF.id) {
        throw new Error("You can only delete your own messages.");
      }
      log.splice(i, 1);
      setTimeout(() => emit({ type: "messageDelete", channelId, messageId }), 80);
    },

    async fetchMessages(channelId, before, limit = 50) {
      await delay(120);
      const log = logs.get(channelId) ?? [];
      if (!before) return log.slice(-limit).map((m) => ({ ...m }));

      // older history below the seeded window: synthesize up to 2 filler pages
      const older = log.filter((m) => BigInt(m.id) < BigInt(before));
      if (older.length >= limit) return older.slice(-limit).map((m) => ({ ...m }));

      const left = fillerPages.get(channelId) ?? 0;
      if (left <= 0) return older.map((m) => ({ ...m }));
      fillerPages.set(channelId, left - 1);

      const oldest = log[0];
      let cursor = (oldest ? oldest.createdAtMs : Date.now()) - 45 * min;
      const page: Message[] = [];
      for (let i = 0; i < 20; i++) {
        const author = users[(i + left) % users.length] ?? AYAAN;
        page.unshift({
          id: genId(cursor),
          channelId,
          authorId: author.id,
          content: `(archive) old log line ${left * 20 - i} â€” from before the demo window`,
          createdAtMs: cursor,
          editedAtMs: null,
        });
        cursor -= 7 * min;
      }
      const merged = [...page, ...log];
      logs.set(channelId, merged);
      return [...page, ...older].slice(-limit).map((m) => ({ ...m }));
    },

    async startTyping(_channelId) {
      /* gateway no-ops in the mock; others' typing comes from the ambient timer */
    },

    async setPresence(status) {
      await delay(60);
      presence[SELF.id] = status;
      emit({ type: "presenceUpdate", userId: SELF.id, status });
    },

    async createGuild(name) {
      await delay(200);
      const g: Guild = {
        id: genId(Date.now()),
        name: name.trim() || "New Guild",
        ownerId: SELF.id,
        inviteCode: `DICE-${Math.random().toString(36).slice(2, 7).toUpperCase()}`,
        members: [],
      };
      g.members = [{ userId: SELF.id, guildId: g.id }];
      const general = mkChannel(g.id, "general", 0);
      guilds.push(g);
      channels.push(general);
      fillerPages.set(general.id, 0);
      emit({ type: "guildCreate", guild: g, channels: [general] });
      return g;
    },

    async joinGuild(code) {
      await delay(250);
      const trimmed = code.trim();
      if (!trimmed) throw new Error("Enter an invite code.");
      const g: Guild = {
        id: genId(Date.now()),
        name: `Guild ${trimmed.toUpperCase()}`,
        ownerId: AYAAN.id,
        inviteCode: trimmed.toUpperCase(),
        members: [],
      };
      g.members = [SELF, AYAAN, PRIYA].map((u) => ({ userId: u.id, guildId: g.id }));
      const general = mkChannel(g.id, "general", 0);
      guilds.push(g);
      channels.push(general);
      fillerPages.set(general.id, 0);
      seed(general, AYAAN, "welcome to the guild!", 1);
      emit({ type: "guildCreate", guild: g, channels: [general] });
      return g;
    },

    async openDm(recipientId) {
      await delay(150);
      const existing = dms.find((c) => c.recipientIds.includes(recipientId));
      if (existing) return { ...existing };
      const ch = mkChannel(null, "", dms.length);
      ch.recipientIds = [SELF.id, recipientId];
      dms.push(ch);
      fillerPages.set(ch.id, 0);
      const other = users.find((u) => u.id === recipientId);
      emit({ type: "dmChannelCreate", channel: { ...ch }, users: other ? [other] : [] });
      return { ...ch };
    },

    async fetchUnread() {
      // The mock has no server-side counts; badges accrue live from the
      // dispatcher as ambient/echoed messages land in non-active channels.
      return {};
    },

    async markRead(_channelId) {
      /* no server in the mock; the store clears locally */
    },

    async notify(_title, _body) {
      /* no OS toast in the browser mock */
    },

    onEvent(cb) {
      subscribers.add(cb);
      startAmbient();
      return () => {
        subscribers.delete(cb);
        if (subscribers.size === 0 && ambientTimer !== null) {
          clearInterval(ambientTimer);
          ambientTimer = null;
        }
      };
    },
  };
}

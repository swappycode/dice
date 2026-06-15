/**
 * Voice store: who is in each voice channel, plus which channel (if any) this
 * client is currently in. Signaling only — there is no audio capture/playback
 * yet (that's the on-hardware phase). Rosters are kept live by the
 * `voiceJoin` / `voiceLeave` / `voiceState` dispatches; `joinVoice` seeds the
 * channel's roster from the REST snapshot and the dispatches reconcile from
 * there (the client receives its own voice events via the guild voice subject).
 */

import { createSignal } from "solid-js";
import { createStore, produce, reconcile } from "solid-js/store";
import { ipc } from "../lib/ipc";
import type { User, VoiceMember } from "../lib/types";

interface VoiceState {
  /** channelId → current members. */
  rosters: Record<string, VoiceMember[]>;
  /** userId → record, for rendering members (warm dictionary). */
  users: Record<string, User>;
  /** The voice channel THIS client is in, or null. */
  active: string | null;
}

const [voice, setVoice] = createStore<VoiceState>({
  rosters: {},
  users: {},
  active: null,
});

/** Members currently in `channelId` (reactive; empty if none). */
export function voiceMembers(channelId: string): VoiceMember[] {
  return voice.rosters[channelId] ?? [];
}

/** The user record for a voice member, if known. */
export function voiceUser(userId: string): User | undefined {
  return voice.users[userId];
}

/** The voice channel this client is in, or null. */
export function activeVoiceChannel(): string | null {
  return voice.active;
}

// --- Self mute / deafen. The signals drive the buttons; the host mirrors them
// into the audio engine (mute stops the mic, deafen stops playback) and fans the
// state out so peers see the muted/deaf tags. Deafen implies mute. ---

const [selfMuted, setSelfMuted] = createSignal(false);
const [selfDeafened, setSelfDeafened] = createSignal(false);

/** Whether this client's mic is muted. */
export const isSelfMuted = selfMuted;
/** Whether this client is deafened (own output silenced). */
export const isSelfDeafened = selfDeafened;

/** Optimistically tag our own roster member, then fan the state to the server
 *  (the VoiceState dispatch reconciles it on every client, including us). */
function pushSelfVoiceState(selfId: string): void {
  const channelId = voice.active;
  if (!channelId) return;
  const muted = selfMuted();
  const deafened = selfDeafened();
  setVoice(
    produce((s) => {
      const me = s.rosters[channelId]?.find((m) => m.userId === selfId);
      if (me) {
        me.muted = muted;
        me.deafened = deafened;
      }
    }),
  );
  void ipc.voiceState(channelId, muted, deafened, false).catch(() => {});
}

/** Toggle our mic mute. Unmuting also lifts deafen. */
export function toggleSelfMute(selfId: string): void {
  setSelfMuted((m) => !m);
  if (!selfMuted()) setSelfDeafened(false);
  pushSelfVoiceState(selfId);
}

/** Toggle deafen — silences our own output and (since you can't usefully talk
 *  while deafened) mutes the mic too. */
export function toggleSelfDeafen(selfId: string): void {
  const next = !selfDeafened();
  setSelfDeafened(next);
  setSelfMuted(next);
  pushSelfVoiceState(selfId);
}

function upsertMember(channelId: string, member: VoiceMember): void {
  setVoice(
    produce((s) => {
      const list = s.rosters[channelId] ?? [];
      const i = list.findIndex((m) => m.userId === member.userId);
      if (i >= 0) list[i] = member;
      else list.push(member);
      s.rosters[channelId] = list;
    }),
  );
}

/** Join a voice channel: seed its roster from the snapshot, then let the
 *  dispatches keep it live. Returns once the server has accepted the join. */
export async function joinVoice(channelId: string): Promise<void> {
  // Fresh join is unmuted/undeafened (matches the args below + the host engine).
  setSelfMuted(false);
  setSelfDeafened(false);
  const roster = await ipc.voiceJoin(channelId, false, false);
  setVoice(
    produce((s) => {
      s.rosters[channelId] = roster.members;
      for (const u of roster.users) s.users[u.id] = u;
      s.active = channelId;
    }),
  );
}

/** Leave the active voice channel (no-op if not in one). */
export async function leaveVoice(): Promise<void> {
  const channelId = voice.active;
  if (!channelId) return;
  setVoice("active", null);
  setSelfMuted(false);
  setSelfDeafened(false);
  await ipc.voiceLeave(channelId);
}

/** Apply a live `voiceJoin`. */
export function applyVoiceJoin(member: VoiceMember, user?: User): void {
  if (user) setVoice("users", user.id, user);
  upsertMember(member.channelId, member);
}

/** Apply a live `voiceLeave`. `isSelf` is true when the leaving user is this
 *  client, so a server-driven removal (kick, or joining voice on another device)
 *  clears our active channel even though we didn't press leave here. */
export function applyVoiceLeave(channelId: string, userId: string, isSelf = false): void {
  setVoice(
    produce((s) => {
      const list = s.rosters[channelId];
      if (list) s.rosters[channelId] = list.filter((m) => m.userId !== userId);
      // Only OUR own departure from the channel we're in clears `active` — a
      // remote peer leaving the same channel must not.
      if (isSelf && s.active === channelId) s.active = null;
    }),
  );
}

/** Apply a live `voiceState` (mute / deafen / speaking change). */
export function applyVoiceState(member: VoiceMember): void {
  upsertMember(member.channelId, member);
}

/** Clear everything on logout / account switch. */
export function resetVoice(): void {
  setVoice(reconcile({ rosters: {}, users: {}, active: null }));
  setSelfMuted(false);
  setSelfDeafened(false);
}

export { voice };

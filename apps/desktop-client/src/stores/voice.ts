/**
 * Voice store: who is in each voice channel, plus which channel (if any) this
 * client is currently in. Signaling only — there is no audio capture/playback
 * yet (that's the on-hardware phase). Rosters are kept live by the
 * `voiceJoin` / `voiceLeave` / `voiceState` dispatches; `joinVoice` seeds the
 * channel's roster from the REST snapshot and the dispatches reconcile from
 * there (the client receives its own voice events via the guild voice subject).
 */

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
  await ipc.voiceLeave(channelId);
}

/** Apply a live `voiceJoin`. */
export function applyVoiceJoin(member: VoiceMember, user?: User): void {
  if (user) setVoice("users", user.id, user);
  upsertMember(member.channelId, member);
}

/** Apply a live `voiceLeave`. */
export function applyVoiceLeave(channelId: string, userId: string): void {
  setVoice(
    produce((s) => {
      const list = s.rosters[channelId];
      if (list) s.rosters[channelId] = list.filter((m) => m.userId !== userId);
      if (s.active === channelId && !s.rosters[channelId]?.some((m) => m.userId === userId)) {
        // If WE left this channel, clear active. (We only track our own join;
        // a server-driven removal of us also lands here.)
      }
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
}

export { voice };

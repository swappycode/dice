import { createSignal } from "solid-js";
import { createStore, reconcile, produce } from "solid-js/store";
import type { Bootstrap, Channel, Guild, User } from "../lib/types";

interface Directory {
  guilds: Guild[];
  channelsByGuild: Record<string, Channel[]>;
  dms: Channel[];
  usersById: Record<string, User>;
}

const [directory, setDirectory] = createStore<Directory>({
  guilds: [],
  channelsByGuild: {},
  dms: [],
  usersById: {},
});

/** null ⇒ DM home (the green Start pill). */
const [selectedGuildId, setSelectedGuildId] = createSignal<string | null>(null);
const [selectedChannelId, setSelectedChannelId] = createSignal<string | null>(null);

function byPosition(a: Channel, b: Channel): number {
  return a.position - b.position;
}

function groupChannels(channels: Channel[]): Record<string, Channel[]> {
  const grouped: Record<string, Channel[]> = {};
  for (const c of channels) {
    if (!c.guildId) continue;
    (grouped[c.guildId] ??= []).push(c);
  }
  for (const list of Object.values(grouped)) list.sort(byPosition);
  return grouped;
}

export function applyBootstrap(b: Bootstrap): void {
  const usersById: Record<string, User> = {};
  for (const u of b.users) usersById[u.id] = u;
  usersById[b.user.id] = b.user;

  // reconcile keeps referential stability across re-syncs (design doc §4.2)
  setDirectory(
    reconcile(
      {
        guilds: b.guilds,
        channelsByGuild: groupChannels(b.channels),
        dms: b.dms,
        usersById,
      },
      { key: "id" },
    ),
  );

  if (!selectedChannelId() && b.lastChannelId) {
    const all = b.channels.find((c) => c.id === b.lastChannelId);
    setSelectedGuildId(all?.guildId ?? null);
    setSelectedChannelId(b.lastChannelId);
  }
}

export function addGuild(guild: Guild, channels: Channel[]): void {
  setDirectory(
    produce((d) => {
      if (!d.guilds.some((g) => g.id === guild.id)) d.guilds.push(guild);
      d.channelsByGuild[guild.id] = [...channels].sort(byPosition);
    }),
  );
}

export function addDm(channel: Channel, users: User[]): void {
  setDirectory(
    produce((d) => {
      for (const u of users) d.usersById[u.id] = u;
      if (!d.dms.some((c) => c.id === channel.id)) d.dms.push(channel);
    }),
  );
}

export function selectGuild(guildId: string): void {
  setSelectedGuildId(guildId);
  const first = directory.channelsByGuild[guildId]?.[0];
  setSelectedChannelId(first ? first.id : null);
}

export function selectDmHome(): void {
  setSelectedGuildId(null);
  const first = directory.dms[0];
  setSelectedChannelId(first ? first.id : null);
}

export function selectChannel(channelId: string): void {
  setSelectedChannelId(channelId);
}

export function selectDm(channelId: string): void {
  setSelectedGuildId(null);
  setSelectedChannelId(channelId);
}

export function userById(id: string): User | undefined {
  return directory.usersById[id];
}

export function displayName(id: string): string {
  const u = directory.usersById[id];
  return u ? u.displayName || u.username : "unknown";
}

export function selectedChannel(): Channel | null {
  const id = selectedChannelId();
  if (!id) return null;
  const gid = selectedGuildId();
  if (gid) return directory.channelsByGuild[gid]?.find((c) => c.id === id) ?? null;
  return directory.dms.find((c) => c.id === id) ?? null;
}

export function selectedGuild(): Guild | null {
  const gid = selectedGuildId();
  if (!gid) return null;
  return directory.guilds.find((g) => g.id === gid) ?? null;
}

/** The other participant of a DM channel (1:1 in M1). */
export function dmPartnerId(channel: Channel, selfId: string | undefined): string | null {
  return channel.recipientIds.find((id) => id !== selfId) ?? null;
}

export function resetDirectory(): void {
  setDirectory(reconcile({ guilds: [], channelsByGuild: {}, dms: [], usersById: {} }));
  setSelectedGuildId(null);
  setSelectedChannelId(null);
}

export { directory, selectedGuildId, selectedChannelId };

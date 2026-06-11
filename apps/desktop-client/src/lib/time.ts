/** Intl-only timestamp helpers (no date library). */

/** Dice snowflake epoch: 2026-01-01T00:00:00Z (docs/protocol.md §11). */
export const DICE_EPOCH_MS = 1767225600000;

/** Derive the creation timestamp from a snowflake id string (BigInt-safe). */
export function snowflakeToMs(id: string): number {
  try {
    return Number(BigInt(id) >> 22n) + DICE_EPOCH_MS;
  } catch {
    return 0;
  }
}

const timeFmt = new Intl.DateTimeFormat(undefined, {
  hour: "2-digit",
  minute: "2-digit",
});

const dayFmt = new Intl.DateTimeFormat(undefined, {
  weekday: "long",
  year: "numeric",
  month: "long",
  day: "numeric",
});

/** "10:42" style clock time. */
export function formatTime(ms: number): string {
  return timeFmt.format(new Date(ms));
}

function dayStart(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** "Today" / "Yesterday" / full localized date — for day dividers. */
export function dayLabel(ms: number): string {
  const today = dayStart(Date.now());
  const that = dayStart(ms);
  const diff = Math.round((today - that) / 86400000);
  if (diff === 0) return "Today";
  if (diff === 1) return "Yesterday";
  return dayFmt.format(new Date(ms));
}

/** True when two timestamps fall on different local days (divider needed). */
export function crossesDay(aMs: number, bMs: number): boolean {
  return dayStart(aMs) !== dayStart(bMs);
}

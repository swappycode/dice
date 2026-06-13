/**
 * A short synthesized notification chime — a rising two-note "blip" via the Web
 * Audio API, so there's no audio asset to bundle (stays within the no-assets,
 * retro spirit). Throttled so a burst of messages can't machine-gun the speaker;
 * best-effort (browser autoplay rules may suppress it until the first gesture,
 * which by login has always happened).
 */

const MIN_GAP_MS = 1500;
let ctx: AudioContext | null = null;
let lastChimeMs = 0;

/** A pleasant two-note blip (A5 → D6, triangle wave, ~0.2 s). */
export function playChime(): void {
  const now = Date.now();
  if (now - lastChimeMs < MIN_GAP_MS) return;
  lastChimeMs = now;

  try {
    ctx ??= new AudioContext();
    if (ctx.state === "suspended") void ctx.resume();
    const t0 = ctx.currentTime;
    for (const [freq, offset] of [
      [880, 0],
      [1174.66, 0.08],
    ] as const) {
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      const start = t0 + offset;
      osc.type = "triangle";
      osc.frequency.value = freq;
      // Quick attack, gentle exponential release (avoids a click).
      gain.gain.setValueAtTime(0.0001, start);
      gain.gain.exponentialRampToValueAtTime(0.11, start + 0.012);
      gain.gain.exponentialRampToValueAtTime(0.0001, start + 0.18);
      osc.connect(gain).connect(ctx.destination);
      osc.start(start);
      osc.stop(start + 0.2);
    }
  } catch {
    /* no Web Audio (or blocked); a missed chime is non-fatal */
  }
}

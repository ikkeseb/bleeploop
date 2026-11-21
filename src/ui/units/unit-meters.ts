/**
 * The units' meters (unit contract §2, the feed's `unitMeters`) as a plain mirror: the engine store writes
 * each frame's meters here (`applyUnitMeters`) and the faces' draw loops (`ott-loop.ts`, `echo-loop.ts`)
 * read them. No Solid: a frame of meters, up to 60 a second, writes no signal, and the loop reading it
 * subscribes to nothing (invariant 6). Per unit a ring of the last `HISTORY` frames of each band's gain
 * range, so the OTT's face draws the last three seconds, and a ring of the last `OUT_HISTORY` frames of its
 * output peak per channel, each stamped with when it arrived, so the ECHO's tape lays them on time;
 * `version` moves on every frame, so a loop draws only what changed.
 */

/** Frames of history kept per unit: three seconds of the engine's 60 frames a second. */
export const HISTORY = 180;
/** The level floor a band's follower reads at silence (dBFS). */
export const LEVEL_FLOOR = -120;
/** Frames of output peaks kept per unit: the ECHO's two bars at the slowest tempo (40 BPM in 5/4, 15 s)
 * at the feed's 60 frames a second, with room for a faster feed. */
export const OUT_HISTORY = 2048;
/** A frame's span when it is the first (no frame before it to start from): one feed frame (ms). */
const FRAME_MS = 1000 / 60;

/** One unit's meters, as the mirror keeps them. */
export interface UnitMeterView {
  /** Moves on every frame that carried this unit. */
  version: number;
  /** Frames written in all (the ring's newest is `(count - 1) % HISTORY`). */
  count: number;
  /** Each band's (low, mid, high) lowest and highest gain (dB) per frame, a ring of `HISTORY`. */
  gainMin: [Float32Array, Float32Array, Float32Array];
  gainMax: [Float32Array, Float32Array, Float32Array];
  /** Each band's follower level (dBFS) per frame, a ring of `HISTORY`. */
  levelPeak: [Float32Array, Float32Array, Float32Array];
  /** The newest frame's gain (dB) and level (dBFS) per band. */
  gain: [number, number, number];
  level: [number, number, number];
  /** Whether the newest frame carried bands (an OTT's). */
  bands: boolean;
  /** Frames of output written in all (the ring's newest is `(outCount - 1) % OUT_HISTORY`). */
  outCount: number;
  /** Each frame's output peak per channel (linear, the feed's `out`), a ring of `OUT_HISTORY`. */
  outL: Float32Array;
  outR: Float32Array;
  /** When each frame arrived (`performance.now()`, ms): it covers the time since the frame before it. */
  outAt: Float64Array;
  /** The `version` of the last frame whose output was not silent (0: none yet): a draw loop that skips a
   * silent glass sees a loud frame that came and went between two of its draws. */
  loud: number;
}

const ring = (): [Float32Array, Float32Array, Float32Array] => [new Float32Array(HISTORY), new Float32Array(HISTORY), new Float32Array(HISTORY)];

/** The mirror, by unit id. Written only here; read by the draw loop. */
export const unitMeters = new Map<number, UnitMeterView>();

interface WireBand {
  gain: number;
  gainMin: number;
  gainMax: number;
  level: number;
  levelPeak: number;
}

/** One feed frame's meters into the mirror (the engine store's frame path). A frame's meters are complete
 * (unit contract §2: one per seated unit, every frame), so a unit the frame omits no longer sounds (parked,
 * withdrawn, held out): its readings and history clear, never freeze on its last frame. */
export function applyUnitMeters(meters: readonly { unit: number; out: readonly [number, number]; bands?: readonly WireBand[] }[]): void {
  const now = performance.now();
  for (const [id, v] of unitMeters) {
    if (v.count === 0 && v.outCount === 0 && !v.bands) continue;
    if (meters.some((m) => m.unit === id)) continue;
    v.count = 0;
    v.outCount = 0;
    v.bands = false;
    v.gain.fill(0);
    v.level.fill(LEVEL_FLOOR);
    v.version++;
  }
  for (const m of meters) {
    let v = unitMeters.get(m.unit);
    if (!v) {
      v = {
        version: 0,
        count: 0,
        gainMin: ring(),
        gainMax: ring(),
        levelPeak: ring(),
        gain: [0, 0, 0],
        level: [LEVEL_FLOOR, LEVEL_FLOOR, LEVEL_FLOOR],
        bands: false,
        outCount: 0,
        outL: new Float32Array(OUT_HISTORY),
        outR: new Float32Array(OUT_HISTORY),
        outAt: new Float64Array(OUT_HISTORY),
        loud: 0,
      };
      unitMeters.set(m.unit, v);
    }
    // the output: stamped with its arrival, never before the frame ahead of it (a burst lands in order)
    const at = v.outCount % OUT_HISTORY;
    const prev = v.outCount > 0 ? v.outAt[(v.outCount - 1) % OUT_HISTORY] : now - FRAME_MS;
    v.outL[at] = m.out[0];
    v.outR[at] = m.out[1];
    v.outAt[at] = Math.max(now, prev);
    v.outCount++;
    v.bands = m.bands !== undefined;
    if (m.bands) {
      const h = v.count % HISTORY;
      for (let b = 0; b < 3; b++) {
        const band = m.bands[b];
        // A range the frame did not fold (no sample since the last) reads as its newest gain.
        const lo = Number.isFinite(band.gainMin) ? band.gainMin : band.gain;
        const hi = Number.isFinite(band.gainMax) ? band.gainMax : band.gain;
        v.gainMin[b][h] = Math.min(lo, hi);
        v.gainMax[b][h] = Math.max(lo, hi);
        v.levelPeak[b][h] = band.levelPeak;
        v.gain[b] = band.gain;
        v.level[b] = band.level;
      }
      v.count++;
    }
    v.version++;
    if (m.out[0] > 0 || m.out[1] > 0) v.loud = v.version;
  }
}

/** Forget every unit's meters (a reset frame: a new engine's followers start again). */
export function dropUnitMeters(): void {
  unitMeters.clear();
}

/** Forget the meters of every unit not in `kept` (a chain report: a removed unit's ring goes with it). */
export function keepUnitMeters(kept: ReadonlySet<number>): void {
  for (const id of unitMeters.keys()) if (!kept.has(id)) unitMeters.delete(id);
}

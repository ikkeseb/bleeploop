/**
 * The units' meters (unit contract §2, the feed's `unitMeters`) as a plain mirror: the engine store writes
 * each frame's meters here (`applyUnitMeters`) and the OTT's draw loop (`ott-loop.ts`) reads them. No
 * Solid: a frame of meters, up to 60 a second, writes no signal, and the loop reading it subscribes to
 * nothing (invariant 6). Per unit a ring of the last `HISTORY` frames of each band's gain range, so the
 * face draws the last three seconds; `version` moves on every frame, so the loop draws only what changed.
 */

/** Frames of history kept per unit: three seconds of the engine's 60 frames a second. */
export const HISTORY = 180;
/** The level floor a band's follower reads at silence (dBFS). */
export const LEVEL_FLOOR = -120;

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

/** One feed frame's meters into the mirror (the engine store's frame path). */
export function applyUnitMeters(meters: readonly { unit: number; bands?: readonly WireBand[] }[]): void {
  for (const m of meters) {
    let v = unitMeters.get(m.unit);
    if (!v) {
      v = { version: 0, count: 0, gainMin: ring(), gainMax: ring(), levelPeak: ring(), gain: [0, 0, 0], level: [LEVEL_FLOOR, LEVEL_FLOOR, LEVEL_FLOOR], bands: false };
      unitMeters.set(m.unit, v);
    }
    v.bands = m.bands !== undefined;
    if (m.bands) {
      const at = v.count % HISTORY;
      for (let b = 0; b < 3; b++) {
        const band = m.bands[b];
        // A range the frame did not fold (no sample since the last) reads as its newest gain.
        const lo = Number.isFinite(band.gainMin) ? band.gainMin : band.gain;
        const hi = Number.isFinite(band.gainMax) ? band.gainMax : band.gain;
        v.gainMin[b][at] = Math.min(lo, hi);
        v.gainMax[b][at] = Math.max(lo, hi);
        v.levelPeak[b][at] = band.levelPeak;
        v.gain[b] = band.gain;
        v.level[b] = band.level;
      }
      v.count++;
    }
    v.version++;
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

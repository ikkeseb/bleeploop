/** DEV measurement arithmetic. Times are milliseconds; no assumed shared clock epoch. */
export const MARKER_CHIPS = '111111000001000011000101001111010001110010010110111011001101010';
export const MARKER_HZ = 6000;

export function markerReference(rate: number): Float32Array {
  return Float32Array.from({ length: Math.ceil(MARKER_CHIPS.length * rate / MARKER_HZ) },
    (_, i) => MARKER_CHIPS[Math.floor(i * MARKER_HZ / rate)] === '1' ? 1 : -1);
}

/** Normalized correlation tolerates gain and cubic-resampler interpolation. Search coarse, refine
 * every candidate locally, then exclude the whole burst before looking for another marker. */
export function findMarkers(samples: ArrayLike<number>, rate: number): { frame: number; score: number }[] {
  const ref = markerReference(rate);
  const found: { frame: number; score: number }[] = [];
  const score = (start: number) => {
    let dot = 0, energy = 0;
    for (let k = 0; k < ref.length; k++) {
      const v = samples[start + k];
      dot += v * ref[k]; energy += v * v;
    }
    return energy > 1e-12 ? dot / Math.sqrt(energy * ref.length) : 0;
  };
  for (let i = 0; i + ref.length < samples.length; i += 4) {
    if (score(i) < 0.72) continue;
    let best = { frame: i, score: -1 };
    for (let j = Math.max(0, i - 4); j <= i + 12 && j + ref.length < samples.length; j++) {
      const value = score(j);
      if (value > best.score) best = { frame: j, score: value };
    }
    if (best.score < 0.85) continue;
    found.push(best);
    i = best.frame + Math.floor(rate * 0.25);
  }
  return found;
}

/** A server instant observed during a round trip bounds native-minus-browser clock offset.
 * Intersect exchanges; an empty intersection detects a clock discontinuity or invalid samples. */
export function clockOffset(pings: { before: number; native: number; after: number }[]) {
  let low = -Infinity, high = Infinity;
  for (const p of pings) {
    if (![p.before, p.native, p.after].every(Number.isFinite) || p.after < p.before) throw Error('Invalid clock exchange');
    low = Math.max(low, p.native - p.after);
    high = Math.min(high, p.native - p.before);
  }
  if (!Number.isFinite(low) || !Number.isFinite(high) || low > high) throw Error('Inconsistent clock exchanges');
  return { low, high, midpoint: (low + high) / 2, uncertainty: (high - low) / 2 };
}

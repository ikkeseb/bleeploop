// Pure fixtures for the DEV native/web marker probe. No browser or driver required.
import assert from 'node:assert/strict';
import { markerReference, findMarkers, clockOffset } from '../src/debug/marker-math.ts';

for (const rate of [44100, 48000, 96000]) {
  const ref = markerReference(rate);
  const samples = new Float32Array(rate * 2);
  const offsets = [137, Math.floor(rate * 0.51) + 3, Math.floor(rate * 1.1) + 1];
  for (const offset of offsets) for (let i = 0; i < ref.length; i++) samples[offset + i] = ref[i] * 0.037;
  assert.deepEqual(findMarkers(samples, rate).map(m => m.frame), offsets);
  assert.equal(findMarkers(new Float32Array(rate), rate).length, 0);
  // Fractional sample interpolation survives both transport resamplers.
  const interpolated = samples.map((v, i) => v * 0.7 + (samples[i - 1] ?? 0) * 0.3);
  assert.deepEqual(findMarkers(interpolated, rate).map(m => m.frame), offsets);
}
const pings = [{ before: 100, native: 112, after: 105 }, { before: 200, native: 211, after: 202 }];
assert.deepEqual(clockOffset(pings), { low: 9, high: 11, midpoint: 10, uncertainty: 1 });
assert.throws(() => clockOffset([...pings, { before: 300, native: 500, after: 301 }]));
assert.throws(() => clockOffset([]));
assert.throws(() => clockOffset([{ before: 3, native: 4, after: 2 }]));
console.log('marker probe: exact/fractional markers at 44.1/48/96 kHz, silence and bounded/discontinuous clocks PASS');

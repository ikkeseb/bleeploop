// marker.mjs — deterministic guard for src/debug/marker-math.ts, the DEV native/web marker probe's
// correlation and clock arithmetic. It runs no native audio and no browser.
// Run: node verify/guards/marker.mjs

import assert from 'node:assert/strict';
import { markerReference, findMarkers, clockOffset } from '../../src/debug/marker-math.ts';

let passed = 0;
let failed = 0;

function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (e) {
    failed++;
    console.error(`FAIL: ${name}\n  ${e.message}`);
  }
}

for (const rate of [44100, 48000, 96000]) {
  const ref = markerReference(rate);
  const samples = new Float32Array(rate * 2);
  const offsets = [137, Math.floor(rate * 0.51) + 3, Math.floor(rate * 1.1) + 1];
  for (const offset of offsets) for (let i = 0; i < ref.length; i++) samples[offset + i] = ref[i] * 0.037;
  check(`${rate} Hz: exact markers found at their frames`, () =>
    assert.deepEqual(findMarkers(samples, rate).map((m) => m.frame), offsets));
  check(`${rate} Hz: silence has no markers`, () => assert.equal(findMarkers(new Float32Array(rate), rate).length, 0));
  // Fractional sample interpolation survives both transport resamplers.
  const interpolated = samples.map((v, i) => v * 0.7 + (samples[i - 1] ?? 0) * 0.3);
  check(`${rate} Hz: fractional markers round to their frames`, () =>
    assert.deepEqual(findMarkers(interpolated, rate).map((m) => m.frame), offsets));
}

const pings = [{ before: 100, native: 112, after: 105 }, { before: 200, native: 211, after: 202 }];
check('clock offset is the intersection of the ping bounds', () =>
  assert.deepEqual(clockOffset(pings), { low: 9, high: 11, midpoint: 10, uncertainty: 1 }));
check('a discontinuous clock throws', () =>
  assert.throws(() => clockOffset([...pings, { before: 300, native: 500, after: 301 }])));
check('no pings throws', () => assert.throws(() => clockOffset([])));
check('a ping that ends before it starts throws', () =>
  assert.throws(() => clockOffset([{ before: 3, native: 4, after: 2 }])));

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

// Production timestamp arithmetic and fallback, independent of browser/driver clock accuracy.
import assert from 'node:assert/strict';
import { renderCursorTailSeconds as tail, medianFinite, computeC } from '../src/audio/record-latency-math.ts';
let passed = 0, failed = 0;
function check(name, run) {
  try { run(); passed++; } catch (error) { failed++; console.error(`FAIL ${name}: ${error.message}`); }
}
check('queue consumption and cursor advance cancel at the callback boundary', () => {
  for (const sr of [44100, 48000, 96000]) {
    const before = tail(1, 0.92, 1000, 1000, sr * 0.01, sr);
    const after = tail(1.01, 0.92, 1000, 1000, 0, sr);
    const later = tail(1.01, 0.92, 1000, 1010, sr * 0.01, sr);
    for (const sample of [before, after, later]) assert.ok(Math.abs(sample - 0.09) < 1e-12);
  }
});
check('missing, stale, future and impossible clock observations are rejected', () => {
  for (const args of [
    [1, 0, 1000, 1000, 0, 48000], [1, 0.9, 0, 1000, 0, 48000],
    [1, 0.9, 1000, 1000, -1, 48000], [1, 0.9, 1000, 1000, 0, 0],
    [1, 0.9, 1002, 1000, 0, 48000], [1, 0.9, 700, 1000, 0, 48000],
    [1, 1.1, 1000, 1000, 0, 48000], [2, 0.1, 1000, 1000, 0, 48000],
    [1, 0.99, 980, 1000, 0, 48000], [NaN, 0.9, 1000, 1000, 0, 48000],
  ]) assert.ok(Number.isNaN(tail(...args)));
});
check('window needs three valid paired samples and expires invalidated entries', () => {
  const buf = new Float64Array([0.09, NaN, 0.09, Infinity, 0.09]);
  const scratch = new Float64Array(5);
  assert.ok(Number.isNaN(medianFinite(buf, 4, scratch)));
  assert.equal(medianFinite(buf, 5, scratch), 0.09);
  buf.fill(NaN);
  assert.ok(Number.isNaN(medianFinite(buf, 5, scratch)));
});
check('timestamp mode has no extra base latency, quantum or floor', () => {
  const terms = { renderCursorSeconds: 0.09, hop1Frames: 128, hop2Frames: 1000,
    cpalOutSeconds: 0.02, baseLatency: 0.01, outputLatency: 0.08,
    outputGraphLatencySeconds: 0.006, trimMs: 0, floorEnabled: true };
  const result = computeC(terms, 48000);
  assert.equal(result.source, 'timestamp');
  assert.equal(result.frames, 3648);
  assert.equal(computeC({ ...terms, baseLatency: 0.9, outputLatency: 0.9 }, 48000).frames, result.frames);
  const clamped = computeC({ ...terms, renderCursorSeconds: 0.01 }, 48000);
  assert.equal(clamped.frames, 0);
  assert.equal(clamped.outputFloored, false);
  assert.equal(computeC({ ...terms, trimMs: 10 }, 48000).frames - result.frames, 480);
  for (const missing of [undefined, null, NaN, Infinity, -1]) {
    const fallback = computeC({ ...terms, renderCursorSeconds: missing }, 48000);
    assert.equal(fallback.source, 'reported');
    assert.ok(fallback.frames > result.frames);
  }
});
console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exitCode = failed ? 1 : 0;

// fs-quantize-verify.mjs — deterministic guard for src/audio/quantize.ts timing helpers.
//
// Imports the REAL source (Node TS type-stripping), not a port, so it cannot drift.
// Run: node fs-quantize-verify.mjs
//
// Sanity checks for secondsPerBeat/framesPerBar (used by the looper's master-loop frame
// math). quantizeTimeToNextBoundary was removed as dead code (no callers) along with its
// dedicated boundary/property-sweep tests that used to live here.

import assert from 'node:assert';
import {
  secondsPerBeat,
  framesPerBar,
  maxWholeBars,
  clampBars,
  averageInterval,
} from '../src/audio/quantize.ts';

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

check('secondsPerBeat 120bpm = 0.5', () => assert.strictEqual(secondsPerBeat(120), 0.5));
check('secondsPerBeat 60bpm = 1', () => assert.strictEqual(secondsPerBeat(60), 1));
check('framesPerBar 120bpm/48k/4 = 96000', () => assert.strictEqual(framesPerBar(120, 48000, 4), 96000));
check('framesPerBar 120bpm/44.1k/4 = 88200', () => assert.strictEqual(framesPerBar(120, 44100, 4), 88200));
check('framesPerBar rounds to integer frames', () =>
  assert.ok(Number.isInteger(framesPerBar(123.4, 44100, 4))));

// maxWholeBars/clampBars — the buffer-fit bound machine.ts commits/arms against. The invariant:
// clampBars(anything, maxWholeBars(buf, fpb)) * fpb <= buf for every buf >= fpb, so a committed
// master length can never exceed the record buffer (a later track's consume() would RangeError).
check('maxWholeBars: exact fit', () => assert.strictEqual(maxWholeBars(96000 * 4, 96000), 4));
check('maxWholeBars: floors a partial trailing bar', () => assert.strictEqual(maxWholeBars(96000 * 4 + 1, 96000), 4));
check('maxWholeBars: never below 1 (buffer smaller than one bar)', () => assert.strictEqual(maxWholeBars(100, 96000), 1));
check('clampBars: clamps up to 1', () => assert.strictEqual(clampBars(0, 8), 1));
check('clampBars: passes an in-range count through', () => assert.strictEqual(clampBars(3, 8), 3));
check('clampBars: clamps down to maxBars', () => assert.strictEqual(clampBars(99, 8), 8));
check('buffer-fit invariant: clamped bars * fpb <= buffer (60 s at 120bpm/48k)', () => {
  const fpb = framesPerBar(120, 48000); // 96000
  const buf = 60 * 48000; // the 60 s record buffer
  for (const req of [1, 7, 30, 31, 999]) {
    const bars = clampBars(req, maxWholeBars(buf, fpb));
    assert.ok(bars * fpb <= buf, `bars=${bars} fpb=${fpb} overflows buf=${buf} (req=${req})`);
    assert.ok(bars >= 1);
  }
});

// averageInterval — the shared core of tap tempo + MIDI-clock smoothing (clock.ts converts to BPM).
check('averageInterval: uniform 500 ms taps -> 500 (= 120 bpm via 60000/avg)', () => {
  assert.strictEqual(averageInterval([0, 500, 1000, 1500]), 500);
  assert.strictEqual(60000 / averageInterval([0, 500, 1000, 1500]), 120);
});
check('averageInterval: mean of uneven intervals', () =>
  assert.strictEqual(averageInterval([0, 400, 1000]), 500)); // 400 + 600 -> 500
check('averageInterval: 24-ppqn tick stream at 120 bpm -> 120 bpm', () => {
  const tickMs = 60000 / (120 * 24); // 20.833… ms between MIDI clock ticks
  const ticks = Array.from({ length: 25 }, (_, i) => i * tickMs);
  const bpm = 60000 / (averageInterval(ticks) * 24);
  assert.ok(Math.abs(bpm - 120) < 1e-9, `bpm=${bpm}`);
});

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

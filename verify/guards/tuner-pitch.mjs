// verify/guards/tuner-pitch.mjs — the tuner's pitch arithmetic, `src/ui/tuner/pitch.ts`: a frequency to
// its note, octave and cents against A4 = 440 Hz (12-TET), the in-tune hysteresis and the cents as the
// tuner prints them.
//
// Imports the REAL source (Node TS type-stripping), not a port, so it cannot drift. It cannot see the
// draw loop that calls it (`tuner-loop.ts`: staleness, the no-signal second, the follower) or the
// overlay; `verify/probes/tuner.mjs` drives those. Run: node verify/guards/tuner-pitch.mjs

import assert from 'node:assert/strict';
import { A4_HZ, IN_TUNE_ENTER, IN_TUNE_LEAVE, centsText, inTuneStep, noteOf, spell } from '../../src/ui/tuner/pitch.ts';

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

/** `hz` as the tuner names it: "E2", "A#4"; and its cents to a tenth. */
const named = (hz) => {
  const n = noteOf(hz);
  return n && `${n.name}${n.octave}`;
};
const cents = (hz) => Math.round(noteOf(hz).cents * 10) / 10;
/** The frequency `c` cents from `hz`. */
const off = (hz, c) => hz * 2 ** (c / 1200);

// ── The guitar's open strings and the range ends, each within a tenth of a cent of its note ───────────
for (const [hz, name] of [
  [82.407, 'E2'],
  [110, 'A2'],
  [146.832, 'D3'],
  [195.998, 'G3'],
  [246.942, 'B3'],
  [329.628, 'E4'],
  [440, 'A4'],
  [30.868, 'B0'],
  [1975.533, 'B6'],
  [466.164, 'A#4'],
]) {
  check(`${hz} Hz is ${name} at 0 cents`, () => {
    assert.equal(named(hz), name);
    assert.ok(Math.abs(noteOf(hz).cents) < 0.1, `${noteOf(hz).cents} cents`);
  });
}
check('A4 is exactly 440 Hz, MIDI 69, 0 cents', () => {
  assert.equal(A4_HZ, 440);
  assert.deepEqual(noteOf(440), { name: 'A', sharp: false, octave: 4, cents: 0, midi: 69 });
});
check('a sharp note carries its sharp, a natural none', () => {
  assert.equal(noteOf(466.164).sharp, true);
  assert.equal(noteOf(466.164).name, 'A#');
  assert.equal(noteOf(82.407).sharp, false);
});
check('the octave turns over at C: B3 then C4 (MIDI 60)', () => {
  assert.equal(named(246.942), 'B3');
  assert.deepEqual([named(261.626), noteOf(261.626).midi], ['C4', 60]);
});

// ── Off pitch: flat is below 0, sharp above ─────────────────────────────────────────────────────────
check('20 cents flat of E2 reads E2 -20', () => {
  assert.equal(named(off(82.407, -20)), 'E2');
  assert.equal(cents(off(82.407, -20)), -20);
});
check('20 cents sharp of E2 reads E2 +20', () => {
  assert.equal(named(off(82.407, 20)), 'E2');
  assert.equal(cents(off(82.407, 20)), 20);
});
check('18 cents flat of E2 and 4 sharp of A2 (the probe states)', () => {
  assert.deepEqual([named(off(82.407, -18)), cents(off(82.407, -18))], ['E2', -18]);
  assert.deepEqual([named(off(110, 4)), cents(off(110, 4))], ['A2', 4]);
});

// ── The boundary between two notes: halfway names the upper one, at -50, and does so every time ──────
const E2 = 440 * 2 ** ((40 - 69) / 12); // MIDI 40
check('exactly halfway between E2 and F2 names F2 at -50 cents', () => {
  const n = noteOf(off(E2, 50));
  assert.deepEqual([n.name, n.octave, n.cents], ['F', 2, -50]);
});
check('the boundary is stable: a float a few ulps either side still names F2 at -50', () => {
  const half = off(E2, 50);
  for (const hz of [half * (1 - 4e-16), half, half * (1 + 4e-16), half * (1 + 1e-12), half * (1 - 1e-12)]) {
    const n = noteOf(hz);
    assert.deepEqual([n.name, n.cents], ['F', -50], `${hz} Hz`);
  }
});
check('just under halfway is still E2 (+49.9), just over F2 (-49.9)', () => {
  assert.deepEqual([named(off(E2, 49.9)), cents(off(E2, 49.9))], ['E2', 49.9]);
  assert.deepEqual([named(off(E2, 50.1)), cents(off(E2, 50.1))], ['F2', -49.9]);
});
check('below A4 the boundary falls the same way: halfway G#4..A4 names A4 at -50', () => {
  const n = noteOf(off(440, -50));
  assert.deepEqual([n.name, n.octave, n.cents], ['A', 4, -50]);
});
check('cents always lie in -50 ..< +50 across the guitar range', () => {
  for (let hz = 30; hz < 2000; hz *= 1.0137) {
    const c = noteOf(hz).cents;
    assert.ok(c >= -50 && c < 50, `${hz} Hz: ${c}`);
  }
});

// ── No pitch ────────────────────────────────────────────────────────────────────────────────────────
for (const [what, hz] of [
  ['0', 0],
  ['a negative', -110],
  ['NaN', Number.NaN],
  ['Infinity', Number.POSITIVE_INFINITY],
  ['-Infinity', Number.NEGATIVE_INFINITY],
]) {
  check(`${what} is no pitch (null)`, () => assert.equal(noteOf(hz), null));
}

// ── The spelling from a MIDI number (the overlay's letter, sharp and octave) ─────────────────────────
check('spell names MIDI 40 E2, 70 A#4, 23 B0, 95 B6', () => {
  assert.deepEqual(spell(40), { name: 'E', sharp: false, octave: 2 });
  assert.deepEqual(spell(70), { name: 'A#', sharp: true, octave: 4 });
  assert.deepEqual(spell(23), { name: 'B', sharp: false, octave: 0 });
  assert.deepEqual(spell(95), { name: 'B', sharp: false, octave: 6 });
});

// ── The in-tune hysteresis: in at 2.5 cents, out past 3.5 ────────────────────────────────────────────
check('the thresholds are 2.5 in and 3.5 out', () => assert.deepEqual([IN_TUNE_ENTER, IN_TUNE_LEAVE], [2.5, 3.5]));
check('out of tune, a reading enters at 2.5 cents either side, not at 2.6', () => {
  assert.equal(inTuneStep(false, 2.5), true);
  assert.equal(inTuneStep(false, -2.5), true);
  assert.equal(inTuneStep(false, 2.6), false);
  assert.equal(inTuneStep(false, -3), false);
});
check('in tune, a reading stays in up to 3.5 cents and leaves past it', () => {
  assert.equal(inTuneStep(true, 3), true);
  assert.equal(inTuneStep(true, -3.5), true);
  assert.equal(inTuneStep(true, 3.6), false);
});
check('a reading wavering 2.4 / 3.2 / 2.9 / 3.4 cents stays in once it entered, never flickers', () => {
  let state = false;
  const seen = [];
  for (const c of [3.2, 2.4, 3.2, 2.9, 3.4, 3.2, 3.6, 3.0, 2.5]) seen.push((state = inTuneStep(state, c)));
  assert.deepEqual(seen, [false, true, true, true, true, true, false, false, true]);
});
check('no pitch is never in tune, whatever came before', () => {
  assert.equal(inTuneStep(true, null), false);
  assert.equal(inTuneStep(true, Number.NaN), false);
});

// ── The cents as printed ────────────────────────────────────────────────────────────────────────────
check('cents print whole, a true minus or a plus, 0 unsigned (never -0)', () => {
  assert.deepEqual([-18, 4, 0, -0.4, 0.4, 49.6, -49.5].map(centsText), ['−18', '+4', '0', '0', '0', '+50', '−49']);
});

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

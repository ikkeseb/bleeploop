// verify/guards/native-spike-rule.mjs — guard for scripts/native-spike-rule.mjs, the rule that turns one
// `pnpm native:spike` probe process into table rows.
//
// Imports the REAL module. Covers: a clean exit with verdicts passes through as those rows; a PASS
// followed by a crash, a signal or a timeout still adds a failing ERROR row; a clean exit with no
// verdict fails; an INVALID line fails; a Share run whose controls pass fails unless its final S1 line
// says PASS (an UNKNOWN or a missing one fails). Cannot see: the probe process, its pipes or the rig.
// Run: node verify/guards/native-spike-rule.mjs

import assert from 'node:assert';
import { spikeRunRows, spikeVerdict } from '../../scripts/native-spike-rule.mjs';

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

const PASS = { pass: true, id: 'A1.lag', rest: 'median 12.0f' };
const run = (over) => ({ verdicts: [PASS], invalid: null, exit: 0, signal: null, timedOutS: null, ...over });
const errors = (rows) => rows.filter((r) => r.id === 'ERROR' && !r.pass);

check('a clean exit with verdicts adds exactly those rows', () => {
  const rows = spikeRunRows(run({}), 'asio 64');
  assert.deepStrictEqual(rows.map((r) => [r.id, r.pass, r.where]), [['A1.lag', true, 'asio 64']]);
});

check('a PASS line, then a non-zero exit, fails', () => {
  const rows = spikeRunRows(run({ exit: 3 }), 'asio 64');
  assert.strictEqual(errors(rows).length, 1);
  assert.match(errors(rows)[0].value, /exit 3/);
});

check('a PASS line, then a signal, fails', () => {
  const rows = spikeRunRows(run({ exit: null, signal: 'SIGTERM' }), 'asio 64');
  assert.match(errors(rows)[0]?.value ?? '', /SIGTERM/);
});

check('a PASS line, then the timeout, fails even with exit 0', () => {
  const rows = spikeRunRows(run({ exit: 0, timedOutS: 180 }), 'asio 64');
  assert.match(errors(rows)[0]?.value ?? '', /180 s timeout/);
});

check('a clean exit that printed no verdict fails', () => {
  assert.strictEqual(errors(spikeRunRows(run({ verdicts: [] }), 'share')).length, 1);
});

check('an INVALID line fails', () => {
  const rows = spikeRunRows(run({ verdicts: [], invalid: 'INVALID 3 of 40 chirps below xcorr 0.8' }), 'asio 64');
  assert.deepStrictEqual(rows.map((r) => [r.id, r.pass]), [['INVALID', false]]);
});

// The Share run's lines as share_probe.rs prints them, read by the real line parser.
const lines = (...bodies) => bodies.map(spikeVerdict).filter(Boolean);
const controls = ['PASS S1.control.process 41.0dB >=30dB_over_floor', 'PASS S1.control.endpoint -20.0dBFS >=-60dBFS'];
const s1 = (word) => `${word} S1 (mute|vol0)&zeros&dual`;
const shareFails = (bodies) => spikeRunRows(run({ verdicts: lines(...bodies) }), 'share', 'S1').some((r) => !r.pass);

check('a Share run with passing controls and a PASS S1 passes', () => {
  assert.strictEqual(shareFails([...controls, s1('PASS')]), false);
});

check('a Share run with passing controls and an UNKNOWN S1 fails', () => {
  assert.strictEqual(shareFails([...controls, 'UNKNOWN S1.zeros.process unknown <=-80dBFS', s1('UNKNOWN')]), true);
});

check('a Share run with passing controls and no S1 line fails', () => {
  assert.strictEqual(shareFails(controls), true);
});

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

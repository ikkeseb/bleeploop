// verify/guards/native-swap-verdict.mjs — the verdict patterns `pnpm native:swap` judges a run by.
//
// scripts/native-probe.mjs launches the whole native app on import, so this guard cannot import it: it
// reads the `swap-stress` entry's `pass` and `failLine` regex literals out of the runner's own source
// (no copy) and runs them over the summary lines src/debug/swap-stress.ts prints:
// `complete: <swapped> swapped, <failed> failed, <n> with an editor open`.
// Run: node verify/guards/native-swap-verdict.mjs

import assert from 'node:assert';
import { readFileSync } from 'node:fs';

const runner = readFileSync(new URL('../../scripts/native-probe.mjs', import.meta.url), 'utf8');
const entry = runner.match(/'swap-stress': \{([\s\S]*?)\n  \},/)?.[1];
assert.ok(entry, "no 'swap-stress' entry in scripts/native-probe.mjs");
const literal = (key) => {
  const src = entry.match(new RegExp(`^\\s*${key}: (/.*/),$`, 'm'))?.[1];
  assert.ok(src, `no ${key} regex literal in the swap-stress entry`);
  return new Function(`return ${src}`)();
};
const pass = literal('pass');
const failLine = literal('failLine');

let passed = 0;
let failed = 0;
function check(name, fn) {
  try {
    fn();
    passed++;
    console.log(`  ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`  FAIL ${name}: ${e.message}`);
  }
}

check('a clean run passes', () => assert.ok(pass.test('complete: 24 swapped, 0 failed, 12 with an editor open')));
check('a failed scenario (a failed editor open counts as one) fails', () =>
  assert.ok(!pass.test('complete: 24 swapped, 1 failed, 11 with an editor open')));
check('an editor-open round with no editor open fails', () =>
  assert.ok(!pass.test('complete: 12 swapped, 0 failed, 0 with an editor open')));
check('the old summary, with no editor-open count, fails', () => assert.ok(!pass.test('complete: 24 swapped, 0 failed')));
check('a count that merely starts with 0 does not pass as zero', () =>
  assert.ok(pass.test('complete: 4 swapped, 0 failed, 10 with an editor open')));
check('the open-failed log line still trips failLine', () =>
  assert.ok(failLine.test('open failed (continuing without editor) Pro-Q [clap]: Error: x')));

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

// verify/guards/engine-test-skip.mjs — guard for the skip rule in scripts/engine-test.mjs: `pnpm
// test:engine` skips only where no cargo executable exists, and fails on a cargo that is there but
// does not start.
//
// Imports the REAL rule and feeds it REAL spawn outcomes: a command that does not exist (ENOENT, the
// one skip), a process that exits non-zero (a broken toolchain), one killed by a signal, and one that
// starts (run). Cannot see: cargo itself, or the test run after the probe.
// Run: node verify/guards/engine-test-skip.mjs

import assert from 'node:assert';
import { spawnSync } from 'node:child_process';
import { cargoPresence } from '../../scripts/engine-test.mjs';

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

const node = (code) => spawnSync(process.execPath, ['-e', code], { encoding: 'utf8' });

check('a missing executable (ENOENT) skips', () => {
  const p = cargoPresence(spawnSync('bleeploop-no-such-cargo-binary', ['--version'], { encoding: 'utf8' }));
  assert.strictEqual(p.verdict, 'skip', p.why);
});

check('an executable that exits non-zero fails, with its stderr', () => {
  const p = cargoPresence(node("process.stderr.write('error: toolchain not installed'); process.exit(1)"));
  assert.strictEqual(p.verdict, 'fail');
  assert.match(p.why, /toolchain not installed/);
});

check('a spawn error other than ENOENT fails', () => {
  const p = cargoPresence(spawnSync(process.execPath, ['-e', 'setTimeout(() => {}, 5000)'], { encoding: 'utf8', timeout: 50 }));
  assert.strictEqual(p.verdict, 'fail', p.why);
});

check('a process killed by a signal fails', () => {
  const p = cargoPresence({ pid: 1, output: [], stdout: '', stderr: '', status: null, signal: 'SIGKILL' });
  assert.strictEqual(p.verdict, 'fail');
  assert.match(p.why, /SIGKILL/);
});

check('a cargo that starts runs the tests', () => {
  const p = cargoPresence(node("console.log('cargo 1.90.0')"));
  assert.strictEqual(p.verdict, 'run', p.why);
});

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

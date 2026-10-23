// verify/guards/run-probes.mjs — a passed RESULT cannot hide a timeout during probe shutdown.
// Tiny Node probes exercise real process exit, SIGTERM and forced shutdown without Vite or a browser.
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { runProbe } from '../harness/run-probe.mjs';

let passed = 0;
let failed = 0;
async function check(name, fn) {
  try {
    await fn();
    passed++;
  } catch (e) {
    failed++;
    console.error(`FAIL: ${name}\n  ${e.message}`);
  }
}

const dir = mkdtempSync(join(tmpdir(), 'bleeploop-probe-'));
async function fake(name, source) {
  const file = join(dir, `${name}.mjs`);
  writeFileSync(file, source);
  return runProbe(file, ['--fixture'], { timeoutMs: 700, killDelayMs: 100 });
}
// A broken escalation must fail promptly too; fixtures clean themselves up as a last resort.
const hanging = `setInterval(() => {}, 1000); setTimeout(() => process.exit(9), 2500);`;
const success = `console.log('=== RESULT: fake passed ===');`;
try {
  await check('passed probe exits normally', async () => {
    const r = await fake('normal', `if (!process.argv.includes('--fixture')) process.exit(2); ${success}`);
    assert.equal(r.ok, true);
    assert.equal(r.code, 0);
    assert.equal(r.timedOut, false);
  });
  await check('passed probe timing out stays failed even with exit 0', async () => {
    const r = await fake('term', `process.on('SIGTERM', () => process.exit(0)); ${success} ${hanging}`);
    assert.match(r.out, /RESULT: fake passed/);
    assert.match(r.out, /timed out/);
    assert.equal(r.ok, false);
    assert.equal(r.timedOut, true);
    if (process.platform !== 'win32') assert.equal(r.code, 0);
  });
  await check('probe ignoring SIGTERM is forcibly stopped', async () => {
    const r = await fake('ignore', `process.on('SIGTERM', () => {}); ${success} ${hanging}`);
    assert.match(r.out, /RESULT: fake passed/);
    assert.equal(r.ok, false);
    assert.equal(r.timedOut, true);
    assert.notEqual(r.code, 9, 'fixture self-exited before the runner killed it');
  });
} finally {
  rmSync(dir, { recursive: true, force: true });
}
console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

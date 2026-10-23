// verify/guards/cargo-test.mjs — cargo's runner must have a passing test, not just successful exits.
// Inject subprocess results so the guard needs neither Cargo nor compiled test binaries.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { cargoTest } from '../../scripts/cargo-test.mjs';

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

const result = (stdout = '', code = 0, stderr = '') => ({ stdout, code, stderr });
const build = result(JSON.stringify({
  reason: 'compiler-artifact', profile: { test: true }, executable: resolve('fake-test'),
  manifest_path: resolve('Cargo.toml'), target: { name: 'fake-test' },
}));
async function simulate(binary, docs = result(), compilation = build) {
  // An older runner lacks the injection seam; never fall through to real Cargo.
  assert.match(readFileSync(new URL('../../scripts/cargo-test.mjs', import.meta.url), 'utf8'), /run = runProcess/,
    'cargoTest must accept the subprocess injection');
  const steps = compilation.code === 0 ? [compilation, binary, docs] : [compilation];
  const answer = await cargoTest([], { jobs: 1, run: async () => {
    assert.ok(steps.length, 'unexpected subprocess');
    return steps.shift();
  } });
  assert.equal(steps.length, 0);
  return answer;
}

await check('zero passed tests fails with its own message', async () => {
  const r = await simulate(result('test result: ok. 0 passed; 0 failed; 3 ignored;'));
  assert.equal(r.code, 1);
  assert.equal(r.passed, 0);
  assert.equal(r.binaries, 1);
  assert.match(r.out, /no test passed/);
});
await check('three passed tests succeeds', async () => {
  const r = await simulate(result('test result: ok. 3 passed;'));
  assert.equal(r.code, 0);
  assert.equal(r.passed, 3);
  assert.equal(r.out, '');
});
await check('binary exit 7 still fails with its output', async () => {
  const r = await simulate(result('', 7, 'binary failure'));
  assert.equal(r.code, 1);
  assert.match(r.out, /binary failure/);
  assert.doesNotMatch(r.out, /no test passed/);
});
await check('build failure keeps its exit code and output', async () => {
  const r = await simulate(result(), result(), result('', 7, 'build failure'));
  assert.deepEqual(r, { code: 7, out: 'build failure', binaries: 0, passed: 0 });
});
await check('doc-test passes count too', async () => {
  const r = await simulate(result('test result: ok. 0 passed;'), result('test result: ok. 2 passed;'));
  assert.equal(r.code, 0);
  assert.equal(r.passed, 2);
});
await check('doc-test failure still fails with its output', async () => {
  const r = await simulate(result('test result: ok. 3 passed;'), result('', 1, 'doc failure'));
  assert.equal(r.code, 1);
  assert.match(r.out, /doc failure/);
});
console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

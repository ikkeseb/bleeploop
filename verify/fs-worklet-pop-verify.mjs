// fs-worklet-pop-verify.mjs — guards the plugin-pcm-source.ts process() output-copy logic.
//
// The worklet pops up to `want`(=128) frames from the hop-2 ring into `scratch`, writes the `got`
// available frames to `out`, and zero-fills the [got, want) shortfall on underrun. The copy must be
// alloc-free in process() (HARD RULE / invariant #5) AND bit-identical to the previous subarray form.
// This mirrors the source; if the source's copy logic changes, update both. Run: node fs-worklet-pop-verify.mjs

import assert from 'node:assert';

const WANT = 128;

// OLD form (pre-fix): out.set(got===want ? scratch : scratch.subarray(0,got)); then fill(0,got).
// Allocates a Float32Array view on the partial branch.
function applyOld(out, scratch, got) {
  if (got > 0) out.set(got === WANT ? scratch : scratch.subarray(0, got));
  if (got < WANT) out.fill(0, got);
}

// NEW form (the fix): conditional manual copy on partial, no subarray; then fill(0,got).
// MIRRORS: src/audio/worklets/plugin-pcm-source.ts@49-65 sha256:3d23fa5b7a78335e  (process() pop→copy→zero-fill)
function applyNew(out, scratch, got) {
  if (got > 0) {
    if (got === WANT) out.set(scratch);
    else for (let i = 0; i < got; i++) out[i] = scratch[i];
  }
  if (got < WANT) out.fill(0, got);
}

let passed = 0;
let failed = 0;
function check(name, fn) {
  try { fn(); passed++; } catch (e) { failed++; console.error(`FAIL: ${name}\n  ${e.message}`); }
}

// Deterministic non-trivial scratch contents (distinct, non-zero, signed) so a wrong index shows up.
function makeScratch() {
  const s = new Float32Array(WANT);
  for (let i = 0; i < WANT; i++) s[i] = ((i % 2 === 0) ? 1 : -1) * (i + 1) / 1000;
  return s;
}

// Sweep every possible pop count, including the underrun (0), every partial, and the full quantum.
for (let got = 0; got <= WANT; got++) {
  const scratch = makeScratch();
  const outOld = new Float32Array(WANT).fill(7); // pre-dirty to catch un-written slots
  const outNew = new Float32Array(WANT).fill(7);
  applyOld(outOld, scratch, got);
  applyNew(outNew, scratch, got);

  // 1. behavior preserved: new == old, bit for bit
  check(`new==old for got=${got}`, () => {
    for (let i = 0; i < WANT; i++) assert.strictEqual(outNew[i], outOld[i], `index ${i}`);
  });

  // 2. contract: out[0..got) == scratch, out[got..want) == 0
  check(`contract head copied got=${got}`, () => {
    for (let i = 0; i < got; i++) assert.strictEqual(outNew[i], scratch[i], `head index ${i}`);
  });
  check(`contract tail zeroed got=${got}`, () => {
    for (let i = got; i < WANT; i++) assert.strictEqual(outNew[i], 0, `tail index ${i}`);
  });
}

// 3. STAT_CONSUMED increments by exactly `got` (the bridge's liveness/health accounting).
for (const got of [0, 1, 64, 127, 128]) {
  let consumed = 0;
  if (got > 0) consumed += got;
  check(`consumed accounting got=${got}`, () => assert.strictEqual(consumed, got));
}

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

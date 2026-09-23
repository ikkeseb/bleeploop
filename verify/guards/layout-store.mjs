// verify/guards/layout-store.mjs — the REAL src/ui/layout/layout-store.ts persistence, loaded under the verify
// hooks. The module reads localStorage once at load, so every case imports a fresh generation (`?g=N`)
// after writing the stored record, then reads the public signals and drives the public setters.
//
// Covers: the looper-hero first-run seed (missing key / corrupt JSON / blocked storage / corrupt stageSizes
// -> DEFAULT_STAGE_WEIGHTS, while a VALID saved layout is honored verbatim, including a legacy one that
// still carries an `instrument` key); the keyboardPlacement allow-list ('top'|'bottom'|'hidden', else
// 'bottom'); the restore placement a hidden keyboard returns to, persisted or derived from an old record;
// and the persist -> reload round trip of every setter.

import assert from 'node:assert';
import '../harness/hooks.ts';

const STORAGE_KEY = 'lf.layout';
const store = new Map();
let throwOnGet = false;
globalThis.localStorage = {
  getItem(k) {
    if (throwOnGet) throw new Error('blocked');
    return store.has(k) ? store.get(k) : null;
  },
  setItem(k, v) {
    store.set(k, String(v));
  },
};

let generation = 0;
/** Store `record` (an object, a raw string, or undefined for no key) and load a fresh module. */
async function load(record, { blocked = false } = {}) {
  store.clear();
  throwOnGet = false;
  if (record !== undefined) store.set(STORAGE_KEY, typeof record === 'string' ? record : JSON.stringify(record));
  throwOnGet = blocked;
  try {
    return await import(`../../src/ui/layout/layout-store.ts?g=${++generation}`);
  } finally {
    throwOnGet = false;
  }
}
/** A fresh module over whatever the previous one persisted. */
const reload = () => import(`../../src/ui/layout/layout-store.ts?g=${++generation}`);

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

const { DEFAULT_STAGE_WEIGHTS } = await load(undefined);
const BASELINE = { keyboard: 0.55, looper: 2.0 };

// ---------------------------------------------------------------------------
// 1. The seed: fresh install, corrupt JSON and blocked storage all start from the looper-hero baseline.
// ---------------------------------------------------------------------------
await check('the baseline weights the looper as the hero and carries no instrument key', () => {
  assert.deepStrictEqual(DEFAULT_STAGE_WEIGHTS, BASELINE);
  assert.ok(DEFAULT_STAGE_WEIGHTS.looper > DEFAULT_STAGE_WEIGHTS.keyboard);
});
for (const [label, record, blocked] of [['no key', undefined, false], ['corrupt JSON', '{ not json', false],
  ['getItem throws', { keyboardPlacement: 'top' }, true]]) {
  await check(`${label} -> baseline, keyboard at the bottom`, async () => {
    const m = await load(record, { blocked });
    assert.deepStrictEqual(m.stageSizes(), BASELINE);
    assert.strictEqual(m.keyboardPlacement(), 'bottom');
    m.toggleKeyboardHidden();
    m.toggleKeyboardHidden();
    assert.strictEqual(m.keyboardPlacement(), 'bottom', 'a hidden keyboard returns to the bottom');
  });
}
await check('the seed is a copy: editing it does not change the baseline', async () => {
  const m = await load(undefined);
  m.stageSizes().looper = 9;
  assert.deepStrictEqual(m.DEFAULT_STAGE_WEIGHTS, BASELINE);
});

// ---------------------------------------------------------------------------
// 2. stageSizes: a valid weight record is honored verbatim; anything else degrades to the baseline.
// ---------------------------------------------------------------------------
for (const sizes of [{ instrument: 0.9, keyboard: 0.5, looper: 2.0 }, { instrument: 1, looper: 1.6 }, { a: 1e-9 }, {}]) {
  await check(`valid stageSizes ${JSON.stringify(sizes)} honored verbatim`, async () => {
    const m = await load({ stageSizes: sizes, slotSizes: { slot0: 1, slot1: 1 } });
    assert.deepStrictEqual(m.stageSizes(), sizes);
  });
}
// Documents a known harmless quirk: arrays are objects with numeric values, so they pass. In practice
// stageSizes always comes from JSON.parse of an object literal, so this never bites — pinned, not endorsed.
await check('stageSizes [1,2] honored (array quirk, documented)', async () => {
  assert.deepStrictEqual((await load({ stageSizes: [1, 2] })).stageSizes(), [1, 2]);
});
// NaN and Infinity have no JSON form: JSON.stringify writes them as null, covered below.
for (const bad of [{ a: 0 }, { a: -1 }, { a: '1' }, { a: null }, { a: 1, b: 0 }, { a: true }, 'notobj', 5, null, false]) {
  await check(`corrupt stageSizes ${JSON.stringify(bad)} -> baseline`, async () => {
    assert.deepStrictEqual((await load({ stageSizes: bad })).stageSizes(), BASELINE);
  });
}
await check('missing stageSizes -> baseline', async () => {
  assert.deepStrictEqual((await load({ keyboardPlacement: 'top' })).stageSizes(), BASELINE);
});

// ---------------------------------------------------------------------------
// 3. keyboardPlacement allow-list: valid preserved, anything else -> 'bottom'.
// ---------------------------------------------------------------------------
for (const p of ['top', 'bottom', 'hidden']) {
  await check(`placement '${p}' preserved`, async () => assert.strictEqual((await load({ keyboardPlacement: p })).keyboardPlacement(), p));
}
for (const bad of ['left', 'TOP', '', 3, null, undefined]) {
  await check(`placement ${JSON.stringify(bad)} -> 'bottom'`, async () => {
    assert.strictEqual((await load({ keyboardPlacement: bad })).keyboardPlacement(), 'bottom');
  });
}

// ---------------------------------------------------------------------------
// 4. Where a hidden keyboard returns: the persisted restore placement, else one derived from an old record.
// ---------------------------------------------------------------------------
for (const [record, want, label] of [
  [{ keyboardPlacement: 'hidden', lastVisiblePlacement: 'top' }, 'top', 'persisted top'],
  [{ keyboardPlacement: 'hidden', lastVisiblePlacement: 'bottom' }, 'bottom', 'persisted bottom'],
  [{ keyboardPlacement: 'hidden' }, 'bottom', 'legacy hidden record (nothing to recover)'],
  [{ keyboardPlacement: 'hidden', lastVisiblePlacement: 'hidden' }, 'bottom', 'invalid persisted restore placement'],
]) {
  await check(`${label}: unhide -> ${want}`, async () => {
    const m = await load(record);
    m.toggleKeyboardHidden();
    assert.strictEqual(m.keyboardPlacement(), want);
  });
}
for (const [record, want, label] of [
  [{ keyboardPlacement: 'top' }, 'top', 'legacy visible top record'],
  [{ keyboardPlacement: 'bottom' }, 'bottom', 'legacy visible bottom record'],
  [{ keyboardPlacement: 'top', lastVisiblePlacement: 'left' }, 'top', 'invalid restore placement, visible top'],
]) {
  await check(`${label}: hide then unhide -> ${want}`, async () => {
    const m = await load(record);
    m.toggleKeyboardHidden();
    assert.strictEqual(m.keyboardPlacement(), 'hidden');
    m.toggleKeyboardHidden();
    assert.strictEqual(m.keyboardPlacement(), want);
  });
}

// ---------------------------------------------------------------------------
// 5. Every setter persists, and a reload restores exactly what was set.
// ---------------------------------------------------------------------------
await check('stage sizes, placement and restore placement survive a reload', async () => {
  const m = await load(undefined);
  m.setStageSizes({ keyboard: 0.4, looper: 3 });
  m.moveKeyboard(); // bottom -> top
  m.toggleKeyboardHidden(); // hidden, restore = top
  const r = await reload();
  assert.deepStrictEqual(r.stageSizes(), { keyboard: 0.4, looper: 3 });
  assert.strictEqual(r.keyboardPlacement(), 'hidden');
  r.toggleKeyboardHidden();
  assert.strictEqual(r.keyboardPlacement(), 'top');
});
await check('each setter persists on its own', async () => {
  const m = await load({ keyboardPlacement: 'top' });
  m.setStageSizes({ keyboard: 1, looper: 5 });
  assert.deepStrictEqual((await reload()).stageSizes(), { keyboard: 1, looper: 5 });
  (await reload()).setKeyboardPlacement('bottom');
  assert.strictEqual((await reload()).keyboardPlacement(), 'bottom');
});
await check('moveKeyboard swaps top/bottom and is a no-op while hidden', async () => {
  const m = await load({ keyboardPlacement: 'top' });
  m.moveKeyboard();
  assert.strictEqual(m.keyboardPlacement(), 'bottom');
  m.moveKeyboard();
  assert.strictEqual(m.keyboardPlacement(), 'top');
  m.setKeyboardPlacement('hidden');
  m.moveKeyboard();
  assert.strictEqual(m.keyboardPlacement(), 'hidden');
  assert.strictEqual(m.keyboardVisible(), false);
});

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

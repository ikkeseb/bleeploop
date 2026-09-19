// fs-layout-store-verify.mjs — deterministic guard for src/ui/layout/layout-store.ts persistence validators.
//
// PORT of isWeightRecord + read() (mirrors layout-store.ts lines 29-54 VERBATIM). A port, not a real
// import, because: read()/isWeightRecord are private (not exported), the module imports solid-js, and it
// runs read() once at module load (freezing the initial signals) — so the validation can't be re-driven
// with different localStorage from the real module. The functions below are copied char-for-char from the
// source; IF YOU CHANGE THAT VALIDATION, UPDATE THIS PORT IN LOCKSTEP. Run: node fs-layout-store-verify.mjs
//
// Covers: the looper-hero first-run seed (missing key / corrupt stageSizes -> DEFAULT_STAGE_WEIGHTS, while
// a VALID saved layout is honored verbatim — including a legacy one that still carries an `instrument`
// key, now that the instrument panel is autoSize and unweighted); the keyboardPlacement allow-list gating
// ('top'|'bottom'|'hidden', else the 'bottom' default since W4.5); backward-compatible restoration of the
// last visible keyboard placement; and isWeightRecord rejecting non-finite/zero/negative/non-number weights.

import assert from 'node:assert';

const STORAGE_KEY = 'lf.layout';

// ---- in-memory localStorage shim, so the ported read() can run getItem verbatim ----
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

// MIRRORS: src/ui/layout/layout-store.ts@43-103 sha256:a61fbd4557262ef0  (DEFAULT_STAGE_WEIGHTS + isWeightRecord + read)
// ===== BEGIN VERBATIM PORT of layout-store.ts =====
function isWeightRecord(v) {
  return (
    !!v &&
    typeof v === 'object' &&
    Object.values(v).every((x) => typeof x === 'number' && Number.isFinite(x) && x > 0)
  );
}

// DEFAULT_STAGE_WEIGHTS — duplicated from layout-store.ts (keep in lockstep); the looper-hero baseline.
// W4.5: the instrument source row is now autoSize (content-hugging) and unweighted, so it's dropped from
// the map — only the keyboard/looper pair is weighted, and looper stays the max, the invariant asserted
// below. Fresh install + missing/invalid placement now default the keyboard to 'bottom'.
const DEFAULT_STAGE_WEIGHTS = { keyboard: 0.55, looper: 2.0 };

function read() {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw)
      return {
        stageSizes: { ...DEFAULT_STAGE_WEIGHTS },
        keyboardPlacement: 'bottom',
        lastVisiblePlacement: 'bottom',
      };
    const p = JSON.parse(raw);
    const placement =
      p.keyboardPlacement === 'bottom' || p.keyboardPlacement === 'hidden' || p.keyboardPlacement === 'top'
        ? p.keyboardPlacement
        : 'bottom';
    const lastVisiblePlacement =
      p.lastVisiblePlacement === 'top' || p.lastVisiblePlacement === 'bottom'
        ? p.lastVisiblePlacement
        : placement === 'top' || placement === 'bottom'
          ? placement
          : 'bottom';
    return {
      stageSizes: isWeightRecord(p.stageSizes) ? p.stageSizes : { ...DEFAULT_STAGE_WEIGHTS },
      keyboardPlacement: placement,
      lastVisiblePlacement,
    };
  } catch {
    return {
      stageSizes: { ...DEFAULT_STAGE_WEIGHTS },
      keyboardPlacement: 'bottom',
      lastVisiblePlacement: 'bottom',
    };
  }
}
// ===== END VERBATIM PORT =====

const DEFAULTS = {
  stageSizes: { ...DEFAULT_STAGE_WEIGHTS },
  keyboardPlacement: 'bottom',
  lastVisiblePlacement: 'bottom',
};

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
function setRaw(obj) {
  store.clear();
  throwOnGet = false;
  if (obj !== undefined) store.set(STORAGE_KEY, typeof obj === 'string' ? obj : JSON.stringify(obj));
}

// ---------------------------------------------------------------------------
// 1. isWeightRecord — unit coverage (the load-bearing guard against a corrupt size map).
// ---------------------------------------------------------------------------
check('isWeightRecord {a:1,b:0.5} -> true', () => assert.strictEqual(isWeightRecord({ a: 1, b: 0.5 }), true));
check('isWeightRecord {} -> true (empty .every)', () => assert.strictEqual(isWeightRecord({}), true));
check('isWeightRecord {a:0} -> false (not > 0)', () => assert.strictEqual(isWeightRecord({ a: 0 }), false));
check('isWeightRecord {a:-1} -> false', () => assert.strictEqual(isWeightRecord({ a: -1 }), false));
check('isWeightRecord {a:NaN} -> false', () => assert.strictEqual(isWeightRecord({ a: NaN }), false));
check('isWeightRecord {a:Infinity} -> false', () => assert.strictEqual(isWeightRecord({ a: Infinity }), false));
check('isWeightRecord {a:"1"} -> false (string)', () => assert.strictEqual(isWeightRecord({ a: '1' }), false));
check('isWeightRecord null -> false', () => assert.strictEqual(isWeightRecord(null), false));
check('isWeightRecord undefined -> false', () => assert.strictEqual(isWeightRecord(undefined), false));
check('isWeightRecord number -> false (not object)', () => assert.strictEqual(isWeightRecord(5), false));
// Documents a known harmless quirk: arrays are objects with numeric values, so they pass. In practice
// stageSizes always comes from JSON.parse of an object literal, so this never bites — pinned, not endorsed.
check('isWeightRecord [1,2] -> true (array quirk, documented)', () => assert.strictEqual(isWeightRecord([1, 2]), true));

// ---------------------------------------------------------------------------
// 2. read() fall-through: missing key, corrupt JSON, getItem throw.
// ---------------------------------------------------------------------------
setRaw(undefined);
check('no key -> defaults', () => assert.deepStrictEqual(read(), DEFAULTS));
setRaw('{ not json');
check('corrupt JSON -> defaults (catch)', () => assert.deepStrictEqual(read(), DEFAULTS));
setRaw(undefined);
throwOnGet = true;
check('getItem throw -> defaults (catch)', () => assert.deepStrictEqual(read(), DEFAULTS));

// ---------------------------------------------------------------------------
// 2b. The looper-hero seed: applied on fresh install / corrupt, NEVER over a valid saved layout.
// ---------------------------------------------------------------------------
setRaw(undefined);
check('fresh install seeds DEFAULT_STAGE_WEIGHTS', () =>
  assert.deepStrictEqual(read().stageSizes, DEFAULT_STAGE_WEIGHTS));
check('fresh install defaults keyboard to bottom (W4.5)', () =>
  assert.strictEqual(read().keyboardPlacement, 'bottom'));
check('seeded looper is the hero (max of the weighted pair)', () =>
  assert.ok(DEFAULT_STAGE_WEIGHTS.looper > DEFAULT_STAGE_WEIGHTS.keyboard));
check('DEFAULT_STAGE_WEIGHTS carries no instrument key (autoSize, unweighted)', () =>
  assert.strictEqual('instrument' in DEFAULT_STAGE_WEIGHTS, false));
// A legacy saved layout that still carries an `instrument` key is honored verbatim (the key is simply
// unused now that the instrument panel is autoSize — isWeightRecord/read must not choke on it).
setRaw({ stageSizes: { instrument: 0.9, keyboard: 0.5, looper: 2.0 } });
check('a VALID saved stageSizes (legacy instrument key) is NOT overwritten by the seed', () =>
  assert.deepStrictEqual(read().stageSizes, { instrument: 0.9, keyboard: 0.5, looper: 2.0 }));

// ---------------------------------------------------------------------------
// 3. keyboardPlacement allow-list: valid preserved, anything else -> 'bottom' (W4.5 default).
// ---------------------------------------------------------------------------
for (const p of ['top', 'bottom', 'hidden']) {
  setRaw({ keyboardPlacement: p });
  check(`placement '${p}' preserved`, () => assert.strictEqual(read().keyboardPlacement, p));
}
for (const bad of ['left', 'TOP', '', 3, null, undefined]) {
  setRaw({ keyboardPlacement: bad });
  check(`placement ${JSON.stringify(bad)} -> 'bottom'`, () => assert.strictEqual(read().keyboardPlacement, 'bottom'));
}
setRaw({}); // missing field
check('placement missing -> bottom', () => assert.strictEqual(read().keyboardPlacement, 'bottom'));

// ---------------------------------------------------------------------------
// 3b. lastVisiblePlacement: new records preserve it; old records derive it when possible.
// ---------------------------------------------------------------------------
setRaw({ keyboardPlacement: 'hidden', lastVisiblePlacement: 'top' });
check('hidden keyboard restores persisted top placement', () =>
  assert.strictEqual(read().lastVisiblePlacement, 'top'));
setRaw({ keyboardPlacement: 'hidden', lastVisiblePlacement: 'bottom' });
check('hidden keyboard restores persisted bottom placement', () =>
  assert.strictEqual(read().lastVisiblePlacement, 'bottom'));
setRaw({ keyboardPlacement: 'top' });
check('legacy visible top record derives top restore placement', () =>
  assert.strictEqual(read().lastVisiblePlacement, 'top'));
setRaw({ keyboardPlacement: 'bottom' });
check('legacy visible bottom record derives bottom restore placement', () =>
  assert.strictEqual(read().lastVisiblePlacement, 'bottom'));
setRaw({ keyboardPlacement: 'hidden' });
check('legacy hidden record defaults restore placement to bottom', () =>
  assert.strictEqual(read().lastVisiblePlacement, 'bottom'));
setRaw({ keyboardPlacement: 'top', lastVisiblePlacement: 'left' });
check('invalid persisted restore placement falls back to visible placement', () =>
  assert.strictEqual(read().lastVisiblePlacement, 'top'));

// ---------------------------------------------------------------------------
// 4. stageSizes: valid weight records preserved; corrupt -> seeded baseline. A legacy stored
//    slotSizes key (the deleted instrument-slot width split) is simply ignored by read().
// ---------------------------------------------------------------------------
setRaw({ stageSizes: { instrument: 1, looper: 1.6 }, slotSizes: { slot0: 1, slot1: 1 } });
check('valid stage weights preserved; legacy slotSizes key ignored', () => {
  const s = read();
  assert.deepStrictEqual(s.stageSizes, { instrument: 1, looper: 1.6 });
  assert.strictEqual('slotSizes' in s, false);
});
for (const bad of [{ a: 0 }, { a: -1 }, { a: '1' }, { a: null }, 'notobj', 5]) {
  setRaw({ stageSizes: bad });
  check(`corrupt stageSizes ${JSON.stringify(bad)} -> seeded baseline`, () =>
    assert.deepStrictEqual(read().stageSizes, DEFAULT_STAGE_WEIGHTS));
}

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

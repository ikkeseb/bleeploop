// verify/guards/audio-settings.mjs — deterministic guard for src/audio/audio-settings.ts persistence validators.
//
// Imports the REAL source (Node TS type-stripping), NOT a port, so it cannot drift. The source is
// import-free and touches `localStorage` only at call-time (never at module load), so a tiny in-memory
// localStorage shim installed before the first call is enough to exercise it. Run: node verify/guards/audio-settings.mjs
//
// Covers readAudioDeviceSettings's field-by-field validation + DEFAULTS fallback (missing key, corrupt
// JSON, out-of-range / wrong-typed bufferFrames, non-string device ids, non-boolean asioEnabled) and
// writeAudioDeviceSettings's partial-merge + best-effort (swallow-throw) contract. These validators run on
// every launch and are easy to silently break; they had zero coverage before this guard.

import assert from 'node:assert';

// ---- in-memory localStorage shim (the source reads/writes localStorage only inside its functions) ----
const store = new Map();
let throwOnGet = false;
let throwOnSet = false;
globalThis.localStorage = {
  getItem(k) {
    if (throwOnGet) throw new Error('blocked');
    return store.has(k) ? store.get(k) : null;
  },
  setItem(k, v) {
    if (throwOnSet) throw new Error('quota exceeded');
    store.set(k, String(v));
  },
  removeItem(k) {
    store.delete(k);
  },
  clear() {
    store.clear();
  },
};

const { readAudioDeviceSettings, writeAudioDeviceSettings, BUFFER_FRAMES_OPTIONS, DEFAULT_BUFFER_FRAMES } =
  await import('../../src/audio/audio-settings.ts');

const KEY = 'lf.audioDevices';
const DEFAULTS = {
  inputDeviceId: '',
  inputChannel: '',
  outputDeviceId: '',
  bufferFrames: DEFAULT_BUFFER_FRAMES,
  asioEnabled: true,
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
function reset() {
  store.clear();
  throwOnGet = false;
  throwOnSet = false;
}

// ---------------------------------------------------------------------------
// 1. Missing key / corrupt JSON -> full DEFAULTS (the two fall-through paths).
// ---------------------------------------------------------------------------
reset();
check('no stored key -> DEFAULTS', () => assert.deepStrictEqual(readAudioDeviceSettings(), DEFAULTS));
reset();
store.set(KEY, '{ not valid json');
check('corrupt JSON -> DEFAULTS (catch)', () => assert.deepStrictEqual(readAudioDeviceSettings(), DEFAULTS));
reset();
store.set(KEY, 'null');
check('stored literal null -> DEFAULTS', () => assert.deepStrictEqual(readAudioDeviceSettings(), DEFAULTS));

// ---------------------------------------------------------------------------
// 2. A fully-valid object round-trips untouched.
// ---------------------------------------------------------------------------
reset();
const full = {
  inputDeviceId: 'in-1',
  inputChannel: '2',
  outputDeviceId: 'out-9',
  bufferFrames: 128,
  asioEnabled: false,
};
store.set(KEY, JSON.stringify(full));
check('valid full object preserved', () => assert.deepStrictEqual(readAudioDeviceSettings(), full));

// ---------------------------------------------------------------------------
// 3. bufferFrames: every option preserved; everything else -> DEFAULT_BUFFER_FRAMES.
// ---------------------------------------------------------------------------
for (const f of BUFFER_FRAMES_OPTIONS) {
  reset();
  store.set(KEY, JSON.stringify({ bufferFrames: f }));
  check(`bufferFrames ${f} preserved`, () => assert.strictEqual(readAudioDeviceSettings().bufferFrames, f));
}
for (const bad of [999, 0, -64, 100, 'abc', null, 1.5, NaN, [], {}]) {
  reset();
  store.set(KEY, JSON.stringify({ bufferFrames: bad }));
  check(`bufferFrames ${JSON.stringify(bad)} -> default ${DEFAULT_BUFFER_FRAMES}`, () =>
    assert.strictEqual(readAudioDeviceSettings().bufferFrames, DEFAULT_BUFFER_FRAMES));
}

// ---------------------------------------------------------------------------
// 4. Device ids: non-string -> ''.  asioEnabled: non-boolean -> true (default); false preserved.
// ---------------------------------------------------------------------------
reset();
store.set(KEY, JSON.stringify({ inputDeviceId: 42, inputChannel: {}, outputDeviceId: true }));
check('non-string device ids -> empty strings', () => {
  const s = readAudioDeviceSettings();
  assert.strictEqual(s.inputDeviceId, '');
  assert.strictEqual(s.inputChannel, '');
  assert.strictEqual(s.outputDeviceId, '');
});
reset();
store.set(KEY, JSON.stringify({ asioEnabled: 'yes' }));
check('non-boolean asioEnabled -> true', () => assert.strictEqual(readAudioDeviceSettings().asioEnabled, true));
reset();
store.set(KEY, JSON.stringify({ asioEnabled: false }));
check('asioEnabled false preserved', () => assert.strictEqual(readAudioDeviceSettings().asioEnabled, false));

// ---------------------------------------------------------------------------
// 5. writeAudioDeviceSettings: partial merge over the existing record, no clobber.
// ---------------------------------------------------------------------------
reset();
writeAudioDeviceSettings({ bufferFrames: 512 });
check('write partial merges over defaults', () => {
  const s = readAudioDeviceSettings();
  assert.strictEqual(s.bufferFrames, 512);
  assert.strictEqual(s.asioEnabled, true);
  assert.strictEqual(s.inputDeviceId, '');
});
writeAudioDeviceSettings({ inputDeviceId: 'mic' });
check('second write does not clobber prior field', () => {
  const s = readAudioDeviceSettings();
  assert.strictEqual(s.bufferFrames, 512);
  assert.strictEqual(s.inputDeviceId, 'mic');
});

// ---------------------------------------------------------------------------
// 6. Best-effort: a throwing localStorage must never escape.
// ---------------------------------------------------------------------------
reset();
throwOnSet = true;
check('write swallows a storage throw', () => assert.doesNotThrow(() => writeAudioDeviceSettings({ bufferFrames: 64 })));
reset();
throwOnGet = true;
check('read swallows a storage throw -> DEFAULTS', () => assert.deepStrictEqual(readAudioDeviceSettings(), DEFAULTS));

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

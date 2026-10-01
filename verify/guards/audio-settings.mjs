// verify/guards/audio-settings.mjs — deterministic guard for src/ui/state/audio-settings.ts persistence validators.
//
// Imports the REAL source (Node TS type-stripping), NOT a port, so it cannot drift. The source is
// import-free and touches `localStorage` only at call-time (never at module load), so a tiny in-memory
// localStorage shim installed before the first call is enough to exercise it. Run: node verify/guards/audio-settings.mjs
//
// Covers readAudioDeviceSettings's field-by-field validation + DEFAULTS fallback (missing key, corrupt
// JSON, out-of-range / wrong-typed bufferFrames, non-string device ids (Share output's and the ASIO
// driver pick included), non-boolean asioEnabled) and
// writeAudioDeviceSettings's partial-merge + best-effort (swallow-throw) contract. These validators run on
// every launch and are easy to silently break; they had zero coverage before this guard. Also the ASIO
// Buffer select's sizes (asioBlock, the mirror of Rust's `transition::asio_block`, and asioBufferChoice):
// a driver fixed at one size, a wide range, and a running block outside the options list. And engine
// mode's per-slot input channels: a saved pair is kept, and settings from before the per-slot pick (or a
// malformed pair) start both slots on the saved global channel. And the rate pick: only 44.1/48 kHz is
// kept (anything else, or settings from before it, is the device's own), and the Sample rate select
// (rateChoice): the picks the device runs, the saved one shown only where it runs and, while a device
// runs, only at the rate it runs at.

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

const { readAudioDeviceSettings, writeAudioDeviceSettings, BUFFER_FRAMES_OPTIONS, DEFAULT_BUFFER_FRAMES, asioBlock, asioBufferChoice, rateChoice } =
  await import('../../src/ui/state/audio-settings.ts');

const KEY = 'lf.audioDevices';
const DEFAULTS = {
  inputDeviceId: '',
  inputChannel: '',
  slotInputChannels: ['', ''],
  outputDeviceId: '',
  bufferFrames: DEFAULT_BUFFER_FRAMES,
  sampleRate: null,
  asioEnabled: true,
  asioDriver: '',
  shareDeviceId: '',
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
  slotInputChannels: ['0', '3'],
  outputDeviceId: 'out-9',
  bufferFrames: 128,
  sampleRate: 44100,
  asioEnabled: false,
  asioDriver: 'Yamaha Steinberg USB ASIO',
  shareDeviceId: 'share-3',
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
store.set(KEY, JSON.stringify({ inputDeviceId: 42, inputChannel: {}, outputDeviceId: true, shareDeviceId: 7, asioDriver: null }));
check('non-string device ids -> empty strings', () => {
  const s = readAudioDeviceSettings();
  assert.strictEqual(s.inputDeviceId, '');
  assert.strictEqual(s.inputChannel, '');
  assert.strictEqual(s.outputDeviceId, '');
  assert.strictEqual(s.shareDeviceId, '');
  assert.strictEqual(s.asioDriver, '', 'a non-string driver pick reads as automatic');
});
reset();
store.set(KEY, JSON.stringify({ asioEnabled: 'yes' }));
check('non-boolean asioEnabled -> true', () => assert.strictEqual(readAudioDeviceSettings().asioEnabled, true));
reset();
store.set(KEY, JSON.stringify({ asioEnabled: false }));
check('asioEnabled false preserved', () => assert.strictEqual(readAudioDeviceSettings().asioEnabled, false));

// ---------------------------------------------------------------------------
// 4b. Per-slot input channels: the migration from the one global channel, and their validation.
// ---------------------------------------------------------------------------
reset();
store.set(KEY, JSON.stringify({ inputChannel: '1' }));
check('no per-slot channels saved -> both slots start on the global channel', () =>
  assert.deepStrictEqual(readAudioDeviceSettings().slotInputChannels, ['1', '1']));
for (const bad of [['1'], ['1', '2', '3'], [1, 2], ['x', '1'], ['-1', ''], 'ab', null, {}]) {
  reset();
  store.set(KEY, JSON.stringify({ inputChannel: '2', slotInputChannels: bad }));
  check(`slotInputChannels ${JSON.stringify(bad)} -> the global channel for both`, () =>
    assert.deepStrictEqual(readAudioDeviceSettings().slotInputChannels, ['2', '2']));
}
reset();
store.set(KEY, JSON.stringify({ inputChannel: 'junk' }));
check('a malformed global channel seeds auto', () => assert.deepStrictEqual(readAudioDeviceSettings().slotInputChannels, ['', '']));
reset();
store.set(KEY, JSON.stringify({ inputChannel: '1', slotInputChannels: ['', '5'] }));
check('a saved pair wins over the global channel', () =>
  assert.deepStrictEqual(readAudioDeviceSettings().slotInputChannels, ['', '5']));
reset();
store.set(KEY, JSON.stringify({ inputChannel: '1' }));
writeAudioDeviceSettings({ bufferFrames: 128 });
check('the first write keeps the migrated pair', () => {
  store.set(KEY, JSON.stringify({ ...JSON.parse(store.get(KEY)), inputChannel: '' }));
  assert.deepStrictEqual(readAudioDeviceSettings().slotInputChannels, ['1', '1']);
});
reset();
check('the defaults hand out a fresh pair each read', () => {
  readAudioDeviceSettings().slotInputChannels[0] = '7';
  assert.deepStrictEqual(readAudioDeviceSettings().slotInputChannels, ['', '']);
});

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

// ---------------------------------------------------------------------------
// 7. The ASIO Buffer select: the size an open asks for, and what the select offers and shows.
// ---------------------------------------------------------------------------
// The same table as `an_asio_open_never_asks_for_a_buffer_outside_the_drivers_range` in
// src-tauri/src/engine_io/transition.rs: the TS copy must not drift from the one that decides.
for (const [[requested, min, max], want, why] of [
  [[64, 512, 512], 512, 'a driver fixed in its control panel takes its one size'],
  [[64, 128, 2048], 128, 'too small: the nearest power of two in range'],
  [[2048, 32, 1024], 1024, 'too large: the nearest power of two in range'],
  [[256, 32, 1024], 256, 'in range: the request'],
  [[64, 480, 480], 480, 'no power of two in range: the minimum'],
  [[1024, 100, 300], 256, 'the nearest power of two, not the bound'],
]) {
  check(`asioBlock(${requested}, ${min}..${max}) = ${want}: ${why}`, () => assert.strictEqual(asioBlock(requested, min, max), want));
}
check('single-size driver, nothing running: only that size, shown, fixed', () =>
  assert.deepStrictEqual(asioBufferChoice(64, { min: 512, max: 512 }, null), { options: [512], shown: 512, fixed: true }));
check('single-size driver running: the same, whatever the saved pick', () =>
  assert.deepStrictEqual(asioBufferChoice(128, { min: 512, max: 512 }, 512), { options: [512], shown: 512, fixed: true }));
check('wide range: the options inside it, the running block shown', () =>
  assert.deepStrictEqual(asioBufferChoice(64, { min: 128, max: 2048 }, 256), { options: [128, 256, 512, 1024], shown: 256, fixed: false }));
check('wide range, nothing running: shows the size the open would take for the saved pick', () =>
  assert.deepStrictEqual(asioBufferChoice(64, { min: 128, max: 2048 }, null), { options: [128, 256, 512, 1024], shown: 128, fixed: false }));
check('a running block outside the options list joins it, in order, and shows', () =>
  assert.deepStrictEqual(asioBufferChoice(256, { min: 32, max: 1024 }, 96), { options: [64, 96, 128, 256, 512, 1024], shown: 96, fixed: false }));
check('a one-size driver at a size not in the list: that size alone', () =>
  assert.deepStrictEqual(asioBufferChoice(256, { min: 480, max: 480 }, null), { options: [480], shown: 480, fixed: true }));
check('no range known: every option, the running block shown', () =>
  assert.deepStrictEqual(asioBufferChoice(256, null, 128), { options: [...BUFFER_FRAMES_OPTIONS], shown: 128, fixed: false }));
check('the saved pick is an input only: the choice never writes it', () => {
  reset();
  writeAudioDeviceSettings({ bufferFrames: 64 });
  asioBufferChoice(readAudioDeviceSettings().bufferFrames, { min: 512, max: 512 }, 512);
  assert.strictEqual(readAudioDeviceSettings().bufferFrames, 64);
});

// ---------------------------------------------------------------------------
// 8. The rate pick and the Sample rate select.
// ---------------------------------------------------------------------------
for (const rate of [44100, 48000]) {
  reset();
  store.set(KEY, JSON.stringify({ sampleRate: rate }));
  check(`sampleRate ${rate} preserved`, () => assert.strictEqual(readAudioDeviceSettings().sampleRate, rate));
}
for (const bad of [96000, 44100.5, '48000', 0, -1, true, {}]) {
  reset();
  store.set(KEY, JSON.stringify({ sampleRate: bad }));
  check(`sampleRate ${JSON.stringify(bad)} -> the device's own (null)`, () => assert.strictEqual(readAudioDeviceSettings().sampleRate, null));
}
reset();
store.set(KEY, JSON.stringify({ bufferFrames: 128, asioEnabled: true }));
check('settings from before the rate pick: the device rate', () => assert.strictEqual(readAudioDeviceSettings().sampleRate, null));
check('a driver that runs both: Device and both, the saved pick shown', () =>
  assert.deepStrictEqual(rateChoice(48000, [44100, 48000], null), { options: [null, 44100, 48000], shown: 48000, fixed: false }));
check('a driver that runs only 44.1 kHz: no 48, a saved 48 shows the device rate', () =>
  assert.deepStrictEqual(rateChoice(48000, [44100], 44100), { options: [null, 44100], shown: null, fixed: false }));
check('a driver that runs neither (or WASAPI): the device rate alone, set elsewhere', () =>
  assert.deepStrictEqual(rateChoice(44100, [], 48000), { options: [null], shown: null, fixed: true }));
check('a driver that runs both but runs at its own: the device rate shown, the pick kept elsewhere', () =>
  assert.deepStrictEqual(rateChoice(44100, [44100, 48000], 48000), { options: [null, 44100, 48000], shown: null, fixed: false }));
check('a rate no player can pick is never offered', () =>
  assert.deepStrictEqual(rateChoice(null, [48000, 96000], null), { options: [null, 48000], shown: null, fixed: false }));

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

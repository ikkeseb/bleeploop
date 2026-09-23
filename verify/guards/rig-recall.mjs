// verify/guards/rig-recall.mjs — the REAL src/audio/rig-recall.ts under the verify hooks: which build
// recalls, and from which keys. The owner's build (no `VITE_LF_PROBE`) uses the `lf.` keys; the
// recall's own native probe (`recall-restart`) keys of its own, never the owner's; every other DEV
// native probe neither restores nor stores, so a run killed with a plugin loaded never hands that
// plugin to its next run. Each case loads a fresh generation (`?g=N`) under its own env.
// The record, the marker and the skip rules across launches: verify/probes/rig-recall.mjs.

import assert from 'node:assert';
import '../harness/hooks.ts';

const store = new Map();
globalThis.localStorage = {
  getItem: (k) => (store.has(k) ? store.get(k) : null),
  setItem: (k, v) => void store.set(k, String(v)),
  removeItem: (k) => void store.delete(k),
};
// The settle window is a plain timer: let it run out at once.
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (fn, _ms, ...args) => realSetTimeout(fn, 0, ...args);

const FX = { id: 'probe.fx', name: 'Probe Amp', format: 'clap', path: 'C:\\probe\\amp.clap', isEffect: true };
const SYN = { id: 'probe.syn', name: 'Probe Synth', format: 'vst3', path: 'C:\\probe\\synth.vst3', isEffect: false };
const saved = (p) => p && { format: p.format, path: p.path, id: p.id, name: p.name };
const record = (a, b) => JSON.stringify([saved(a), saved(b)]);

let generation = 0;
/** A fresh launch of the module under `probe` (undefined = the owner's build), over `seed`. */
async function launch(probe, seed) {
  store.clear();
  for (const [k, v] of Object.entries(seed)) store.set(k, v);
  globalThis.__importMetaEnv = probe === undefined ? { DEV: true } : { DEV: true, VITE_LF_PROBE: probe };
  const m = await import(`../../src/audio/rig-recall.ts?g=${++generation}`);
  const loads = [];
  await m.recallRig([FX, SYN], async (slot, d) => void loads.push([slot, d.id]));
  return { m, loads };
}

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

await check("the owner's build restores from the lf. keys", async () => {
  const { m, loads } = await launch(undefined, { 'lf.rigRecall': record(FX, null), 'lf.probe.recall-restart.rigRecall': record(null, SYN) });
  assert.deepEqual(loads, [[0, 'probe.fx']]);
  assert.equal(store.has('lf.rigRecallInFlight'), false, 'the marker is gone after the settle window');
  m.rememberSlotPlugin(1, SYN);
  assert.equal(JSON.parse(store.get('lf.rigRecall'))[1].id, 'probe.syn');
  assert.equal(store.get('lf.probe.recall-restart.rigRecall'), record(null, SYN), "the probe's record is untouched");
});

await check('the recall probe restores from its own keys, never the owner\'s', async () => {
  const owner = record(SYN, null);
  const { m, loads } = await launch('recall-restart', { 'lf.rigRecall': owner, 'lf.probe.recall-restart.rigRecall': record(FX, null) });
  assert.deepEqual(loads, [[0, 'probe.fx']]);
  m.rememberSlotPlugin(1, SYN);
  m.forgetSlotPlugin(0);
  assert.deepEqual(JSON.parse(store.get('lf.probe.recall-restart.rigRecall')).map((p) => p?.id ?? null), [null, 'probe.syn']);
  assert.equal(store.get('lf.rigRecall'), owner, "the owner's record is untouched");
});

for (const probe of ['editor-smoke', 'restart-survey', 'swap-stress']) {
  await check(`${probe} neither restores nor stores`, async () => {
    const seed = {
      'lf.rigRecall': record(FX, SYN),
      [`lf.probe.${probe}.rigRecall`]: record(FX, SYN),
      [`lf.probe.${probe}.rigRecallInFlight`]: 'Probe Amp (clap)',
    };
    const { m, loads } = await launch(probe, seed);
    assert.deepEqual(loads, [], 'nothing restored');
    assert.equal(m.rigRecallDone(), true);
    assert.equal(m.recallInFlight(), false);
    m.rememberSlotPlugin(0, SYN);
    m.forgetSlotPlugin(1);
    assert.deepEqual(Object.fromEntries(store), seed, 'storage untouched');
  });
}

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

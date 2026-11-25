// verify/guards/rig-upgrade.mjs — the rig record's pure rules (src/ui/state/rig-record.ts, unit contract
// §1, §6) in plain Node: a v0.8.9 profile's upgrade into `lf.rig` case by case (a plugin slot found or
// missing from the scan, an effect or not, a stored level; the legacy crash marker; a SYNTH from
// `lf.slotSource`; unset sources, the guitar-first install's included; an Off slot's input level; each
// send on, off, at its defaults or not, bound to a pedal or not, no bindings report at all; the OTT, which
// v0.8.9 never stored, only when on, changed or bound; a SYNTH-headed slot's pending; chains a DEV run
// reported before any record), ids from an injected mint; the placement of `pending` by the head's role;
// the record's units from a `chains` report; the record's text, read tolerantly.
// What it cannot see: the keys' resolvers (`instrument.ts`, `engine-store.ts`, `midi.ts`), the storage, the
// restore's order of adds and its marker: the chain-mode cases of verify/probes/rig-recall.mjs and
// verify/probes/legacy-storage.mjs.
// Run: node verify/guards/rig-upgrade.mjs

import assert from 'node:assert/strict';
import {
  addSpec,
  boundKinds,
  defaultPluginLevel,
  headRole,
  parseRig,
  placed,
  serializeRig,
  storedSlotSource,
  unitsFromReport,
  upgradeRig,
  withPlaced,
} from '../../src/ui/state/rig-record.ts';

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

// ── a v0.8.9 profile, as its resolvers read it ─────────────────────────────────────────────────────────

const AMP = { format: 'vst3', path: 'C:\\VST3\\Amp.vst3', id: 'amp.id', name: 'Amp' };
const KEYS = { lead: 'gainLead', bass: 'gainBass', pad: 'gainPad', piano: 'gainPiano', organ: 'gainOrgan', drum: 'gainDrum' };
const SYNTH_GAINS = { lead: 0.8, bass: 1.2, pad: 1, piano: 0.5, organ: 1, drum: 1.1 };
const synthParams = (instrument) => ({ instrument, ...Object.fromEntries(Object.entries(SYNTH_GAINS).map(([id, v]) => [KEYS[id], v])) });
// The sends' defaults as v0.8.9 resolves them (`git show v0.8.9:src/ui/state/engine-store.ts`
// INPUT_SEND_PARAMS; the echo's time is the lane delay's, `fx-metadata.ts` DELAY_PARAMS: index 1), what a
// v0.8.9 boot stored for an untouched send; the OTT's are the dev build's (v0.8.9 has none).
const DEFAULTS = {
  ott: { ottDepth: 0.5, ottTime: 0.5, ottUpward: 1, ottDownward: 1 },
  ring: { ringFreq: 440, ringLevel: 0.5 },
  echo: { echoTime: 1, echoFeedback: 0.4, echoLevel: 0.5 },
  reverb: { reverbLevel: 0.5 },
};
const send = (kind, on = false, params = {}) => ({ on, params: { ...DEFAULTS[kind], ...params }, defaults: { ...DEFAULTS[kind] } });
const quietSends = () => ({ ott: send('ott'), ring: send('ring'), echo: send('echo'), reverb: send('reverb') });
const slot = (over = {}) => ({ plugin: null, savedSource: null, defaultSynth: 'lead', inputGain: 1, ...over });
/** A snapshot: both slots on their defaults, every send off at its defaults, no pedal bound. */
function snap(over = {}) {
  return {
    slots: [slot(), slot({ defaultSynth: 'bass' })],
    guitarFirst: false,
    synthGains: SYNTH_GAINS,
    sends: quietSends(),
    bound: { ott: false, ring: false, echo: false, reverb: false },
    legacyMarker: false,
    reported: null,
    ...over,
  };
}
/** An injected mint: 101, 102, … in call order. */
function counter(start = 101) {
  let n = start;
  return () => n++;
}
const kinds = (units) => (units ?? []).map((u) => u.kind);
/** Every id in the record, units and pending of both slots: none twice. */
function assertIdsUnique(r) {
  const ids = r.slots.flatMap((s) => [...s.units, ...(s.pending ?? [])].map((u) => u.id));
  assert.equal(new Set(ids).size, ids.length, `ids unique across both slots: ${JSON.stringify(ids)}`);
}

// ── the upgrade ────────────────────────────────────────────────────────────────────────────────────────

check('a plugin slot found as an effect: a plugin unit with its name, volume 0.9; the other slot its default synth', () => {
  const r = upgradeRig(snap({ slots: [slot({ plugin: { ref: AMP, isEffect: true, stored: null } }), slot({ defaultSynth: 'bass' })] }), counter());
  assert.deepEqual(r.slots[0], { volume: 0.9, units: [{ id: 101, kind: 'plugin', plugin: AMP, bypass: false }] });
  assert.deepEqual(r.slots[1], { volume: 1, units: [{ id: 102, kind: 'synth', params: synthParams(1), bypass: false }] });
});

check('a plugin found as an instrument starts at 0.1; a stored level wins over either default', () => {
  const found = (isEffect, stored) => upgradeRig(snap({ slots: [slot({ plugin: { ref: AMP, isEffect, stored } }), slot()] }), counter()).slots[0].volume;
  assert.equal(found(false, null), 0.1);
  assert.equal(found(true, 0.4), 0.4);
  assert.equal(defaultPluginLevel(true), 0.9);
});

check('a plugin missing from the scan stays a plugin unit: its stored level, else 0.1 (decision f)', () => {
  const missing = (stored) => upgradeRig(snap({ slots: [slot({ plugin: { ref: AMP, isEffect: null, stored } }), slot()] }), counter()).slots[0];
  assert.deepEqual(kinds(missing(null).units), ['plugin']);
  assert.equal(missing(null).volume, 0.1);
  assert.equal(missing(0.7).volume, 0.7);
});

check('the legacy crash marker drops the legacy plugins: the slot gets its source (v0.8.9 would have forgotten)', () => {
  const r = upgradeRig(snap({ legacyMarker: true, slots: [slot({ plugin: { ref: AMP, isEffect: true, stored: null }, savedSource: 'off', inputGain: 0.6 }), slot()] }), counter());
  assert.deepEqual(r.slots[0], { volume: 0.6, units: [] });
});

check('a SYNTH from lf.slotSource: the instrument index in SYNTHS order and every instrument level, volume 1', () => {
  const r = upgradeRig(snap({ slots: [slot({ savedSource: 'pad' }), slot({ savedSource: 'drum', defaultSynth: 'bass' })] }), counter());
  assert.deepEqual(r.slots[0], { volume: 1, units: [{ id: 101, kind: 'synth', params: synthParams(2), bypass: false }] });
  assert.deepEqual(r.slots[1], { volume: 1, units: [{ id: 102, kind: 'synth', params: synthParams(5), bypass: false }] });
});

check('unset sources: Lead in A, Bass in B; an unknown stored source reads the default too', () => {
  const r = upgradeRig(snap({ slots: [slot({ savedSource: 'theremin' }), slot({ defaultSynth: 'bass' })] }), counter());
  assert.deepEqual(r.slots.map((s) => s.units[0].params.instrument), [0, 1]);
  assert.equal(storedSlotSource('theremin', false), null);
});

check('a guitar-first install: unset slots are Off, no unit, volume the input level', () => {
  const r = upgradeRig(snap({ guitarFirst: true, slots: [slot({ inputGain: 1.2 }), slot({ savedSource: 'organ' })] }), counter());
  assert.deepEqual(r.slots[0], { volume: 1.2, units: [] });
  assert.equal(r.slots[1].units[0].params.instrument, 4, 'a stored source still wins');
  assert.equal(storedSlotSource(null, true), 'unset');
});

check('an Off slot: no unit, volume lf.slotInputGain', () => {
  const r = upgradeRig(snap({ slots: [slot({ savedSource: 'off', inputGain: 1.3 }), slot({ savedSource: 'off', inputGain: 0 })] }), counter());
  assert.deepEqual(r.slots, [{ volume: 1.3, units: [] }, { volume: 0, units: [] }]);
});

check('every send off at its defaults and bound to no pedal: nothing pending', () => {
  const r = upgradeRig(snap({ slots: [slot({ savedSource: 'off' }), slot({ savedSource: 'off' })] }), counter());
  assert.equal('pending' in r.slots[0] || 'pending' in r.slots[1], false);
});

check('sends: on → on, off but changed → bypassed, off at defaults but bound → bypassed; RING, ECHO, REVERB order', () => {
  const sends = { ...quietSends(), echo: send('echo', true, { echoFeedback: 0.6 }), ring: send('ring', false, { ringFreq: 300 }), reverb: send('reverb') };
  const r = upgradeRig(snap({ sends, bound: { ott: false, ring: false, echo: false, reverb: true }, slots: [slot({ savedSource: 'off' }), slot({ plugin: { ref: AMP, isEffect: true, stored: null } })] }), counter());
  for (const s of r.slots) assert.deepEqual(kinds(s.pending), ['ring', 'echo', 'reverb']);
  const [ring, echo, reverb] = r.slots[0].pending;
  assert.deepEqual(echo, { id: 102, kind: 'echo', params: { echoTime: 1, echoFeedback: 0.6, echoLevel: 0.5 }, bypass: false });
  assert.deepEqual([ring.bypass, ring.params.ringFreq], [true, 300]);
  assert.equal(reverb.bypass, true);
  assertIdsUnique(r);
});

check('a v0.8.9 profile has no OTT keys: no OTT; one on, changed or bound comes first in pending', () => {
  // v0.8.9's defaults, an input-fed slot: only the ECHO that was on is pending, exactly
  const plain = upgradeRig(snap({ slots: [slot({ savedSource: 'off' }), slot({ savedSource: 'off' })], sends: { ...quietSends(), echo: send('echo', true) } }), counter());
  assert.deepEqual(plain.slots[0].pending, [{ id: 101, kind: 'echo', params: DEFAULTS.echo, bypass: false }]);
  assert.deepEqual(plain.slots[1].pending, [{ id: 102, kind: 'echo', params: DEFAULTS.echo, bypass: false }]);
  const on = upgradeRig(snap({ slots: [slot({ savedSource: 'off' }), slot({ savedSource: 'off' })], sends: { ...quietSends(), ott: send('ott', true), echo: send('echo', true) } }), counter());
  assert.deepEqual(kinds(on.slots[0].pending), ['ott', 'echo']);
  assert.equal(on.slots[0].pending[0].bypass, false);
  const changed = upgradeRig(snap({ slots: [slot({ savedSource: 'off' }), slot()], sends: { ...quietSends(), ott: send('ott', false, { ottDepth: 0.8 }) } }), counter());
  assert.deepEqual(changed.slots[0].pending.map((u) => [u.kind, u.bypass]), [['ott', true]]);
  const bound = upgradeRig(snap({ slots: [slot({ savedSource: 'off' }), slot()], bound: { ott: true, ring: false, echo: false, reverb: false } }), counter());
  assert.deepEqual(bound.slots[0].pending.map((u) => [u.kind, u.bypass]), [['ott', true]]);
});

check('a SYNTH-headed slot gets only the OTT pending; the input-fed slot all of them', () => {
  const sends = { ...quietSends(), ott: send('ott', true), echo: send('echo', true) };
  const r = upgradeRig(snap({ sends, slots: [slot({ savedSource: 'off' }), slot({ savedSource: 'bass' })] }), counter());
  assert.deepEqual(kinds(r.slots[0].pending), ['ott', 'echo']);
  assert.deepEqual(kinds(r.slots[1].pending), ['ott']);
});

check('no bindings report (null): every send counts as bound, all four migrate bypassed', () => {
  const r = upgradeRig(snap({ slots: [slot({ savedSource: 'off' }), slot({ savedSource: 'off' })], bound: boundKinds(null) }), counter());
  assert.deepEqual(r.slots[0].pending.map((u) => [u.kind, u.bypass]), [['ott', true], ['ring', true], ['echo', true], ['reverb', true]]);
});

check("boundKinds reads the InFx actions of the first bindings report", () => {
  const b = (action) => ({ binding: { action } });
  assert.deepEqual(boundKinds([b('inFxEcho'), b('playAll'), b('inFxRing')]), { ott: false, ring: true, echo: true, reverb: false });
  assert.deepEqual(boundKinds([b('inFxReverb')]), { ott: false, ring: false, echo: false, reverb: true });
  assert.deepEqual(boundKinds([b('inFxOtt')]), { ott: true, ring: false, echo: false, reverb: false });
  assert.deepEqual(boundKinds([]), { ott: false, ring: false, echo: false, reverb: false });
});

check('ids come from the mint: slot 0 units, its pending, then slot 1', () => {
  const sends = { ...quietSends(), echo: send('echo', true) };
  const r = upgradeRig(snap({ sends, slots: [slot({ plugin: { ref: AMP, isEffect: true, stored: null } }), slot({ savedSource: 'off' })] }), counter(7));
  assert.deepEqual([r.slots[0].units[0].id, r.slots[0].pending[0].id, r.slots[1].pending[0].id], [7, 8, 9]);
  assertIdsUnique(r);
  const all = upgradeRig(snap({ bound: boundKinds(null), slots: [slot({ plugin: { ref: AMP, isEffect: true, stored: null } }), slot({ savedSource: 'pad' })] }), counter());
  assertIdsUnique(all);
});

check('chains a DEV run reported before any record are adopted whole; the sends still go pending', () => {
  const reported = [[{ id: 55, kind: 'echo', params: DEFAULTS.echo, bypass: false }], [{ id: 66, kind: 'synth', params: { instrument: 3 }, bypass: false }]];
  const sends = { ...quietSends(), ott: send('ott', true), reverb: send('reverb', true) };
  const r = upgradeRig(snap({ reported, sends, slots: [slot({ plugin: { ref: AMP, isEffect: true, stored: 0.3 } }), slot()] }), counter());
  assert.deepEqual(r.slots[0].units, reported[0]);
  assert.deepEqual(r.slots[1].units, reported[1]);
  assert.deepEqual(r.slots.map((s) => s.volume), [1, 1], 'no slot gain reported: unity');
  assert.deepEqual([kinds(r.slots[0].pending), kinds(r.slots[1].pending)], [['ott', 'reverb'], ['ott']]);
  assertIdsUnique(r);
});

check('an adopted chain keeps the slot gain the reset frame replayed; one out of range or absent reads unity', () => {
  const reported = [[{ id: 55, kind: 'echo', params: DEFAULTS.echo, bypass: false }], []];
  const vol = (reportedVolumes) => upgradeRig(snap({ reported, reportedVolumes }), counter()).slots.map((s) => s.volume);
  assert.deepEqual(vol([0.4, null]), [0.4, 1]);
  assert.deepEqual(vol([9, 0]), [1, 0]);
  assert.deepEqual(upgradeRig(snap({ reportedVolumes: [0.4, 0.4], slots: [slot({ savedSource: 'off', inputGain: 1.2 }), slot()] }), counter()).slots[0].volume, 1.2,
    'without adopted chains the reported gain is not read');
});

// ── placement ──────────────────────────────────────────────────────────────────────────────────────────

const state = (id, over = {}) => ({ id, kind: 'plugin', plugin: AMP, bypass: false, source: false, name: 'Amp', latency: 0, state: 'seated', ...over });
const plug = { id: 9, kind: 'plugin', plugin: AMP, bypass: false };

check("headRole: none or a built-in effect → input, SYNTH → source, a plugin only once applied", () => {
  assert.equal(headRole([], []), 'input');
  assert.equal(headRole([{ id: 3, kind: 'ott' }], []), 'input');
  assert.equal(headRole([{ id: 3, kind: 'synth' }], []), 'source');
  assert.equal(headRole([plug], []), null, 'missing from the chain (not in the scan, add not reported)');
  assert.equal(headRole([plug], [state(9, { state: 'loading' })]), null);
  assert.equal(headRole([plug], [state(9, { state: 'waiting' })]), null);
  assert.equal(headRole([plug], [state(9, { state: 'failed', error: 'x' })]), null, 'a failed add is no role: its cause is unknown, the slot keeps its pending');
  assert.equal(headRole([plug], [state(9)]), 'input');
  assert.equal(headRole([plug], [state(9, { source: true })]), 'source');
  assert.equal(headRole([plug], [state(9, { state: 'failed', parked: true, source: true })]), 'source', 'parked keeps its seat');
});

check('placement: an input-fed chain takes every pending unit at its end; a source-headed one only an OTT that was on', () => {
  const pending = [{ id: 20, kind: 'ott', bypass: false }, { id: 21, kind: 'ring', bypass: true }, { id: 22, kind: 'echo', bypass: false }];
  const s = { volume: 0.9, units: [plug], pending };
  assert.deepEqual(withPlaced(s, 'input'), { volume: 0.9, units: [plug, ...pending] });
  assert.deepEqual(withPlaced(s, 'source'), { volume: 0.9, units: [plug, pending[0]] });
  assert.deepEqual(placed({ ...s, pending: [{ ...pending[0], bypass: true }, pending[2]] }, 'source'), [], 'a bypassed OTT is not placed on a source chain');
});

// ── the writer: a chains report into the record's units ────────────────────────────────────────────────

check('a report writes its units in its order, a failed add\'s plate kept (no cause named); only leaving is dropped', () => {
  const report = [
    state(1, { state: 'loading', name: 'Amp 2' }),
    { id: 2, kind: 'echo', params: DEFAULTS.echo, bypass: true, source: false, name: 'ECHO', latency: 0, state: 'waiting' },
    state(3, { state: 'leaving' }),
    state(4, { state: 'failed', error: 'no' }),
    state(5, { state: 'failed', parked: true, error: 'park' }),
  ];
  const out = unitsFromReport([], report, new Set());
  assert.deepEqual(out.map((u) => u.id), [1, 2, 4, 5]);
  assert.deepEqual(out[2], { id: 4, kind: 'plugin', plugin: { ...AMP }, bypass: false }, 'the failed plate keeps its id and plugin');
  assert.deepEqual(out[0], { id: 1, kind: 'plugin', plugin: { ...AMP, name: 'Amp 2' }, bypass: false });
  assert.deepEqual(out[1], { id: 2, kind: 'echo', params: DEFAULTS.echo, bypass: true });
});

check('a record unit no report named keeps its place (a missing plugin, an add on its way); one named before is removed', () => {
  const record = [plug, { id: 10, kind: 'echo', bypass: false }, { id: 11, kind: 'reverb', bypass: false }];
  const report = [{ id: 11, kind: 'reverb', bypass: false, source: false, name: 'REVERB', latency: 0, state: 'seated' }];
  assert.deepEqual(unitsFromReport(record, report, new Set([11])).map((u) => u.id), [9, 10, 11]);
  assert.deepEqual(unitsFromReport(record, report, new Set([10, 11])).map((u) => u.id), [9, 11]);
});

// ── the record's text ──────────────────────────────────────────────────────────────────────────────────

check('the record round-trips; a plugin keeps its name inside plugin; empty pending is not written', () => {
  const r = { version: 1, slots: [{ volume: 0.9, units: [plug], pending: [{ id: 4, kind: 'ott', params: DEFAULTS.ott, bypass: true }] }, { volume: 1, units: [], pending: [] }] };
  const text = serializeRig(r);
  assert.deepEqual(JSON.parse(text).slots[1], { volume: 1, units: [] });
  assert.deepEqual(parseRig(text), { ...r, slots: [r.slots[0], { volume: 1, units: [] }] });
  assert.equal(JSON.parse(text).slots[0].units[0].plugin.name, 'Amp');
});

check('parse is tolerant: unreadable → absent; a bad unit, an unknown param or a reused id is skipped; a bad volume reads 1', () => {
  for (const raw of [null, '', '{', '[]', '{"version":2,"slots":[{},{}]}', '{"version":1,"slots":[{}]}', '{"version":1,"slots":[1,2]}']) {
    assert.equal(parseRig(raw), null, String(raw));
  }
  const raw = JSON.stringify({ version: 1, slots: [
    { volume: 9, units: [{ id: 0, kind: 'ott' }, { id: 1, kind: 'flanger' }, { id: 2, kind: 'ott', params: { ottDepth: 0.2, wobble: 1, ottTime: 'x' } }, { id: 3, kind: 'plugin', plugin: { format: 'au', path: 'p', id: 'i' } }] },
    { volume: 0.5, units: [{ id: 2, kind: 'echo' }], pending: [{ id: 4, kind: 'ring', bypass: 'yes' }] },
  ] });
  assert.deepEqual(parseRig(raw), { version: 1, slots: [{ volume: 1, units: [{ id: 2, kind: 'ott', params: { ottDepth: 0.2 } }] }, { volume: 0.5, units: [] }] });
});

check("add's spec leaves the record's plugin name off the wire", () => {
  assert.deepEqual(addSpec({ ...plug, plugin: AMP }), { kind: 'plugin', plugin: { format: 'vst3', path: AMP.path, id: 'amp.id' }, bypass: false });
  assert.deepEqual(addSpec({ id: 5, kind: 'echo', params: { echoLevel: 1 }, bypass: true }), { kind: 'echo', params: { echoLevel: 1 }, bypass: true });
});

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

// verify/guards/rig-upgrade.mjs — the rig record's upgrade (src/ui/state/rig-record.ts, unit contract §6) in
// plain Node: a v0.8.9 profile's upgrade into the record the page imports, case by case (a plugin slot found
// or missing from the scan, an effect or not, a stored level; the legacy crash marker; a SYNTH from
// `lf.slotSource`; unset sources, the guitar-first install's included; an Off slot's input level; each send
// on, off, at its defaults or not, bound to a pedal or not, no bindings report at all; the OTT, which v0.8.9
// never stored, only when on, changed or bound; a SYNTH-headed slot's pending), ids from an injected mint;
// the text the import hands over; the fixture native shares (`verify/fixtures/rig-record.json`: a snapshot
// and the record its upgrade makes, ids minted from 1). The record's rules from then on (its parse, the
// placement of `pending`, the units from a report) are native's: `src-tauri/crates/lf-rig`'s tests.
// What it cannot see: the keys' resolvers (`instrument.ts`, `engine-store.ts`, `midi.ts`), the handover's
// order and answers: verify/probes/rig-recall.mjs's H cases and verify/probes/legacy-storage.mjs.
// Run: node verify/guards/rig-upgrade.mjs

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { boundKinds, defaultPluginLevel, serializeRig, storedSlotSource, upgradeRig } from '../../src/ui/state/rig-record.ts';

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

// ── the shared fixture (D12: the native record is `lf.rig` v1 unchanged) ─────────────────────────────

check('the fixture: upgradeRig(input) is its record, and serializeRig writes that value (the Rust half: lf-rig tests/fixture.rs)', () => {
  const { input, record } = JSON.parse(readFileSync(new URL('../fixtures/rig-record.json', import.meta.url), 'utf8'));
  const upgraded = upgradeRig(input, counter(1));
  assert.deepEqual(upgraded, record);
  assert.deepEqual(JSON.parse(serializeRig(upgraded)), record);
});

// ── the text the import hands over ─────────────────────────────────────────────────────────────────────

check('the text: a plugin keeps its name inside plugin; an empty pending is not written', () => {
  const plug = { id: 9, kind: 'plugin', plugin: AMP, bypass: false };
  const r = { version: 1, slots: [{ volume: 0.9, units: [plug], pending: [{ id: 4, kind: 'ott', params: DEFAULTS.ott, bypass: true }] }, { volume: 1, units: [], pending: [] }] };
  const text = JSON.parse(serializeRig(r));
  assert.deepEqual(text, { version: 1, slots: [r.slots[0], { volume: 1, units: [] }] });
  assert.equal(text.slots[0].units[0].plugin.name, 'Amp');
});

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

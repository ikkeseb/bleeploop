// verify/guards/midi-wire.mjs — the TS half of native MIDI's wire (`src/platform/midi-wire.ts`, mirroring
// the serde types of `src-tauri/src/engine_io/midi/mod.rs` and `store.rs`).
//
// `verify/fixtures/midi-wire.json` holds native MIDI's 25 action ids (`ActionId::ALL`), one example of
// every `MidiEvent` (every port state, binding state and origin, store problem, UI action and learn
// refusal), every `InputEvent` the UI sends and import reports, written from the Rust serde shapes. This
// guard runs the REAL TS decoders over it: every event parses, every field the Rust side writes is one the
// TS side reads (a field TS would ignore fails here), every variant is covered, each action id reads
// (`src/app/midi-actions.ts`'s typecheck holds them equal to `src/app/actions.ts`'s), the UI's input
// events are what the TS types produce, and a drifted name (a snake_case field, an unknown variant) is
// refused. It cannot see Tauri's IPC or serde itself: a Rust test over the same JSON holds the Rust half.
// Run: node verify/guards/midi-wire.mjs

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { decodeImportReport, decodeInputEvent, decodeMidiEvent, encodeInput, MIDI_ACTION_IDS } from '../../src/platform/midi-wire.ts';

const fixture = JSON.parse(readFileSync(new URL('../fixtures/midi-wire.json', import.meta.url), 'utf8'));

let passed = 0;
let failed = 0;
function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (err) {
    failed++;
    console.error(`FAIL ${name}: ${err.message}`);
  }
}

/** An externally tagged value's variant name and payload. */
const tag = (v) => (typeof v === 'string' ? [v, undefined] : Object.entries(v)[0]);
const sorted = (list) => [...new Set(list)].sort();

const EVENTS = ['ports', 'gone', 'bindings', 'learning', 'awaitingRelease', 'learned', 'refused', 'run', 'pressed', 'store', 'held'];
const INPUT_EVENTS = ['note', 'blur', 'selectTarget', 'allNotesOff'];

/** The decoded event back in the wire's shape. */
function rewire(ev) {
  const { type, ...fields } = ev;
  return type === 'pressed' ? 'pressed' : { [type]: fields };
}

// ── Events: every field the Rust side writes is read ────────────────────────────────────────────────
for (const e of fixture.midiEvents) {
  check(`event ${tag(e)[0]} reads every field`, () => assert.deepEqual(rewire(decodeMidiEvent(structuredClone(e))), e));
}
check('the fixture covers every event', () => assert.deepEqual(sorted(fixture.midiEvents.map((e) => tag(e)[0])), sorted(EVENTS)));
const listed = fixture.midiEvents.flatMap((e) => e.bindings?.bindings ?? []);
check('the fixture lists every binding state', () =>
  assert.deepEqual(sorted(listed.map((l) => l.state)), sorted(['live', 'blocked', 'noPort', 'severalPorts', 'severalAbsent'])),
);
check('the fixture lists both origins, both kinds and a HOLD', () => {
  assert.deepEqual(sorted(listed.map((l) => l.origin)), ['legacy', 'native']);
  assert.deepEqual(sorted(listed.map((l) => l.binding.kind)), ['cc', 'note']);
  assert.ok(listed.some((l) => l.binding.hold));
});
check('the fixture has every port state', () =>
  assert.deepEqual(sorted(fixture.midiEvents.flatMap((e) => e.ports?.ports ?? []).map((p) => p.state)), ['busy', 'closed', 'open']),
);
check('the fixture has every store problem', () =>
  assert.deepEqual(sorted(fixture.midiEvents.filter((e) => e.store).map((e) => tag(e.store.problem)[0])), sorted(['readOnly', 'conflict', 'failed', 'rejected'])),
);
check('the fixture runs every UI action', () =>
  assert.deepEqual(sorted(fixture.midiEvents.filter((e) => e.run).map((e) => e.run.action)), sorted(['goLive', 'stageView', 'stageNextView', 'tapTempo'])),
);
check('learning and awaitingRelease come set and null', () => {
  assert.ok(fixture.midiEvents.some((e) => e.learning?.learning === null) && fixture.midiEvents.some((e) => e.learning?.learning));
  assert.ok(fixture.midiEvents.some((e) => e.awaitingRelease?.binding === null) && fixture.midiEvents.some((e) => e.awaitingRelease?.binding));
});

// ── The action ids: native MIDI's `ActionId::ALL`, in order: the UI's (`decodeMidiEvent`'s list, which
// `src/app/midi-actions.ts`'s typecheck holds equal to `src/app/actions.ts`'s) ──────────────────────────
check('every native action id reads, in the picker\'s order', () => {
  const read = fixture.actionIds.map((action) => decodeMidiEvent({ learning: { learning: { action, target: null } } }).learning.action);
  assert.deepEqual(read, fixture.actionIds);
  assert.deepEqual([...MIDI_ACTION_IDS], fixture.actionIds, "the UI's ids, in its order");
  assert.equal(new Set(read).size, 25);
});

// ── Input events: the UI sends these; each reads as sent ────────────────────────────────────────────
/** What the UI's encoder (`index.ts` `input` calls it) makes for the event `e` names. */
function encodeLike(e) {
  const [name, p] = tag(e);
  if (name === 'note') return encodeInput.note(p.owner, p.note, p.velocity, p.on);
  if (name === 'selectTarget') return encodeInput.selectTarget(p.slot, p.target);
  return encodeInput[name]();
}
for (const e of fixture.inputEvents) {
  check(`input event ${JSON.stringify(e)} is what the UI sends`, () => assert.deepEqual(encodeLike(e), e));
  check(`input event ${JSON.stringify(e)} reads as sent`, () => assert.deepEqual(decodeInputEvent(structuredClone(e)), e));
}
for (const { sent, reads } of fixture.inputEventsAccepted) {
  check(`input event ${JSON.stringify(sent)} reads as ${JSON.stringify(reads)}`, () => assert.deepEqual(decodeInputEvent(structuredClone(sent)), reads));
}
check('the UI rounds and clamps a velocity to MIDI', () => {
  assert.equal(encodeInput.note('key:KeyA', 60, 99.6, true).note.velocity, 100);
  assert.equal(encodeInput.note('key:KeyA', 60, 140, true).note.velocity, 127);
});
check('the fixture covers every input event', () =>
  assert.deepEqual(sorted(fixture.inputEvents.map((e) => tag(e)[0])), sorted(INPUT_EVENTS)),
);
check('the fixture selects every note target, and no slot', () => {
  const picks = fixture.inputEvents.filter((e) => e.selectTarget).map((e) => e.selectTarget);
  assert.deepEqual(sorted(picks.map((p) => tag(p.target)[0])), ['Builtin', 'Off', 'Slot']);
  assert.ok(picks.some((p) => p.slot === null));
});

// ── Import reports ──────────────────────────────────────────────────────────────────────────────────
for (const r of fixture.importReports) {
  check(`import report ${JSON.stringify(r).slice(0, 60)}`, () => assert.deepEqual(decodeImportReport(structuredClone(r)), r));
}
check('the fixture has an import that ran, one already done and an unreadable one', () => {
  assert.ok(fixture.importReports.some((r) => r.already));
  assert.ok(fixture.importReports.some((r) => r.blocked.length && r.rejected.length && r.skipped.length && r.imported.length));
  assert.ok(fixture.importReports.some((r) => r.unreadable !== null));
});

// ── A drifted name is refused, not silently read ─────────────────────────────────────────────────────
const bindings = fixture.midiEvents.find((e) => e.bindings);
const refused = {
  'a snake_case binding field': () => {
    const e = structuredClone(bindings);
    const b = e.bindings.bindings[0].binding;
    b.port_id = b.portId;
    delete b.portId;
    decodeMidiEvent(e);
  },
  'a snake_case listed field': () => {
    const e = structuredClone(bindings);
    const l = e.bindings.bindings[0];
    l.display_name = l.displayName;
    delete l.displayName;
    decodeMidiEvent(e);
  },
  'a binding state in another case': () => {
    const e = structuredClone(bindings);
    e.bindings.bindings[2].state = 'no_port';
    decodeMidiEvent(e);
  },
  'an event in PascalCase': () => decodeMidiEvent({ Held: { notes: [], changes: 0 } }),
  'an unknown action': () => decodeMidiEvent({ run: { action: 'loopAll' } }),
  'a unit event with a payload': () => decodeMidiEvent({ pressed: {} }),
  'a store problem in snake_case': () => decodeMidiEvent({ store: { problem: { read_only: { why: 'x' } } } }),
  'a held note out of range': () => decodeMidiEvent({ held: { notes: [128], changes: 1 } }),
  'a velocity out of range': () => decodeInputEvent({ note: { owner: 'key:KeyA', note: 60, velocity: 128, on: true } }),
  'a fractional velocity': () => decodeInputEvent({ note: { owner: 'key:KeyA', note: 60, velocity: 0.8, on: true } }),
  'a note target in another case': () => decodeInputEvent({ selectTarget: { slot: 0, target: 'off' } }),
  'an import report in snake_case': () => decodeImportReport({ ...fixture.importReports[0], already: undefined, already_done: true }),
};
for (const [name, fn] of Object.entries(refused)) check(`refuses ${name}`, () => assert.throws(fn));

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

// verify/guards/engine-wire.mjs — the TS half of the engine wire's fixture check.
//
// `verify/fixtures/engine-wire.json` holds one example of every command, engine event and device event,
// plus device requests, statuses and feed frames; `src-tauri/src/engine_io/wire.rs`'s cargo test parses
// each entry and writes it back unchanged. This guard runs the REAL TS decoders
// (`src/platform/engine-wire.ts`) over the same entries: every entry parses, every field the Rust side
// writes is one the TS side reads (a field TS would ignore fails here), every variant is covered, and a
// drifted name (snake_case, a wrong case, an unknown variant) is refused. It cannot see Tauri's IPC or
// serde itself; the cargo test holds the Rust half.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  decodeCommand,
  decodeDeviceEvent,
  decodeDeviceRequest,
  decodeDeviceStatus,
  decodeEvent,
  decodeFeedFrame,
  decodeOpenError,
} from '../../src/platform/engine-wire.ts';

const fixture = JSON.parse(readFileSync(new URL('../fixtures/engine-wire.json', import.meta.url), 'utf8'));

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

/** The keys of a JSON object, sorted. */
const keys = (o) => Object.keys(o).sort();

const COMMANDS = [
  'RecDub', 'PlayStop', 'Stop', 'Undo', 'Reverse', 'Copy', 'Trim', 'Clear', 'PlayAll', 'StopAll', 'ClearAll', 'Action',
  'ActionOn', 'SelectTrack', 'SetBpm', 'SetMetronome', 'SetClickVolume', 'SetMasterVolume', 'SetMasterMute',
  'SetLoopEndStop', 'SetFadeBars', 'SetFixedLength', 'SetFixedBars', 'SetRetake', 'SetAutoRecord', 'SetAutoSensitivity',
  'SetVolume', 'SetMute', 'SetDubFeedback', 'SetFxParam', 'SetFxBypass', 'SelectInstrument', 'NoteOn', 'NoteOff', 'PitchBend', 'Modulation',
  'AllNotesOff', 'SetSlotLive', 'SetSlotGain', 'SetInstrumentGain', 'SetInputSend', 'SetInputSendParam', 'Press',
];
const ACTIONS = [
  'RecDub', 'PlayStop', 'Undo', 'Clear', 'NextTrack', 'PrevTrack', 'PlayAll', 'StopAll', 'Mute', 'Reverse', 'Copy', 'Halve',
  'Hold', 'Release', 'FadeAll',
];
const REFUSALS = [
  'Stopping', 'PlayFirst', 'Reversed', 'OtherRecording', 'Empty', 'NoUndo', 'NoClear', 'ConfirmClear', 'Capturing', 'NoTrim',
  'NoMute', 'NoReverse', 'NoCopy', 'NoFreeLane', 'Fading', 'NoFade',
];
const EVENTS = ['Lane', 'Transport', 'Beat', 'Selected', 'Refused', 'TakeRejected', 'PassDropped', 'Copied', 'Cleared', 'Muted'];
const DEVICE_EVENTS = ['Lost', 'Recovered', 'Fallback', 'ShareLost', 'EngineFaulted', 'LoopsDropped'];

// ── Commands: the TS side sends these; each fixture example is one the TS types accept as is ────────
for (const c of fixture.commands) {
  check(`command ${JSON.stringify(c)}`, () => assert.deepEqual(decodeCommand(structuredClone(c)), c));
}
check('the fixture covers every command the TS side can send', () =>
  assert.deepEqual([...new Set(fixture.commands.map((c) => tag(c)[0]))].sort(), [...COMMANDS].sort()),
);
check('the fixture sends every note target', () => {
  const targets = fixture.commands.filter((c) => c.SelectInstrument !== undefined).map((c) => tag(c.SelectInstrument)[0]);
  assert.deepEqual([...new Set(targets)].sort(), ['Builtin', 'Off', 'Slot']);
});
check('the fixture sends every hands-free action', () => {
  const actions = fixture.commands.map((c) => c.Action ?? c.ActionOn?.[1]).filter((a) => a !== undefined).map((a) => tag(a)[0]);
  assert.deepEqual([...new Set(actions)].sort(), [...ACTIONS].sort());
});

// ── Events: every field the Rust side writes is read ────────────────────────────────────────────────
/** The decoded event back in the wire's shape, from the fields the decoder produced. */
function rewire(ev) {
  const { type, ...fields } = ev;
  return { [type]: fields };
}
for (const e of fixture.events) {
  check(`event ${tag(e)[0]} reads every field`, () => assert.deepEqual(rewire(decodeEvent(structuredClone(e))), e));
}
check('the fixture covers every event', () =>
  assert.deepEqual([...new Set(fixture.events.map((e) => tag(e)[0]))].sort(), [...EVENTS].sort()),
);
check('the fixture answers every refusal', () => {
  const reasons = fixture.events.map((e) => e.Refused?.reason).filter((r) => r !== undefined);
  assert.deepEqual([...new Set(reasons)].sort(), [...REFUSALS].sort());
});

// ── Device events, requests, statuses ────────────────────────────────────────────────────────────────
for (const d of fixture.deviceEvents) {
  check(`device event ${tag(d)[0]}`, () => {
    const decoded = decodeDeviceEvent(structuredClone(d));
    const [name, payload] = tag(d);
    assert.equal(decoded.type, name);
    if (name === 'Recovered' || name === 'Fallback') assert.deepEqual(decoded.status, payload);
    else if (payload !== undefined) assert.deepEqual(keys(decoded).filter((k) => k !== 'type'), keys(payload));
  });
}
check('the fixture covers every device event', () =>
  assert.deepEqual([...new Set(fixture.deviceEvents.map((d) => tag(d)[0]))].sort(), [...DEVICE_EVENTS].sort()),
);
// ── Open errors: a refusal reads as one, with every field; a failure is its text ───────────────────
for (const e of fixture.openErrors) {
  check(`open error ${JSON.stringify(e)}`, () => {
    const decoded = decodeOpenError(structuredClone(e));
    if (typeof e === 'string') return assert.deepEqual(decoded, { type: 'Failed', text: e });
    const [name, payload] = tag(e);
    assert.deepEqual(rewire(decoded), { [name]: payload });
  });
}
check('the fixture has a refusal and a failure', () =>
  assert.deepEqual([...new Set(fixture.openErrors.map((e) => (typeof e === 'string' ? 'Failed' : tag(e)[0])))].sort(), ['Failed', 'RateChange']),
);
check('a refusal with a drifted field is refused', () =>
  assert.throws(() => decodeOpenError({ RateChange: { device: 'x', from_rate: 44100, to: 48000 } })),
);

for (const r of fixture.deviceRequests) {
  check(`device request ${JSON.stringify(r)}`, () => assert.deepEqual(decodeDeviceRequest(structuredClone(r)), r));
}
check('a device request with one channel for both slots reads as sent (the Rust side sets both)', () => {
  const one = { backend: 'Asio', input: null, output: null, inputChannel: 2, buffer: null };
  assert.deepEqual(decodeDeviceRequest(structuredClone(one)), one);
});
for (const s of fixture.deviceStatuses) {
  check(`device status ${s.backend}`, () => assert.deepEqual(decodeDeviceStatus(structuredClone(s)), s));
}

// ── Feed frames: the whole frame is read, including the three states of `status` ──────────────────
for (const f of fixture.feed) {
  check(`feed frame ${f.seq}`, () => {
    const decoded = decodeFeedFrame(structuredClone(f));
    assert.deepEqual(keys(decoded), keys(f), 'the same top-level fields');
    assert.equal(decoded.seq, f.seq);
    assert.equal(decoded.reset, f.reset);
    assert.deepEqual(decoded.events.map(rewire), f.events);
    assert.equal(decoded.device.length, f.device.length);
    if ('status' in f) assert.deepEqual(decoded.status, f.status);
    if ('settings' in f) assert.deepEqual(decoded.settings, f.settings);
    assert.deepEqual(decoded.anchor, f.anchor);
    assert.deepEqual(decoded.meter, f.meter);
    assert.deepEqual(decoded.peaks, f.peaks);
  });
}
check('the fixture has a reset frame with remembered settings', () =>
  assert.ok(fixture.feed.some((f) => f.reset && Array.isArray(f.settings) && f.settings.length > 0)),
);
check('the fixture has a device that opened without input', () => {
  const statuses = [
    ...fixture.deviceStatuses,
    ...fixture.feed.map((f) => f.status).filter(Boolean),
    ...fixture.deviceEvents.map((d) => d.Recovered ?? d.Fallback).filter(Boolean),
  ];
  assert.ok(statuses.some((s) => s.inputOpen === false));
});
check('the fixture has a status that is set, null and absent', () => {
  const statuses = fixture.feed.map((f) => ('status' in f ? (f.status === null ? 'null' : 'set') : 'absent'));
  assert.deepEqual([...new Set(statuses)].sort(), ['absent', 'null', 'set']);
});

// ── A drifted name is refused, not silently read ─────────────────────────────────────────────────────
const lane = fixture.events.find((e) => tag(e)[0] === 'Lane');
const refused = {
  'a snake_case field': () => {
    const e = structuredClone(lane);
    e.Lane.info.auto_armed = e.Lane.info.autoArmed;
    delete e.Lane.info.autoArmed;
    decodeEvent(e);
  },
  'a lane state in another case': () => {
    const e = structuredClone(lane);
    e.Lane.info.state = 'PLAYING';
    decodeEvent(e);
  },
  'an unknown event': () => decodeEvent({ Tempo: { frame: 0 } }),
  'a PascalCase FX param': () => decodeCommand({ SetFxParam: [0, 'Cutoff', 1] }),
  'an instrument by its Rust name': () => decodeCommand({ SelectInstrument: { Builtin: 'drums' } }),
  'an input send by its Rust name': () => decodeCommand({ SetInputSend: ['Echo', true] }),
  'a snake_case input send param': () => decodeCommand({ SetInputSendParam: ['echo_level', 0.5] }),
  'a lane past the fifth': () => decodeCommand({ RecDub: 5 }),
  'a trim of no bars': () => decodeCommand({ Trim: [0, 0] }),
  'a trim of a fractional bar count': () => decodeCommand({ Trim: [0, 1.5] }),
  'a fade of a fractional bar count': () => decodeCommand({ SetFadeBars: 2.5 }),
  'a lane info without fading': () => {
    const e = structuredClone(lane);
    delete e.Lane.info.fading;
    decodeEvent(e);
  },
  'an unknown command': () => decodeCommand('Panic'),
  'an unknown note target': () => decodeCommand({ SelectInstrument: 'None' }),
  'an Off target with a payload': () => decodeCommand({ SelectInstrument: { Off: 0 } }),
  'an instrument level by the Rust name': () => decodeCommand({ SetInstrumentGain: ['Pad', 0.5] }),
  'a device request with a third slot': () =>
    decodeDeviceRequest({ backend: 'Asio', input: null, output: null, inputChannels: [0, 1, 2], buffer: null }),
  'an unknown action': () => decodeCommand({ Action: 'Panic' }),
  'a HOLD without its control': () => decodeCommand({ Action: 'Hold' }),
  'a HOLD release past a u8 control': () => decodeCommand({ ActionOn: [0, { Release: 256 }] }),
  'a unit action with a payload': () => decodeCommand({ Action: { FadeAll: 1 } }),
  'a press with a payload': () => decodeCommand({ Press: 0 }),
  'a feed frame without its events': () => decodeFeedFrame({ seq: 0, reset: false }),
  'a status without inputOpen': () => {
    const s = structuredClone(fixture.deviceStatuses[0]);
    delete s.inputOpen;
    decodeDeviceStatus(s);
  },
  "a status with a third slot's channel": () => {
    const s = structuredClone(fixture.deviceStatuses[0]);
    s.inputChannels = [0, 1, 2];
    decodeDeviceStatus(s);
  },
  'an anchor without its grid': () => {
    const f = structuredClone(fixture.feed.find((x) => x.anchor));
    delete f.anchor.grid;
    decodeFeedFrame(f);
  },
};
for (const [name, fn] of Object.entries(refused)) check(`refuses ${name}`, () => assert.throws(fn));

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

// verify/guards/engine-wire.mjs — the TS half of the engine wire's fixture check, and the session
// bytes' TS codec (`engine_snapshot`'s stems and an export's master: `src-tauri/src/engine_io/session.rs`
// owns the layout; its engine_io tests read the Rust half).
//
// `verify/fixtures/engine-wire.json` holds one example of every command, engine event and device event,
// plus device requests, statuses, feed frames, snapshot headers and load headers (each track's mix in
// session.json's track shape); `src-tauri/src/engine_io/wire.rs`'s cargo test parses each entry and writes it back
// unchanged. This guard runs the REAL TS decoders
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
  decodeLoadSession,
  decodeOpenError,
  decodeSnapshot,
  encodeSessionBytes,
  splitSessionBytes,
} from '../../src/platform/engine-wire.ts';
import { FX_PARAM_RANGES, defaultLaneMix, fakeMixModel } from '../../src/platform/host.web.ts';
import { FX_PARAM_DEFS, defaultFxStates, validateFxStates } from '../../src/ui/state/fx-metadata.ts';

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
  const one = { backend: 'Asio', input: null, output: null, inputChannel: 2, buffer: null, sampleRate: null };
  assert.deepEqual(decodeDeviceRequest(structuredClone(one)), one);
});
check('a device request from before the rate pick asks for the device rate (null, as Rust reads it)', () => {
  const old = { backend: 'Wasapi', input: null, output: null, inputChannels: [null, 1], buffer: 256 };
  assert.deepEqual(decodeDeviceRequest(structuredClone(old)), { ...old, sampleRate: null });
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
  'a device request with a fractional rate': () =>
    decodeDeviceRequest({ backend: 'Asio', input: null, output: null, inputChannels: [0, 1], buffer: null, sampleRate: 44100.5 }),
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

// ── Session bytes: a snapshot's stems, an export's master after them, or the render's error ──────────
// The fixture's snapshot headers: every field the Rust side writes is read, each track's mix included.
for (const entry of fixture.snapshotHeaders) {
  check(`snapshot header ${JSON.stringify(entry).slice(0, 60)}… reads every field`, () => {
    const frames = entry.masterLengthFrames;
    const stems = entry.tracks.map(() => new Float32Array(frames));
    const master = entry.master ? { left: new Float32Array(frames), right: new Float32Array(frames) } : undefined;
    const { header } = decodeSnapshot(encodeSessionBytes(structuredClone(entry), stems, master).buffer);
    assert.deepEqual(header, entry);
    for (const t of header.tracks) validateFxStates(t.mix.fx, `track ${t.index}`);
  });
}
check('the fixture has a snapshot with its master and one with its error', () => {
  assert.ok(fixture.snapshotHeaders.some((h) => h.master) && fixture.snapshotHeaders.some((h) => h.masterError));
});
check("the engine's default mix (the fixture's, written by Rust) is the UI's and the browser fake's", () => {
  const written = fixture.snapshotHeaders[0].tracks[1].mix;
  assert.deepEqual(written.fx, defaultFxStates());
  assert.deepEqual({ ...written, volume: 1 }, defaultLaneMix());
});
check("the browser fake's FX ranges are the UI's (the engine's)", () => {
  const ui = Object.fromEntries(Object.values(FX_PARAM_DEFS).flat().map((d) => [d.key, { min: d.min, max: d.max, integer: d.integer === true }]));
  assert.deepEqual(FX_PARAM_RANGES, ui);
});
// The fake's mix model: a reset frame starts it from the defaults, then the commands and events below.
{
  const frame = (events, extra = {}) => fakeMixModel.frame({ reset: false, events, ...extra });
  check("the browser fake's COPY gives the destination the source's mix as the COPY latched it", () => {
    frame([], { reset: true, settings: [] });
    fakeMixModel.command({ SetVolume: [0, 0.5] });
    fakeMixModel.command({ SetFxBypass: [0, 'delay', false] });
    fakeMixModel.command({ Copy: 0 });
    fakeMixModel.command({ SetVolume: [0, 0.2] });
    fakeMixModel.command({ SetFxBypass: [0, 'delay', true] });
    frame([{ type: 'Copied', frame: 0, from: 0, to: 1, feedback: 0.7 }]);
    const copied = fakeMixModel.lane(1);
    assert.equal(copied.volume, 0.5);
    assert.equal(copied.fx[3].bypassed, false);
    assert.equal(copied.dubFeedback, 0.7, "the DUB FEEDBACK the engine says it copied");
    assert.equal(fakeMixModel.lane(0).volume, 0.2, 'the source keeps its move');
  });
  check("the browser fake stores a mix value as the engine clamps and rounds it", () => {
    frame([], { reset: true, settings: [] });
    fakeMixModel.command({ SetVolume: [0, 4] });
    fakeMixModel.command({ SetDubFeedback: [0, -1] });
    fakeMixModel.command({ SetFxParam: [0, 'semitones', 0.5] });
    fakeMixModel.command({ SetFxParam: [0, 'time', 7] });
    fakeMixModel.command({ SetFxParam: [0, 'feedback', 1] });
    const m = fakeMixModel.lane(0);
    assert.deepEqual([m.volume, m.dubFeedback, m.fx[1].params.semitones, m.fx[3].params.time, m.fx[3].params.feedback], [1.5, 0, 1, 3, 0.95]);
    validateFxStates(m.fx, 'the clamped mix');
  });
}
{
  const mixed = fixture.snapshotHeaders[0];
  const track = (edit) => {
    const h = structuredClone(mixed);
    edit(h.tracks[0]);
    return () => decodeSnapshot(encodeSessionBytes(h, h.tracks.map(() => new Float32Array(h.masterLengthFrames))).buffer);
  };
  const refusedMix = {
    'a track without its mix': track((t) => delete t.mix),
    'a mix with an unknown field': track((t) => (t.mix.pan = 0)),
    'a mix of four effects': track((t) => t.mix.fx.pop()),
    'an effect missing a param': track((t) => delete t.mix.fx[3].params.mix),
    'an effect with another kind\'s param': track((t) => (t.mix.fx[0].params = { cutoff: 800, amount: 1 })),
    'a param that is no number': track((t) => (t.mix.fx[1].params.semitones = '-5')),
    'a mute that is no boolean': track((t) => (t.mix.muted = 1)),
  };
  for (const [name, fn] of Object.entries(refusedMix)) check(`refuses ${name}`, () => assert.throws(fn));
}
// The fixture's load headers (the UI writes them, the Rust host reads them): every field is read, each
// track's mix included, and the fake's load sets each loaded lane's mix over what was sent before.
for (const entry of fixture.loadHeaders) {
  check(`load header ${JSON.stringify(entry).slice(0, 60)}… reads every field`, () => {
    const pcm = entry.tracks.map(() => new Float32Array(entry.masterLengthFrames));
    const { header } = decodeLoadSession(encodeSessionBytes(structuredClone(entry), pcm).buffer);
    assert.deepEqual(header, entry);
    for (const t of header.tracks) validateFxStates(t.mix.fx, `track ${t.index}`);
  });
}
check("the browser fake's load sets each loaded lane's mix, clamped as the engine's", () => {
  fakeMixModel.frame({ reset: true, settings: [], events: [] });
  fakeMixModel.command({ SetVolume: [0, 0.9] });
  fakeMixModel.command({ SetFxBypass: [0, 'delay', false] });
  const header = structuredClone(fixture.loadHeaders[0]);
  header.tracks[0].mix.volume = 4;
  fakeMixModel.load(header);
  assert.deepEqual(fakeMixModel.lane(0), { ...fixture.loadHeaders[0].tracks[0].mix, volume: 1.5 });
  assert.deepEqual(fakeMixModel.lane(2), fixture.loadHeaders[0].tracks[1].mix);
});
{
  const loaded = fixture.loadHeaders[0];
  const track = (edit) => {
    const h = structuredClone(loaded);
    edit(h.tracks[0], h);
    return () => decodeLoadSession(encodeSessionBytes(h, h.tracks.map(() => new Float32Array(h.masterLengthFrames))).buffer);
  };
  const refusedLoad = {
    'a load track without its mix': track((t) => delete t.mix),
    'a load mix of four effects': track((t) => t.mix.fx.pop()),
    'a load mix with an unknown field': track((t) => (t.mix.pan = 0)),
    'a load track that overdubs': track((t) => (t.state = 'Overdubbing')),
    'a load track with an unknown field': track((t) => (t.gain = 1)),
    'a load track listed twice': track((t, h) => h.tracks.push(structuredClone(t))),
    'a load header with an unknown field': track((_, h) => (h.rate = 48000)),
  };
  for (const [name, fn] of Object.entries(refusedLoad)) check(`refuses ${name}`, () => assert.throws(fn));
}
{
  const mix = defaultLaneMix();
  const header = (extra = {}) => ({
    rate: 48000,
    masterLengthFrames: 4,
    bpm: 120,
    tracks: [
      { index: 0, frames: 4, reversed: false, state: 'Playing', mix },
      { index: 3, frames: 4, reversed: true, state: 'Stopped', mix },
    ],
    ...extra,
  });
  const stems = [Float32Array.from([0.1, 0.2, 0.3, 0.4]), Float32Array.from([-1, 0, 1, 2])];
  const master = { left: Float32Array.from([1, 2, 3, 4]), right: Float32Array.from([5, 6, 7, 8]) };
  const bytes = (h, m) => encodeSessionBytes(h, stems, m).buffer;
  check('a snapshot without a master decodes to its stems', () => {
    const s = decodeSnapshot(bytes(header()));
    assert.deepEqual(s.pcm.map((b) => Array.from(b)), stems.map((b) => Array.from(b)));
    assert.equal(s.master, null);
    assert.deepEqual(Object.keys(s.header).sort(), ['bpm', 'masterLengthFrames', 'rate', 'tracks']);
  });
  check("an export's snapshot carries its master after the stems, left then right", () => {
    const raw = bytes(header({ master: { frames: 4 } }), master);
    const s = decodeSnapshot(raw);
    assert.deepEqual(s.pcm.map((b) => Array.from(b)), stems.map((b) => Array.from(b)));
    assert.deepEqual([Array.from(s.master.left), Array.from(s.master.right)], [[1, 2, 3, 4], [5, 6, 7, 8]]);
    assert.deepEqual(s.header.master, { frames: 4 });
    const tail = new Float32Array(raw.slice(raw.byteLength - 32));
    assert.deepEqual(Array.from(tail), [1, 2, 3, 4, 5, 6, 7, 8], 'the master is the last 2 × frames samples');
    assert.equal(splitSessionBytes(raw).master.left.length, 4);
  });
  check("a failed render's snapshot carries its error and the stems alone", () => {
    const s = decodeSnapshot(bytes(header({ masterError: 'export render: no' })));
    assert.equal(s.master, null);
    assert.equal(s.header.masterError, 'export render: no');
    assert.equal(s.pcm.length, 2);
  });
  const refusedSession = {
    'a master the bytes do not hold': () => {
      const raw = bytes(header({ master: { frames: 4 } }), master);
      decodeSnapshot(raw.slice(0, raw.byteLength - 4));
    },
    'master PCM the header does not name': () => {
      const raw = new Uint8Array(bytes(header({ master: { frames: 4 } }), master));
      const json = new TextEncoder().encode(JSON.stringify(header()));
      const out = new Uint8Array(4 + json.length + (raw.byteLength - 4 - new DataView(raw.buffer).getUint32(0, true)));
      new DataView(out.buffer).setUint32(0, json.length, true);
      out.set(json, 4);
      out.set(raw.subarray(raw.byteLength - (out.length - 4 - json.length)), 4 + json.length);
      decodeSnapshot(out.buffer);
    },
    'a master that is not one loop long': () => decodeSnapshot(encodeSessionBytes(header({ master: { frames: 3 } }), stems, { left: new Float32Array(3), right: new Float32Array(3) }).buffer),
    'a master and its error at once': () => decodeSnapshot(bytes(header({ master: { frames: 4 }, masterError: 'x' }), master)),
    'a non-string master error': () => decodeSnapshot(bytes(header({ masterError: 7 }))),
    'an encode with master PCM and no master in the header': () => encodeSessionBytes(header(), stems, master),
  };
  for (const [name, fn] of Object.entries(refusedSession)) check(`refuses ${name}`, () => assert.throws(fn));
}

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

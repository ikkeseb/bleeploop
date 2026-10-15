// verify/guards/import.mjs — deterministic guard for src/session/session-schema.ts (validateSession,
// and validateSessionPlugins: the slot tones engine mode's export names).
// Imports the REAL session validator from the PURE schema module (Node TS type-stripping — import.ts
// itself statically imports engine/looper and is browser-only) so it cannot drift from the source.
// Asserts the export.ts session.json schema gate: a golden session round-trips, the formatVersion gate
// (missing = legacy v1, ===1 accepted, newer rejected) and the meter it carries (v1 is 4/4, v2 names an
// offered meter in this build's beats; an archive `prepareStemArchive` writes for each offered meter reads
// back with its bars, as export and recovery write it), every malformed shape throws descriptively, and
// volume clamp + legacy muted/state values normalize (a missing dubFeedback reads as 1, a missing pan as
// the centre), while invalid shapes reject. This is what SESSION IMPORT relies on to never hand looper.loadSession a payload that
// could corrupt the master grid.
// Run: node verify/guards/import.mjs
import { validateSession as validateWith, validateSessionPlugins } from '../../src/session/session-schema.ts';
import { prepareStemArchive, snapshotBars } from '../../src/session/stem-archive.ts';
import { TIME_SIGNATURES } from '../../src/platform/engine-wire.ts';
import { defaultFxStates } from '../../src/ui/state/fx-metadata.ts';
import { framesPerBar } from '../../src/ui/state/quantize.ts';

/** The validator as import.ts calls it, with the meters this build offers. */
const validateSession = (json) => validateWith(json, TIME_SIGNATURES);

let fails = 0,
  checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) {
    fails++;
    console.log(`  FAIL  ${name}  ${detail}`);
  }
}
/** Assert fn throws, optionally requiring `msgPart` in the error message. */
function throws(name, fn, msgPart = '') {
  checks++;
  try {
    fn();
    fails++;
    console.log(`  FAIL  ${name}  (did not throw)`);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    if (msgPart && !msg.includes(msgPart)) {
      fails++;
      console.log(`  FAIL  ${name}  (threw, but "${msg}" lacks "${msgPart}")`);
    }
  }
}

const MASTER = 176400; // 2 bars @ 120bpm, 44.1k — an exact whole-bar integer frame count
const fx5 = () =>
  [
    { bypassed: true, params: { cutoff: 1200, q: 2 } },
    { bypassed: true, params: { semitones: 0 } },
    { bypassed: false, params: { rate: 1 } },
    { bypassed: true, params: { time: 1, feedback: 0.4, mix: 0.3 } },
    { bypassed: true, params: { amount: 0.3 } },
  ];
/** A fresh golden session (the exact shape export.ts writes) — mutate per case. */
const golden = () => ({
  app: 'BleepLoop',
  exported: '2026-07-10T12:00:00.000Z',
  bpm: 120,
  bars: 2,
  masterLengthFrames: MASTER,
  sampleRate: 44100,
  tracks: [
    { track: 1, file: 'lf-track1.wav', volume: 1, muted: false, reversed: true, frames: MASTER, fx: fx5() },
    { track: 3, file: 'lf-track3.wav', volume: 0.5, muted: true, reversed: false, frames: MASTER, fx: fx5() },
  ],
  master: { file: 'lf-master.wav', kind: 'wet-engine', level: 0.75 },
});

/** Rewrite the three grid fields + per-track frame declarations as one internally-consistent grid. */
function setGrid(session, { bpm = session.bpm, bars = session.bars, sampleRate = session.sampleRate } = {}) {
  session.bpm = bpm;
  session.bars = bars;
  session.sampleRate = sampleRate;
  session.masterLengthFrames = bars * framesPerBar(bpm, sampleRate);
  for (const track of session.tracks) track.frames = session.masterLengthFrames;
  return session;
}

// ---- A. golden session passes + round-trips every consumed field ----
{
  const out = validateSession(golden());
  ok('A.bpm round-trips', out.bpm === 120, String(out.bpm));
  ok('A.bars round-trips', out.bars === 2, String(out.bars));
  ok('A.masterLengthFrames round-trips', out.masterLengthFrames === MASTER, String(out.masterLengthFrames));
  ok('A.sampleRate round-trips', out.sampleRate === 44100, String(out.sampleRate));
  ok('A.two tracks', out.tracks.length === 2, String(out.tracks.length));
  ok('A.track numbers round-trip', out.tracks[0].track === 1 && out.tracks[1].track === 3);
  ok('A.files round-trip', out.tracks[0].file === 'lf-track1.wav' && out.tracks[1].file === 'lf-track3.wav');
  ok('A.volume round-trips (in range)', out.tracks[0].volume === 1 && out.tracks[1].volume === 0.5);
  ok('A.muted round-trips', out.tracks[0].muted === false && out.tracks[1].muted === true);
  ok('A.reversed round-trips', out.tracks[0].reversed === true && out.tracks[1].reversed === false);
  ok('A.frames round-trip', out.tracks.every((t) => t.frames === MASTER));
  ok('A.fx is 5 entries each', out.tracks.every((t) => t.fx.length === 5));
  ok('A.fx values round-trip', out.tracks[0].fx[0].params.cutoff === 1200 && out.tracks[0].fx[2].bypassed === false);
  // Normalization returns fresh objects — mutating the output must not touch the input (deep copy).
  const input = golden();
  const parsed = validateSession(input);
  parsed.tracks[0].fx[0].params.cutoff = 999;
  ok('A.fx params deep-copied', input.tracks[0].fx[0].params.cutoff === 1200);
  ok('A.export shape with master.level validates', out.tracks.length === 2);
}

// ---- A2. per-track playback state is preserved and legacy capture state normalizes safely ----
// Missing state is a pre-state archive and therefore PLAYING. OVERDUBBING archives contain only the
// last committed loop, so they resume as PLAYING rather than reviving an impossible capture state.
{
  const s = golden();
  s.tracks[0].state = 'PLAYING';
  s.tracks[1].state = 'STOPPED';
  const out = validateSession(s);
  ok('A2.PLAYING state round-trips', out.tracks[0].state === 'PLAYING', out.tracks[0].state);
  ok('A2.STOPPED state round-trips', out.tracks[1].state === 'STOPPED', out.tracks[1].state);
  ok('A2.missing legacy state defaults PLAYING', validateSession(golden()).tracks.every((track) => track.state === 'PLAYING'));
  const overdubbing = golden();
  overdubbing.tracks[0].state = 'OVERDUBBING';
  ok('A2.OVERDUBBING normalizes PLAYING', validateSession(overdubbing).tracks[0].state === 'PLAYING');
}
for (const state of ['RECORDING', 'EMPTY', 'paused', 1, null]) {
  throws(`A2.invalid state ${JSON.stringify(state)} rejects`, () => {
    const s = golden();
    s.tracks[0].state = state;
    validateSession(s);
  }, 'state must be PLAYING, OVERDUBBING or STOPPED');
}

// ---- A3. formatVersion gate: missing = legacy v1, ===1 accepted, integer > 1 rejected, garbage rejected ----
{
  const legacy = golden(); // no formatVersion/reversed — every pre-versioning export still imports as v1
  for (const track of legacy.tracks) delete track.reversed;
  const legacyOut = validateSession(legacy);
  ok('A3.missing formatVersion accepts (legacy v1)', legacyOut.tracks.length === 2, String(legacyOut.tracks.length));
  ok('A3.missing reversed defaults false', legacyOut.tracks.every((track) => track.reversed === false));
  ok('A3.formatVersion 1 accepts', validateSession({ ...golden(), formatVersion: 1 }).bpm === 120);
}
throws('A3.formatVersion 3 rejects with newer-version message',
  () => validateSession({ ...golden(), formatVersion: 3 }), 'newer version of BleepLoop');
throws('A3.formatVersion 99 rejects with newer-version message',
  () => validateSession({ ...golden(), formatVersion: 99 }), 'newer version of BleepLoop');
throws('A3.non-integer formatVersion rejected as invalid',
  () => validateSession({ ...golden(), formatVersion: 1.5 }), 'positive integer');
throws('A3.zero formatVersion rejected as invalid',
  () => validateSession({ ...golden(), formatVersion: 0 }), 'positive integer');
throws('A3.string formatVersion rejected as invalid',
  () => validateSession({ ...golden(), formatVersion: 'one' }), 'positive integer');

// ---- A4. the meter: v1 is 4/4, v2 names an offered meter in this build's beats, the bars count in it ----
const meterOf = (num, den) => TIME_SIGNATURES.find((m) => m.num === num && m.den === den);
/** `ok` on what `json` validates to, a refusal failing the check instead of the guard. */
function reads(name, json, cond) {
  let out;
  try {
    out = validateSession(json);
  } catch (e) {
    return ok(name, false, e instanceof Error ? e.message : String(e));
  }
  ok(name, cond(out), JSON.stringify([out.meter, out.bars, out.masterLengthFrames]));
}
/** A v2 session in `meter` (a wire `TimeSignature`), its grid `bars` of it at 120 BPM / 48 kHz. */
function inMeter(meter, bars = 3) {
  const s = golden();
  s.formatVersion = 2;
  s.meter = { ...meter };
  s.bpm = 120;
  s.bars = bars;
  s.sampleRate = 48000;
  s.masterLengthFrames = bars * framesPerBar(120, 48000, meter.beats);
  for (const track of s.tracks) track.frames = s.masterLengthFrames;
  return s;
}
reads('A4.v1 without a meter reads 4/4', golden(), (out) => out.meter === meterOf(4, 4));
reads('A4.v1 naming 4/4 reads 4/4', { ...golden(), formatVersion: 1, meter: { num: 4, den: 4, beats: 4 } }, (out) => out.meter === meterOf(4, 4));
for (const m of TIME_SIGNATURES) {
  reads(`A4.v2 ${m.num}/${m.den} reads its meter and 3 bars of it`, inMeter(m), (out) => out.meter === m && out.bars === 3);
}
// 2 bars of 3/4 at 120 / 48 kHz are 144000 frames: not 2 bars of 4/4 (192000), and 6/8's 2 bars at a dotted
// 120 are one 4/4 bar: the frames check reads the meter's bars.
reads('A4.the frames check counts the meter (3/4)', inMeter(meterOf(3, 4), 2), (out) => out.masterLengthFrames === 144000);
reads('A4.the frames check counts the meter (6/8)', inMeter(meterOf(6, 8), 2), (out) => out.masterLengthFrames === 96000);
throws('A4.v2 without a meter rejects', () => {
  const s = inMeter(meterOf(6, 8));
  delete s.meter;
  validateSession(s);
}, 'has none');
throws('A4.v1 carrying 3/4 rejects', () => validateSession({ ...inMeter(meterOf(3, 4)), formatVersion: 1 }), 'format v1 session is 4/4');
throws('A4.missing formatVersion carrying 6/8 rejects', () => {
  const s = inMeter(meterOf(6, 8));
  delete s.formatVersion;
  validateSession(s);
}, 'format v1 session is 4/4');
throws('A4.6/8 counted in another beat rejects, naming the reading', () => validateSession({ ...inMeter(meterOf(6, 8)), meter: { num: 6, den: 8, beats: 6 } }), 'counts 6/8 in 6 beats a bar');
throws('A4.a meter with no beats rejects', () => validateSession({ ...inMeter(meterOf(3, 4)), meter: { num: 3, den: 4 } }), 'counts 3/4 in undefined');
throws('A4.an unoffered meter rejects', () => validateSession({ ...inMeter(meterOf(4, 4)), meter: { num: 7, den: 8, beats: 7 } }), 'not a time signature this build offers');
throws('A4.a meter that is not an object rejects', () => validateSession({ ...inMeter(meterOf(4, 4)), meter: '6/8' }), 'meter must be an object');
throws('A4.v2 6/8 bars that are 4/4 bars reject', () => {
  const s = inMeter(meterOf(6, 8), 2);
  s.masterLengthFrames = 2 * framesPerBar(120, 48000);
  for (const track of s.tracks) track.frames = s.masterLengthFrames;
  validateSession(s);
}, 'grid expects 96000 frames');

// ---- A5. what export and recovery write reads back: every offered meter, odd bar counts ----
// `prepareStemArchive` is the session.json both write (recovery in its worker); `snapshotBars` their bars.
for (const meter of TIME_SIGNATURES) {
  for (const bars of [1, 3, 5]) {
    const rate = 48000;
    const bpm = 97;
    const frames = bars * framesPerBar(bpm, rate, meter.beats);
    const snap = { sampleRate: rate, masterLengthFrames: frames, bpm, meter, tracks: [
      { index: 0, pcm: new Float32Array(frames), volume: 1, muted: false, reversed: false, state: 'PLAYING', fx: defaultFxStates() },
    ] };
    const name = `A5.${meter.num}/${meter.den} x${bars}`;
    ok(`${name} snapshotBars counts the meter`, snapshotBars(snap) === bars, String(snapshotBars(snap)));
    const { session } = prepareStemArchive(snap, { bpm, bars: snapshotBars(snap) }, 'lf', 'float32');
    const fourFour = meter.num === 4 && meter.den === 4;
    ok(`${name} writes v${fourFour ? 1 : 2}`, session.formatVersion === (fourFour ? 1 : 2), String(session.formatVersion));
    ok(`${name} names the meter only off 4/4`, fourFour ? !('meter' in session) : JSON.stringify(session.meter) === JSON.stringify({ num: meter.num, den: meter.den, beats: meter.beats }), JSON.stringify(session.meter));
    reads(`${name} reads back`, JSON.parse(JSON.stringify(session)), (out) => out.meter === meter && out.bars === bars && out.masterLengthFrames === frames);
  }
}

// ---- B. app tag gate ----
throws('B.wrong app tag', () => validateSession({ ...golden(), app: 'NotBleepLoop' }), 'app tag');
ok('B.the earlier app name still imports', validateSession({ ...golden(), app: 'LoopForge' }).tracks.length > 0);
throws('B.missing app tag', () => {
  const s = golden();
  delete s.app;
  validateSession(s);
}, 'app tag');
throws('B.not an object', () => validateSession('BleepLoop'), 'not a JSON object');
throws('B.null', () => validateSession(null), 'not a JSON object');

// ---- C. masterLengthFrames: missing / mis-typed / non-integer / non-positive ----
throws('C.missing masterLengthFrames', () => {
  const s = golden();
  delete s.masterLengthFrames;
  validateSession(s);
}, 'masterLengthFrames');
throws('C.string masterLengthFrames', () => validateSession({ ...golden(), masterLengthFrames: '176400' }), 'masterLengthFrames');
throws('C.float masterLengthFrames', () => validateSession({ ...golden(), masterLengthFrames: 176400.5 }), 'positive integer');
throws('C.zero masterLengthFrames', () => validateSession({ ...golden(), masterLengthFrames: 0 }), 'positive integer');
throws('C.negative masterLengthFrames', () => validateSession({ ...golden(), masterLengthFrames: -1 }), 'positive integer');
throws('C.missing bpm', () => {
  const s = golden();
  delete s.bpm;
  validateSession(s);
}, 'bpm');
throws('C.missing sampleRate', () => {
  const s = golden();
  delete s.sampleRate;
  validateSession(s);
}, 'sampleRate');
throws('C.missing bars', () => {
  const s = golden();
  delete s.bars;
  validateSession(s);
}, 'bars');
throws('C.zero bars', () => validateSession({ ...golden(), bars: 0 }), 'positive integer');
throws('C.float bars', () => validateSession({ ...golden(), bars: 1.5 }), 'positive integer');
throws('C.zero sampleRate', () => validateSession({ ...golden(), sampleRate: 0 }), 'positive integer');
throws('C.float sampleRate', () => validateSession({ ...golden(), sampleRate: 44100.5 }), 'positive integer');
throws('C.bpm contradicts bars/master frames', () => {
  const s = golden();
  s.bpm = 121;
  validateSession(s);
}, 'grid expects');
throws('C.bars contradict bpm/master frames', () => {
  const s = golden();
  s.bars = 3;
  validateSession(s);
}, 'grid expects');
throws('C.sampleRate contradicts bpm/master frames', () => {
  const s = golden();
  s.sampleRate = 48000;
  validateSession(s);
}, 'grid expects');

// ---- D. track list shape: empty / >5 / duplicate numbers / out-of-range numbers ----
throws('D.empty tracks', () => validateSession({ ...golden(), tracks: [] }), 'need 1..5');
throws('D.tracks not an array', () => validateSession({ ...golden(), tracks: 'nope' }), 'tracks');
throws('D.six tracks', () => {
  const s = golden();
  s.tracks = [1, 2, 3, 4, 5, 6].map((n) => ({ track: n, file: `t${n}.wav`, volume: 1, muted: false, frames: MASTER, fx: fx5() }));
  validateSession(s);
}, 'need 1..5');
throws('D.duplicate track numbers', () => {
  const s = golden();
  s.tracks[1].track = 1;
  validateSession(s);
}, 'duplicate track number');
throws('D.duplicate stem files', () => {
  const s = golden();
  s.tracks[1].file = s.tracks[0].file;
  validateSession(s);
}, 'multiple tracks reference');
throws('D.track 6', () => {
  const s = golden();
  s.tracks[1].track = 6;
  validateSession(s);
}, 'integer 1..5');
throws('D.track 0', () => {
  const s = golden();
  s.tracks[0].track = 0;
  validateSession(s);
}, 'integer 1..5');
throws('D.float track number', () => {
  const s = golden();
  s.tracks[0].track = 1.5;
  validateSession(s);
}, 'integer 1..5');

// ---- E. per-track frames must equal masterLengthFrames ----
throws('E.frames mismatch', () => {
  const s = golden();
  s.tracks[1].frames = MASTER - 1;
  validateSession(s);
}, 'masterLengthFrames');
throws('E.frames missing', () => {
  const s = golden();
  delete s.tracks[0].frames;
  validateSession(s);
}, 'frames');
throws('E.file missing', () => {
  const s = golden();
  delete s.tracks[0].file;
  validateSession(s);
}, 'file');

// ---- F. fx: exactly 5 entries of { bypassed: boolean, params: object } ----
throws('F.4-entry fx', () => {
  const s = golden();
  s.tracks[0].fx = fx5().slice(0, 4);
  validateSession(s);
}, 'exactly 5');
throws('F.6-entry fx', () => {
  const s = golden();
  s.tracks[0].fx = [...fx5(), { bypassed: true, params: {} }];
  validateSession(s);
}, 'exactly 5');
throws('F.fx missing', () => {
  const s = golden();
  delete s.tracks[0].fx;
  validateSession(s);
}, 'exactly 5');
throws('F.fx bypassed not boolean', () => {
  const s = golden();
  s.tracks[0].fx[2].bypassed = 'yes';
  validateSession(s);
}, 'bypassed');
throws('F.fx params not object', () => {
  const s = golden();
  s.tracks[0].fx[3].params = 42;
  validateSession(s);
}, 'params');
throws('F.fx params array', () => {
  const s = golden();
  s.tracks[0].fx[3].params = [1, 2];
  validateSession(s);
}, 'params');
// Param VALUES are validated too (review fix): a non-finite or non-number value would otherwise
// blow up inside loadSession's mutation loop and leave a half-loaded state.
throws('F.fx param value Infinity', () => {
  const s = golden();
  s.tracks[0].fx[0].params.cutoff = Infinity; // what JSON.parse gives for a hand-edited 1e999
  validateSession(s);
}, 'finite number');
throws('F.fx param value string', () => {
  const s = golden();
  s.tracks[0].fx[0].params.cutoff = 'hello';
  validateSession(s);
}, 'finite number');
throws('F.fx param value null', () => {
  const s = golden();
  s.tracks[0].fx[2].params.rate = null;
  validateSession(s);
}, 'finite number');
throws('F.fx missing exact param key', () => {
  const s = golden();
  delete s.tracks[0].fx[0].params.q;
  validateSession(s);
}, 'missing "q"');
throws('F.fx unknown param key', () => {
  const s = golden();
  s.tracks[0].fx[4].params.decay = 2.6;
  validateSession(s);
}, 'unknown key "decay"');

for (const [name, fxIndex, key, value, message] of [
  ['cutoff below range', 0, 'cutoff', -1, 'outside 120..14000'],
  ['q below range', 0, 'q', -50, 'outside 0.1..14'],
  ['pitch above range', 1, 'semitones', 13, 'outside -12..12'],
  ['pitch non-integer', 1, 'semitones', 0.5, 'must be an integer'],
  ['stutter choice non-integer', 2, 'rate', 1.5, 'must be an integer'],
  ['delay choice above range', 3, 'time', 4, 'outside 0..3'],
  ['delay feedback above range', 3, 'feedback', 0.96, 'outside 0..0.95'],
  ['delay mix below range', 3, 'mix', -0.01, 'outside 0..1'],
  ['reverb amount above range', 4, 'amount', 1.01, 'outside 0..1'],
]) {
  throws(`F.fx ${name}`, () => {
    const s = golden();
    s.tracks[0].fx[fxIndex].params[key] = value;
    validateSession(s);
  }, message);
}

// ---- F2. bpm range: mirror the clock's [40,300] integer contract (review fix) ----
// A clamped/rounded bpm would recover the WRONG bar count from the frame math with no error.
throws('F2.bpm 350 rejected', () => {
  const s = golden();
  s.bpm = 350;
  validateSession(s);
}, '40..300');
throws('F2.bpm 30 rejected', () => {
  const s = golden();
  s.bpm = 30;
  validateSession(s);
}, '40..300');
throws('F2.bpm non-integer rejected', () => {
  const s = golden();
  s.bpm = 120.5;
  validateSession(s);
}, '40..300');
{
  const s = setGrid(golden(), { bpm: 40 });
  ok('F2.bpm 40 (edge) passes', validateSession(s).bpm === 40);
  const s2 = setGrid(golden(), { bpm: 300 });
  ok('F2.bpm 300 (edge) passes', validateSession(s2).bpm === 300);
}

// ---- G. normalization: volume clamps; muted accepts boolean, legacy 0/1, missing only ----
{
  const s = golden();
  s.tracks[0].volume = 9;
  s.tracks[1].volume = -2;
  const out = validateSession(s);
  ok('G.volume 9 clamps to 1.5', out.tracks[0].volume === 1.5, String(out.tracks[0].volume));
  ok('G.volume -2 clamps to 0', out.tracks[1].volume === 0, String(out.tracks[1].volume));
}
throws('G.volume mis-typed still rejects', () => {
  const s = golden();
  s.tracks[0].volume = 'loud';
  validateSession(s);
}, 'volume');
{
  const s = golden();
  s.tracks[0].muted = 1;
  s.tracks[1].muted = 0;
  const out = validateSession(s);
  ok('G.muted 1 coerces to true', out.tracks[0].muted === true, String(out.tracks[0].muted));
  ok('G.muted 0 coerces to false', out.tracks[1].muted === false, String(out.tracks[1].muted));
  const s2 = golden();
  delete s2.tracks[0].muted;
  ok('G.muted missing coerces to false', validateSession(s2).tracks[0].muted === false);
}
throws('G.muted string false rejects', () => {
  const s = golden();
  s.tracks[0].muted = 'false';
  validateSession(s);
}, 'boolean or 0/1');
throws('G.muted numeric 2 rejects', () => {
  const s = golden();
  s.tracks[0].muted = 2;
  validateSession(s);
}, 'boolean or 0/1');
throws('G.reversed non-boolean rejects', () => {
  const s = golden();
  s.tracks[0].reversed = 1;
  validateSession(s);
}, 'reversed must be a boolean');

// ── H: the slot tones (`plugins`) ───────────────────────────────────────────────────────────────────
const amp = { slot: 'A', format: 'vst3', path: 'C:\\VST3\\Amp.vst3', id: 'abc', name: 'Amp', file: 'lf-tone-slot-a.bin' };
const withPlugins = (plugins) => ({ ...golden(), plugins });
const trackFiles = golden().tracks.map((t) => t.file);
ok('H.no plugins field: no tones (older exports, recoveries)', validateSessionPlugins(golden(), trackFiles).length === 0);
{
  const parsed = validateSessionPlugins(withPlugins([amp, { ...amp, slot: 'B', format: 'clap', file: 'lf-tone-slot-b.bin' }]), trackFiles);
  ok('H.two slots parse, A → 0 and B → 1', parsed.length === 2 && parsed[0].slot === 0 && parsed[1].slot === 1 && parsed[1].format === 'clap');
  ok('H.the identity and file come through', parsed[0].path === amp.path && parsed[0].id === 'abc' && parsed[0].name === 'Amp' && parsed[0].file === amp.file);
}
ok('H.validateSession ignores plugins (an older build imports the loops)', validateSession(withPlugins([amp])).tracks.length === 2);
throws('H.plugins not an array rejects', () => validateSessionPlugins(withPlugins({}), trackFiles), 'must be an array');
throws('H.a slot other than A/B rejects', () => validateSessionPlugins(withPlugins([{ ...amp, slot: 0 }]), trackFiles), '"A" or "B"');
throws('H.a slot listed twice rejects', () => validateSessionPlugins(withPlugins([amp, { ...amp, file: 'x.bin' }]), trackFiles), 'listed twice');
{
  const parsed = validateSessionPlugins(withPlugins([{ ...amp, format: 'vst2', path: 'C:\\VST2\\Amp.dll' }]), trackFiles);
  ok('H.a vst2 plugin parses', parsed.length === 1 && parsed[0].format === 'vst2' && parsed[0].path === 'C:\\VST2\\Amp.dll');
}
throws('H.an unknown format rejects', () => validateSessionPlugins(withPlugins([{ ...amp, format: 'au' }]), trackFiles), '"clap", "vst3" or "vst2"');
throws('H.vst2 spelled in capitals is not a format', () => validateSessionPlugins(withPlugins([{ ...amp, format: 'VST2' }]), trackFiles), '"clap", "vst3" or "vst2"');
throws('H.a missing file rejects', () => validateSessionPlugins(withPlugins([{ ...amp, file: undefined }]), trackFiles), 'file missing');
throws('H.an empty id rejects', () => validateSessionPlugins(withPlugins([{ ...amp, id: '' }]), trackFiles), 'id missing');
throws('H.a tone file that is a stem rejects', () => validateSessionPlugins(withPlugins([{ ...amp, file: 'lf-track1.wav' }]), trackFiles), 'already another entry');
throws('H.two slots sharing a tone file reject', () =>
  validateSessionPlugins(withPlugins([amp, { ...amp, slot: 'B' }]), trackFiles), 'already another entry');

// ── I: DUB FEEDBACK (engine mode's lane setting): missing = 1 (every earlier export), clamps to 0..1 ──
{
  const out = validateSession(golden());
  ok('I.missing dubFeedback reads as 1 (a plain sum)', out.tracks.every((t) => t.dubFeedback === 1));
  const s = golden();
  s.tracks[0].dubFeedback = 0.4;
  s.tracks[1].dubFeedback = 0;
  const kept = validateSession(s);
  ok('I.dubFeedback round-trips', kept.tracks[0].dubFeedback === 0.4 && kept.tracks[1].dubFeedback === 0);
  s.tracks[0].dubFeedback = 3;
  s.tracks[1].dubFeedback = -1;
  const clamped = validateSession(s);
  ok('I.dubFeedback clamps to 0..1', clamped.tracks[0].dubFeedback === 1 && clamped.tracks[1].dubFeedback === 0);
}
throws('I.dubFeedback mis-typed rejects', () => {
  const s = golden();
  s.tracks[0].dubFeedback = 'half';
  validateSession(s);
}, 'dubFeedback must be a number');

// ── J: pan (engine mode's lane setting, formatVersion 1 unchanged): missing = 0 (every export before it,
// the centre), clamps to -1..1, a non-number rejects ──
{
  const out = validateSession(golden());
  ok('J.missing pan reads as 0 (the centre: a legacy session imports centred)', out.tracks.every((t) => Object.is(t.pan, 0)), JSON.stringify(out.tracks.map((t) => t.pan)));
  const s = golden();
  s.tracks[0].pan = -0.3;
  s.tracks[1].pan = 1;
  const kept = validateSession(s);
  ok('J.pan round-trips', kept.tracks[0].pan === -0.3 && kept.tracks[1].pan === 1, JSON.stringify(kept.tracks.map((t) => t.pan)));
  s.tracks[0].pan = -4;
  s.tracks[1].pan = 2.5;
  const clamped = validateSession(s);
  ok('J.pan clamps to -1..1', clamped.tracks[0].pan === -1 && clamped.tracks[1].pan === 1, JSON.stringify(clamped.tracks.map((t) => t.pan)));
  s.tracks[0].pan = -0;
  ok('J.a pan of -0 reads as the centre, 0', Object.is(validateSession(s).tracks[0].pan, 0));
}
for (const [what, bad] of [['a string', '-0.3'], ['null', null], ['a boolean', true], ['an object', { pan: 0 }]]) {
  throws(`J.pan as ${what} rejects`, () => {
    const s = golden();
    s.tracks[1].pan = bad;
    validateSession(s);
  }, 'pan must be a number');
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);

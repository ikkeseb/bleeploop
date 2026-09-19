// verify/fs-import-verify.mjs — deterministic guard for src/audio/export/session-schema.ts (validateSession).
// Imports the REAL session validator from the PURE schema module (Node TS type-stripping — import.ts
// itself statically imports engine/looper and is browser-only) so it cannot drift from the source.
// Asserts the export.ts session.json schema gate: a golden session round-trips, the formatVersion gate
// (missing = legacy v1, ===1 accepted, newer rejected), every malformed shape throws descriptively, and
// volume clamp + legacy muted values normalize, while invalid muted shapes reject. This is what
// SESSION IMPORT relies on to never hand looper.loadSession a payload that could corrupt the master grid.
// Run: node verify/fs-import-verify.mjs
import { validateSession } from '../src/audio/export/session-schema.ts';
import { framesPerBar } from '../src/audio/quantize.ts';

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
  master: { file: 'lf-master.wav', kind: 'wet-v1', level: 0.75 },
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

// ---- A2. the advisory per-track `state` (export.ts writes it) is TOLERATED, not required ----
// export.ts records PLAYING/OVERDUBBING/STOPPED per track so the file is self-describing about which
// stems fed the master; validateSession must ignore it (loadSession plays every track) AND still accept
// a pre-`state` export. The golden above (no `state`) already proves not-required; this proves tolerated.
{
  const s = golden();
  s.tracks[0].state = 'PLAYING';
  s.tracks[1].state = 'STOPPED';
  const out = validateSession(s);
  ok('A2.session with per-track state still validates', out.tracks.length === 2, String(out.tracks.length));
  ok('A2.state is ignored (not surfaced on the parsed track)', out.tracks[0].state === undefined, JSON.stringify(out.tracks[0].state));
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
throws('A3.formatVersion 2 rejects with newer-version message',
  () => validateSession({ ...golden(), formatVersion: 2 }), 'newer version of BleepLoop');
throws('A3.formatVersion 99 rejects with newer-version message',
  () => validateSession({ ...golden(), formatVersion: 99 }), 'newer version of BleepLoop');
throws('A3.non-integer formatVersion rejected as invalid',
  () => validateSession({ ...golden(), formatVersion: 1.5 }), 'positive integer');
throws('A3.zero formatVersion rejected as invalid',
  () => validateSession({ ...golden(), formatVersion: 0 }), 'positive integer');
throws('A3.string formatVersion rejected as invalid',
  () => validateSession({ ...golden(), formatVersion: 'one' }), 'positive integer');

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

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);

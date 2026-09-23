// The first-track COUNT-IN: the REAL looper (src/audio/looper/{machine,capture}.ts), capture worklet
// and clock under the verify rig. The record tap carries a MARKER level during the count and a
// different level from the counted downbeat on, so any count audio that leaks into the take shows.
//
// What this PROVES (the analog feel is owed a by-ear check on the PC):
//   - the press arms the take exactly one bar + the scheduling lead ahead:
//     recording.pendingStartFrame = round((HBL + COUNT_IN_BEATS*beatPeriod)*sr)            [startRecording]
//   - the count is discarded frame-exactly and the take starts at frame 0 on the downbeat, however the
//     drain batches straddle it (steady drain, ragged stalls, one batch spanning count + take) [consume]
//   - a take shorter than one bar keeps its content from frame 0 and pads the rest of the bar
//   - the 4 count clicks sound with the metronome OFF; the come-in "1" is on the LED but silent; with
//     the metronome ON every beat clicks and the loop "1" is accented                       [pulseTick]
//   - stop DURING the count aborts to EMPTY, discards the pre-roll, resets the arm and hands the
//     pulse back to free-run; a later-track arm abort leaves the master pulse alone         [stop]
//   - a fast abort -> re-record never swallows the new count "1" (anti-flam reset)           [startCountIn]

import { bootLooper } from '../harness/rig.ts';
import { framesPerBar } from '../../src/audio/quantize.ts';
import { COUNT_IN_BEATS, HEARTBEAT_INTERNAL_LATENCY as HBL, countInArm } from '../../src/audio/looper/grid-math.ts';

let fails = 0, checks = 0;
const approx = (a, b, eps) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }
const FLT = 1e-6; // Float32 storage: 0.7 -> 0.69999998
const MARKER = 0.5, TAKE = 0.7;

/** Press REC on lane 0 and switch the record tap from MARKER to TAKE at the armed start frame. */
async function pressWithMarker(rig) {
  rig.setInput(MARKER);
  await rig.looper.recDub(0);
  const start = rig.state.engineState.recording?.startFrame ?? null;
  rig.setInput((frame) => (frame < start ? MARKER : TAKE));
  return start;
}
function leaks(record, from, to) {
  for (let k = from; k < to; k++) if (approx(record[k], MARKER, FLT)) return k;
  return -1;
}

console.log('=== A. The press arms one bar + the lead ahead: pending = round((HBL + N*beatPeriod)*sr) ===');
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 90, 137, 100, 200, 73.5]) {
    const { beatPeriod, recordStart, anchor, pendingFrames } = countInArm(1000, bpm, sr);
    const expect = Math.round((HBL + COUNT_IN_BEATS * beatPeriod) * sr);
    ok(`A countInArm pending exact bpm=${bpm} sr=${sr}`, pendingFrames === expect, `${pendingFrames} vs ${expect}`);
    ok(`A recordStart = anchor+4beats bpm=${bpm} sr=${sr}`, approx(recordStart - anchor, COUNT_IN_BEATS * beatPeriod, 1e-12));
  }
}
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 137, 73]) {
    const rig = await bootLooper({ sampleRate: sr, startTime: 7.3 });
    rig.clock.setBpm(bpm);
    const now = rig.now();
    const mark = rig.draws().length;
    await rig.looper.recDub(0);
    const es = rig.state.engineState;
    const beat = 60 / bpm;
    const expect = Math.round((HBL + COUNT_IN_BEATS * beat) * sr);
    ok(`A looper arms pending exactly bpm=${bpm} sr=${sr}`, (es.recording?.pendingStartFrame ?? 0) === expect, `${es.recording?.pendingStartFrame ?? 0} vs ${expect}`);
    ok(`A take starts on the counted downbeat bpm=${bpm} sr=${sr}`,
      (es.recording?.startFrame ?? null) === Math.round((now + HBL + COUNT_IN_BEATS * beat) * sr), `start=${es.recording?.startFrame ?? null}`);
    const count = rig.draws().slice(mark)[0];
    ok(`A count "1" at now + HBL bpm=${bpm} sr=${sr}`, count && approx(count.time, now + HBL, 1e-12) && count.countLeft === 4, JSON.stringify(count));
  }
}

console.log('=== B. NO dead air: the count is discarded, the take starts at frame 0 (frame-exact) ===');
const regimes = {
  'steady 25 ms drain': async (rig) => { await rig.advance(2.02 + 4 + 0.05); },
  'ragged stalls': async (rig) => {
    const end = rig.now() + 2.02 + 4 + 0.05;
    const stalls = [0.37, 0.011, 0.9, 0.003, 1.2, 0.05, 0.6, 1.9];
    for (let i = 0; rig.now() < end; i++) {
      await rig.stall(Math.min(stalls[i % stalls.length], end - rig.now()));
      await rig.advance(0.013);
    }
  },
  'one batch spanning count + take': async (rig) => { await rig.stall(2.02 + 4 + 0.05); },
};
for (const [label, run] of Object.entries(regimes)) {
  const rig = await bootLooper();
  const start = await pressWithMarker(rig);
  await run(rig);
  await rig.looper.recDub(0); // close the take
  await rig.advance(0.3);
  const t = rig.tracks[0];
  const master = rig.looper.masterLengthFrames();
  ok(`B committed exactly 2 bars [${label}]`, master === 2 * framesPerBar(120, rig.sr), `master=${master}`);
  ok(`B armed cleared, pending consumed [${label}]`, t.armed === false && (rig.state.engineState.recording?.pendingStartFrame ?? 0) === 0);
  ok(`B frame 0 is the TAKE, not the count [${label}]`, approx(t.record[0], TAKE, FLT), `record[0]=${t.record[0]}`);
  ok(`B last loop frame is the take [${label}]`, approx(t.record[master - 1], TAKE, FLT));
  ok(`B no count-bar leak anywhere in the loop [${label}]`, leaks(t.record, 0, master) === -1, `leak at ${leaks(t.record, 0, master)}`);
  const periods = (start / rig.sr - rig.state.engineState.masterStartTime) / (master / rig.sr);
  ok(`B the loop grid is anchored on the armed downbeat [${label}]`, approx(periods, Math.round(periods), 1e-6), `periods=${periods}`);
}

console.log('=== B2. A take shorter than one bar keeps its content from frame 0 ===');
{
  const rig = await bootLooper();
  await pressWithMarker(rig);
  const fpb = framesPerBar(120, rig.sr);
  await rig.advance(2.02 + 0.5); // a quarter of a bar past the downbeat
  await rig.looper.recDub(0);
  await rig.advance(2.5);
  const t = rig.tracks[0];
  const master = rig.looper.masterLengthFrames();
  const played = t.record.findIndex((v) => v === 0);
  ok('B2 pads up to the one-bar minimum', master === fpb, `master=${master}`);
  ok('B2 short take frame 0 is content', approx(t.record[0], TAKE, FLT));
  ok('B2 the retained part is a quarter bar of take, then silence', approx(played, fpb / 4, 256), `content ends at ${played}`);
  ok('B2 no count leak', leaks(t.record, 0, master) === -1);
}

console.log('=== C. Count clicks FORCED audible (metronome off); accent on every bar "1" ===');
for (const metronome of [false, true]) {
  const rig = await bootLooper();
  rig.clock.setMetronome(metronome);
  const clickMark = rig.clicks().length, drawMark = rig.draws().length;
  rig.setInput(TAKE);
  await rig.looper.recDub(0);
  await rig.advance(2.02 + 2.2); // the count and the first bar of the take
  const beats = rig.draws().slice(drawMark);
  const anchor = beats[0].time;
  const clicks = rig.clicks().slice(clickMark).filter((c) => c.audible);
  const clickAt = (n) => clicks.find((c) => approx(c.time, anchor + n * 0.5, 1e-9));
  const tag = metronome ? 'metronome on' : 'metronome off';
  for (let n = 0; n < COUNT_IN_BEATS; n++) {
    ok(`C count beat ${n} clicks (${tag})`, !!clickAt(n));
    ok(`C count beat ${n} accent==${n === 0} (${tag})`, clickAt(n)?.accent === (n === 0));
  }
  ok(`C come-in beat is the bar "1" on the LED (${tag})`, beats[4]?.beat === 0 && beats[4]?.countLeft === 0);
  if (!metronome) {
    ok('C come-in beat silent with the metronome off', !clickAt(4));
    ok('C beat 5 silent with the metronome off', !clickAt(5));
  } else {
    ok('C metronome on: come-in "1" clicks, accented', clickAt(4)?.accent === true);
    ok('C metronome on: every take beat clicks, accent on the bar "1"', [5, 6, 7, 8].every((n) => clickAt(n)?.accent === (n % 4 === 0)));
  }
}

console.log('=== D. Stop DURING the count aborts to EMPTY (no dead silent loop) ===');
{
  const rig = await bootLooper();
  await pressWithMarker(rig);
  await rig.advance(1.0); // pre-downbeat frames drained (and discarded)
  const t = rig.tracks[0];
  ok('D still armed before the come-in', t.armed === true && t.writeHead === 0);
  const es = rig.state.engineState;
  ok('D the arm countdown reads the frames still to come', (es.recording?.pendingStartFrame ?? 0) === (es.recording?.startFrame ?? null) - es.captureFrontierFrame &&
    (es.recording?.pendingStartFrame ?? 0) > 0, `pending=${es.recording?.pendingStartFrame ?? 0} left=${(es.recording?.startFrame ?? null) - es.captureFrontierFrame}`);
  const mark = rig.draws().length;
  rig.looper.stop(0);
  ok('D aborts to EMPTY (no committed loop)', t.state === 'EMPTY' && t.lengthFrames === 0 && rig.looper.masterLengthFrames() === 0);
  ok('D pending reset', (es.recording?.pendingStartFrame ?? 0) === 0);
  ok('D recorder released', es.recording === null);
  ok('D record buffer silent (no dead loop committed)', t.record.every((x) => x === 0));
  await rig.advance(2);
  const after = rig.draws().slice(mark);
  ok('D count pulse torn down: no numerals, free-run cadence',
    after.length >= 3 && after.every((b) => b.countLeft === 0) && after.every((b, i) => i === 0 || approx(b.time - after[i - 1].time, 0.5, 1e-9)));
}

console.log('=== E. A later-track arm abort leaves the master pulse alone ===');
{
  const rig = await bootLooper();
  const master = await rig.recordFirstTake({ bars: 2 });
  const anchor = rig.state.engineState.masterStartTime;
  const beat = master / rig.sr / 8;
  await rig.looper.recDub(1);
  ok('E later lane armed', rig.tracks[1].armed === true && rig.tracks[1].state === 'RECORDING');
  const mark = rig.draws().length;
  rig.looper.stop(1);
  await rig.advance(2);
  const after = rig.draws().slice(mark);
  ok('E later abort to EMPTY', rig.tracks[1].state === 'EMPTY' && (rig.state.engineState.recording?.pendingStartFrame ?? 0) === 0);
  ok('E master pulse still on the loop grid', after.length >= 3 && after.every((b) => approx((b.time - anchor) / beat, Math.round((b.time - anchor) / beat), 1e-6)));
}

console.log('=== F. Anti-flam: a fast abort -> re-record never swallows the new count "1" ===');
{
  const rig = await bootLooper();
  const clickMark = rig.clicks().length;
  await rig.looper.recDub(0); // count "1" dispatched at now + 20 ms
  await rig.advance(0.01);
  await rig.looper.recDub(0); // abort 10 ms later
  await rig.advance(0.02);
  const mark = rig.draws().length;
  await rig.looper.recDub(0); // re-record: its "1" lands within 0.12 s of the aborted one
  const newOne = rig.draws().slice(mark).find((b) => b.countLeft === 4);
  await rig.advance(2.1);
  const clicks = rig.clicks().slice(clickMark).filter((c) => c.audible);
  ok('F precondition: the new "1" is within 0.12 s of the aborted "1"', newOne && newOne.time - clicks[0].time < 0.12);
  ok('F the re-record count "1" sounds', clicks.some((c) => c.time === newOne?.time && c.accent), JSON.stringify(clicks.map((c) => c.time)));
  ok('F the re-record count is complete (4 beats)', [0, 1, 2, 3].every((n) => clicks.some((c) => approx(c.time, newOne.time + n * 0.5, 1e-9))));
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

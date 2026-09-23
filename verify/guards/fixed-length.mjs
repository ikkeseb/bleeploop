// FIXED next-take length: the REAL looper (machine.ts configureRecordingEnd / stopCapture / finishCapture,
// capture.ts consume) under the verify rig. The record tap carries a MARKER during the count and a
// frame code from the counted downbeat on, so the take's frames can be identified exactly.
//
// What this PROVES:
//   - a FIXED first take's window is bars*fpb, clamped DOWN to whole bars that fit the 60 s buffer;
//     the bar selector clamps to [1, 32] and rounds                                         [configureRecordingEnd]
//   - it auto-commits at EXACTLY its window, frame-exact under steady, ragged and one-batch drains:
//     no count leak, nothing past the end, recorder released, BPM still locked             [consume, finishCapture]
//   - planCommit of N whole bars is N bars at the press tempo                                [planCommit]
//   - an abort during the count or the take returns to EMPTY and unlocks
//   - a manual stop mid-take commits the completed bars; with a tail in flight it waits, and a
//     repeated stop cannot extend the end                                                    [stopCapture]
//   - the 42 bpm / 32-bar case clamps to 10 bars that fit, and a later take fills that master
//   - a later FIXED take selects its bars (clamped to the master); RETAKE keeps master-length passes

import { bootLooper } from '../harness/rig.ts';
import { framesPerBar } from '../../src/audio/quantize.ts';
import { planCommit } from '../../src/audio/looper/grid-math.ts';

let fails = 0, checks = 0;
const approx = (a, b, eps) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }
const MARKER = 0.75;
const code = (frame) => ((frame % 8192) + 1) / 16384; // exact in Float32, never equal to MARKER

/** Boot, enable FIXED `bars`, press REC. The tap is MARKER until the armed start, then the frame code. */
async function fixedTake({ sr = 48000, bpm = 120, bars = 4, trimMs = null, startTime = 1 } = {}) {
  const rig = await bootLooper({ sampleRate: sr, startTime });
  rig.clock.setBpm(bpm);
  if (trimMs !== null) {
    const latency = await rig.import('audio/record-latency.ts');
    latency.beginMonitorGeneration(0, 0);
    latency.setOffsetMs(trimMs);
  }
  rig.looper.setFixedLengthEnabled(true);
  rig.looper.setFixedLengthBars(bars);
  rig.setInput(MARKER);
  await rig.looper.recDub(0);
  const es = rig.state.engineState;
  const start = es.captureStartFrame;
  rig.setInput((f) => (f < start ? MARKER : code(f)));
  return { rig, es, start, end: es.captureEndFrame, fpb: framesPerBar(bpm, sr) };
}
const committed = (rig) => rig.looper.masterLengthFrames() > 0 && rig.looper.trackInfo(0).state === 'PLAYING';
async function untilCommitted(rig, limit) {
  const end = rig.now() + limit;
  while (!committed(rig) && rig.now() < end) await rig.advance(0.01);
}

console.log('=== A. The FIXED window = bars*fpb, clamped to whole bars that fit the buffer ===');
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 90, 137, 100, 73]) {
    for (const bars of [1, 2, 4, 8, 16]) {
      const { rig, start, end, fpb } = await fixedTake({ sr, bpm, bars });
      ok(`A window = bars*fpb bpm=${bpm} sr=${sr} bars=${bars}`, end - start === bars * fpb, `window=${end - start}`);
      ok(`A BPM locked at the press bpm=${bpm} bars=${bars}`, rig.clock.bpmLocked() === true);
    }
  }
}
{
  // 32 bars at 40 bpm at 48 kHz asks for 9.2 M frames of a 2.88 M buffer: whole bars that fit, not a raw min.
  const { rig, start, end, fpb } = await fixedTake({ sr: 48000, bpm: 40, bars: 32 });
  const cap = rig.tracks[0].record.length;
  ok('A an over-long request clamps to whole bars that fit', end - start === Math.floor(cap / fpb) * fpb, `window=${end - start}`);
  ok('A the clamped window is a whole-bar multiple', (end - start) % fpb === 0);
}
{
  const rig = await bootLooper({ init: false });
  const bars = (n) => { rig.looper.setFixedLengthBars(n); return rig.looper.fixedLengthBars(); };
  ok('A setFixedLengthBars(0) = 1', bars(0) === 1);
  ok('A setFixedLengthBars(-3) = 1', bars(-3) === 1);
  ok('A setFixedLengthBars(99) = 32', bars(99) === 32);
  ok('A setFixedLengthBars(4.6) = 5 (rounds)', bars(4.6) === 5);
}

console.log('=== B. Auto-commit at EXACTLY the window, frame-exact across drain regimes ===');
const regimes = {
  'steady 25 ms drain': (rig, seconds) => rig.advance(seconds),
  'ragged stalls': async (rig, seconds) => {
    const end = rig.now() + seconds;
    const stalls = [0.21, 0.004, 1.3, 0.07, 0.9, 0.013];
    for (let i = 0; rig.now() < end; i++) { await rig.stall(Math.min(stalls[i % stalls.length], end - rig.now())); await rig.advance(0.011); }
  },
  'one batch spanning count + take + overplay': (rig, seconds) => rig.stall(Math.min(seconds, 10)),
};
for (const [sr, bpm, bars] of [[48000, 120, 4], [44100, 100, 2], [48000, 90, 2], [44100, 137, 1]]) {
  for (const [label, run] of Object.entries(regimes)) {
    const { rig, es, start, fpb } = await fixedTake({ sr, bpm, bars });
    const target = bars * fpb;
    // The count, the take, then 0.25 s of overplay that must be dropped.
    await run(rig, (start - rig.frame()) / sr + target / sr + 0.25);
    await rig.advance(0.05);
    const t = rig.tracks[0];
    const tag = `[${label}] bpm=${bpm} bars=${bars}`;
    ok(`B auto-committed ${tag}`, committed(rig));
    ok(`B master == target ${tag}`, rig.looper.masterLengthFrames() === target, `master=${rig.looper.masterLengthFrames()}`);
    ok(`B frame 0 is the take's first frame ${tag}`, t.record[0] === code(start), `record[0]=${t.record[0]}`);
    ok(`B the last loop frame is the window's last ${tag}`, t.record[target - 1] === code(start + target - 1));
    ok(`B nothing past the window written ${tag}`, t.record[target] === 0, `record[target]=${t.record[target]}`);
    let leak = -1;
    for (let k = 0; k < target && leak < 0; k++) if (t.record[k] === MARKER) leak = k;
    ok(`B no count-bar leak ${tag}`, leak === -1, `leak at ${leak}`);
    ok(`B recorder released ${tag}`, es.activeRecordIndex === -1 && es.captureStartFrame === null && es.captureEndFrame === null);
    ok(`B BPM still locked after the commit ${tag}`, rig.clock.bpmLocked() === true && rig.clock.bpm() === Math.round(bpm));
  }
}

console.log('=== C. planCommit of N whole bars is N bars at the press tempo ===');
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 90, 137, 100, 73, 200]) {
    for (const bars of [1, 2, 3, 4, 7, 16]) {
      const fpb = framesPerBar(bpm, sr);
      const plan = planCommit(bars * fpb, bpm, sr, bars * fpb + 16);
      ok(`C master == N*fpb bpm=${bpm} sr=${sr} bars=${bars}`, plan.master === bars * fpb && plan.bars === bars);
      ok(`C derived bpm ~= press bpm=${bpm} sr=${sr} bars=${bars}`, approx(plan.derivedBpm, bpm, 0.05), `derived=${plan.derivedBpm}`);
    }
  }
}

console.log('=== D/E. An abort during the count or the take -> EMPTY, window cleared, BPM unlocked ===');
for (const [label, when] of [['count', 1.0], ['take', 3.5]]) {
  const { rig, es } = await fixedTake({ bars: 4 });
  await rig.advance(when);
  rig.looper.stop(0);
  await rig.advance(0.5);
  ok(`D/E abort during the ${label} -> EMPTY`, rig.looper.trackInfo(0).state === 'EMPTY' && rig.looper.masterLengthFrames() === 0);
  ok(`D/E abort during the ${label} clears the window and the recorder`, es.captureStartFrame === null && es.captureEndFrame === null && es.activeRecordIndex === -1);
  ok(`D/E abort during the ${label} unlocks BPM`, rig.clock.bpmLocked() === false);
  ok(`D/E abort during the ${label} keeps no audio`, rig.tracks[0].record.every((x) => x === 0));
}

console.log('=== F. A manual stop mid-take commits the completed bars; BPM stays locked ===');
{
  const { rig, start, fpb } = await fixedTake({ bars: 4 });
  await rig.advanceTo((start + 2.4 * fpb) / 48000);
  await rig.looper.recDub(0);
  await untilCommitted(rig, 1);
  ok('F quantized to 2 completed bars', rig.looper.masterLengthFrames() === 2 * fpb, `master=${rig.looper.masterLengthFrames()}`);
  ok('F BPM stays locked (a master now exists)', rig.clock.bpmLocked() === true);
}

console.log('=== I. A manual FIXED stop tightens its end once and waits for the missing tail ===');
{
  const { rig, es, start, fpb } = await fixedTake({ bars: 4, trimMs: 100 });
  const downbeat = (start - es.captureCompensationFrames) / 48000;
  await rig.advanceTo(downbeat + (2 * fpb + 100) / 48000);
  await rig.looper.recDub(0);
  ok('I the manual stop replaces the longer automatic end', es.captureEndFrame === start + 2 * fpb, `end-start=${es.captureEndFrame - start}`);
  ok('I the missing tail keeps the recorder and the lock', !committed(rig) && es.activeRecordIndex === 0 && rig.clock.bpmLocked());
  await rig.advanceTo(downbeat + (2 * fpb + 1500) / 48000);
  if (!committed(rig)) await rig.looper.recDub(0); // a press a bar later cannot extend the end
  ok('I a repeated stop cannot extend the end', committed(rig) || es.captureEndFrame === start + 2 * fpb);
  await untilCommitted(rig, 1);
  const t = rig.tracks[0];
  ok('I the tail completes exactly two bars', rig.looper.masterLengthFrames() === 2 * fpb);
  ok('I the retained tail survives up to the last frame', t.record[2 * fpb - 1] === code(start + 2 * fpb - 1));
  ok('I no audio beyond the shortened end', t.record[2 * fpb] === 0);
}

console.log('=== H. 42 bpm, 32 bars, 44.1 kHz: clamp to whole bars that fit; a later take fills the master ===');
{
  const sr = 44100;
  const { rig, start, end, fpb } = await fixedTake({ sr, bpm: 42, bars: 32 });
  const cap = rig.tracks[0].record.length;
  ok('H precondition: the buffer is not a whole-bar multiple', cap % fpb !== 0 && 32 * fpb > cap);
  ok('H the window is the largest whole-bar region that fits', end - start === Math.floor(cap / fpb) * fpb, `window=${end - start} cap=${cap}`);
  await rig.advance((end - rig.frame()) / sr + 0.1);
  const master = rig.looper.masterLengthFrames();
  ok('H auto-committed exactly the window', master === end - start && master % fpb === 0 && master <= cap, `master=${master}`);
  rig.looper.setFixedLengthEnabled(false);
  await rig.looper.recDub(1);
  await rig.advance((master * 2) / sr + 0.2);
  ok('H a later take fills master frames without a RangeError', rig.looper.trackInfo(1).state === 'PLAYING' &&
    rig.tracks[1].lengthFrames === master && !rig.logs.some((l) => l.level === 'error'), rig.looper.trackInfo(1).state);
}

console.log('=== J. A later FIXED take selects its bars; RETAKE keeps master-length passes ===');
{
  const rig = await bootLooper();
  const master = await rig.recordFirstTake({ bars: 8 });
  const fpb = framesPerBar(120, rig.sr);
  const es = rig.state.engineState;
  const laterWindow = async ({ fixed, bars, retake }) => {
    rig.looper.setFixedLengthEnabled(fixed);
    rig.looper.setFixedLengthBars(bars);
    rig.looper.setRetakeEnabled(retake);
    await rig.looper.recDub(1);
    const window = es.captureEndFrame - es.captureStartFrame;
    rig.looper.stop(1);
    return window;
  };
  ok('J precondition: an 8-bar master', master === 8 * fpb);
  ok('J a later FIXED take uses its selected whole-bar window', (await laterWindow({ fixed: true, bars: 3, retake: false })) === 3 * fpb);
  ok('J a later FIXED take clamps at the master bars', (await laterWindow({ fixed: true, bars: 12, retake: false })) === master);
  ok('J FIXED off keeps the full master window', (await laterWindow({ fixed: false, bars: 3, retake: false })) === master);
  ok('J RETAKE ignores later FIXED and keeps the master pass', (await laterWindow({ fixed: true, bars: 3, retake: true })) === master);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

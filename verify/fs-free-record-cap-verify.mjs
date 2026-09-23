// Free-record CAPACITY of the first take: the REAL looper (machine.ts configureRecordingEnd, capture.ts
// consume) under the verify rig. A free take's window ends at start + the 60 s record buffer; when that
// end arrives the take commits the largest whole-bar region that fits. A take below the capacity keeps
// recording; a FIXED take ends at its own bar count first. 42 bpm at 44.1 kHz fits 10.5 bars: the
// commit must floor to 10, never round past the buffer.

import { bootLooper } from './harness/rig.ts';
import { framesPerBar } from '../src/audio/quantize.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }
const code = (frame) => ((frame % 8192) + 1) / 16384;
const MAX_LOOP_SECONDS = 60; // looper/state.ts

async function freeTake({ sr, bpm, fixedBars = 0 }) {
  const rig = await bootLooper({ sampleRate: sr });
  rig.clock.setBpm(bpm);
  rig.looper.setFixedLengthEnabled(fixedBars > 0);
  if (fixedBars > 0) rig.looper.setFixedLengthBars(fixedBars);
  rig.setInput(code);
  await rig.looper.recDub(0);
  const es = rig.state.engineState;
  return { rig, start: es.captureStartFrame, end: es.captureEndFrame, cap: rig.tracks[0].record.length, fpb: framesPerBar(bpm, sr) };
}

console.log('=== A. A free take held past 60 s AUTO-COMMITS at the buffer capacity ===');
for (const [sr, bpm] of [[48000, 120], [44100, 100], [48000, 137], [44100, 42]]) {
  const { rig, start, end, cap, fpb } = await freeTake({ sr, bpm });
  ok(`A the capacity is the 60 s buffer sr=${sr}`, cap === Math.ceil(MAX_LOOP_SECONDS * sr) && end === start + cap, `cap=${cap} window=${end - start}`);
  let committedAt = null;
  while (rig.now() < (start + cap) / sr + 5) {
    await rig.advance(0.025);
    if (committedAt === null && rig.looper.masterLengthFrames() > 0) committedAt = rig.now();
  }
  const master = rig.looper.masterLengthFrames();
  ok(`A committed at the capacity (not stuck RECORDING) sr=${sr}`, rig.looper.trackInfo(0).state === 'PLAYING');
  ok(`A the commit came within one drain of the deadline sr=${sr}`,
    committedAt !== null && committedAt - (start + cap) / sr >= 0 && committedAt - (start + cap) / sr <= 0.03,
    `late by ${committedAt === null ? 'never' : (committedAt - (start + cap) / sr).toFixed(4)}s`);
  ok(`A recorder released sr=${sr}`, rig.state.engineState.activeRecordIndex === -1);
  ok(`A master = the largest whole-bar region that fits sr=${sr}`, master === Math.floor(cap / fpb) * fpb && master <= cap, `master=${master} cap=${cap}`);
  ok(`A no capture loss sr=${sr}`, rig.looper.captureOverruns() === 0);
  if (sr === 48000 && bpm === 120) {
    const t = rig.tracks[0];
    ok('B kept audio intact: frame 0 is the take start', t.record[0] === code(start));
    ok('B kept audio intact: the last loop frame is in place', t.record[master - 1] === code(start + master - 1));
  }
}

console.log('=== D. No regression: below the capacity a free take keeps recording; FIXED ends first ===');
{
  const { rig } = await freeTake({ sr: 48000, bpm: 120 });
  await rig.advance(12);
  ok('D a 10 s free take did NOT auto-commit', rig.looper.trackInfo(0).state === 'RECORDING' && rig.looper.masterLengthFrames() === 0);
  ok('D the recorder is still held', rig.state.engineState.activeRecordIndex === 0);
  ok('D no capture loss', rig.looper.captureOverruns() === 0);
}
{
  const { rig, start, end, fpb } = await freeTake({ sr: 48000, bpm: 120, fixedBars: 2 });
  ok('D FIXED sets its end at start + bars*fpb, before the capacity', end === start + 2 * fpb);
  await rig.advance(2.02 + 4 + 0.1);
  ok('D FIXED committed at its own end', rig.looper.masterLengthFrames() === 2 * fpb && rig.looper.trackInfo(0).state === 'PLAYING',
    `master=${rig.looper.masterLengthFrames()}`);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

// Free-record STOP of the first take: the REAL looper (src/audio/looper/machine.ts stopCapture, capture.ts
// consume) under the verify rig. Musical time chooses complete bars with a quarter-beat grace; the
// capture window's end is an exclusive absolute frame; a stop whose end is still in flight waits for
// its tail. Record-latency compensation C comes from the real record-latency module (a native monitor
// generation plus manual trim), exactly as the app applies it.
//
// What this PROVES:
//   - a stop just after a bar line keeps the completed bars; with C in flight the commit waits for the
//     tail and then lands exactly N bars, within C of the press                             [stopCapture]
//   - a mid-bar stop floors to the completed bars and commits at once
//   - a press up to a quarter beat EARLY keeps the bar (and waits for it); earlier drops it
//   - a sub-bar stop retains audio through the press (+C) and pads to one bar
//   - C shifts both window edges and never the musical length; a repeated stop cannot extend the end
//     (the sub-bar case, where a later press would end later)
// The 60 s capacity commit lives in fs-free-record-cap-verify.mjs.

import { bootLooper } from './harness/rig.ts';
import { framesPerBar } from '../src/audio/quantize.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

/** Exactly representable in Float32: identifies which absolute frame landed where. */
const code = (frame) => ((frame % 8192) + 1) / 16384;

/** Boot, optionally arm a native-monitor C, press REC; returns the take's downbeat (s) and start frame. */
async function startTake({ bpm = 120, sr = 48000, trimMs = null } = {}) {
  const rig = await bootLooper({ sampleRate: sr, startTime: 50 });
  rig.clock.setBpm(bpm);
  if (trimMs !== null) {
    const latency = await rig.import('audio/record-latency.ts');
    latency.beginMonitorGeneration(0, 0);
    latency.setOffsetMs(trimMs);
  }
  rig.setInput(code);
  const mark = rig.draws().length;
  await rig.looper.recDub(0);
  const downbeat = rig.draws().slice(mark).find((b) => b.countLeft === 4).time + 4 * (60 / bpm);
  const es = rig.state.engineState;
  return { rig, downbeat, start: es.captureStartFrame, C: es.captureCompensationFrames, fpb: framesPerBar(bpm, sr) };
}
const committed = (rig) => rig.looper.trackInfo(0).state === 'PLAYING' && rig.looper.masterLengthFrames() > 0;
/** Advance in drain-sized steps until the take commits; returns the ctx time it committed at. */
async function untilCommitted(rig, limit = 2) {
  const end = rig.now() + limit;
  while (!committed(rig) && rig.now() < end) await rig.advance(0.005);
  return rig.now();
}

console.log('=== A. RIG SCENARIO: 8 bars, stop 50 ms after the bar-9 downbeat, C = 100 ms in flight ===');
for (const [bpm, sr] of [[120, 44100], [120, 48000], [90, 44100], [137, 48000]]) {
  const { rig, downbeat, start, C, fpb } = await startTake({ bpm, sr, trimMs: 100 });
  ok(`A precondition: C > 50 ms bpm=${bpm} sr=${sr}`, C > 0.05 * sr, `C=${C}`);
  await rig.advanceTo(downbeat + (8 * fpb) / sr + 0.05);
  const press = rig.now();
  await rig.looper.recDub(0);
  const es = rig.state.engineState;
  ok(`A the end is exactly 8 bars from the take start bpm=${bpm} sr=${sr}`, es.captureEndFrame === start + 8 * fpb,
    `end-start=${es.captureEndFrame - start}`);
  ok(`A the tail is in flight: not committed at the press bpm=${bpm} sr=${sr}`, !committed(rig) && es.activeRecordIndex === 0);
  const at = await untilCommitted(rig);
  ok(`A commits exactly 8 bars bpm=${bpm} sr=${sr}`, rig.looper.masterLengthFrames() === 8 * fpb, `master=${rig.looper.masterLengthFrames()}`);
  ok(`A the wait is bounded by C (+ one drain) bpm=${bpm} sr=${sr}`, at - press <= C / sr + 0.03, `waited=${(at - press).toFixed(3)}s`);
  const t = rig.tracks[0];
  ok(`A the take starts on the compensated downbeat frame bpm=${bpm} sr=${sr}`, t.record[0] === code(start));
  ok(`A the last kept frame is the one before the end bpm=${bpm} sr=${sr}`, t.record[8 * fpb - 1] === code(start + 8 * fpb - 1));
}

console.log('=== B. mid-bar stop: completed-bars floor, immediate commit ===');
for (const [bpm, sr] of [[120, 44100], [90, 48000]]) {
  for (const drained of [false, true]) {
    // `drained`: the press lands right after a drain tick, so the ring holds nothing new to consume.
    const { rig, downbeat, fpb } = await startTake({ bpm, sr });
    await rig.advanceTo(downbeat + (7.5 * fpb) / sr);
    if (drained) await rig.untilDrained();
    await rig.looper.recDub(0);
    const tag = `bpm=${bpm}${drained ? ', ring empty at the press' : ''}`;
    ok(`B immediate commit ${tag}`, committed(rig));
    ok(`B commits 7 completed bars ${tag}`, rig.looper.masterLengthFrames() === 7 * fpb, `master=${rig.looper.masterLengthFrames()}`);
  }
}

console.log('=== C. grace: a press <= a quarter beat EARLY keeps the bar; earlier drops it ===');
for (const [bpm, sr] of [[120, 44100], [200, 48000]]) {
  const beat = 60 / bpm;
  {
    const { rig, downbeat, start, fpb } = await startTake({ bpm, sr });
    await rig.advanceTo(downbeat + (4 * fpb) / sr - 0.6 * (beat / 4));
    await rig.looper.recDub(0);
    ok(`C within grace -> waits for the 4th bar bpm=${bpm}`, !committed(rig) && rig.state.engineState.captureEndFrame === start + 4 * fpb);
    await untilCommitted(rig);
    ok(`C within grace -> 4 bars bpm=${bpm}`, rig.looper.masterLengthFrames() === 4 * fpb, `master=${rig.looper.masterLengthFrames()}`);
  }
  {
    const { rig, downbeat, fpb } = await startTake({ bpm, sr });
    await rig.advanceTo(downbeat + (4 * fpb) / sr - beat / 2);
    await rig.looper.recDub(0);
    await untilCommitted(rig);
    ok(`C half a beat early -> 3 bars bpm=${bpm}`, rig.looper.masterLengthFrames() === 3 * fpb, `master=${rig.looper.masterLengthFrames()}`);
  }
}

console.log('=== D. sub-bar stop retains its tail (+C) before one-bar padding ===');
{
  const { rig, downbeat, start, C, fpb } = await startTake({ bpm: 120, sr: 44100, trimMs: 60 });
  await rig.advanceTo(downbeat + (0.4 * fpb) / 44100);
  const pressFrame = rig.frame();
  await rig.looper.recDub(0);
  const es = rig.state.engineState;
  ok('D the end is the press + C, not the drained head', es.captureEndFrame === pressFrame + C, `end=${es.captureEndFrame} press+C=${pressFrame + C}`);
  ok('D the tail is deferred even below one bar', !committed(rig));
  await rig.advance(0.02);
  await rig.looper.recDub(0); // a second press while the tail is in flight would end later
  ok('D a repeated stop cannot extend the end', committed(rig) || es.captureEndFrame === pressFrame + C, `end=${es.captureEndFrame}`);
  await untilCommitted(rig, 3);
  const t = rig.tracks[0];
  const kept = pressFrame + C - start;
  ok('D pads to the one-bar minimum', rig.looper.masterLengthFrames() === fpb);
  ok('D retains exactly through the press', t.record[kept - 1] === code(start + kept - 1) && t.record[kept] === 0 && t.record[fpb - 1] === 0,
    `kept=${kept}`);
}

console.log('=== G. C moves both edges, never the length; a repeated stop cannot extend the window ===');
{
  const plain = await startTake({ bpm: 120, sr: 48000 });
  const comp = await startTake({ bpm: 120, sr: 48000, trimMs: 150 });
  ok('G C shifts the take start by exactly C', comp.start - comp.C === plain.start - plain.C && plain.C === 0 && comp.C > 0,
    `plain=${plain.start} comp=${comp.start} C=${comp.C}`);
  await comp.rig.advanceTo(comp.downbeat + (2 * comp.fpb) / 48000 + 0.02);
  await comp.rig.looper.recDub(0);
  const end = comp.rig.state.engineState.captureEndFrame;
  ok('G compensation preserves the musical length', end - comp.start === 2 * comp.fpb, `len=${end - comp.start}`);
  await untilCommitted(comp.rig);
  ok('G the compensated take commits the same bars', comp.rig.looper.masterLengthFrames() === 2 * comp.fpb);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

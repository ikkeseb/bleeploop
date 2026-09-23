// Full-length later-track ARM frame identity: the REAL looper (machine.ts startRecording, capture.ts
// consume) and capture worklet under the verify rig, with the tap carrying a frame code so every kept
// sample names the absolute render frame it came from. Short FIXED windows and their tiling live in
// fs-short-take-verify.mjs and fs-looper-arm-verify.mjs.
//
// What this PROVES, across rates, tempos, loop lengths and press phases:
//   - a later take's frame 0 is the render frame of the next master boundary, an exact multiple of
//     the master length after track 1's frame 0 (phase-identical)                          [startRecording, consume]
//   - an unshortened later take keeps exactly master frames, in order, with no capture loss
//   - a main-thread stall during the take within the ring's capacity loses nothing and still lands
//     frame-exact; one past it drops packets and the take is REJECTED, never committed shifted  [rejectRecordLoss]
// It cannot see currentTime stability inside a browser task: capture-clock.mjs measures that.

import { bootLooper } from './harness/rig.ts';
import { framesPerBar } from '../src/audio/quantize.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }
const SPAN = 1 << 22; // frame codes stay exact in Float32 and unique over any take here
const code = (frame) => ((frame % SPAN) + 1) / SPAN;
const frameAt = (sample, near) => { // recover the absolute frame nearest `near`
  const f = Math.round(sample * SPAN) - 1;
  return f + Math.round((near - f) / SPAN) * SPAN;
};

console.log('=== A. Later takes land frame-exact on the master grid ===');
for (const sr of [44100, 48000]) {
  for (const bpm of [60, 97, 137, 220]) {
    for (const bars of [1, 2]) {
      const rig = await bootLooper({ sampleRate: sr, startTime: 0.337 });
      rig.clock.setBpm(bpm);
      const master = await rig.recordFirstTake({ bars });
      const fpb = framesPerBar(bpm, sr);
      rig.setInput(code);
      const es = rig.state.engineState;
      const track1Frame0 = Math.round(es.masterStartTime * sr);
      ok(`A precondition master sr=${sr} bpm=${bpm} bars=${bars}`, master === bars * fpb, `master=${master}`);
      const period = master / sr;
      for (const phase of [0.013, 0.37, 0.81]) {
        // Press at `phase` through a loop period, then let the whole take arrive.
        const elapsed = rig.now() - es.masterStartTime;
        await rig.advanceTo(es.masterStartTime + (Math.floor(elapsed / period) + 1 + phase) * period);
        await rig.looper.recDub(1);
        const boundary = es.captureStartFrame;
        await rig.advance(period * 2.1);
        const t = rig.tracks[1];
        const tag = `sr=${sr} bpm=${bpm} bars=${bars} phase=${phase}`;
        const first = frameAt(t.record[0], boundary);
        ok(`A committed at master length ${tag}`, t.state === 'PLAYING' && t.lengthFrames === master, t.state);
        ok(`A frame 0 == the next master boundary ${tag}`, first === boundary, `first=${first} boundary=${boundary}`);
        ok(`A phase-locked to track 1 (k*master) ${tag}`, (first - track1Frame0) % master === 0, `delta=${first - track1Frame0}`);
        let order = -1;
        for (let k = 1; k < master && order < 0; k++) if (frameAt(t.record[k], first + k) !== first + k) order = k;
        ok(`A the take holds exactly master frames in order ${tag}`, order === -1, `break at ${order}`);
        ok(`A no capture loss ${tag}`, rig.looper.captureOverruns() === 0);
        rig.looper.clear(1);
      }
    }
  }
}

console.log('=== B. A stall during the take: within the ring nothing is lost; past it the take is rejected ===');
for (const [label, stall, lossExpected] of [['1.5 s stall', 1.5, false], ['12 s stall (past the ~10.9 s ring)', 12, true]]) {
  const rig = await bootLooper();
  const master = await rig.recordFirstTake({ bars: 4 }); // 8 s loop
  rig.setInput(code);
  const es = rig.state.engineState;
  await rig.advance(0.5);
  await rig.looper.recDub(1);
  const boundary = es.captureStartFrame;
  await rig.advanceTo(boundary / rig.sr + 0.3);
  await rig.stall(stall);
  await rig.advance(master / rig.sr + 0.5);
  const t = rig.tracks[1];
  if (!lossExpected) {
    ok(`B [${label}] no capture loss`, rig.looper.captureOverruns() === 0);
    ok(`B [${label}] committed frame-exact`, t.state === 'PLAYING' && frameAt(t.record[0], boundary) === boundary &&
      frameAt(t.record[master - 1], boundary + master - 1) === boundary + master - 1);
  } else {
    ok(`B [${label}] the ring overflowed`, rig.looper.captureOverruns() > 0, `overruns=${rig.looper.captureOverruns()}`);
    ok(`B [${label}] the damaged take is rejected, never committed`, t.state === 'EMPTY' && t.lengthFrames === 0, t.state);
    ok(`B [${label}] the rejection is reported once`, rig.logs.filter((l) => l.level === 'error' && /rejected track 1/.test(String(l.args[0]))).length === 1);
    ok(`B [${label}] the master keeps playing`, rig.tracks[0].state === 'PLAYING' && rig.looper.masterLengthFrames() === master);
  }
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

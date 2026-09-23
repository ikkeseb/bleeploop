// Executable verification of the COUNT-IN ANCHOR: the real grid-math countInArm swept across tempos, rates
// and press times, plus the REAL looper (src/audio/looper/machine.ts startRecording, capture.ts
// armSplitOffset / consume) under the verify rig with a frame-coded input.
//
// Contract: the idle free-run grid is SILENT (the click is a transport mode — count-in + recording/playback
// only; clock.setTransportActive), so there is no audible grid for the count to stay in phase with. The
// count anchors at minLead (now + HBL) for BOTH metronome states — the snappiest possible count start.
//
// What this PROVES (the deterministic core; the analog feel is owed a by-ear check on the PC):
//   - the anchor is minLead and never in the past; the 4 count beats + the loop downbeat (recordStart)
//     form ONE self-consistent grid: beat n = anchor + n*P, recordStart = anchor + one exact bar  [A, B]
//   - on the real looper the count LED beats and forced clicks sit on that grid, identically with the
//     metronome on and off, and the take's frame 0 is exactly the frame at recordStart, whether the ring
//     drains in 25 ms batches or (with the producer ahead of the press clock) one long batch straddles
//     the downbeat after a stall                                                                [C]

let fails = 0, checks = 0;
const approx = (a, b, eps) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

import {
  COUNT_IN_BEATS,
  HEARTBEAT_INTERNAL_LATENCY as HBL,
  countInArm,
} from '../src/audio/looper/grid-math.ts';
import { bootLooper } from './harness/rig.ts';

function armCountIn(now, bpm, sr) {
  const a = countInArm(now, bpm, sr);
  return { beatPeriod: a.beatPeriod, anchor: a.anchor, recordStart: a.recordStart, pending: a.pendingFrames };
}

// ════════════════════════════════════════════════════════════════════════════════════════════
console.log('=== A. anchor == minLead, never in the past; recordStart = anchor + one exact bar ===');
for (const bpm of [120, 90, 137, 100, 200, 73.5, 40, 300]) {
  for (const sr of [48000, 44100]) {
    for (const now of [0.0, 0.3, 0.49, 1.0, 12.7, 41.99, 100.05, 999.999]) {
      const a = armCountIn(now, bpm, sr);
      ok(`A anchor==minLead bpm=${bpm} now=${now}`, approx(a.anchor, now + HBL, 1e-12), `anchor=${a.anchor}`);
      ok(`A not in the past bpm=${bpm} now=${now}`, a.anchor >= now + HBL - 1e-9);
      ok(`A recordStart = anchor + 1 bar bpm=${bpm} sr=${sr}`,
         approx(a.recordStart - a.anchor, 4 * (60 / bpm), 1e-12));
      // the 4 count beats form one self-consistent grid off the anchor
      for (let n = 0; n < COUNT_IN_BEATS; n++) {
        const beat = a.anchor + n * a.beatPeriod;
        ok(`A count beat ${n} spacing bpm=${bpm} now=${now}`,
           approx(beat - a.anchor, n * (60 / bpm), 1e-12));
      }
    }
  }
}

console.log('=== B. pendingFrames closed form: round((HBL + N*beatPeriod)*sr) ===');
for (const bpm of [120, 90, 137, 200]) {
  for (const sr of [48000, 44100]) {
    for (const now of [0.0, 50.001, 100.137, 999.999]) {
      const a = armCountIn(now, bpm, sr);
      const expect = Math.round((HBL + COUNT_IN_BEATS * (60 / bpm)) * sr);
      ok(`B pending closed form bpm=${bpm} sr=${sr} now=${now}`, a.pending === expect && a.pending > 0,
         `${a.pending} vs ${expect}`);
    }
  }
}

console.log('=== C. the real count-in: LED + clicks on the anchor grid for both metronome states; frame 0 exact ===');
/** Exactly representable in Float32 over 2^22 frames: identifies which absolute frame landed where. */
const code = (frame) => ((frame % 4194304) + 1) / 8388608;
for (const bpm of [120, 90, 200]) {
  for (const sr of [48000, 44100]) {
    const anchors = {};
    for (const metronome of [true, false]) {
      for (const stall of [false, true]) {
        const rig = await bootLooper({ sampleRate: sr, startTime: 50 });
        rig.clock.setBpm(bpm);
        rig.clock.setMetronome(metronome);
        await rig.advance(0.0371); // an arbitrary press phase
        rig.setInput(code);
        // The producer is 4 quanta ahead of the clock the press reads: the take must still start on the
        // timestamp, not on a count of frames from whatever the first drained batch holds.
        if (stall) rig.renderAhead(4);
        const now = rig.now();
        const a = armCountIn(now, bpm, sr);
        const drawMark = rig.draws().length;
        const clickMark = rig.clicks().length;
        await rig.looper.recDub(0);
        const tag = `bpm=${bpm} sr=${sr} metronome=${metronome ? 'on' : 'off'}${stall ? ' stall' : ''}`;
        const es = rig.state.engineState;
        ok(`C ${tag} the take window opens at recordStart`, es.captureStartFrame === Math.round(a.recordStart * sr) &&
          es.pendingRecordStartFrame === a.pending, `start=${es.captureStartFrame} pending=${es.pendingRecordStartFrame}`);
        if (stall) {
          // The count is already queued; the main thread then blocks across the downbeat, so one long
          // drain batch straddles the take's frame 0.
          await rig.advanceTo(a.recordStart - 0.05);
          await rig.stall(0.25);
        }
        await rig.advanceTo(a.recordStart + (4 * 60) / bpm + 0.05);
        const count = rig.draws().slice(drawMark).filter((d) => d.countLeft > 0);
        const countBeats = count.map((d) => d.time);
        ok(`C ${tag} four count LED beats on the anchor grid`, count.length === COUNT_IN_BEATS &&
          countBeats.every((time, n) => approx(time, a.anchor + n * a.beatPeriod, 1e-9)), JSON.stringify(countBeats));
        const clicks = rig.clicks().slice(clickMark).filter((c) => c.time < a.recordStart - 1e-9);
        ok(`C ${tag} four forced count clicks, accent on the first`, clicks.length === COUNT_IN_BEATS &&
          clicks.every((c, n) => c.audible && approx(c.time, a.anchor + n * a.beatPeriod, 1e-9) && c.accent === (n === 0)),
          JSON.stringify(clicks.map((c) => [c.time, c.accent, c.audible])));
        (anchors[stall] ??= []).push(countBeats[0]);
        await rig.looper.recDub(0);
        await rig.advance(0.3);
        const t = rig.tracks[0];
        const start = Math.round(a.recordStart * sr);
        const master = rig.looper.masterLengthFrames();
        ok(`C ${tag} committed one bar`, master > 0 && rig.looper.trackInfo(0).state === 'PLAYING', `master=${master}`);
        ok(`C ${tag} frame 0 is the frame at recordStart`, t.record[0] === code(start), `got ${t.record[0] * 8388608 - 1} want ${start}`);
        ok(`C ${tag} the take is contiguous through its last frame`, t.record[master - 1] === code(start + master - 1));
      }
    }
    for (const stall of [false, true]) {
      ok(`C bpm=${bpm} sr=${sr}${stall ? ' stall' : ''} the anchor is identical with the metronome on and off`,
        anchors[stall].length === 2 && anchors[stall][0] === anchors[stall][1], JSON.stringify(anchors[stall]));
    }
  }
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

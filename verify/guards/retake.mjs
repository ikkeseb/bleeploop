// RETAKE arithmetic: which pass a stop gesture keeps (grid-math planRetakeStop), and that a take whose
// window + phase anchor slide by whole passes still commits ON the counted grid (commitAnchor) however
// many passes rolled. The dispatchers (completeRetakePass, stopCapture, the lane handoff) are exercised
// by golden-jam.mjs — a green run here says nothing about them.
let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

import { framesPerBar } from '../../src/audio/quantize.ts';
import { commitAnchor, planCommit, planFreeStop, planRetakeStop } from '../../src/audio/looper/grid-math.ts';

for (const sr of [44100, 48000]) {
  for (const bpm of [60, 97, 120, 174]) {
    const fpb = framesPerBar(bpm, sr);
    const grace = fpb / 16;
    const passEnd = 1_000_000;

    // ── the stop rule ──
    ok('mid-pass + kept → keep-last', planRetakeStop(passEnd - fpb * 2, passEnd, fpb, true) === 'keep-last');
    ok('mid-pass + nothing kept → stop-now', planRetakeStop(passEnd - fpb * 2, passEnd, fpb, false) === 'stop-now');
    ok('inside grace → finish-pass (kept)', planRetakeStop(passEnd - Math.floor(grace), passEnd, fpb, true) === 'finish-pass');
    ok('inside grace → finish-pass (none kept)', planRetakeStop(passEnd - Math.floor(grace), passEnd, fpb, false) === 'finish-pass');
    ok('one frame outside grace → keep-last', planRetakeStop(passEnd - Math.floor(grace) - 1, passEnd, fpb, true) === 'keep-last');
    ok('on the pass edge → finish-pass', planRetakeStop(passEnd, passEnd, fpb, true) === 'finish-pass');
    // A press just AFTER an edge is judged against the NEXT pass end: the pass that just finished is kept.
    ok('just after an edge → keep-last', planRetakeStop(passEnd + 1, passEnd + 4 * fpb, fpb, true) === 'keep-last');

    // The grace is the SAME quarter beat a free-record stop gets: where planFreeStop counts the bar in
    // flight as complete, planRetakeStop finishes the pass in flight — and nowhere else.
    for (const bars of [1, 2, 4]) {
      const L = bars * fpb;
      for (const early of [0, 1, Math.floor(grace) - 1, Math.floor(grace) + 2, fpb]) {
        const elapsedSec = (L - early) / sr;
        const free = planFreeStop(elapsedSec, bpm, sr, 60 * sr).bars >= bars;
        const retake = planRetakeStop(L - early, L, fpb, true) === 'finish-pass';
        ok(`grace agrees with planFreeStop (${bars} bars, ${early} early)`, free === retake, `free=${free} retake=${retake}`);
      }
    }

    // ── sliding the take by whole passes keeps the commit on the counted grid ──
    for (const bars of [1, 4]) {
      const L = bars * fpb;
      const period = L / sr;
      const downbeat0 = 3.217; // the counted come-in downbeat
      for (const passes of [1, 2, 7, 40]) {
        let downbeat = downbeat0;
        for (let k = 0; k < passes; k++) downbeat += L / sr; // completeRetakePass: anchor += one pass
        const plan = planCommit(L, bpm, sr, 60 * sr);
        ok(`kept pass commits at its own length (${bars} bars)`, plan.master === L);
        for (const into of [0.013, period * 0.5, period - 0.002]) {
          const playAt = downbeat + into; // approve this far into the pass in flight
          const a = commitAnchor(downbeat, plan.master, sr, playAt, L);
          const gridSlip = ((a.gridAnchor - downbeat0) / period) % 1;
          const slip = Math.min(Math.abs(gridSlip), Math.abs(1 - Math.abs(gridSlip))) * period * sr;
          ok(`grid anchor stays on the counted grid after ${passes} passes`, slip < 0.01, `slip=${slip} frames`);
          ok('playback enters at the live phase', Math.abs(a.startOffset - into) * sr < 0.01 && a.playWhen === playAt,
            `offset=${a.startOffset} into=${into}`);
        }
      }
    }
  }
}

console.log(`=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails ? 1 : 0);

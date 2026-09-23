// RETAKE: the REAL looper (machine.ts configureRecordingEnd / completeRetakePass / stopCapture /
// finishCapture, capture.ts consume) under the verify rig, plus the pure stop rule (grid-math
// planRetakeStop). The record tap carries a MARKER during the count and a frame code from the counted
// downbeat on, so a committed loop names exactly which captured frames it holds.
//
// What this PROVES:
//   - the stop rule: mid-pass keeps the last clean pass, the quarter-beat grace before a pass edge
//     finishes the pass in flight, nothing kept stops now; the grace equals planFreeStop's    [planRetakeStop]
//   - a rolling take slides its capture window by exactly one pass per edge and commits nothing  [completeRetakePass]
//   - a mid-pass stop commits the LAST COMPLETE pass, frame-exact, at once; the loop plays on the
//     grid the count-in set, however many passes rolled                                        [stopCapture keep-last]
//   - a stop inside the grace finishes the pass in flight and commits it                        [finish-pass]
//   - the grace is judged in captured frames: a compensated press (C > 0) is shifted by C       [retakeStopPlan]
//   - a stop in pass 1 is an ordinary free stop: whole completed bars of pass 1                 [stop-now]
//   - a pass that lost audio is dropped with the kept pass before it and taints the next pass;
//     a stop with nothing clean kept rejects the take                                           [completeRetakePass, rejectRecordLoss]
//   - a later take rolls master-length passes on the master grid; REC on another lane approves it
//     and that lane records next                                                                [finishCapture handoff]
// It cannot show real render timing, WebView2 or anything audible (golden-jam.mjs drives RETAKE in a
// real browser).

import { bootLooper } from '../harness/rig.ts';
import { framesPerBar } from '../../src/audio/quantize.ts';
import { planFreeStop, planRetakeStop } from '../../src/audio/looper/grid-math.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }
const MARKER = 0.75;
const code = (frame) => ((frame % 1048576) + 1) / 2097152; // exact in Float32, never equal to MARKER

/** How many frames of `pcm` differ from the code of captured frames `from`, `from + 1`, … */
function mismatches(pcm, from) {
  let bad = 0;
  for (let k = 0; k < pcm.length; k++) if (pcm[k] !== code(from + k)) bad++;
  return bad;
}
const loop = (rig, lane) => rig.tracks[lane].record.subarray(0, rig.looper.masterLengthFrames());
/** When loop frame 0 sounds (`when - offset`) for the newest looping source started since `mark`. */
function loopZero(rig, mark) {
  const src = rig.sources().slice(mark).filter((s) => s.loop && s.startTime !== null).at(-1);
  return src ? src.startTime - src.offset : NaN;
}
/** Distance, in frames, of ctx time `t` from the nearest point of the grid `origin + k * period`. */
function gridSlip(t, origin, period, sr) {
  const n = (t - origin) / period;
  return Math.abs(n - Math.round(n)) * period * sr;
}

console.log('=== A. The stop rule (pure) ===');
for (const sr of [44100, 48000]) {
  for (const bpm of [60, 97, 120, 174]) {
    const fpb = framesPerBar(bpm, sr);
    const grace = fpb / 16;
    const passEnd = 1_000_000;
    ok('A mid-pass + kept → keep-last', planRetakeStop(passEnd - fpb * 2, passEnd, fpb, true) === 'keep-last');
    ok('A mid-pass + nothing kept → stop-now', planRetakeStop(passEnd - fpb * 2, passEnd, fpb, false) === 'stop-now');
    ok('A inside grace → finish-pass (kept)', planRetakeStop(passEnd - Math.floor(grace), passEnd, fpb, true) === 'finish-pass');
    ok('A inside grace → finish-pass (none kept)', planRetakeStop(passEnd - Math.floor(grace), passEnd, fpb, false) === 'finish-pass');
    ok('A one frame outside grace → keep-last', planRetakeStop(passEnd - Math.floor(grace) - 1, passEnd, fpb, true) === 'keep-last');
    ok('A on the pass edge → finish-pass', planRetakeStop(passEnd, passEnd, fpb, true) === 'finish-pass');
    // A press just AFTER an edge is judged against the NEXT pass end: the pass that just finished is kept.
    ok('A just after an edge → keep-last', planRetakeStop(passEnd + 1, passEnd + 4 * fpb, fpb, true) === 'keep-last');
    // The grace is the SAME quarter beat a free-record stop gets: where planFreeStop counts the bar in
    // flight as complete, planRetakeStop finishes the pass in flight — and nowhere else.
    for (const bars of [1, 2, 4]) {
      const L = bars * fpb;
      for (const early of [0, 1, Math.floor(grace) - 1, Math.floor(grace) + 2, fpb]) {
        const free = planFreeStop((L - early) / sr, bpm, sr, 60 * sr).bars >= bars;
        const retake = planRetakeStop(L - early, L, fpb, true) === 'finish-pass';
        ok(`A grace agrees with planFreeStop (${bars} bars, ${early} early)`, free === retake, `free=${free} retake=${retake}`);
      }
    }
  }
}

/**
 * Boot, enable FIXED `bars` + RETAKE, press REC on lane 0. The counted downbeat comes from the count-in
 * LEDs, never from the capture window under test. Pass p (1-based) captures [start + (p-1)L, start + pL).
 */
async function rollingFirstTake({ sr = 48000, bpm = 120, bars = 1, trimMs = null } = {}) {
  const rig = await bootLooper({ sampleRate: sr });
  rig.clock.setBpm(bpm);
  rig.clock.setMetronome(false);
  if (trimMs !== null) {
    const latency = await rig.import('audio/record-latency.ts');
    latency.beginMonitorGeneration(0, 0);
    latency.setOffsetMs(trimMs);
  }
  rig.looper.setFixedLengthEnabled(true);
  rig.looper.setFixedLengthBars(bars);
  rig.looper.setRetakeEnabled(true);
  rig.setInput(MARKER);
  const mark = rig.draws().length;
  await rig.looper.recDub(0);
  const es = rig.state.engineState;
  const count = rig.draws().slice(mark).find((d) => d.countLeft === 4);
  const fpb = framesPerBar(bpm, sr);
  const beat = 60 / bpm;
  const downbeat = count.time + 4 * beat;
  const C = es.captureCompensationFrames;
  const start = Math.round(downbeat * sr) + C;
  rig.setInput((f) => (f < start ? MARKER : code(f)));
  const L = bars * fpb;
  /** Render until the capture frame (= render frame + C) reaches `frame`. */
  const toCaptured = (frame) => rig.advanceTo((frame - C) / sr);
  return { rig, es, fpb, L, C, start, downbeat, toCaptured, passStart: (p) => start + (p - 1) * L };
}

console.log('=== B. A rolling take slides one pass per edge and commits nothing ===');
for (const [sr, bpm] of [[48000, 120], [44100, 97], [48000, 174]]) {
  const { rig, es, L, start, passStart, toCaptured } = await rollingFirstTake({ sr, bpm });
  const tag = `sr=${sr} bpm=${bpm}`;
  ok(`B the first window is one pass from the counted downbeat ${tag}`, es.captureStartFrame === start && es.captureEndFrame === start + L,
    `start=${es.captureStartFrame} (want ${start}) end=${es.captureEndFrame}`);
  for (const p of [1, 2, 3, 4]) {
    await toCaptured(passStart(p) + L / 2);
    await rig.untilDrained();
    ok(`B pass ${p}: window = [start + ${p - 1}L, start + ${p}L) ${tag}`,
      es.captureStartFrame === passStart(p) && es.captureEndFrame === passStart(p + 1),
      `window=[${es.captureStartFrame}, ${es.captureEndFrame}) want [${passStart(p)}, ${passStart(p + 1)})`);
    ok(`B pass ${p}: the lane reports pass ${p}, still RECORDING, nothing committed ${tag}`,
      rig.looper.trackInfo(0).retakePass === p && rig.looper.trackInfo(0).state === 'RECORDING' && rig.looper.masterLengthFrames() === 0,
      `pass=${rig.looper.trackInfo(0).retakePass} state=${rig.looper.trackInfo(0).state} master=${rig.looper.masterLengthFrames()}`);
    ok(`B pass ${p}: the pass in flight writes from its own downbeat ${tag}`,
      rig.tracks[0].writeHead > 0 && mismatches(rig.tracks[0].record.subarray(0, rig.tracks[0].writeHead), passStart(p)) === 0);
    if (p > 1) ok(`B pass ${p}: the previous clean pass is kept ${tag}`, es.retakeKept === true);
  }
  rig.release();
}

console.log('=== C. A mid-pass stop commits the last complete pass, on the counted grid ===');
for (const [sr, bpm, bars, passes] of [[48000, 120, 1, 3], [44100, 97, 2, 2], [48000, 137, 1, 7]]) {
  const { rig, es, L, downbeat, passStart, toCaptured } = await rollingFirstTake({ sr, bpm, bars });
  const tag = `sr=${sr} bpm=${bpm} bars=${bars} pass=${passes}`;
  await toCaptured(passStart(passes) + Math.round(L * 0.45));
  const mark = rig.sources().length;
  await rig.looper.recDub(0);
  ok(`C the stop commits at once ${tag}`, rig.looper.trackInfo(0).state === 'PLAYING' && es.activeRecordIndex === -1,
    `state=${rig.looper.trackInfo(0).state} recorder=${es.activeRecordIndex}`);
  ok(`C the loop is one pass long ${tag}`, rig.looper.masterLengthFrames() === L, `master=${rig.looper.masterLengthFrames()} want ${L}`);
  const bad = mismatches(loop(rig, 0), passStart(passes - 1));
  ok(`C the loop holds exactly the last complete pass ${tag}`, bad === 0, `${bad} frames differ from pass ${passes - 1}`);
  await rig.advance(0.05);
  const slip = gridSlip(loopZero(rig, mark), downbeat, L / sr, sr);
  ok(`C loop frame 0 sounds on the count-in grid ${tag}`, slip < 0.01, `slip=${slip.toFixed(3)} frames`);
  rig.release();
}

console.log('=== D. A stop inside the grace finishes the pass in flight ===');
for (const trimMs of [null, 40]) {
  const { rig, es, fpb, L, C, passStart, toCaptured } = await rollingFirstTake({ trimMs });
  const tag = `C=${C}`;
  if (trimMs !== null) ok('D the trim reaches the capture compensation', C > 0, `C=${C}`);
  // Just inside the grace before pass 2's edge, in CAPTURED frames. With C > 0 a press that ignored C
  // would sit C frames earlier: outside the grace, and keep pass 1.
  const grace = fpb / 16;
  await toCaptured(passStart(3) - Math.floor(grace) + 256);
  const pressCaptured = rig.frame() + C;
  ok(`D the press sits inside the grace ${tag}`, passStart(3) - pressCaptured <= grace && passStart(3) - pressCaptured > 0,
    `${passStart(3) - pressCaptured} frames before the edge`);
  if (C > 0) ok(`D the press would be outside the grace uncompensated ${tag}`, passStart(3) - (pressCaptured - C) > grace);
  await rig.looper.recDub(0);
  ok(`D the take keeps recording to its edge ${tag}`, rig.looper.trackInfo(0).state === 'RECORDING' && rig.looper.masterLengthFrames() === 0,
    `state=${rig.looper.trackInfo(0).state}`);
  await toCaptured(passStart(3) + fpb / 4);
  await rig.untilDrained();
  ok(`D it commits one pass at the edge ${tag}`, rig.looper.trackInfo(0).state === 'PLAYING' && rig.looper.masterLengthFrames() === L,
    `state=${rig.looper.trackInfo(0).state} master=${rig.looper.masterLengthFrames()}`);
  const bad = mismatches(loop(rig, 0), passStart(2));
  ok(`D the loop is the pass in flight (pass 2), not pass 1 ${tag}`, bad === 0, `${bad} frames differ`);
  ok(`D no third pass started ${tag}`, es.retakePass === 0 && es.activeRecordIndex === -1);
  rig.release();
}

console.log('=== E. A stop in pass 1 is an ordinary free stop ===');
{
  const { rig, fpb, start, toCaptured } = await rollingFirstTake({ bars: 4 });
  await toCaptured(start + Math.round(2.5 * fpb));
  await rig.looper.recDub(0);
  await rig.advance(0.1);
  await rig.untilDrained();
  ok('E pass 1 stopped mid-way commits its completed bars', rig.looper.masterLengthFrames() === 2 * fpb,
    `master=${rig.looper.masterLengthFrames()} want ${2 * fpb}`);
  ok('E the loop is the start of pass 1', mismatches(loop(rig, 0), start) === 0);
  rig.release();
}

console.log('=== F. A lossy pass is dropped with the kept pass, and taints the next ===');
async function lossyRoll() {
  const r = await rollingFirstTake({});
  const { injectRecordLossForTest } = await r.rig.import('audio/plugin-bridge.ts');
  await r.toCaptured(r.passStart(2) + r.L / 2);
  injectRecordLossForTest({ droppedFrames: 32 });
  return r;
}
{
  const { rig, es, L, passStart, toCaptured } = await lossyRoll();
  await toCaptured(passStart(3) + L / 4);
  await rig.untilDrained();
  ok('F the lossy pass 2 is dropped, and pass 1 with it', es.retakeKept === false && es.retakePass === 3);
  ok('F the drop is logged and shown', rig.logs.some((l) => l.level === 'error' && String(l.args[0]).includes('pass 2')) &&
    rig.notify.toasts().some((t) => /pass 2 dropped/.test(t.message)));
  await toCaptured(passStart(4) + L / 4);
  await rig.untilDrained();
  ok('F the clean pass after the loss is not kept either (tainted)', es.retakeKept === false && es.retakePass === 4);
  await toCaptured(passStart(5) + L / 4);
  await rig.untilDrained();
  ok('F the next clean pass is kept again', es.retakeKept === true && es.retakePass === 5);
  await rig.looper.recDub(0);
  ok('F a stop then commits that pass (4)', rig.looper.masterLengthFrames() === L && mismatches(loop(rig, 0), passStart(4)) === 0);
  rig.release();
}
{
  const { rig, L, passStart, toCaptured } = await lossyRoll();
  await toCaptured(passStart(3) + L / 2);
  await rig.untilDrained();
  await rig.looper.recDub(0);
  await rig.advance(0.1);
  await rig.untilDrained();
  ok('F a stop in the tainted pass with nothing kept rejects the take', rig.looper.masterLengthFrames() === 0 &&
    rig.looper.trackInfo(0).state === 'EMPTY', `state=${rig.looper.trackInfo(0).state} master=${rig.looper.masterLengthFrames()}`);
  ok('F the rejection names the pass edge', rig.logs.some((l) => l.level === 'error' && String(l.args[0]).includes('interruption at the edge')));
  rig.release();
}

console.log('=== G. A later take rolls master-length passes on the master grid ===');
/** Commit a two-bar first take, then roll a RETAKE on lane 2 with the frame code as input. */
async function rollingLaterTake({ fixed }) {
  const rig = await bootLooper();
  rig.clock.setMetronome(false);
  const master = await rig.recordFirstTake({ bars: 2 });
  const es = rig.state.engineState;
  const lane0Zero = loopZero(rig, 0);
  ok(`G the first take plays (fixed=${fixed})`, Number.isFinite(lane0Zero) && master > 0);
  rig.looper.setFixedLengthEnabled(fixed);
  rig.looper.setFixedLengthBars(1);
  rig.looper.setRetakeEnabled(true);
  rig.setInput((f) => code(f));
  await rig.looper.recDub(1);
  const s1 = es.captureStartFrame;
  const period = master / rig.sr;
  const tag = `(fixed=${fixed})`;
  ok(`G the later take arms on a master boundary ${tag}`, gridSlip(s1 / rig.sr, lane0Zero, period, rig.sr) < 0.01,
    `slip=${gridSlip(s1 / rig.sr, lane0Zero, period, rig.sr)}`);
  ok(`G its pass is the master ${tag}`, es.captureEndFrame - s1 === master, `window=${es.captureEndFrame - s1}`);
  await rig.advanceTo((s1 + 2 * master + master / 2) / rig.sr);
  await rig.untilDrained();
  ok(`G it rolled into pass 3 ${tag}`, rig.looper.trackInfo(1).retakePass === 3 && es.captureStartFrame === s1 + 2 * master,
    `pass=${rig.looper.trackInfo(1).retakePass}`);
  return { rig, es, master, s1, period, lane0Zero, tag };
}
for (const fixed of [true, false]) {
  const { rig, es, master, s1, period, lane0Zero, tag } = await rollingLaterTake({ fixed });
  // REC on lane 3 approves lane 2's rolling take and takes over the recorder.
  const mark = rig.sources().length;
  await rig.looper.recDub(2);
  ok(`G REC on another lane commits the rolling lane ${tag}`, rig.looper.trackInfo(1).state === 'PLAYING', `lane2=${rig.looper.trackInfo(1).state}`);
  ok(`G the approved lane holds its last complete pass (2) ${tag}`, mismatches(loop(rig, 1), s1 + master) === 0);
  ok(`G the approving lane records next ${tag}`, es.activeRecordIndex === 2 && rig.looper.trackInfo(2).state === 'RECORDING',
    `lane3=${rig.looper.trackInfo(2).state} recorder=${es.activeRecordIndex}`);
  await rig.advance(0.05);
  const slip = gridSlip(loopZero(rig, mark), lane0Zero, period, rig.sr);
  ok(`G the approved lane plays on the master grid ${tag}`, slip < 0.01, `slip=${slip.toFixed(3)} frames`);
  rig.release();
}
{
  // Inside the grace, a later take finishes the pass in flight (3) and commits it at the edge.
  const { rig, es, master, s1, tag } = await rollingLaterTake({ fixed: false });
  const fpb = framesPerBar(rig.clock.bpm(), rig.sr);
  await rig.advanceTo((s1 + 3 * master - fpb / 32) / rig.sr);
  await rig.looper.recDub(1);
  ok(`G a press inside the grace keeps the later take recording ${tag}`, rig.looper.trackInfo(1).state === 'RECORDING');
  await rig.advanceTo((s1 + 3 * master + fpb / 4) / rig.sr);
  await rig.untilDrained();
  ok(`G it commits the pass in flight (3) at the edge ${tag}`, rig.looper.trackInfo(1).state === 'PLAYING' &&
    mismatches(loop(rig, 1), s1 + 2 * master) === 0 && es.activeRecordIndex === -1,
    `state=${rig.looper.trackInfo(1).state} mismatches=${mismatches(loop(rig, 1), s1 + 2 * master)}`);
  rig.release();
}

console.log(`=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails ? 1 : 0);

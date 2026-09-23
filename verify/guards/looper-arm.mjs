// Later-track ARM, stop-during-arm and the recorder slot: the REAL looper (machine.ts stopCapture / stop
// / clear / finishCapture / resetMasterIfBlank, capture.ts consume) under the verify rig.
//
// What this PROVES:
//   - stopping a later lane still waiting for its boundary aborts to EMPTY: no silent loop is
//     committed, the arm countdown resets, the recorder is released, the master keeps playing  [stopCapture -> stop]
//   - a normal later take records real audio from frame 0 and commits at master length
//   - a short (FIXED) later take tiles across the whole master region
//   - STOP on an overdubbing lane keeps the committed PCM and the master grid; an immediate STOP
//     cancels a pending loop-end stop                                                          [stop]
//   - only the recording lane can release the recording window; an in-flight lane keeps the master
//     grid after the last committed lane is cleared; its own cancellation then resets the blank
//     session (master 0, BPM unlocked)                                                         [resetMasterIfBlank]

import { bootLooper } from '../harness/rig.ts';
import { framesPerBar } from '../../src/audio/quantize.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL ${name} ${detail}`); } }
const allZero = (a, n = a.length) => { for (let k = 0; k < n; k++) if (a[k] !== 0) return false; return true; };

/** A rig with a committed 4-bar master on lane 0 (120 bpm, 48 kHz). */
async function withMaster() {
  const rig = await bootLooper();
  const master = await rig.recordFirstTake({ bars: 4, level: 0.4 });
  return { rig, master, fpb: framesPerBar(120, rig.sr), es: rig.state.engineState };
}
/** Arm `lane` for the next boundary, landing mid-period so the wait is long. */
async function armLater(rig, lane) {
  await rig.advance(0.9);
  await rig.looper.recDub(lane);
}

console.log('=== 1. Stop a still-armed later track: aborts to EMPTY, no dead silent loop ===');
for (const [label, press] of [['REC again', (r) => r.looper.recDub(1)], ['PLAY/STOP', (r) => r.looper.playStop(1)], ['STOP', (r) => r.looper.stop(1)]]) {
  const { rig, master, es } = await withMaster();
  rig.setInput(0.5);
  await armLater(rig, 1);
  await rig.advance(0.2); // pre-boundary frames drained and discarded
  const t = rig.tracks[1];
  ok(`1 [${label}] still armed before the boundary`, t.armed === true && t.writeHead === 0 && es.pendingRecordStartFrame > 0);
  await press(rig);
  ok(`1 [${label}] aborts to EMPTY (no dead loop)`, t.state === 'EMPTY' && t.lengthFrames === 0);
  ok(`1 [${label}] arm countdown reset`, es.pendingRecordStartFrame === 0);
  ok(`1 [${label}] recorder released`, es.activeRecordIndex === -1);
  ok(`1 [${label}] no audio kept`, allZero(t.record, master) && t.peakCount === 0);
  ok(`1 [${label}] the master keeps playing, BPM locked`, rig.looper.masterLengthFrames() === master && rig.tracks[0].state === 'PLAYING' && rig.clock.bpmLocked());
  await rig.advance(master / rig.sr + 0.1);
  ok(`1 [${label}] nothing records at the boundary afterwards`, t.state === 'EMPTY' && allZero(t.record, master));
}

console.log('=== 2. A normal later take records real audio (the abort is scoped) ===');
{
  const { rig, master } = await withMaster();
  rig.setInput(0.7);
  await armLater(rig, 1);
  await rig.advance((2 * master) / rig.sr);
  const t = rig.tracks[1];
  ok('2 committed PLAYING at master length', t.state === 'PLAYING' && t.lengthFrames === master);
  ok('2 the take starts at frame 0 with content and fills the loop', t.record[0] > 0.6 && t.record[master - 1] > 0.6);
  ok('2 arm flag cleared', t.armed === false);
}

console.log('=== 3. A short later take tiles across the committed master region ===');
{
  const { rig, master, fpb } = await withMaster();
  rig.setInput((f) => ((f % 997) + 1) / 1024);
  rig.looper.setFixedLengthEnabled(true);
  rig.looper.setFixedLengthBars(1);
  await armLater(rig, 1);
  await rig.advance((2 * master) / rig.sr);
  const t = rig.tracks[1];
  let mismatch = -1;
  for (let k = fpb; k < master && mismatch < 0; k++) if (t.record[k] !== t.record[k % fpb]) mismatch = k;
  ok('3 short take commits at master length', t.state === 'PLAYING' && t.lengthFrames === master);
  ok('3 the one-bar take repeats sample-exactly through the final master frame', mismatch === -1 && t.record[master - 1] !== 0, `mismatch at ${mismatch}`);
}

console.log('=== 4. STOP keeps committed PCM; an immediate STOP cancels a pending loop-end stop ===');
{
  const { rig, master } = await withMaster();
  const before = rig.tracks[0].record.slice(0, master);
  await rig.looper.recDub(0); // overdub
  await rig.advance(0.3);
  ok('4 precondition: OVERDUBBING', rig.tracks[0].state === 'OVERDUBBING');
  rig.looper.stop(0);
  const t = rig.tracks[0];
  ok('4 stop(): a committed lane becomes STOPPED', t.state === 'STOPPED');
  ok('4 stop(): the committed PCM survives', t.record.subarray(0, master).every((v, k) => v === before[k]));
  ok('4 stop(): the master grid survives', rig.looper.masterLengthFrames() === master && rig.clock.bpmLocked());
}
{
  const { rig } = await withMaster();
  rig.looper.setLoopEndStopEnabled(true);
  rig.looper.playStop(0);
  ok('4 precondition: a loop-end stop is pending', rig.tracks[0].stopAt !== null && rig.tracks[0].state === 'PLAYING');
  rig.looper.stop(0);
  ok('4 an immediate stop cancels the pending loop-end stop', rig.tracks[0].stopAt === null && rig.tracks[0].state === 'STOPPED');
}

console.log('=== 5. Only the recording lane releases the window; an in-flight lane keeps the grid ===');
{
  const { rig, master, es } = await withMaster();
  await armLater(rig, 1);
  const window = { start: es.captureStartFrame, end: es.captureEndFrame, pending: es.pendingRecordStartFrame };
  rig.looper.clear(3);
  rig.looper.stop(4);
  ok('5 another lane cannot release the recording window', es.activeRecordIndex === 1 && es.captureStartFrame === window.start &&
    es.captureEndFrame === window.end && es.pendingRecordStartFrame === window.pending);
  rig.looper.clear(0); // the only committed lane
  ok('5 an in-flight lane preserves the master grid', rig.looper.masterLengthFrames() === master && rig.clock.bpmLocked());
  rig.looper.stop(1);
  ok('5 the owner cancellation resets the now-blank session',
    es.activeRecordIndex === -1 && rig.looper.masterLengthFrames() === 0 && !rig.clock.bpmLocked());
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

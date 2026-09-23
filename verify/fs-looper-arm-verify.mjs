import { armSplitAt, commitLaterTake } from '../src/audio/looper/grid-math.ts';
// Reproduce the stop-during-arm bug and show the fix. Faithful port of the relevant looper state
// transitions (looper.ts was split into src/audio/looper/{state,capture,peaks,playback,machine,
// mixer}.ts + a facade on 2026-07-01): consume arm-split
// (capture.ts consume), stopCapture, stop, and the recording completion. None of the split files can be imported in Node
// (Web Audio deps). This clean-stream model omits loss rejection, AUTO and audio scheduling.

let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL ${name} ${detail}`); } }
const allZero = (a) => a.every((x) => x === 0);
const anyNonZero = (a) => a.some((x) => x !== 0);

const FPB = 96000; // one bar @ 120/48k
const MASTER = 4 * FPB;
function makeTrack(index = 2) {
  return { index, state: 'EMPTY', armed: false, writeHead: 0, fillFrames: 0, lengthFrames: 0,
           peakCount: 0, record: new Float32Array(MASTER), stopAt: null };
}
// module-level shared state (mirrors looper/state.ts's engineState)
let pendingRecordStartFrame = 0;
let activeRecordIndex = -1;
let masterFrames = MASTER;
let bpmLocked = true;

// MIRRORS: src/audio/looper/machine.ts@981-999 sha256:bd08c5442c9537c8  (resetMaster + resetMasterIfBlank)
function resetMasterIfBlank(tracks) {
  if (activeRecordIndex < 0 && tracks.every((t) => t.state === 'EMPTY')) {
    masterFrames = 0;
    bpmLocked = false;
  }
}

// startRecording later-track fixture; the master boundary is supplied as an absolute frame.
function armLater(t, i, framesToBoundary) {
  t.armed = true; t.state = 'RECORDING'; t.writeHead = 0;
  t.index = i; t.captureFrame = 0; t.captureStartFrame = framesToBoundary;
  t.captureEndFrame = framesToBoundary + MASTER;
  pendingRecordStartFrame = framesToBoundary;
  activeRecordIndex = i;
}
// MIRRORS: src/audio/looper/capture.ts@339-383 sha256:72293488834b3bda  (consume: recording append and timestamp completion; overdub not modelled)
// consume() arm-split + later-track write — returns frames written this batch
function consume(t, data) {
  const count = data.length;
  const firstFrame = t.captureFrame;
  t.captureFrame += count;
  let offset = 0;
  if (t.armed) {
    const split = armSplitAt(t.captureStartFrame, firstFrame, count);
    pendingRecordStartFrame = split.pending;
    if (split.offset < 0) return;
    offset = split.offset;
    t.armed = false; t.writeHead = 0; t.fillFrames = 0;
  }
  const remaining = t.record.length - t.writeHead;
  const end = Math.min(count, (t.captureEndFrame ?? Infinity) - firstFrame);
  const n = Math.max(0, Math.min(end - offset, remaining));
  t.record.set(data.subarray(offset, offset + n), t.writeHead);
  t.writeHead += n; t.fillFrames = t.writeHead;
  if (t.captureEndFrame !== null && firstFrame + count >= t.captureEndFrame) finishRecording(t);
}
// finishRecording later-take subset: tile its chosen whole-bar window, then commit master length.
// MIRRORS: src/audio/looper/machine.ts@250-271 sha256:3dc46b2b25d96a01  (finishCapture: completion owns recorder release)
// Models the clean later-recording completion and its shared dispatcher release.
function finishRecording(t) {
  if (activeRecordIndex !== t.index) return;
  const takeFrames = Math.min(t.writeHead, t.captureEndFrame - t.captureStartFrame);
  commitLaterTake(t.record, takeFrames, FPB, MASTER);
  t.writeHead = MASTER;
  t.lengthFrames = MASTER; t.fillFrames = MASTER; t.armed = false; t.state = 'PLAYING';
  releaseRecorderState(t);
}
// MIRRORS: src/audio/looper/machine.ts@608-613 sha256:f1ac00fd5fc132e0  (stopCapture: armed capture delegates to stop)
// MIRRORS: src/audio/looper/machine.ts@820-851 sha256:63141d8ed4fdc7cc  (stop: capture abort and true-blank reset)
// stopRecording — OLD (buggy): always pad+commit
function stopRecording_OLD(t) {
  if (t.writeHead < MASTER) t.record.fill(0, t.writeHead, MASTER);
  t.writeHead = MASTER; finishRecording(t);
}
// Only the armed-abort branch is modelled here. Manual capture deadlines run in record-stop-window.mjs.
function stopRecording_NEW(t, tracks = [t]) {
  if (t.armed) {
    stop(t, tracks);
    return;
  }
  throw new Error('This model only exercises stop during arm');
}
// MIRRORS: src/audio/looper/machine.ts@380-401 sha256:3be20034b52c094c  (releaseRecorderState: only the owner resets capture state)
function releaseRecorderState(t) {
  if (activeRecordIndex !== t.index) return;
  activeRecordIndex = -1;
  pendingRecordStartFrame = 0;
  t.captureStartFrame = null;
  t.captureEndFrame = null;
  if (masterFrames === 0) bpmLocked = false;
}
// stop() capture abort: reset the shared arm and wipe only an uncommitted RECORDING take.
function stop(t, tracks = [t]) {
  if (t.state === 'EMPTY') return;
  t.stopAt = null;
  const discardUncommitted = t.state === 'RECORDING' && t.lengthFrames === 0;
  if (t.state === 'RECORDING' || t.state === 'OVERDUBBING') {
    t.armed = false;
    releaseRecorderState(t);
  }
  if (discardUncommitted) {
    t.record.fill(0); t.writeHead = 0; t.fillFrames = 0; t.peakCount = 0;
  }
  t.state = t.lengthFrames > 0 ? 'STOPPED' : 'EMPTY';
  if (discardUncommitted) resetMasterIfBlank(tracks);
}

console.log('=== 1. Reproduce the BUG (old code): stop a still-armed later track ===');
{
  const t = makeTrack();
  armLater(t, 1, 12000);                 // armed, waiting ~12000 frames for the boundary
  // user feeds a couple of pre-boundary batches, then stops BEFORE the boundary
  consume(t, new Float32Array(4096).fill(0.5)); // pre-boundary, discarded; still armed
  ok('old: still armed before boundary', t.armed === true && t.writeHead === 0);
  stopRecording_OLD(t);
  ok('old BUG: committed a full-length loop', t.lengthFrames === MASTER && t.state === 'PLAYING');
  ok('old BUG: that loop is ENTIRELY SILENT', allZero(t.record), 'dead silent track occupies the slot');
  console.log(`    old: state=${t.state} length=${t.lengthFrames} silent=${allZero(t.record)}  <-- dead silent loop`);
}

console.log('=== 2. The FIX: stop a still-armed later track aborts to EMPTY ===');
{
  pendingRecordStartFrame = 0; activeRecordIndex = -1;
  const t = makeTrack();
  armLater(t, 1, 12000);
  consume(t, new Float32Array(4096).fill(0.5));
  ok('new: still armed before boundary', t.armed === true);
  stopRecording_NEW(t);
  ok('new FIX: aborts to EMPTY (no dead loop)', t.state === 'EMPTY' && t.lengthFrames === 0);
  ok('new FIX: pendingRecordStartFrame reset', pendingRecordStartFrame === 0);
  ok('new FIX: activeRecordIndex released', activeRecordIndex === -1);
  ok('new FIX: blank session resets the old master grid', masterFrames === 0 && bpmLocked === false);
  console.log(`    new: state=${t.state} length=${t.lengthFrames} pending=${pendingRecordStartFrame}  <-- clean abort`);
}

console.log('=== 3. Regression: the NORMAL later-track take still records real audio (fix is scoped) ===');
{
  pendingRecordStartFrame = 0; activeRecordIndex = -1;
  const t = makeTrack();
  armLater(t, 1, 5000);
  // straddling batch: 5000 pre-boundary frames discarded, the rest begin the take at frame 0
  const batch = new Float32Array(5000 + MASTER); batch.fill(0.7);
  consume(t, batch); // crosses boundary, then fills exactly MASTER
  ok('normal: committed PLAYING', t.state === 'PLAYING' && t.lengthFrames === MASTER);
  ok('normal: take has REAL audio (not silent)', anyNonZero(t.record));
  // (0.7 stored in a Float32Array rounds to ~0.69999998, so compare with tolerance, not ===)
  ok('normal: take starts at frame 0 with content', t.record[0] > 0.6 && t.record[MASTER - 1] > 0.6);
  ok('normal: arm flag cleared', t.armed === false);
  console.log(`    normal: state=${t.state} length=${t.lengthFrames} hasAudio=${anyNonZero(t.record)}`);
}

console.log('=== 3b. A short later-track take tiles across the committed master region ===');
{
  pendingRecordStartFrame = 0; activeRecordIndex = -1;
  const t = makeTrack();
  armLater(t, 1, 5000);
  t.captureEndFrame = t.captureStartFrame + FPB;
  const batch = new Float32Array(5000 + FPB); batch.fill(0.4);
  consume(t, batch);
  ok('short take commits at master length', t.state === 'PLAYING' && t.lengthFrames === MASTER);
  ok('short take audio repeats through the final master frame', t.record[0] > 0.3 && t.record[MASTER - 1] > 0.3);
}

console.log('=== 4. The ▶/■ stop() path also resets the shared arm-count ===');
{
  pendingRecordStartFrame = 9999; activeRecordIndex = 2; masterFrames = MASTER; bpmLocked = true;
  const t = makeTrack();
  t.state = 'RECORDING'; t.armed = true; t.writeHead = 32; t.fillFrames = 32; t.peakCount = 1;
  t.record.fill(0.8, 0, 32);
  stop(t);
  ok('stop(): armed cleared', t.armed === false);
  ok('stop(): pendingRecordStartFrame reset', pendingRecordStartFrame === 0);
  ok('stop(): EMPTY (no length)', t.state === 'EMPTY');
  ok('stop(): uncommitted PCM cleared', allZero(t.record));
  ok('stop(): write/fill/peaks reset', t.writeHead === 0 && t.fillFrames === 0 && t.peakCount === 0);
  ok('stop(): blank session resets the old master grid', masterFrames === 0 && bpmLocked === false);
}

console.log('=== 5. STOP preserves committed PCM ===');
{
  masterFrames = MASTER; bpmLocked = true;
  const t = makeTrack();
  t.state = 'OVERDUBBING'; t.lengthFrames = MASTER; t.record.fill(0.6);
  stop(t);
  ok('stop(): committed track becomes STOPPED', t.state === 'STOPPED');
  ok('stop(): committed PCM survives', anyNonZero(t.record));
  ok('stop(): committed track keeps the master grid', masterFrames === MASTER && bpmLocked === true);
}

{
  const t = makeTrack();
  t.state = 'PLAYING'; t.lengthFrames = MASTER; t.stopAt = 10;
  stop(t);
  ok('immediate stop cancels a pending loop-end stop', t.stopAt === null && t.state === 'STOPPED');
}
{
  masterFrames = MASTER; bpmLocked = true;
  const recording = makeTrack(1), other = makeTrack(3);
  armLater(recording, 1, 12000);
  const start = recording.captureStartFrame, end = recording.captureEndFrame;
  releaseRecorderState(other);
  ok('another lane cannot release the recording window', activeRecordIndex === 1 && recording.captureStartFrame === start && recording.captureEndFrame === end && pendingRecordStartFrame === 12000);
  resetMasterIfBlank([recording, other]);
  ok('an in-flight lane preserves its master grid', masterFrames === MASTER && bpmLocked);
  stop(recording, [recording, other]);
  ok('owner cancellation resets the now-blank session', activeRecordIndex === -1 && masterFrames === 0 && !bpmLocked);
}
console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

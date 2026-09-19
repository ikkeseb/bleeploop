// Deterministic FIXED recording model. Pure bar/arm/commit arithmetic is imported from production.
// The model covers the clean-stream timestamp append, manual shortening and recorder/BPM cleanup.
// It does not execute AUTO detection, audio scheduling, loss rejection or public dispatchers.
// Those are exercised by golden-jam.mjs and record-stop-window.mjs.
// MIRRORS: src/audio/looper/machine.ts@277-284 sha256:6ea9bda3068d34ac  (configureRecordingEnd: fixed target fits whole bars)
// MIRRORS: src/audio/looper/capture.ts@331-375 sha256:72293488834b3bda  (consume: append and exclusive timestamp completion; overdub omitted)
// MIRRORS: src/audio/looper/machine.ts@355-376 sha256:3be20034b52c094c  (releaseRecorderState: owner guard and BPM unlock)
// MIRRORS: src/audio/looper/machine.ts@584-621 sha256:808f13bb29cd043f  (stopCapture: shorten the timestamp window and finish when drained)

import { armSplitAt, countInArm, planCommit, planFreeStop } from '../src/audio/looper/grid-math.ts';
import { framesPerBar } from '../src/audio/quantize.ts';
let fails = 0, checks = 0;
const approx = (a, b, eps) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }
const FLT = 1e-6;
const MAX_FIXED_BARS = 32;
function clampBars(n) { return Math.max(1, Math.min(MAX_FIXED_BARS, Math.round(n))); }

function armFixed(now, bpm, sr, bars, bufferLen) {
  const { beatPeriod, recordStart, pendingFrames: pending } = countInArm(now, bpm, sr);
  const fpb = framesPerBar(bpm, sr);
  const target = clampBars(bars) * fpb;
  const maxBars = Math.max(1, Math.floor(bufferLen / fpb));
  const useBars = Math.min(clampBars(bars), maxBars);
  const targetFrames = useBars * fpb;
  return { beatPeriod, recordStart, pending, target, fpb, maxBars, useBars, targetFrames, bpmLocked: true };
}
function makeTrack(cap) {
  return { state: 'RECORDING', armed: false, writeHead: 0, fillFrames: 0, lengthFrames: 0,
           record: new Float32Array(cap) };
}
// Test fixture origins start at frame zero. targetFrames is an input, not retained engine state.
function captureState(t, { pending = 0, targetFrames = 0, activeRecordIndex = 0, ...rest }) {
  t.index = activeRecordIndex;
  return { pending, activeRecordIndex, captureStartFrame: pending,
    captureEndFrame: pending + (targetFrames || t.record.length),
    frame: t.armed ? 0 : pending + t.writeHead, ...rest };
}
function releaseRecorderState(state, i) {
  if (state.activeRecordIndex !== i) return;
  state.activeRecordIndex = -1;
  state.pending = 0;
  state.captureStartFrame = null;
  state.captureEndFrame = null;
  if (state.master === 0) state.bpmLocked = false;
}
function consumeFirst(state, t, data) {
  const count = data.length;
  const firstFrame = state.frame;
  state.frame += count;
  let offset = 0;
  if (t.armed) {
    const split = armSplitAt(state.captureStartFrame, firstFrame, count);
    state.pending = split.pending;
    if (split.offset < 0) return;
    offset = split.offset;
    t.armed = false; t.writeHead = 0; t.fillFrames = 0;
  }
  const end = Math.min(count, state.captureEndFrame - firstFrame);
  const n = Math.max(0, Math.min(end - offset, t.record.length - t.writeHead));
  t.record.set(data.subarray(offset, offset + n), t.writeHead);
  t.writeHead += n; t.fillFrames = t.writeHead;
  if (firstFrame + count >= state.captureEndFrame) finishRecording(state, t, state.bpmAtCommit, state.srAtCommit);
}
// MIRRORS: src/audio/looper/machine.ts@233-254 sha256:3dc46b2b25d96a01  (finishCapture: completion owns recorder release)
// Models the clean recording completion, including its shared dispatcher release.
function finishRecording(state, t, bpm, sr) {
  if (state.activeRecordIndex !== t.index) return;
  const raw = Math.min(t.writeHead, state.captureEndFrame - state.captureStartFrame);
  const plan = planCommit(raw, bpm, sr, t.record.length);
  state.master = plan.master; state.masterLen = plan.master; state.bars = plan.bars; state.derivedBpm = plan.derivedBpm;
  state.bpmLocked = true;
  if (raw < plan.master) t.record.fill(0, raw, plan.master);
  t.writeHead = plan.master; t.lengthFrames = plan.master; t.fillFrames = plan.master; t.state = 'PLAYING';
  releaseRecorderState(state, t.index);
  state.committed = true;
}
function stopCapture(state, t, bpm, sr, nowFrame = state.captureStartFrame + t.writeHead) {
  if (t.armed) { stopAbort(state, t); return; }
  const { bars, target } = planFreeStop((nowFrame - state.captureStartFrame) / sr, bpm, sr, t.record.length);
  const end = bars >= 1 ? state.captureStartFrame + target : nowFrame;
  state.captureEndFrame = Math.min(state.captureEndFrame, end);
  if (state.frame >= state.captureEndFrame) finishRecording(state, t, bpm, sr);
}
function stopAbort(state, t) {
  if (t.state === 'RECORDING' || t.state === 'OVERDUBBING') {
    const wasCountIn = t.state === 'RECORDING' && state.master === 0;
    t.armed = false;
    releaseRecorderState(state, t.index);
    if (wasCountIn) state.countPulseTornDown = true;
    if (t.lengthFrames === 0) { t.record.fill(0); t.writeHead = 0; t.fillFrames = 0; }
    t.state = t.lengthFrames > 0 ? 'STOPPED' : 'EMPTY';
  }
}

// Helper: feed a [pending of 0.5][takeLen of 0.7] stream through consumeFirst in the given batch sizes.
function runFixedCapture(arm, takeLen, batchSizes, bpm, sr, bufferLen) {
  const cap = bufferLen || (arm.pending + takeLen + 8192); // model the real record buffer when given
  const t = makeTrack(cap); t.armed = true; t.state = 'RECORDING';
  const state = captureState(t, { pending: arm.pending, targetFrames: arm.targetFrames, activeRecordIndex: 7,
                  master: 0, committed: false, masterLen: 0, bpmLocked: arm.bpmLocked,
                  bpmAtCommit: bpm, srAtCommit: sr });
  const stream = new Float32Array(arm.pending + takeLen);
  stream.fill(0.5, 0, arm.pending);
  stream.fill(0.7, arm.pending, arm.pending + takeLen);
  let pos = 0, bi = 0;
  while (pos < stream.length && !state.committed) {
    const bs = Math.min(batchSizes[bi++ % batchSizes.length], stream.length - pos);
    consumeFirst(state, t, stream.subarray(pos, pos + bs));
    pos += bs;
  }
  return { t, state };
}

// ════════════════════════════════════════════════════════════════════════════════════════════
console.log('=== A. target = bars*framesPerBar(bpm,sr); clamp to buffer ===');
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 90, 137, 100, 73]) {
    for (const bars of [1, 2, 4, 8, 16]) {
      const bufferLen = Math.ceil(60 * sr); // MAX_LOOP_SECONDS
      const arm = armFixed(1000, bpm, sr, bars, bufferLen);
      ok(`A target=bars*fpb bpm=${bpm} sr=${sr} bars=${bars}`,
         arm.target === bars * framesPerBar(bpm, sr), `target=${arm.target}`);
      ok(`A unclamped (fits buffer) bpm=${bpm} bars=${bars}`, arm.targetFrames === arm.target);
      ok(`A bpm locked at press bpm=${bpm}`, arm.bpmLocked === true);
    }
  }
}
{
  // Extreme: 32 bars @ 40 bpm @ 48k = 32*4*1.5*48000 = 9.216M frames > 60s buffer (2.88M) -> clamp DOWN
  // to whole bars that fit (NOT a raw min-to-buffer, which would leave a non-bar-aligned target).
  const sr = 48000, bufferLen = Math.ceil(60 * sr);
  const arm = armFixed(1000, 40, sr, 32, bufferLen);
  const fpb = framesPerBar(40, sr);
  ok('A extreme over-long clamps to whole bars that fit',
     arm.target > bufferLen && arm.targetFrames === Math.floor(bufferLen / fpb) * fpb && arm.targetFrames <= bufferLen,
     `targetFrames=${arm.targetFrames} target=${arm.target} buf=${bufferLen}`);
  ok('A extreme: targetFrames is a whole-bar multiple', arm.targetFrames % fpb === 0);
}
{
  // Bars selector clamps to [1, 32].
  ok('A clampBars(0)=1', clampBars(0) === 1);
  ok('A clampBars(-3)=1', clampBars(-3) === 1);
  ok('A clampBars(99)=32', clampBars(99) === 32);
  ok('A clampBars(4.6)=5 (rounds)', clampBars(4.6) === 5);
}

console.log('=== B. consume caps at EXACTLY target + auto-commits frame-exact across batch regimes ===');
for (const [sr, bpm, bars] of [[48000, 120, 4], [44100, 100, 2], [48000, 90, 8], [44100, 137, 1]]) {
  const arm = armFixed(1000, bpm, sr, bars, Math.ceil(60 * sr));
  const target = arm.targetFrames;
  const overplay = target + 12000; // user keeps playing past N bars — the extra MUST be dropped
  for (const [label, sizes] of [
    ['mid-batch (128 quanta)', [128]],
    ['count edge then take edge', [arm.pending, target, 9999]],
    ['ragged drain', [1024, 333, 5000, 128, 20000, 777]],
    ['one giant batch straddling count+take+overshoot', [arm.pending + overplay]],
  ]) {
    const { t, state } = runFixedCapture(arm, overplay, sizes, bpm, sr);
    ok(`B auto-committed [${label}] bpm=${bpm} bars=${bars}`, state.committed === true);
    ok(`B writeHead == target (${target}) [${label}]`, t.writeHead === target, `writeHead=${t.writeHead}`);
    ok(`B master == target [${label}]`, state.masterLen === target, `master=${state.masterLen}`);
    ok(`B bars derived == ${bars} [${label}]`, state.bars === bars, `bars=${state.bars}`);
    ok(`B frame 0 is the TAKE not the count [${label}]`, approx(t.record[0], 0.7, FLT), `record[0]=${t.record[0]}`);
    ok(`B last in-loop frame present [${label}]`, approx(t.record[target - 1], 0.7, FLT));
    ok(`B nothing past target written [${label}]`, t.record[target] === 0, `record[target]=${t.record[target]}`);
    ok(`B ring released (activeRecordIndex=-1) [${label}]`, state.activeRecordIndex === -1);
    ok(`B bpm STILL locked after commit [${label}]`, state.bpmLocked === true);
    // No count-bar (0.5) leak anywhere in the recorded loop — no head dead air.
    let leak = false;
    for (let k = 0; k < target; k++) if (approx(t.record[k], 0.5, FLT)) { leak = true; break; }
    ok(`B no count-bar leak (no head dead air) [${label}]`, !leak);
  }
}

console.log('=== C. finishRecording with raw==N*fpb yields master==N*fpb exactly; derived bpm ~= press ===');
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 90, 137, 100, 73, 200]) {
    for (const bars of [1, 2, 3, 4, 7, 16]) {
      const fpb = framesPerBar(bpm, sr);
      const raw = bars * fpb; // what writeHead equals at fixed-length auto-stop
      const t = makeTrack(raw + 16); t.writeHead = raw;
      const state = captureState(t, { master: 0, targetFrames: raw, committed: false, bpmLocked: true });
      finishRecording(state, t, bpm, sr);
      ok(`C master == N*fpb bpm=${bpm} sr=${sr} bars=${bars}`, state.masterLen === raw, `master=${state.masterLen} raw=${raw}`);
      ok(`C bars round-trips to ${bars}`, state.bars === bars, `bars=${state.bars}`);
      // derivedBpm ~= the integer press bpm (round() in framesPerBar introduces <0.5 frame/bar error).
      ok(`C derived bpm ~= press bpm=${bpm}`, approx(state.derivedBpm, bpm, 0.05), `derived=${state.derivedBpm.toFixed(4)}`);
    }
  }
}

console.log('=== D. abort DURING the count-in (armed) -> EMPTY, fixed arm cleared, bpm unlocked ===');
{
  const arm = armFixed(1000, 120, 48000, 4, Math.ceil(60 * 48000));
  const t = makeTrack(arm.pending + 96000); t.armed = true; t.state = 'RECORDING'; t.lengthFrames = 0;
  const state = captureState(t, { pending: arm.pending, targetFrames: arm.targetFrames, activeRecordIndex: 3,
                  master: 0, masterLen: 0, committed: false, bpmLocked: true, countPulseTornDown: false });
  consumeFirst(state, t, new Float32Array(4096).fill(0.5)); // some discarded count frames
  ok('D still armed before come-in', t.armed === true && t.writeHead === 0);
  stopCapture(state, t, 120, 48000);                       // master===0 -> count-in abort
  ok('D aborts to EMPTY (no committed loop)', t.state === 'EMPTY' && t.lengthFrames === 0);
  ok('D fixed arm cleared', state.captureStartFrame === null && state.captureEndFrame === null);
  ok('D bpm UNLOCKED on abort', state.bpmLocked === false);
  ok('D count pulse torn down', state.countPulseTornDown === true);
  ok('D ring released', state.activeRecordIndex === -1);
  ok('D record buffer silent (no dead loop committed)', t.record.every((x) => x === 0));
}

console.log('=== E. abort DURING the take (not armed, master===0) -> EMPTY, arm cleared, bpm unlocked ===');
{
  // The come-in happened (armed cleared), the take is partway, then the user hits stop() directly.
  const arm = armFixed(1000, 120, 48000, 4, Math.ceil(60 * 48000));
  const t = makeTrack(arm.targetFrames + 16); t.armed = false; t.state = 'RECORDING'; t.lengthFrames = 0;
  t.writeHead = 20000; t.fillFrames = 20000; // mid-take
  const state = captureState(t, { pending: 0, targetFrames: arm.targetFrames, activeRecordIndex: 3,
                  master: 0, masterLen: 0, committed: false, bpmLocked: true, countPulseTornDown: false });
  stopAbort(state, t);
  ok('E aborts to EMPTY', t.state === 'EMPTY');
  ok('E fixed arm cleared', state.captureStartFrame === null && state.captureEndFrame === null);
  ok('E bpm UNLOCKED on take abort', state.bpmLocked === false);
  ok('E count pulse torn down (broadened wasCountIn covers mid-take)', state.countPulseTornDown === true);
}

console.log('=== F. manual early stop mid-take -> commit partial (quantized), arm cleared, bpm STAYS locked ===');
{
  const sr = 48000, bpm = 120;
  const fpb = framesPerBar(bpm, sr);
  // User set 4 bars but hits REC again after ~2.4 bars of take. stopCapture (not armed, master===0)
  // commits the partial raw quantized to whole bars.
  const raw = Math.round(2.4 * fpb);
  const t = makeTrack(4 * fpb + 16); t.armed = false; t.state = 'RECORDING'; t.writeHead = raw; t.fillFrames = raw;
  const state = captureState(t, { pending: 0, targetFrames: 4 * fpb, activeRecordIndex: 1,
                  master: 0, masterLen: 0, committed: false, bpmLocked: true });
  stopCapture(state, t, bpm, sr);
  ok('F committed', state.committed === true);
  ok('F quantized to 2 bars (floor(2.4)=2)', state.bars === 2, `bars=${state.bars}`);
  ok('F master == 2*fpb', state.masterLen === 2 * fpb);
  ok('F fixed arm cleared', state.captureStartFrame === null && state.captureEndFrame === null);
  ok('F bpm STAYS locked (a master now exists)', state.bpmLocked === true);
}

console.log('=== G. Regression: free-record below its capacity does not auto-commit ===');
{
  const arm = armFixed(1000, 120, 48000, 4, Math.ceil(60 * 48000));
  // Free-record: targetFrames = 0. Feed count + a long take; consume must NOT auto-commit, just append.
  const t = makeTrack(arm.pending + 200000); t.armed = true; t.state = 'RECORDING';
  const state = captureState(t, { pending: arm.pending, targetFrames: 0, activeRecordIndex: 4,
                  master: 0, committed: false, bpmAtCommit: 120, srAtCommit: 48000 });
  const stream = new Float32Array(arm.pending + 150000);
  stream.fill(0.5, 0, arm.pending);
  stream.fill(0.7, arm.pending, stream.length);
  let pos = 0; while (pos < stream.length) { const bs = Math.min(4096, stream.length - pos); consumeFirst(state, t, stream.subarray(pos, pos + bs)); pos += bs; }
  ok('G free-record did NOT auto-commit', state.committed === false);
  ok('G free-record appended whole take (150000)', t.writeHead === 150000, `writeHead=${t.writeHead}`);
  ok('G free-record frame 0 is the take', approx(t.record[0], 0.7, FLT));
  ok('G free-record ring still held', state.activeRecordIndex === 4);
}

console.log('=== H. Buffer-clamp path (the bug the 5-lens review found + the fix) ===');
{
  // The exact combo the review flagged: 42 bpm, 32 bars, 44.1k -> requested target far over the 60s buffer.
  // PRE-FIX bug: Math.min(target, bufferLen) left a non-bar-aligned target; finishRecording's round()
  // rounded UP past the buffer -> master > record.length -> zero-padded tail + a later-track RangeError.
  // FIX 1 (startRecording): clamp DOWN to whole bars that fit. FIX 2 (finishRecording): defensive maxBars.
  const sr = 44100, bpm = 42, reqBars = 32;
  const bufferLen = Math.ceil(60 * sr);          // the real per-track record buffer (looper/capture.ts buildEngine)
  const fpb = framesPerBar(bpm, sr);
  const arm = armFixed(1000, bpm, sr, reqBars, bufferLen);
  ok('H requested target DID exceed the buffer (clamp engaged)', arm.target > bufferLen, `target=${arm.target} buf=${bufferLen}`);
  ok('H targetFrames is a whole-bar multiple', arm.targetFrames % fpb === 0, `targetFrames=${arm.targetFrames} fpb=${fpb}`);
  ok('H targetFrames <= buffer', arm.targetFrames <= bufferLen);
  ok('H useBars = largest whole-bar count that fits', arm.targetFrames === Math.floor(bufferLen / fpb) * fpb);

  // Feed it through the real capture + auto-commit with the buffer-sized track; assert no overshoot.
  const { state } = runFixedCapture(arm, arm.targetFrames + 5000, [128], bpm, sr, bufferLen);
  ok('H auto-committed', state.committed === true);
  ok('H master == targetFrames (exact, no round-up)', state.masterLen === arm.targetFrames, `master=${state.masterLen}`);
  ok('H master is a whole-bar multiple', state.masterLen % fpb === 0);
  ok('H master <= buffer (no overrun, no zero-pad tail)', state.masterLen <= bufferLen);

  // FIX 2 in isolation: a 60s+ FREE-record fills the buffer to a NON-multiple raw (= bufferLen). The
  // defensive maxBars clamp in finishRecording must keep master <= buffer (pre-fix it rounded UP past it).
  ok('H precondition: bufferLen is NOT a whole-bar multiple (round() would overshoot)', bufferLen % fpb !== 0);
  const tFree = makeTrack(bufferLen); tFree.writeHead = bufferLen;
  const sFree = captureState(tFree, { master: 0, targetFrames: 0, committed: false, masterLen: 0, bpmLocked: false });
  finishRecording(sFree, tFree, bpm, sr);
  ok('H free-record overshoot: master <= buffer (defensive clamp)', sFree.masterLen <= bufferLen, `master=${sFree.masterLen} buf=${bufferLen}`);
  ok('H free-record overshoot: master is a whole-bar multiple', sFree.masterLen % fpb === 0);

  // The REGRESSION the bug caused: a LATER track records `master` frames into a buffer-sized Float32Array.
  // With master > buffer this threw RangeError inside drainTick; with master <= buffer it's safe.
  for (const master of [state.masterLen, sFree.masterLen]) {
    const laterRec = new Float32Array(bufferLen);
    let threw = false;
    try {
      let wh = 0;
      while (wh < master) {
        const n = Math.min(128, master - wh);
        laterRec.set(new Float32Array(n), wh); // mirrors consume() later-track t.record.set(...)
        wh += n;
      }
    } catch { threw = true; }
    ok(`H later-track fill of master=${master} frames does NOT throw (no RangeError)`, threw === false);
  }
}

console.log('=== I. Manual FIXED stop tightens its end once and waits for the missing tail ===');
{
  const sr = 48000, bpm = 120, fpb = framesPerBar(bpm, sr);
  const t = makeTrack(4 * fpb);
  const state = captureState(t, { targetFrames: 4 * fpb, master: 0, committed: false,
    bpmLocked: true, bpmAtCommit: bpm, srAtCommit: sr });
  consumeFirst(state, t, new Float32Array(2 * fpb - 5000).fill(0.5));
  stopCapture(state, t, bpm, sr, 2 * fpb + 100);
  ok('I manual stop replaces the longer automatic end', state.captureEndFrame === 2 * fpb);
  ok('I missing tail keeps recorder and lock', !state.committed && state.activeRecordIndex === t.index && state.bpmLocked);
  stopCapture(state, t, bpm, sr, 3 * fpb + 100);
  ok('I repeated stop cannot extend the end', state.captureEndFrame === 2 * fpb);
  consumeFirst(state, t, new Float32Array(6000).fill(0.7));
  ok('I tail completes exactly two bars', state.committed && t.lengthFrames === 2 * fpb);
  ok('I retained tail survives up to the last frame', approx(t.record[2 * fpb - 1], 0.7, FLT));
  ok('I no audio beyond the shortened end', t.record[2 * fpb] === 0);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

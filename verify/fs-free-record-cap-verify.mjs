// Clean-stream free-record capacity regression. The take has an absolute end at start+capacity,
// and timestamped capture commits when that end arrives. A non-bar-sized capacity commits the
// largest whole-bar region that fits. The historical fixed-only commit gate is a counterexample.
// This deterministic model omits count-in, loss rejection and playback scheduling.
import { planCommit } from '../src/audio/looper/grid-math.ts';
let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }
const approx = (a, b, eps = 1e-6) => Math.abs(a - b) <= eps;

// ── Constants / helpers mirrored from looper/state.ts ────────────────────────────────────────────
const MAX_LOOP_SECONDS = 60;        // looper/state.ts
function framesPerBar(bpm, sr) { return Math.round((60 / bpm) * 4 * sr); }

// MIRRORS: src/audio/looper/machine.ts@297-306 sha256:6e4955535e889e2c  (configureRecordingEnd: free-record capacity or fixed bar end)
// MIRRORS: src/audio/looper/capture.ts@334-375 sha256:c1bc32e9ca3a9e9d  (consume: recording timestamp append/completion; overdub omitted)
function consumeFirst(state, t, data) {
  const count = data.length;
  const firstFrame = state.frame;
  state.frame += count;
  const end = Math.min(count, state.captureEndFrame - firstFrame);
  const n = Math.max(0, Math.min(end, t.record.length - t.writeHead));
  t.record.set(data.subarray(0, n), t.writeHead);
  t.writeHead += n;
  t.fillFrames = t.writeHead;
  state.droppedFrames += count - n;
  if (firstFrame + count >= state.captureEndFrame) {
    state.activeRecordIndex = -1;
    state.autoCapHit = state.captureEndFrame - state.captureStartFrame === t.record.length;
    finishRecording(state, t);
    state.captureStartFrame = null;
    state.captureEndFrame = null;
  }
}

// ── consume() first-track branch BEFORE the Thread-G3 fix (the bug) — for the Section C contrast only.
// The commit was gated on fixed-length, so free-record never committed at the buffer cap.
function consumeFirstPreFix(state, t, data) {
  const count = data.length;
  const offset = 0;
  const limit = state.legacyFixedTarget > 0 ? state.legacyFixedTarget : t.record.length;
  const room = limit - t.writeHead;
  const n = Math.min(count - offset, room);
  t.record.set(data.subarray(offset, offset + n), t.writeHead);
  t.writeHead += n;
  t.fillFrames = t.writeHead;
  state.droppedFrames += (count - offset) - n;
  if (state.legacyFixedTarget > 0 && t.writeHead >= state.legacyFixedTarget) { // OLD: free-record (==0) never commits
    state.activeRecordIndex = -1;
    finishRecording(state, t);
  }
}

// First-take commit length is imported from production planCommit.
function finishRecording(state, t) {
  const raw = t.writeHead;
  const { bars, master } = planCommit(raw, state.bpm, state.sr, t.record.length);
  state.master = master; state.masterLen = master; state.bars = bars;
  t.lengthFrames = master; t.fillFrames = master; t.state = 'PLAYING';
  state.committed = true;
}

function makeTrack(cap) {
  return { state: 'RECORDING', armed: false, writeHead: 0, fillFrames: 0, lengthFrames: 0,
           record: new Float32Array(cap) };
}

// Drive a free-record take that OVERRUNS the buffer by `overFrames`, in 128-frame quanta, via `consumeFn`.
function runFreeOverrun(sr, bpm, overFrames, consumeFn) {
  const cap = Math.ceil(MAX_LOOP_SECONDS * sr);   // the real record buffer (looper/capture.ts buildEngine: ceil(60*sr))
  const t = makeTrack(cap);
  const state = { frame: 12345, captureStartFrame: 12345, captureEndFrame: 12345 + cap, legacyFixedTarget: 0, activeRecordIndex: 9, master: 0, masterLen: 0, committed: false,
                  droppedFrames: 0, autoCapHit: false, bpm, sr };
  const total = cap + overFrames;
  let pos = 0;
  const QUANTUM = 128;
  while (pos < total && !state.committed) {
    const bs = Math.min(QUANTUM, total - pos);
    const batch = new Float32Array(bs);
    for (let k = 0; k < bs; k++) batch[k] = 0.5; // non-zero audio every frame (a kept frame != silence)
    consumeFn(state, t, batch);
    pos += bs;
  }
  return { t, state, cap, total };
}

// ════════════════════════════════════════════════════════════════════════════════════════════
console.log('=== A. SHIPPED (Thread-G3 fix): free-record held past 60s AUTO-COMMITS at the buffer cap ===');
for (const [sr, bpm] of [[48000, 120], [44100, 100], [48000, 137]]) {
  const overFrames = 5 * sr; // ~5s of audio past the 60s cap that the user keeps playing
  const { t, state, cap } = runFreeOverrun(sr, bpm, overFrames, consumeFirst);
  const fpb = framesPerBar(bpm, sr);

  ok(`A auto-committed at the cap (not stuck RECORDING) sr=${sr}`, state.committed === true && state.autoCapHit === true, `committed=${state.committed}`);
  ok(`A track PLAYING sr=${sr}`, t.state === 'PLAYING');
  ok(`A ring RELEASED (activeRecordIndex=-1) sr=${sr}`, state.activeRecordIndex === -1);
  ok(`A master = largest whole-bar multiple that fits the buffer sr=${sr}`,
     state.masterLen === Math.floor(cap / fpb) * fpb && state.masterLen <= cap, `master=${state.masterLen} cap=${cap}`);
  ok(`A at most one quantum dropped before commit sr=${sr}`, state.droppedFrames <= 128, `dropped=${state.droppedFrames}`);
}

console.log('=== B. The committed loop holds the kept take intact (no corruption at the cap) ===');
{
  const sr = 48000, bpm = 120;
  const { t, state } = runFreeOverrun(sr, bpm, 2 * sr, consumeFirst);
  ok('B commit fired (writeHead reached the cap)', state.committed === true);
  ok('B kept audio intact: frame 0 is the take', approx(t.record[0], 0.5));
  ok('B kept audio intact: last in-loop frame is the take', approx(t.record[state.masterLen - 1], 0.5));
}

console.log('=== C. PRE-FIX CONTRAST: the OLD gated-commit branch silent-drops — shipped MUST differ ===');
for (const [sr, bpm] of [[48000, 120], [44100, 100]]) {
  const overFrames = 5 * sr;
  const old = runFreeOverrun(sr, bpm, overFrames, consumeFirstPreFix);
  ok(`C OLD: writeHead pinned at the cap, still RECORDING sr=${sr}`,
     old.t.writeHead === old.cap && old.t.state === 'RECORDING' && old.state.committed === false, `wh=${old.t.writeHead}`);
  ok(`C OLD: post-cap audio SILENTLY dropped (the bug) sr=${sr}`, old.state.droppedFrames === overFrames, `dropped=${old.state.droppedFrames}`);
  // The guard: the SHIPPED model commits exactly where the OLD one does not — a source revert flips A red.
  const shipped = runFreeOverrun(sr, bpm, overFrames, consumeFirst);
  ok(`C shipped DIFFERS from OLD (commits vs stuck) sr=${sr}`, shipped.state.committed === true && old.state.committed === false);
}
{
  // OLD bug was UNBOUNDED: an arbitrarily long hold dropped arbitrarily many frames, still no commit.
  const sr = 48000, bpm = 120;
  const old = runFreeOverrun(sr, bpm, 30 * sr, consumeFirstPreFix);
  ok('C OLD unbounded: 30s-past-cap still RECORDING', old.t.state === 'RECORDING');
  ok('C OLD unbounded: ~30s of frames dropped', old.state.droppedFrames === 30 * sr, `dropped=${old.state.droppedFrames}`);
}

console.log('=== D. No regression: under-cap free-record never commits; fixed-length uses its earlier timestamp end ===');
{
  // Under-cap free-record (the normal case): never reaches the cap, must NOT auto-commit.
  const sr = 48000, bpm = 120;
  const cap = Math.ceil(MAX_LOOP_SECONDS * sr);
  const t = makeTrack(cap);
  const state = { frame: 12345, captureStartFrame: 12345, captureEndFrame: 12345 + cap, legacyFixedTarget: 0, activeRecordIndex: 4, master: 0, masterLen: 0, committed: false,
                  droppedFrames: 0, autoCapHit: false, bpm, sr };
  let pos = 0; const total = 10 * sr; // 10s, well under the 60s cap
  while (pos < total) { const bs = Math.min(128, total - pos); consumeFirst(state, t, new Float32Array(bs).fill(0.5)); pos += bs; }
  ok('D under-cap free-record did NOT auto-commit', state.committed === false && state.autoCapHit === false);
  ok('D under-cap ring still held', state.activeRecordIndex === 4);
  ok('D no frames dropped under the cap', state.droppedFrames === 0);
}
{
  // A fixed take sets end=start+bars*fpb, before the capacity deadline.
  const sr = 48000, bpm = 120, bars = 2;
  const fpb = framesPerBar(bpm, sr);
  const cap = Math.ceil(MAX_LOOP_SECONDS * sr);
  const t = makeTrack(cap);
  const state = { frame: 12345, captureStartFrame: 12345, captureEndFrame: 12345 + bars * fpb, legacyFixedTarget: bars * fpb, activeRecordIndex: 4, master: 0, masterLen: 0, committed: false,
                  droppedFrames: 0, autoCapHit: false, bpm, sr };
  let pos = 0; const total = bars * fpb + 12000;
  while (pos < total && !state.committed) { const bs = Math.min(128, total - pos); consumeFirst(state, t, new Float32Array(bs).fill(0.5)); pos += bs; }
  ok('D fixed-length committed at its timestamp end, before the buffer cap', state.masterLen === bars * fpb, `master=${state.masterLen}`);
  ok('D fixed-length is NOT flagged as a free-record auto-cap', state.autoCapHit === false);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

// fs-undo-verify.mjs — deterministic guard for the one-level overdub undo/redo (looper/machine.ts).
//
// PORT of the undo state transitions (mirrors looper/machine.ts: startOverdub's undoBuf snapshot, the
// overdub sum + boundary swap, endOverdub commit, undoLastOverdub's record<->undoBuf toggle, and clear).
// A port because looper.ts (split into src/audio/looper/{state,capture,peaks,playback,machine,mixer}.ts
// + a facade on 2026-07-01) needs AudioContext/ringbuf/Tone and
// the buffers are private. The model below mirrors the SAME array ops; if you change that logic, update
// this in lockstep. Run: node fs-undo-verify.mjs
//
// Proves: undoBuf is snapshotted at startOverdub as a SEPARATE copy (overdub summing never mutates it);
// undo restores the loop to BEFORE the whole overdub session (not the last pass); undo/redo is a clean
// toggle; frame count is preserved; a fresh session re-baselines undoBuf; the guards (null buffer / wrong
// state) make undo a no-op; clear() drops the undo buffer.

import assert from 'node:assert';

let passed = 0;
let failed = 0;
function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (e) {
    failed++;
    console.error(`FAIL: ${name}\n  ${e.message}`);
  }
}
const arr = (t) => Array.from(t.record);
const eq = (t, expected) => assert.deepStrictEqual(arr(t), expected);

// MIRRORS: src/audio/looper/machine.ts@680-692 sha256:3f15be4ec4d701d7  (startOverdub — undoBuf snapshot)
// MIRRORS: src/audio/looper/machine.ts@735-753 sha256:acb253ec85a4c162  (undoLastOverdub — record<->undoBuf swap)
// ===== BEGIN PORT of looper/machine.ts undo logic =====
function mkTrack(record) {
  return { record: Float32Array.from(record), undoBuf: null, overdubBuf: null, state: 'PLAYING', master: record.length, stopAt: null };
}
function canUndo(t) {
  return t.undoBuf !== null && (t.state === 'PLAYING' || t.state === 'STOPPED');
}
function startOverdub(t) {
  const m = t.master;
  t.overdubBuf = t.record.slice(0, m); // working copy (summed into)
  t.undoBuf = t.record.slice(0, m); // SEPARATE pre-dub snapshot for undo
  t.state = 'OVERDUBBING';
}
function sumInto(t, layer) {
  // mirrors the worklet summing incoming PCM into overdubBuf while OVERDUBBING (looper/capture.ts:271-279)
  for (let k = 0; k < t.master; k++) t.overdubBuf[k] += layer[k];
}
function commitSwap(t) {
  // scheduleOverdubSwap commits the summed layer while retaining the same working copy.
  t.record.set(t.overdubBuf.subarray(0, t.master), 0);
}
function endOverdub(t) {
  t.record.set(t.overdubBuf.subarray(0, t.master), 0);
  t.overdubBuf = null;
  t.state = 'PLAYING';
}
function undoLastOverdub(t) {
  if (!t.undoBuf) return;
  if (t.stopAt !== null) return;
  if (t.state !== 'PLAYING' && t.state !== 'STOPPED') return;
  const prev = t.record.slice(0, t.master);
  t.record.set(t.undoBuf.subarray(0, t.master), 0);
  t.undoBuf = prev;
}
function clear(t) {
  t.record.fill(0);
  t.overdubBuf = null;
  t.undoBuf = null;
  t.state = 'EMPTY';
}
// ===== END PORT =====

// ---------------------------------------------------------------------------
// 1. Single overdub pass: snapshot is separate, summing doesn't touch undoBuf.
// ---------------------------------------------------------------------------
let t = mkTrack([1, 2, 3, 4]);
check('fresh take: nothing to undo', () => assert.strictEqual(canUndo(t), false));
startOverdub(t);
check('startOverdub snapshots pre-dub into undoBuf', () =>
  assert.deepStrictEqual(Array.from(t.undoBuf), [1, 2, 3, 4]));
sumInto(t, [10, 10, 10, 10]);
check('summing mutates overdubBuf', () => assert.deepStrictEqual(Array.from(t.overdubBuf), [11, 12, 13, 14]));
check('summing does NOT mutate undoBuf (separate copy)', () =>
  assert.deepStrictEqual(Array.from(t.undoBuf), [1, 2, 3, 4]));
check('summing does NOT mutate record yet', () => eq(t, [1, 2, 3, 4]));
endOverdub(t);
check('endOverdub commits summed layer to record', () => eq(t, [11, 12, 13, 14]));
check('canUndo true after a committed overdub (PLAYING)', () => assert.strictEqual(canUndo(t), true));

// ---------------------------------------------------------------------------
// 2. Undo / redo toggle, frame count preserved.
// ---------------------------------------------------------------------------
undoLastOverdub(t);
check('undo restores the pre-dub loop', () => eq(t, [1, 2, 3, 4]));
check('undo length preserved', () => assert.strictEqual(t.record.length, 4));
undoLastOverdub(t);
check('redo (second undo) returns to the dubbed loop', () => eq(t, [11, 12, 13, 14]));
undoLastOverdub(t);
check('third toggle = undo again', () => eq(t, [1, 2, 3, 4]));

// ---------------------------------------------------------------------------
// 3. Multi-pass session: undo restores to BEFORE the whole session, not the last pass.
// ---------------------------------------------------------------------------
t = mkTrack([11, 12, 13, 14]);
startOverdub(t); // undoBuf = [11,12,13,14]
sumInto(t, [1, 1, 1, 1]);
commitSwap(t); // mid-session boundary -> record = [12,13,14,15]
check('mid-session commit updated record', () => eq(t, [12, 13, 14, 15]));
check('undoBuf still holds the PRE-SESSION loop after a mid-session commit', () =>
  assert.deepStrictEqual(Array.from(t.undoBuf), [11, 12, 13, 14]));
sumInto(t, [1, 1, 1, 1]);
endOverdub(t); // record = [13,14,15,16]
check('end of multi-pass session committed', () => eq(t, [13, 14, 15, 16]));
undoLastOverdub(t);
check('undo a multi-pass session restores the pre-SESSION loop (whole layer removed)', () =>
  eq(t, [11, 12, 13, 14]));

// ---------------------------------------------------------------------------
// 4. A fresh overdub session re-baselines undoBuf to the current record.
// ---------------------------------------------------------------------------
t = mkTrack([1, 1, 1, 1]);
startOverdub(t);
sumInto(t, [2, 2, 2, 2]);
endOverdub(t); // record = [3,3,3,3], undoBuf = [1,1,1,1]
startOverdub(t); // new session -> undoBuf re-baselines to [3,3,3,3]
check('new session re-baselines undoBuf to current record', () =>
  assert.deepStrictEqual(Array.from(t.undoBuf), [3, 3, 3, 3]));
sumInto(t, [1, 1, 1, 1]);
endOverdub(t); // record = [4,4,4,4]
undoLastOverdub(t);
check('undo after a re-baseline restores the prior committed loop', () => eq(t, [3, 3, 3, 3]));

// ---------------------------------------------------------------------------
// 5. Guards: no undo buffer / wrong state -> no-op. Buffers are distinct (no aliasing).
// ---------------------------------------------------------------------------
t = mkTrack([5, 6, 7, 8]);
undoLastOverdub(t);
check('undo with no undoBuf is a no-op', () => eq(t, [5, 6, 7, 8]));
startOverdub(t);
sumInto(t, [1, 1, 1, 1]);
check('undo while OVERDUBBING is a no-op (record untouched mid-take)', () => {
  const before = arr(t);
  undoLastOverdub(t);
  assert.deepStrictEqual(arr(t), before);
});
check('canUndo false while OVERDUBBING', () => assert.strictEqual(canUndo(t), false));
endOverdub(t);
// distinctness: mutating record must not bleed into undoBuf, and vice versa
t.record[0] = 999;
check('record and undoBuf are distinct arrays (no shared backing)', () =>
  assert.notStrictEqual(t.undoBuf[0], 999));

// ---------------------------------------------------------------------------
// 6. STOPPED is undoable; clear() drops the undo buffer.
// ---------------------------------------------------------------------------
t = mkTrack([1, 2, 3, 4]);
startOverdub(t);
sumInto(t, [1, 1, 1, 1]);
endOverdub(t);
t.state = 'STOPPED';
check('canUndo true while STOPPED', () => assert.strictEqual(canUndo(t), true));
undoLastOverdub(t);
check('undo works while STOPPED (buffer swapped in place)', () => eq(t, [1, 2, 3, 4]));
clear(t);
check('clear() drops the undo buffer', () => assert.strictEqual(t.undoBuf, null));
check('canUndo false after clear (EMPTY)', () => assert.strictEqual(canUndo(t), false));

check('undo cannot replace a source waiting for loop-end stop', () => {
  const pending = mkTrack([1, 2, 3, 4]);
  pending.undoBuf = Float32Array.from([4, 3, 2, 1]);
  pending.stopAt = 10;
  undoLastOverdub(pending);
  eq(pending, [1, 2, 3, 4]);
});
console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

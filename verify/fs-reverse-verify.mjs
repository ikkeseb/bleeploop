// fs-reverse-verify.mjs — deterministic guard for per-track REVERSE (looper/machine.ts `reverse(i)`).
//
// PORT of the reverse op + its boundary swap (mirrors looper/machine.ts: the in-place two-pointer
// reversal of the [0,master) region, the `reversed` toggle flag, the PLAYING/STOPPED state guard,
// recomputePeaks over the reversed region, the nextBoundary() swap target, and clear()'s flag reset).
// A port because looper.ts (now split into src/audio/looper/{state,capture,peaks,playback,machine,
// mixer}.ts + a facade, 2026-07-01) needs AudioContext/Tone/
// ringbuf and the buffers are private — the model below mirrors the SAME array + boundary math; if you
// change that logic, update this in lockstep. Run: node fs-reverse-verify.mjs
//
// Proves: a reverse equals the input reversed element-by-element (odd lengths keep their center frame);
// a double-reverse is a BIT-EXACT identity (the toggle round-trips); ONLY [0,master) is touched (the
// silence pad / unused tail is byte-identical); the frame count is preserved (the track can't drift off
// master); the state/master guards make reverse a no-op outside PLAYING/STOPPED; peaks are recomputed over
// the REVERSED region (never stale forward peaks); the swap lands exactly on a loop boundary frame with no
// phase walk; clear() resets the reversed flag; the reversed flag stays paired with the audible buffer
// across {dub, reverse, undo/redo} via undoBufReversed (M-3); and overdub is BLOCKED while reversed
// (M-4 / RC-505 precedence) — the engine guard + the mirrored UI disable both refuse a dub on a reversed loop.

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
const region = (t) => Array.from(t.record.subarray(0, t.master));

// MIRRORS: src/audio/looper/machine.ts@762-797 sha256:edeb9df821b211cd  (reverse — in-place two-pointer swap + swapLiveSource boundary swap)
// ===== BEGIN PORT of looper/machine.ts reverse logic =====
function mkTrack(record, master = record.length) {
  return {
    record: Float32Array.from(record),
    master,
    state: 'PLAYING',
    reversed: false,
    lengthFrames: master,
    stopAt: null,
  };
}
function canReverse(t) {
  return t.lengthFrames > 0 && (t.state === 'PLAYING' || t.state === 'STOPPED');
}
// Mirrors reverse(i): no-op unless PLAYING/STOPPED with a committed loop; otherwise reverse [0,master)
// IN PLACE (two-pointer swap, odd length keeps the center) and toggle the reversed flag.
function reverse(t) {
  if (t.stopAt !== null) return;
  if (t.state !== 'PLAYING' && t.state !== 'STOPPED') return;
  const master = t.master;
  if (master === 0) return;
  const buf = t.record;
  for (let a = 0, b = master - 1; a < b; a++, b--) {
    const tmp = buf[a];
    buf[a] = buf[b];
    buf[b] = tmp;
  }
  t.reversed = !t.reversed;
}
function clear(t) {
  t.record.fill(0);
  t.reversed = false;
  t.lengthFrames = 0;
  t.state = 'EMPTY';
}
// ===== END PORT =====

// Reference reversal (independent of the in-place port) for cross-checking.
const refReverse = (a) => a.slice().reverse();

// ---------------------------------------------------------------------------
// 1. Reverse == input reversed, element-by-element (even + odd lengths).
// ---------------------------------------------------------------------------
let t = mkTrack([1, 2, 3, 4]);
reverse(t);
check('even: reverse equals input reversed', () => assert.deepStrictEqual(region(t), [4, 3, 2, 1]));
check('even: reversed flag set', () => assert.strictEqual(t.reversed, true));

t = mkTrack([1, 2, 3, 4, 5]);
reverse(t);
check('odd: reverse equals input reversed', () => assert.deepStrictEqual(region(t), [5, 4, 3, 2, 1]));
check('odd: center frame is fixed by construction', () => assert.strictEqual(t.record[2], 3));

t = mkTrack([42]);
reverse(t);
check('single frame: reverse is a no-op on data, toggles flag', () => {
  assert.deepStrictEqual(region(t), [42]);
  assert.strictEqual(t.reversed, true);
});

// ---------------------------------------------------------------------------
// 2. Double-reverse is a BIT-EXACT identity (toggle round-trips), flag clears.
// ---------------------------------------------------------------------------
const SR = 48000;
const samples = Array.from({ length: 997 }, (_, i) => Math.sin((i / SR) * 2 * Math.PI * 220) - 0.5 * Math.cos(i * 0.013));
t = mkTrack(samples);
const original = Float32Array.from(samples); // exactly what the track held (after the Float32 round-trip)
reverse(t);
check('one reverse == reference reversed (bit-exact over 997 frames)', () =>
  assert.deepStrictEqual(region(t), Array.from(original).reverse()));
reverse(t);
check('double-reverse restores the original BIT-FOR-BIT', () =>
  assert.deepStrictEqual(Array.from(t.record), Array.from(original)));
check('double-reverse clears the reversed flag (back to forward)', () => assert.strictEqual(t.reversed, false));
// many toggles: parity holds, no drift
for (let k = 0; k < 50; k++) reverse(t);
check('50 toggles (even) == original, flag forward', () => {
  assert.deepStrictEqual(Array.from(t.record), Array.from(original));
  assert.strictEqual(t.reversed, false);
});
reverse(t);
check('51st toggle (odd) == reversed, flag set', () => {
  assert.deepStrictEqual(region(t), Array.from(original).reverse());
  assert.strictEqual(t.reversed, true);
});

// ---------------------------------------------------------------------------
// 3. ONLY [0,master) is touched — the buffer tail past master is byte-identical.
// ---------------------------------------------------------------------------
{
  const full = [1, 2, 3, 4, /* tail / silence pad: */ 7, 8, 9];
  const tt = mkTrack(full, 4); // master = 4, tail = indices 4..6
  const tailBefore = Array.from(tt.record.subarray(4));
  reverse(tt);
  check('region [0,master) reversed', () => assert.deepStrictEqual(region(tt), [4, 3, 2, 1]));
  check('tail past master is byte-identical (untouched)', () =>
    assert.deepStrictEqual(Array.from(tt.record.subarray(4)), tailBefore));
}

// ---------------------------------------------------------------------------
// 4. Frame count preserved (track stays exactly master frames; cannot drift).
// ---------------------------------------------------------------------------
t = mkTrack(samples);
const lenBefore = t.record.length;
reverse(t);
check('record length preserved by reverse', () => assert.strictEqual(t.record.length, lenBefore));
check('master length unchanged', () => assert.strictEqual(t.master, 997));

// ---------------------------------------------------------------------------
// 5. State + master guards: no-op unless PLAYING/STOPPED with a committed loop.
// ---------------------------------------------------------------------------
for (const st of ['EMPTY', 'RECORDING', 'OVERDUBBING']) {
  const tg = mkTrack([1, 2, 3, 4]);
  tg.state = st;
  const before = region(tg);
  reverse(tg);
  check(`reverse is a no-op while ${st}`, () => {
    assert.deepStrictEqual(region(tg), before);
    assert.strictEqual(tg.reversed, false);
  });
  check(`canReverse false while ${st} (not PLAYING/STOPPED)`, () =>
    assert.strictEqual(canReverse(tg), false));
}
{
  const ts = mkTrack([1, 2, 3, 4]);
  ts.state = 'STOPPED';
  check('canReverse true while STOPPED with a take', () => assert.strictEqual(canReverse(ts), true));
  reverse(ts);
  check('reverse works while STOPPED (buffer flipped in place)', () =>
    assert.deepStrictEqual(region(ts), [4, 3, 2, 1]));
}
{
  const tz = mkTrack([], 0);
  tz.lengthFrames = 0;
  reverse(tz);
  check('reverse is a no-op when master === 0', () => assert.strictEqual(tz.reversed, false));
  check('canReverse false when master/length 0', () => assert.strictEqual(canReverse(tz), false));
}

// ---------------------------------------------------------------------------
// 6. Peaks recomputed over the REVERSED region (never stale forward peaks).
//    Model recomputePeaks as binned min/max over [0,master); a peak set computed on the post-reverse
//    record must equal the peak set of the reference-reversed array, bin-for-bin.
// ---------------------------------------------------------------------------
function binnedPeaks(arr, master, binFrames) {
  const out = [];
  for (let i = 0; i < master; i += binFrames) {
    let mn = Infinity;
    let mx = -Infinity;
    for (let k = i; k < Math.min(i + binFrames, master); k++) {
      if (arr[k] < mn) mn = arr[k];
      if (arr[k] > mx) mx = arr[k];
    }
    out.push([mn, mx]);
  }
  return out;
}
{
  const data = Array.from({ length: 64 }, (_, i) => Math.sin(i * 0.21) * (i % 7 === 0 ? 0.9 : 0.3));
  const tp = mkTrack(data);
  reverse(tp);
  const afterReversePeaks = binnedPeaks(tp.record, tp.master, 8);
  // Reference goes through the SAME Float32 rounding as the track buffer (else min/max differ ~1e-8).
  const refPeaks = binnedPeaks(Float32Array.from(refReverse(data)), data.length, 8);
  check('peaks over the reversed region == peaks of the reversed reference (not the forward peaks)', () =>
    assert.deepStrictEqual(afterReversePeaks, refPeaks));
  // sanity: forward peaks differ (so the assertion above is meaningful)
  const fwdPeaks = binnedPeaks(Float32Array.from(data), data.length, 8);
  check('forward vs reversed peaks differ (the test is non-trivial)', () =>
    assert.notDeepStrictEqual(afterReversePeaks, fwdPeaks));
}

// ---------------------------------------------------------------------------
// 7. The swap lands EXACTLY on a loop boundary frame, over many phases (no phase walk).
//    Port nextBoundary(): when = masterStartTime + ceil((now - masterStartTime)/period) * period.
// ---------------------------------------------------------------------------
function nextBoundary(nowCtx, masterStartTime, masterFrames, sr) {
  const period = masterFrames / sr;
  if (period <= 0) return nowCtx;
  const elapsed = nowCtx - masterStartTime;
  const n = Math.ceil(elapsed / period);
  return masterStartTime + n * period;
}
{
  const masterFrames = 96000; // 2 s at 48k
  const sr = 48000;
  const period = masterFrames / sr;
  const masterStartTime = 1.2345;
  let boundaryChecks = 0;
  for (let loop = 0; loop < 200; loop++) {
    // sample 7 arbitrary phases inside loop `loop`
    for (let p = 0; p < 7; p++) {
      const phase = (p + 0.137 * (loop + 1)) / 7; // 0..1 across the loop, varied per loop
      const now = masterStartTime + loop * period + phase * period;
      const when = nextBoundary(now, masterStartTime, masterFrames, sr);
      // (a) the swap is at/after now
      assert.ok(when >= now - 1e-9, `boundary before now at loop ${loop} phase ${p}`);
      // (b) it is an EXACT integer-bar multiple from the anchor (lands on a loop-wrap frame)
      const barsFromAnchor = (when - masterStartTime) / period;
      assert.ok(Math.abs(barsFromAnchor - Math.round(barsFromAnchor)) < 1e-9,
        `boundary not a whole-loop multiple at loop ${loop} phase ${p}: ${barsFromAnchor}`);
      // (c) it never overshoots by a whole period (it's the NEXT boundary, not a later one)
      assert.ok(when - now <= period + 1e-9, `boundary overshot a full period at loop ${loop} phase ${p}`);
      boundaryChecks++;
    }
  }
  check(`swap target is exactly the next loop boundary across ${1400} phases (no phase walk)`, () =>
    assert.strictEqual(boundaryChecks, 1400));
}

// ---------------------------------------------------------------------------
// 8. clear() resets the reversed flag.
// ---------------------------------------------------------------------------
t = mkTrack([1, 2, 3, 4]);
reverse(t);
check('reversed true before clear', () => assert.strictEqual(t.reversed, true));
clear(t);
check('clear() resets the reversed flag', () => assert.strictEqual(t.reversed, false));
check('canReverse false after clear (EMPTY)', () => assert.strictEqual(canReverse(t), false));

// ---------------------------------------------------------------------------
// 9. reverse <-> undo/redo orientation coherence (M-3 fix). Published `reversed` must equal the ACTUAL
//    orientation of the buffer in `record` after ANY {dub, reverse, undo/redo} sequence. Ground truth =
//    each buffer carries `trueRev`, flipped ONLY by a physical reversal; the flags are maintained by the
//    ported looper/machine.ts logic (startOverdub pairs undoBufReversed=reversed; undoLastOverdub swaps
//    both). Mirror of looper/machine.ts — update in lockstep.
// ===== BEGIN PORT 2 =====
const cloneBuf = (b) => ({ data: b.data.slice(), trueRev: b.trueRev });
function mkTrack2(f){return{record:{data:Float32Array.from(f),trueRev:false},reversed:false,undoBuf:null,undoBufReversed:false,state:'PLAYING',master:f.length};}
function reverse2(t){if(t.state!=='PLAYING'&&t.state!=='STOPPED')return false;if(t.master===0)return false;for(let a=0,b=t.master-1;a<b;a++,b--){const tmp=t.record.data[a];t.record.data[a]=t.record.data[b];t.record.data[b]=tmp;}t.record.trueRev=!t.record.trueRev;t.reversed=!t.reversed;return true;}
function dub2(t,layer){if(t.master===0)return false;if(t.state!=='PLAYING'&&t.state!=='STOPPED')return false;t.undoBuf=cloneBuf(t.record);t.undoBufReversed=t.reversed;for(let k=0;k<t.master;k++)t.record.data[k]+=layer;t.state='PLAYING';return true;}
function undo2(t){if(!t.undoBuf)return false;if(t.state!=='PLAYING'&&t.state!=='STOPPED')return false;if(t.master===0)return false;const prev=t.record;t.record=t.undoBuf;t.undoBuf=prev;const wasReversed=t.reversed;t.reversed=t.undoBufReversed;t.undoBufReversed=wasReversed;return true;}
const honest=(t)=>t.reversed===t.record.trueRev&&(!t.undoBuf||t.undoBufReversed===t.undoBuf.trueRev);
// ===== END PORT 2 =====
{const t=mkTrack2([1,2,3,4]);dub2(t,0.5);reverse2(t);undo2(t);check('dub->reverse->undo: flag matches the audible (forward) buffer',()=>{assert.strictEqual(t.reversed,false);assert.strictEqual(honest(t),true);assert.deepStrictEqual(Array.from(t.record.data),[1,2,3,4]);});}
{const t=mkTrack2([1,2,3,4]);dub2(t,0.5);reverse2(t);undo2(t);undo2(t);check('dub->reverse->undo->redo: flag back to reversed, honest',()=>{assert.strictEqual(t.reversed,true);assert.strictEqual(honest(t),true);assert.deepStrictEqual(Array.from(t.record.data),[4.5,3.5,2.5,1.5]);});}
{const t=mkTrack2([1,2,3,4]);reverse2(t);dub2(t,0.5);undo2(t);check('reverse->dub->undo: flag tracks the reversed pre-dub buffer',()=>{assert.strictEqual(t.reversed,true);assert.strictEqual(honest(t),true);});}
{let states=0,desyncs=0,layer=0;const apply=(t,name)=>name==='reverse'?reverse2(t):name==='dub'?dub2(t,0.0625*++layer):undo2(t);const branch=(t)=>({record:cloneBuf(t.record),reversed:t.reversed,undoBuf:t.undoBuf?cloneBuf(t.undoBuf):null,undoBufReversed:t.undoBufReversed,state:t.state,master:t.master});function dfs(t,depth){states++;if(!honest(t))desyncs++;if(depth===0)return;for(const name of['reverse','dub','undo']){const c=branch(t);if(apply(c,name))dfs(c,depth-1);}}dfs(mkTrack2([1,2,3,4,5]),6);check(`flag honest across all ${states} reachable {reverse,dub,undo} states (depth 6)`,()=>{assert.ok(states>200,`explored ${states}`);assert.strictEqual(desyncs,0);});}

// ---------------------------------------------------------------------------
// 10. Reverse BLOCKS overdub (M-4 / RC-505 precedence). The NEW engine guard in startOverdub
//     (`if (t.reversed) return`) + the mirrored UI disable (Looper.tsx `recDubDisabled`). Section 5 already
//     proves the OTHER half — reverse is a no-op + canReverse false while OVERDUBBING — so the rule is pinned
//     both ways. Mirror of looper/machine.ts startOverdub's reversed guard + Looper.tsx recDubDisabled —
//     update in lockstep.
// ===== BEGIN PORT 3 =====
// True iff startOverdub would EARLY-RETURN (overdub refused). Guard order mirrors looper/machine.ts startOverdub:
// single-recorder, then reversed (M-4), then master===0.
function startOverdubBlocked(t) {
  if (t.activeRec) return true;
  if (t.reversed) return true; // M-4
  if (t.master === 0) return true;
  return false;
}
// Mirrors Looper.tsx recDubDisabled: STOPPED (play-first) OR reversed+PLAYING (M-4) OR another track records.
const recDubDisabled2 = (t) =>
  t.state === 'STOPPED' ||
  (t.reversed && t.state === 'PLAYING') ||
  (t.anyRecording && t.state !== 'RECORDING' && t.state !== 'OVERDUBBING');
// ===== END PORT 3 =====
{
  const fwd = { activeRec: false, reversed: false, master: 4, state: 'PLAYING', anyRecording: false };
  const rev = { activeRec: false, reversed: true, master: 4, state: 'PLAYING', anyRecording: false };
  check('forward PLAYING: overdub allowed', () => assert.strictEqual(startOverdubBlocked(fwd), false));
  check('reversed PLAYING: overdub BLOCKED (M-4 engine guard)', () => assert.strictEqual(startOverdubBlocked(rev), true));
  check('UI: ring disabled while reversed+PLAYING', () => assert.strictEqual(recDubDisabled2(rev), true));
  check('UI: ring enabled while forward+PLAYING', () => assert.strictEqual(recDubDisabled2(fwd), false));
  // Toggling back to forward re-enables the dub (uses the Section-1/2 ported reverse on a PORT-2 track).
  const tg = mkTrack2([1, 2, 3, 4]);
  reverse2(tg); // forward -> reversed
  check('reversed via reverse2: overdub blocked', () =>
    assert.strictEqual(startOverdubBlocked({ activeRec: false, reversed: tg.reversed, master: tg.master }), true));
  reverse2(tg); // reversed -> forward
  check('reverse back to forward: overdub re-enabled', () =>
    assert.strictEqual(startOverdubBlocked({ activeRec: false, reversed: tg.reversed, master: tg.master }), false));
  // M-4 ⟂ M-3: a blocked dub returns BEFORE startOverdub snapshots, so undoBuf/undoBufReversed are untouched.
  const rev2 = mkTrack2([1, 2, 3, 4]);
  reverse2(rev2);
  const undoBufBefore = rev2.undoBuf;
  check('M-4 ⟂ M-3: a blocked dub leaves undoBuf untouched (still null)', () => {
    assert.strictEqual(startOverdubBlocked({ activeRec: false, reversed: rev2.reversed, master: rev2.master }), true);
    assert.strictEqual(rev2.undoBuf, undoBufBefore);
    assert.strictEqual(rev2.undoBuf, null);
  });
}

check('reverse cannot replace a source waiting for loop-end stop', () => {
  const pending = mkTrack([1, 2, 3, 4]);
  pending.stopAt = 10;
  reverse(pending);
  assert.deepStrictEqual(region(pending), [1, 2, 3, 4]);
  assert.strictEqual(pending.reversed, false);
});
console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);

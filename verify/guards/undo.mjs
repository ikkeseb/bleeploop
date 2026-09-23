// One-level overdub undo/redo: the REAL looper (src/audio/looper/machine.ts startOverdub's snapshot,
// undoLastOverdub, swapLiveSource, clear, rejectRecordLoss) under the verify rig. The first take is a
// frame code, the overdub a constant, so every buffer state is identifiable frame by frame.
//
// What this PROVES:
//   - DUB snapshots the pre-dub loop as a separate copy: summing and mid-session boundary commits never
//     touch it; undo removes the whole session, not the last pass                           [A]
//   - undo/redo is a clean toggle; a PLAYING lane swaps its source on the next boundary to exactly the
//     restored loop                                                                        [A]
//   - a fresh session re-baselines the snapshot to the current loop                          [B]
//   - undo is a no-op with no snapshot, while OVERDUBBING and while a loop-end stop is pending [C]
//   - STOPPED undoes in place without starting playback; PLAY plays the restored loop; CLEAR drops it [D]
//   - an overdub rejected for capture loss restores the pre-layer loop AND the previous undo target [E]

import { bootLooper } from '../harness/rig.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

const code = (frame) => ((frame % 8192) + 1) / 16384;
const DUB = 1 / 64;

async function playingLoop() {
  const rig = await bootLooper({ sampleRate: 48000, startTime: 20 });
  rig.clock.setBpm(200);
  rig.setInput(code);
  const mark = rig.draws().length;
  await rig.looper.recDub(0);
  const downbeat = rig.draws().slice(mark).find((b) => b.countLeft === 4).time + 4 * 0.3;
  await rig.advanceTo(downbeat + 1.2 + 0.05);
  await rig.looper.recDub(0);
  await rig.advance(0.25);
  const master = rig.looper.masterLengthFrames();
  const period = master / rig.sr;
  const next = () => {
    const start = rig.state.engineState.masterStartTime;
    return start + (Math.floor((rig.now() - start) / period) + 1) * period;
  };
  return { rig, master, period, next, t: rig.tracks[0] };
}
const snap = (buf, master) => Float32Array.from(buf.subarray(0, master));
const same = (a, b, master) => { for (let k = 0; k < master; k++) if (a[k] !== b[k]) return false; return true; };
const canUndo = (rig) => rig.looper.trackInfo(0).canUndo;

/** DUB from mid-period, across `boundaries` boundary swaps, then a quarter period more. */
async function overdubSession(loop, boundaries, input = DUB) {
  const { rig } = loop;
  rig.setInput(input);
  await rig.advanceTo(loop.next() - loop.period / 2);
  await rig.looper.recDub(0);
  for (let b = 0; b < boundaries; b++) await rig.advanceTo(loop.next() + 0.01);
  await rig.advance(loop.period / 4);
  await rig.looper.recDub(0);
  await rig.advance(0.02);
  rig.setInput(0);
}

console.log('=== A. snapshot, multi-pass session, undo/redo toggle, live source swap ===');
{
  const loop = await playingLoop();
  const { rig, master, t } = loop;
  ok('A a fresh take has nothing to undo', !canUndo(rig) && t.undoBuf === null);
  const pre = snap(t.record, master);
  rig.setInput(DUB);
  await rig.advanceTo(loop.next() - loop.period / 2);
  await rig.looper.recDub(0);
  ok('A DUB snapshots the pre-dub loop', t.undoBuf !== null && same(t.undoBuf, pre, master));
  ok('A the snapshot is a separate copy', t.undoBuf !== t.record && t.undoBuf.buffer !== t.overdubBuf.buffer &&
    t.undoBuf.buffer !== t.record.buffer);
  ok('A no undo while OVERDUBBING', !canUndo(rig));
  await rig.advanceTo(loop.next() - 0.01);
  ok('A summing leaves the loop alone until the boundary', same(t.record, pre, master));
  await rig.advanceTo(loop.next() + 0.01);
  ok('A the mid-session boundary commits the layer', !same(t.record, pre, master));
  ok('A the snapshot still holds the pre-SESSION loop', same(t.undoBuf, pre, master));
  await rig.advanceTo(loop.next() - loop.period / 2);
  await rig.looper.recDub(0);
  await rig.advance(0.02);
  rig.setInput(0);
  const dubbed = snap(t.record, master);
  ok('A the session committed one period of layer', dubbed.reduce((s, v, k) => s + v - pre[k], 0) === master * DUB,
    `added=${dubbed.reduce((s, v, k) => s + v - pre[k], 0) / DUB} frames`);
  ok('A undo is available after the commit', canUndo(rig));
  await rig.advanceTo(loop.next() + 0.01); // finishOverdub's restart is live
  for (const [step, want, label] of [[1, pre, 'undo'], [2, dubbed, 'redo'], [3, pre, 'undo again']]) {
    const mark = rig.sources().length;
    const boundary = loop.next();
    rig.looper.undoLastOverdub(0);
    ok(`A ${step} ${label} sets the loop`, same(t.record, want, master));
    ok(`A ${step} ${label} keeps the frame count`, t.lengthFrames === master && rig.looper.masterLengthFrames() === master);
    const src = rig.sources()[mark];
    ok(`A ${step} ${label} swaps the source on the next boundary`, rig.sources().length === mark + 1 &&
      Math.abs(src.startTime - boundary) < 1e-9 && src.offset === 0, `start=${src?.startTime} boundary=${boundary}`);
    ok(`A ${step} ${label}: the new source plays exactly the loop`, same(src.buffer.getChannelData(0), want, master) &&
      src.buffer.length === master);
    ok(`A ${step} ${label} leaves undo available`, canUndo(rig));
    await rig.advanceTo(boundary + 0.01);
  }
}

console.log('=== B. a fresh session re-baselines the snapshot ===');
{
  const loop = await playingLoop();
  const { rig, master, t } = loop;
  await overdubSession(loop, 0);
  const first = snap(t.record, master);
  rig.setInput(DUB);
  await rig.advanceTo(loop.next() + loop.period / 4);
  await rig.looper.recDub(0);
  ok('B the new session snapshots the current loop', same(t.undoBuf, first, master));
  await rig.advance(loop.period / 4);
  await rig.looper.recDub(0);
  await rig.advance(0.02);
  ok('B the second layer committed', !same(t.record, first, master));
  rig.looper.undoLastOverdub(0);
  ok('B undo restores the prior committed loop, not the take', same(t.record, first, master));
}

console.log('=== C. undo is a no-op without a snapshot, while OVERDUBBING, with a loop-end stop pending ===');
{
  const loop = await playingLoop();
  const { rig, master, t } = loop;
  const take = snap(t.record, master);
  const mark = rig.sources().length;
  rig.looper.undoLastOverdub(0);
  ok('C no snapshot: loop and playback untouched', same(t.record, take, master) && rig.sources().length === mark);
  rig.setInput(DUB);
  await rig.advanceTo(loop.next() - loop.period / 2);
  await rig.looper.recDub(0);
  await rig.advanceTo(loop.next() + 0.01);
  const mid = snap(t.record, master);
  const snapshot = t.undoBuf;
  rig.looper.undoLastOverdub(0);
  ok('C OVERDUBBING: loop, snapshot and state untouched', same(t.record, mid, master) && t.undoBuf === snapshot &&
    rig.looper.trackInfo(0).state === 'OVERDUBBING');
  await rig.looper.recDub(0);
  await rig.advance(0.02);
  const dubbed = snap(t.record, master);
  rig.looper.setLoopEndStopEnabled(true);
  rig.looper.playStop(0);
  ok('C precondition: a loop-end stop is pending', t.stopAt !== null);
  const mark2 = rig.sources().length;
  rig.looper.undoLastOverdub(0);
  ok('C loop-end stop pending: loop and playback untouched', same(t.record, dubbed, master) && rig.sources().length === mark2);
}

console.log('=== D. STOPPED undoes in place; PLAY plays the restored loop; CLEAR drops the snapshot ===');
{
  const loop = await playingLoop();
  const { rig, master, t } = loop;
  const pre = snap(t.record, master);
  await overdubSession(loop, 1);
  rig.looper.playStop(0);
  ok('D precondition: STOPPED with undo available', rig.looper.trackInfo(0).state === 'STOPPED' && canUndo(rig));
  const mark = rig.sources().length;
  rig.looper.undoLastOverdub(0);
  ok('D STOPPED undo restores the loop in place', same(t.record, pre, master));
  ok('D STOPPED undo starts no playback', rig.sources().length === mark && t.source === null);
  rig.looper.playStop(0);
  await rig.advance(0.05);
  ok('D PLAY plays the restored loop', t.source !== null && same(t.source.buffer.getChannelData(0), pre, master));
  rig.looper.clear(0);
  await rig.advance(0.05);
  ok('D CLEAR drops the snapshot', t.undoBuf === null && !canUndo(rig));
  // The lane keeps its full record capacity: a longer take than the undone loop still fits.
  rig.setInput(code);
  const mark2 = rig.draws().length;
  await rig.looper.recDub(0);
  const downbeat = rig.draws().slice(mark2).find((b) => b.countLeft === 4).time + 4 * 0.3;
  await rig.advanceTo(downbeat + 3 * 1.2 + 0.05);
  await rig.looper.recDub(0);
  await rig.advance(0.25);
  ok('D after undo and CLEAR a 3-bar take still fits', rig.looper.masterLengthFrames() === 3 * master,
    `master=${rig.looper.masterLengthFrames()} want=${3 * master}`);
}

console.log('=== E. an overdub rejected for capture loss restores the loop and the previous undo target ===');
{
  const loop = await playingLoop();
  const { rig, master, t } = loop;
  const take = snap(t.record, master);
  await overdubSession(loop, 0);
  const first = snap(t.record, master);
  rig.setInput(DUB);
  await rig.advanceTo(loop.next() - loop.period / 2);
  await rig.looper.recDub(0);
  await rig.stall(12); // the capture ring (~10.9 s) overflows while the main thread is blocked
  await rig.looper.recDub(0);
  await rig.advance(0.05);
  ok('E precondition: the layer was rejected', rig.logs.some((l) => /rejected track 0's overdub layer/.test(String(l.args[0]))));
  ok('E the pre-layer loop is back', same(t.record, first, master));
  ok('E the lane plays on', rig.looper.trackInfo(0).state === 'PLAYING');
  ok('E undo still targets the loop before the kept layer', canUndo(rig) && same(t.undoBuf, take, master));
  rig.looper.undoLastOverdub(0);
  ok('E undo removes the kept layer', same(t.record, take, master));
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

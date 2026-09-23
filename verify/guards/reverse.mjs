// Per-track REVERSE: the REAL looper (src/audio/looper/machine.ts reverse / swapLiveSource / startOverdub's
// reversed guard / undoLastOverdub's orientation swap / clear) and the REAL UI gate (src/ui/looper/gates.ts
// recDubGate) under the verify rig. The take is a frame code, so orientation is readable frame by frame.
//
// What this PROVES:
//   - reverse flips exactly [0, master) in place (odd masters keep the centre frame), leaves the buffer
//     tail and the frame count alone, and recomputes the peaks over the reversed loop       [A]
//   - a PLAYING lane swaps to exactly the reversed loop on the next boundary, at any phase; a double
//     reverse is a bit-exact identity                                                      [B]
//   - reverse is a no-op while EMPTY, RECORDING, OVERDUBBING or with a loop-end stop pending; STOPPED
//     flips in place and PLAY plays it reversed; CLEAR resets the flag                      [C]
//   - over a long {dub, reverse, undo} sequence the published flag always matches the orientation of
//     the loop in `record` and of the undo snapshot; a dub is refused (engine and UI gate) while
//     reversed and leaves the undo snapshot untouched                                       [D]

import { bootLooper } from '../harness/rig.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

const code = (frame) => ((frame % 8192) + 1) / 16384;
const PEAK_FRAMES = 1024;

async function playingLoop({ bpm = 200, sr = 48000 } = {}) {
  const rig = await bootLooper({ sampleRate: sr, startTime: 20 });
  rig.clock.setBpm(bpm);
  rig.setInput(code);
  const mark = rig.draws().length;
  await rig.looper.recDub(0);
  const beat = 60 / bpm;
  const downbeat = rig.draws().slice(mark).find((b) => b.countLeft === 4).time + 4 * beat;
  await rig.advanceTo(downbeat + 4 * beat + 0.05);
  await rig.looper.recDub(0);
  await rig.advance(0.25);
  rig.setInput(0);
  const master = rig.looper.masterLengthFrames();
  const period = master / rig.sr;
  const next = () => {
    const start = rig.state.engineState.masterStartTime;
    return start + (Math.floor((rig.now() - start) / period) + 1) * period;
  };
  return { rig, master, period, next, t: rig.tracks[0] };
}
const snap = (buf, n) => Float32Array.from(buf.subarray(0, n));
const same = (a, b, n) => { for (let k = 0; k < n; k++) if (a[k] !== b[k]) return false; return true; };
const reversedCopy = (a) => Float32Array.from(a).reverse();
const flag = (rig, i = 0) => rig.looper.trackInfo(i).reversed;

console.log('=== A. in-place flip of [0, master), tail and length kept, peaks recomputed ===');
for (const [bpm, sr] of [[200, 48000], [137, 44100]]) {
  const { rig, master, t } = await playingLoop({ bpm, sr });
  const tag = `master=${master}`;
  const pre = snap(t.record, master);
  t.record.fill(0.25, master, master + 4096); // a marked tail past the loop
  const tail = snap(t.record.subarray(master), 4096);
  const capacity = t.record.length;
  rig.looper.reverse(0);
  ok(`A ${tag} the loop equals the take reversed`, same(t.record, reversedCopy(pre), master));
  if (master % 2 === 1) ok(`A ${tag} an odd loop keeps its centre frame`, t.record[(master - 1) / 2] === pre[(master - 1) / 2]);
  ok(`A ${tag} the tail past master is untouched`, same(t.record.subarray(master), tail, 4096));
  ok(`A ${tag} frame count and capacity are kept`, t.lengthFrames === master && rig.looper.masterLengthFrames() === master &&
    t.record.length === capacity);
  ok(`A ${tag} the published flag is set`, flag(rig) === true);
  const bins = Math.ceil(master / PEAK_FRAMES);
  let peaksOk = t.peakCount === bins;
  for (let b = 0; b < bins && peaksOk; b++) {
    const region = t.record.subarray(b * PEAK_FRAMES, Math.min(master, (b + 1) * PEAK_FRAMES));
    peaksOk = t.peakMin[b] === Math.min(...region) && t.peakMax[b] === Math.max(...region);
  }
  ok(`A ${tag} the peaks describe the reversed loop`, peaksOk);
}

console.log('=== B. the PLAYING swap lands on the next boundary at any phase; double reverse is identity ===');
{
  const { rig, master, period, next, t } = await playingLoop();
  const pre = snap(t.record, master);
  await rig.advanceTo(next() + 0.01);
  for (let k = 0; k < 7; k++) {
    await rig.advance(((k * 3 + 1) / 23) * period);
    const boundary = next();
    const mark = rig.sources().length;
    rig.looper.reverse(0);
    const want = k % 2 === 0 ? reversedCopy(pre) : pre;
    const src = rig.sources()[mark];
    ok(`B ${k} one swap source on the next boundary`, rig.sources().length === mark + 1 &&
      Math.abs(src.startTime - boundary) < 1e-9 && src.offset === 0, `start=${src?.startTime} boundary=${boundary}`);
    ok(`B ${k} it plays exactly the ${k % 2 === 0 ? 'reversed' : 'forward'} loop`,
      src.buffer.length === master && same(src.buffer.getChannelData(0), want, master) && same(t.record, want, master));
    ok(`B ${k} the flag follows`, flag(rig) === (k % 2 === 0));
    await rig.advanceTo(boundary + 0.01);
    ok(`B ${k} the swap is live and the previous source retired`, t.source === src && t.retiringSources.size === 0);
  }
  rig.looper.reverse(0);
  ok('B an even number of reverses restores the take bit for bit', same(t.record, pre, master) && flag(rig) === false);
}

console.log('=== C. no-op while EMPTY / RECORDING / OVERDUBBING / stopping; STOPPED in place; CLEAR resets ===');
{
  const { rig, master, next, t } = await playingLoop();
  const pre = snap(t.record, master);
  const lane1 = rig.tracks[1];
  rig.looper.reverse(1);
  ok('C EMPTY lane: no-op', flag(rig, 1) === false && lane1.lengthFrames === 0);
  rig.setInput(code);
  await rig.looper.recDub(1); // a later take arms to the next boundary
  await rig.advanceTo(next() + 0.1);
  ok('C precondition: lane 2 RECORDING', rig.looper.trackInfo(1).state === 'RECORDING');
  const rec = snap(lane1.record, 1000);
  rig.looper.reverse(1);
  ok('C RECORDING lane: no-op', flag(rig, 1) === false && same(lane1.record, rec, 1000));
  rig.looper.stop(1);
  await rig.advance(0.05);
  rig.setInput(1 / 64);
  await rig.looper.recDub(0);
  ok('C precondition: lane 1 OVERDUBBING', rig.looper.trackInfo(0).state === 'OVERDUBBING');
  const mid = snap(t.record, master);
  let mark = rig.sources().length;
  rig.looper.reverse(0);
  ok('C OVERDUBBING: no-op', flag(rig) === false && same(t.record, mid, master) && rig.sources().length === mark);
  await rig.looper.recDub(0);
  await rig.advance(0.05);
  rig.setInput(0);
  const dubbed = snap(t.record, master);
  rig.looper.setLoopEndStopEnabled(true);
  rig.looper.playStop(0);
  ok('C precondition: a loop-end stop is pending', t.stopAt !== null);
  mark = rig.sources().length;
  rig.looper.reverse(0);
  ok('C loop-end stop pending: no-op', flag(rig) === false && same(t.record, dubbed, master) && rig.sources().length === mark);
  await rig.advanceTo(next() + 0.05);
  ok('C precondition: STOPPED', rig.looper.trackInfo(0).state === 'STOPPED' && t.source === null);
  mark = rig.sources().length;
  rig.looper.reverse(0);
  ok('C STOPPED: flipped in place, no playback started', flag(rig) === true && same(t.record, reversedCopy(dubbed), master) &&
    rig.sources().length === mark);
  rig.looper.setLoopEndStopEnabled(false);
  rig.looper.playStop(0);
  await rig.advance(0.05);
  ok('C PLAY plays the reversed loop', t.source !== null && same(t.source.buffer.getChannelData(0), reversedCopy(dubbed), master));
  rig.looper.clear(0);
  await rig.advance(0.05);
  ok('C CLEAR resets the flag', flag(rig) === false && t.reversed === false);
  void pre;
}

console.log('=== D. the flag stays honest across {dub, reverse, undo}; dub is refused while reversed ===');
{
  const { rig, master, t } = await playingLoop();
  const gates = await rig.import('ui/looper/gates.ts');
  // Independent model: each buffer carries its true orientation, flipped only by a physical reversal.
  let cur = { data: snap(t.record, master), rev: false };
  let undo = null;
  let seed = 12345;
  const rand = () => ((seed = (seed * 1103515245 + 12345) >>> 0) >>> 16) % 3;
  const counts = { dub: 0, refused: 0, reverse: 0, undo: 0 };
  let layer = 0;
  for (let step = 0; step < 36; step++) {
    const op = ['dub', 'reverse', 'undo'][rand()];
    if (op === 'reverse') {
      rig.looper.reverse(0);
      cur = { data: reversedCopy(cur.data), rev: !cur.rev };
      counts.reverse++;
    } else if (op === 'undo') {
      rig.looper.undoLastOverdub(0);
      if (undo) [cur, undo] = [undo, cur];
      counts.undo++;
    } else if (cur.rev) {
      ok(`D ${step} the UI gate refuses DUB while reversed`, gates.recDubGate(0).ok === false &&
        /reversed/.test(gates.recDubGate(0).reason));
      const snapshot = t.undoBuf;
      await rig.looper.recDub(0);
      ok(`D ${step} the engine refuses DUB while reversed`, rig.looper.trackInfo(0).state === 'PLAYING' &&
        t.undoBuf === snapshot && same(t.record, cur.data, master));
      counts.refused++;
    } else {
      ok(`D ${step} the UI gate allows DUB while forward`, gates.recDubGate(0).ok === true);
      rig.setInput(++layer / 256);
      await rig.looper.recDub(0);
      await rig.advance(0.1);
      await rig.looper.recDub(0);
      await rig.advance(0.02);
      rig.setInput(0);
      undo = cur;
      cur = { data: snap(t.record, master), rev: cur.rev };
      ok(`D ${step} the dub committed a layer`, !same(cur.data, undo.data, master));
      counts.dub++;
    }
    ok(`D ${step} ${op}: record holds the expected loop`, same(t.record, cur.data, master));
    ok(`D ${step} ${op}: the flag matches its orientation`, flag(rig) === cur.rev, `flag=${flag(rig)} true=${cur.rev}`);
    ok(`D ${step} ${op}: the undo snapshot and its orientation match`, undo === null
      ? t.undoBuf === null
      : same(t.undoBuf, undo.data, master) && t.undoBufReversed === undo.rev);
  }
  ok('D the sequence exercised every transition', Object.values(counts).every((n) => n >= 3), JSON.stringify(counts));
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

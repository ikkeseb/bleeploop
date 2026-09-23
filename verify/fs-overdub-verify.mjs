// Overdub timer ownership and boundary progression: the REAL looper (src/audio/looper/machine.ts
// startOverdub / finishOverdub / endOverdubAfterFailedSwap / playStop, playback.ts scheduleOverdubSwap /
// cancelOverdubSwap) under the verify rig. A swap is a playback source the boundary timer starts "now";
// finishOverdub's restart is scheduled ahead for the next boundary and is not counted as one.
//
// What this PROVES:
//   - one pending swap timer per overdubbing lane, none otherwise; DUB end->start cycles inside one loop
//     period never leave a parallel chain: exactly one swap per boundary                   [A, B]
//   - an overdub that ends without a restart swaps no more                                    [C]
//   - a timer that fires while ctx still reads a hair BEFORE its boundary re-arms for the NEXT one (no
//     duplicate swap into the live buffer); after a main-thread stall the re-arm jumps to the first
//     future boundary instead of replaying the missed ones                                    [D]
//   - STOP during an overdub cancels the swap, commits the layer and lands STOPPED            [E]
//   - a swap that throws at any step (commit, peaks, buffer fill, source start) is logged once and
//     ends the session: STOPPED, the layer kept, no successor timer, no later swap           [F]
// Real timeout registration in the browser: overdub-timers.mjs. Punch-out tail frames: overdub-window.mjs.

import { bootLooper } from './harness/rig.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); }
}

const SWAP_TIMER = /scheduleOverdubSwap/;
const DUB = 1 / 64; // overdub input: exact in Float32, so summed layers compare exactly

/** A committed one-bar loop on lane 0, playing. Boundaries are masterStart + n * period. */
async function playingLoop(bpm = 200) {
  const rig = await bootLooper({ sampleRate: 48000, startTime: 20 });
  rig.clock.setBpm(bpm);
  const master = await rig.recordFirstTake({ bars: 1, level: 0.5 });
  const masterStart = rig.state.engineState.masterStartTime;
  const period = master / rig.sr;
  const boundary = (n) => masterStart + n * period;
  /** The first boundary index strictly after now. */
  const nextIndex = () => Math.floor((rig.now() - masterStart) / period) + 1;
  rig.setInput(DUB);
  return { rig, master, period, boundary, nextIndex };
}

/** Swap sources started from `mark` on, counted per boundary index. */
function swapsByBoundary(rig, mark, loop) {
  const counts = new Map();
  for (const src of rig.sources().slice(mark)) {
    if (src.startTime === null || src.startTime - src.createdAt > 0.01) continue; // scheduled ahead: not a swap
    const n = Math.round((src.startTime - src.offset - loop.boundary(0)) / loop.period);
    counts.set(n, (counts.get(n) ?? 0) + 1);
  }
  return counts;
}
/** Distance, in periods, from `time` to the nearest boundary. */
const phase = (time, loop) => { const x = (time - loop.boundary(0)) / loop.period; return x - Math.round(x); };
const pendingSwaps = (rig) => rig.timers.pending(SWAP_TIMER);
const layerSum = (t, master) => { let s = 0; for (let k = 0; k < master; k++) s += t.record[k]; return s; };

console.log('=== A/B. DUB end->start cycles inside one period: one timer, one swap per boundary ===');
for (const cycles of [0, 1, 2, 4]) {
  const loop = await playingLoop();
  const { rig } = loop;
  const first = loop.nextIndex();
  await rig.advanceTo(loop.boundary(first - 1) + 0.05);
  const mark = rig.sources().length;
  const taps = ['start'];
  for (let c = 0; c < cycles; c++) taps.push('end', 'start');
  for (const tap of taps) {
    await rig.looper.recDub(0);
    await rig.advance(0.02);
    const state = rig.looper.trackInfo(0).state;
    ok(`A cycles=${cycles} after ${tap}: lane ${tap === 'start' ? 'OVERDUBBING' : 'PLAYING'}`,
      state === (tap === 'start' ? 'OVERDUBBING' : 'PLAYING'), state);
    ok(`A cycles=${cycles} after ${tap}: pending swap timers match the capture state`,
      pendingSwaps(rig) === (state === 'OVERDUBBING' ? 1 : 0), `pending=${pendingSwaps(rig)}`);
  }
  await rig.advanceTo(loop.boundary(first + 5) + 0.05);
  const swaps = swapsByBoundary(rig, mark, loop);
  const perBoundary = [0, 1, 2, 3, 4, 5].map((k) => swaps.get(first + k) ?? 0);
  ok(`B cycles=${cycles}: exactly one swap per boundary`, perBoundary.every((n) => n === 1), JSON.stringify(perBoundary));
  ok(`B cycles=${cycles}: still one pending swap timer`, pendingSwaps(rig) === 1, `pending=${pendingSwaps(rig)}`);
  const swapSources = rig.sources().slice(mark).filter((x) => x.startTime - x.createdAt <= 0.01);
  ok(`B cycles=${cycles}: a swap never writes into the buffer the outgoing source plays`,
    swapSources.every((x, k) => k === 0 || x.buffer !== swapSources[k - 1].buffer));
  const src = rig.tracks[0].source;
  ok(`B cycles=${cycles}: the live source sits on the grid`,
    Math.abs(src.startTime - src.offset - loop.boundary(first + 5)) < 1e-9, `start=${src.startTime} offset=${src.offset}`);
}

console.log('=== C. an overdub that ends without a restart swaps no more ===');
{
  const loop = await playingLoop();
  const { rig } = loop;
  await rig.advanceTo(loop.boundary(loop.nextIndex() - 1) + 0.05);
  await rig.looper.recDub(0);
  await rig.advanceTo(loop.boundary(loop.nextIndex()) + 0.05); // one swap
  const mark = rig.sources().length;
  await rig.looper.recDub(0);
  await rig.advance(0.01);
  ok('C the layer commits: PLAYING, no pending swap timer', rig.looper.trackInfo(0).state === 'PLAYING' && pendingSwaps(rig) === 0,
    `state=${rig.looper.trackInfo(0).state} pending=${pendingSwaps(rig)}`);
  await rig.advance(5 * loop.period);
  ok('C no swap after the end', swapsByBoundary(rig, mark, loop).size === 0);
  ok('C only the scheduled final restart plays', rig.sources().length - mark === 1, `sources=${rig.sources().length - mark}`);
}

console.log('=== D. early fire re-arms for the NEXT boundary; a stall jumps to the first future one ===');
for (const bpm of [200, 137, 120]) {
  for (const hairQuanta of [1, 3]) {
    const loop = await playingLoop(bpm);
    const { rig } = loop;
    await rig.advanceTo(loop.boundary(loop.nextIndex() - 1) + 0.05);
    await rig.looper.recDub(0);
    const n = loop.nextIndex();
    const when = loop.boundary(n);
    // The wall-clock timer fires while ctx still reads up to a few quanta BEFORE the boundary.
    await rig.advanceTo(when - (hairQuanta * 128) / rig.sr - 1e-6);
    ok(`D bpm=${bpm} ctx is before the boundary`, rig.now() < when);
    const mark = rig.sources().length;
    rig.timers.fire(rig.timers.nextDue(SWAP_TIMER));
    await rig.flush();
    const swap = rig.sources()[mark];
    ok(`D bpm=${bpm} hair=${hairQuanta}q the early swap starts exactly on its boundary`,
      rig.sources().length === mark + 1 && swap.startTime === when && swap.offset === 0,
      `sources=${rig.sources().length - mark} start=${swap?.startTime} when=${when}`);
    // Its delay is measured from the early ctx reading, so it fires up to that skew after the boundary.
    ok(`D bpm=${bpm} hair=${hairQuanta}q the re-arm targets the next boundary`,
      Math.abs(rig.timers.nextDue(SWAP_TIMER) / 1000 - loop.boundary(n + 1)) < ((hairQuanta + 1) * 128) / rig.sr,
      `due=${rig.timers.nextDue(SWAP_TIMER) / 1000} next=${loop.boundary(n + 1)}`);
    await rig.advanceTo(loop.boundary(n + 2) + 0.05);
    const per = [n, n + 1, n + 2].map((k) => swapsByBoundary(rig, mark, loop).get(k) ?? 0);
    ok(`D bpm=${bpm} hair=${hairQuanta}q no duplicate swap at any boundary`, per.every((c) => c === 1), JSON.stringify(per));
  }
}
for (const stalledPeriods of [1.2, 2.0, 4.7]) {
  const loop = await playingLoop();
  const { rig } = loop;
  await rig.advanceTo(loop.boundary(loop.nextIndex() - 1) + 0.05);
  await rig.looper.recDub(0);
  const n = loop.nextIndex();
  await rig.advanceTo(loop.boundary(n) - 0.01);
  const mark = rig.sources().length;
  await rig.stall(stalledPeriods * loop.period);
  const future = loop.nextIndex();
  ok(`D stall ${stalledPeriods}p: one late swap, not one per missed boundary`, rig.sources().length - mark === 1,
    `swaps=${rig.sources().length - mark}`);
  const due = rig.timers.nextDue(SWAP_TIMER) / 1000;
  ok(`D stall ${stalledPeriods}p: the re-arm targets the first future boundary`,
    Math.abs(due - loop.boundary(future)) < 0.003 && due > rig.now() && due - rig.now() <= loop.period,
    `due=${due} first future=${loop.boundary(future)} now=${rig.now()}`);
  const late = rig.sources()[mark];
  ok(`D stall ${stalledPeriods}p: the late swap stays on the grid`,
    Math.abs(phase(late.startTime - late.offset, loop)) < 1e-6,
    `start=${late.startTime} offset=${late.offset}`);
}

console.log('=== E. STOP during an overdub commits, cancels the swap and lands STOPPED ===');
for (const trimMs of [null, 100]) {
  // With C in flight the capture tail crosses the next boundary: the swap must not restart sound there.
  const loop = await playingLoop();
  const { rig, master } = loop;
  if (trimMs !== null) {
    const latency = await rig.import('audio/record-latency.ts');
    latency.beginMonitorGeneration(0, 0);
    latency.setOffsetMs(trimMs);
  }
  const tag = trimMs === null ? 'C=0' : `C=${trimMs}ms+`;
  await rig.advanceTo(loop.boundary(loop.nextIndex() - 1) + 0.05);
  const pre = layerSum(rig.tracks[0], master);
  await rig.looper.recDub(0);
  const punchIn = rig.frame();
  await rig.advanceTo(loop.boundary(loop.nextIndex()) - 0.03);
  rig.looper.playStop(0);
  const stopFrame = rig.frame();
  ok(`E ${tag} no swap timer survives STOP`, pendingSwaps(rig) === 0, `pending=${pendingSwaps(rig)}`);
  const mark = rig.sources().length;
  await rig.advance(3 * loop.period);
  const t = rig.tracks[0];
  ok(`E ${tag} the lane is STOPPED with no live source`, rig.looper.trackInfo(0).state === 'STOPPED' && t.source === null);
  ok(`E ${tag} no playback starts after STOP`, rig.sources().length === mark, `sources=${rig.sources().length - mark}`);
  ok(`E ${tag} the layer through the press is committed`, layerSum(t, master) - pre === (stopFrame - punchIn) * DUB,
    `added=${(layerSum(t, master) - pre) / DUB} frames, window=${stopFrame - punchIn}`);
}

console.log('=== F. a swap that throws at any step ends the session: STOPPED, layer kept, no successor ===');
/** Make the swap step `step` throw once on the next boundary firing. */
function inject(rig, step) {
  const t = rig.tracks[0];
  const fault = new Error(`injected ${step} failure`);
  if (step === 'commit') {
    t.record.set = function () { delete t.record.set; throw fault; };
  } else if (step === 'peaks') {
    const peakMin = t.peakMin;
    Object.defineProperty(t, 'peakMin', {
      configurable: true,
      get() { Object.defineProperty(t, 'peakMin', { value: peakMin, writable: true, configurable: true }); throw fault; },
    });
  } else if (step === 'fill') {
    const buf = t.overdubSwapBufs[t.overdubSwapIdx];
    buf.getChannelData = function () { delete buf.getChannelData; throw fault; };
  } else rig.failNextSourceStart(fault);
}
const failures = [];
for (const step of ['commit', 'peaks', 'fill', 'start']) {
  for (const failAt of [0, 1, 3]) failures.push({ step, failAt, trimMs: null });
  failures.push({ step, failAt: 1, trimMs: 100 }); // the STOP's capture tail is still in flight after the failure
}
for (const { step, failAt, trimMs } of failures) {
  const loop = await playingLoop();
  const { rig, master } = loop;
  if (trimMs !== null) {
    const latency = await rig.import('audio/record-latency.ts');
    latency.beginMonitorGeneration(0, 0);
    latency.setOffsetMs(trimMs);
  }
  const tag = `${step}@${failAt}${trimMs === null ? '' : ` C=${trimMs}ms+`}`;
  await rig.advanceTo(loop.boundary(loop.nextIndex() - 1) + 0.05);
  const pre = layerSum(rig.tracks[0], master);
  await rig.looper.recDub(0);
  const punchIn = rig.frame();
  const first = loop.nextIndex();
  const mark = rig.sources().length;
  const errorsBefore = rig.logs.filter((l) => l.level === 'error').length;
  await rig.advanceTo(loop.boundary(first + failAt) - 0.01);
  inject(rig, step);
  await rig.advanceTo(loop.boundary(first + failAt) + 0.005);
  ok(`F ${tag} no swap timer is re-armed after the failure`, pendingSwaps(rig) === 0, `pending=${pendingSwaps(rig)}`);
  await rig.advanceTo(loop.boundary(first + failAt) + 0.2);
  const t = rig.tracks[0];
  const errors = rig.logs.filter((l) => l.level === 'error').slice(errorsBefore);
  ok(`F ${tag} the failure is logged once`, errors.length === 1 && /overdub boundary swap failed/.test(String(errors[0]?.args[0])),
    JSON.stringify(errors.map((e) => String(e.args[0]))));
  ok(`F ${tag} the lane leaves OVERDUBBING into STOPPED`, rig.looper.trackInfo(0).state === 'STOPPED', rig.looper.trackInfo(0).state);
  ok(`F ${tag} no swap timer is pending`, pendingSwaps(rig) === 0 && t.overdubTimer === null, `pending=${pendingSwaps(rig)}`);
  ok(`F ${tag} the capture is released`, t.overdubBuf === null && rig.state.engineState.activeRecordIndex === -1);
  ok(`F ${tag} the owner is told`, rig.notify.toasts().some((n) => /overdub stopped/.test(n.message)));
  const swaps = swapsByBoundary(rig, mark, loop);
  ok(`F ${tag} swaps stop at the failed boundary`,
    [...swaps.keys()].every((n) => n < first + failAt) && swaps.size === failAt, JSON.stringify([...swaps]));
  // The failed boundary's layer: everything summed from the punch-in to the failing boundary (+ the
  // quantum the timer fired in), committed by the STOP the failure hands off to.
  const added = (layerSum(t, master) - pre) / DUB;
  const toBoundary = Math.round(loop.boundary(first + failAt) * rig.sr) - punchIn;
  ok(`F ${tag} the failed boundary's layer is kept`, added >= toBoundary && added <= toBoundary + 128,
    `added=${added} frames, to boundary=${toBoundary}`);
  const mark2 = rig.sources().length;
  await rig.advance(3 * loop.period);
  ok(`F ${tag} nothing restarts on its own`, rig.sources().length === mark2 && t.source === null);
  rig.looper.playStop(0);
  await rig.advance(0.05);
  const replay = rig.tracks[0].source;
  ok(`F ${tag} PLAY restarts the committed loop`, rig.looper.trackInfo(0).state === 'PLAYING' && replay !== null &&
    replay.buffer.getChannelData(0).every((v, k) => v === t.record[k]));
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

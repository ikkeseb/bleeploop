// The FREE-RUN beat pulse: the REAL clock (src/audio/clock.ts) under the verify rig. The pulse is ONE
// ctx-time lookahead scheduler with three anchors (free-run / count-in / master); this file covers the
// free-run anchor and the hand-offs into and out of it. Every beat is labelled beatInBar = N % 4 with N
// derived from the anchor, so an accent can only land off the bar if the anchor arithmetic breaks.
//
// What this PROVES:
//   - fresh start: beat 0 at now, cadence exactly 60/bpm, accent iff N % 4 == 0           [ensureRunning]
//   - a bpm change mid-free-run is PHASE-CARRYING: the next beat keeps the outgoing grid's time AND
//     bar index, later beats space at the new period, none twice, none in the past           [setBpm]
//   - a master reset (clearAll) falls back to free-run carrying the loop grid's phase: the LED keeps
//     beating, the seam gap is <= one period and the bar index is continuous                [resetMaster]
//   - a count-in abort falls back clean: no forced click and no count numeral after it       [stopCountIn]
//   - setBpm re-anchors ONLY a live free-run pulse; a value-identical set is a full no-op    [setBpm]
//   - the carried next-beat time survives a re-anchor at a large beat index (hours in)       [startFreeRunPulse]

import { bootLooper } from '../harness/rig.ts';
import { HEARTBEAT_INTERNAL_LATENCY as HBL } from '../../src/audio/looper/grid-math.ts';

let fails = 0, checks = 0;
const approx = (a, b, eps = 1e-9) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

const since = (list, mark) => list.slice(mark);
/** The beat index of a free-run draw, given the grid it was scheduled on. */
const indexOn = (time, anchor, period) => Math.round((time - anchor) / period);

console.log('=== A. Fresh start: beat 0 at now, exact cadence, accent iff N%4==0 ===');
for (const bpm of [40, 90, 120, 137, 200, 300]) {
  const rig = await bootLooper({ init: false, startTime: 100 });
  rig.clock.setBpm(bpm); // no pulse yet: only the value changes
  const now0 = rig.now();
  const period = 60 / bpm;
  rig.clock.ensureRunning();
  await rig.advance(8 * period + 0.05);
  const beats = rig.draws();
  ok(`A beat 0 fires at now bpm=${bpm}`, beats[0]?.time === now0 && beats[0]?.at === now0, JSON.stringify(beats[0]));
  ok(`A every beat at now + n*period bpm=${bpm}`, beats.every((b, n) => b.time === now0 + n * period));
  ok(`A accent iff n%4==0 bpm=${bpm}`, beats.every((b, n) => b.beat === n % 4));
  ok(`A no beat scheduled twice bpm=${bpm}`, beats.every((b, n) => n === 0 || b.time > beats[n - 1].time));
}

console.log('=== B. bpm change mid-free-run: phase-carrying re-anchor (no hop, no double-fire) ===');
for (const [bpmA, bpmB] of [[120, 90], [90, 200], [200, 40], [120, 121]]) {
  const rig = await bootLooper({ init: false, startTime: 50 });
  rig.clock.setBpm(bpmA);
  const now0 = rig.now();
  const pA = 60 / bpmA, pB = 60 / bpmB;
  rig.clock.ensureRunning();
  await rig.advance(5 * pA + 0.013);
  const before = rig.draws();
  const nextN = before.length; // beats 0..nextN-1 are already queued on grid A
  const expectedNextT = now0 + nextN * pA;
  const nowChange = rig.now();
  rig.clock.setBpm(bpmB);
  await rig.advance(6 * pB + 0.05);
  const after = since(rig.draws(), before.length);
  ok(`B next beat keeps the OUTGOING grid's time ${bpmA}->${bpmB}`,
    after.length > 0 && approx(after[0].time, expectedNextT), `got ${after[0]?.time} want ${expectedNextT}`);
  ok(`B bar index continuous across the re-anchor ${bpmA}->${bpmB}`,
    after[0]?.beat === nextN % 4 && before.every((b, n) => b.beat === n % 4));
  ok(`B post-change spacing == new period ${bpmA}->${bpmB}`,
    after.every((b, i) => i === 0 || approx(b.time - after[i - 1].time, pB)));
  ok(`B later beats keep counting the bar ${bpmA}->${bpmB}`, after.every((b, i) => b.beat === (nextN + i) % 4));
  const all = [...before, ...after];
  ok(`B no beat fired twice ${bpmA}->${bpmB}`, all.every((b, i) => i === 0 || b.time > all[i - 1].time));
  ok(`B no beat fired in the past ${bpmA}->${bpmB}`, after.every((b) => b.time >= b.at && b.time >= nowChange));
}

console.log('=== C. Master reset (clearAll) falls back to free-run CARRYING the loop grid phase ===');
{
  // A committed 2-bar loop at 137 bpm: the master beat period is the integer-frame one, slightly off 60/137.
  const rig = await bootLooper({ startTime: 10 });
  const { looper, clock, state } = rig;
  clock.setBpm(137);
  rig.setInput(0.5);
  await looper.recDub(0);
  await rig.advance(4 * 60 / 137 + 0.1 + 2 * 4 * 60 / 137 + 0.03); // count-in + two bars
  await looper.recDub(0);
  await rig.advance(0.5);
  const master = looper.masterLengthFrames();
  const masterPeriod = master / rig.sr / 8;
  ok('C precondition: a 2-bar master is playing', master > 0 && looper.trackInfo(0).state === 'PLAYING', `master=${master}`);
  await rig.advance(60); // a minute of loop playback
  const anchor = state.engineState.masterStartTime;
  const beforeReset = rig.draws();
  const last = beforeReset[beforeReset.length - 1];
  const lastN = indexOn(last.time, anchor, masterPeriod);
  ok('C the loop grid is the master grid', approx(last.time, anchor + lastN * masterPeriod, 1e-9));
  const expectedNextT = anchor + (lastN + 1) * masterPeriod;
  looper.clearAll(); // resetMaster -> stopMasterPulse -> free-run
  await rig.advance(3);
  const after = since(rig.draws(), beforeReset.length);
  ok('C first free-run beat lands where the master grid had it (sub-ns)',
    after.length > 0 && approx(after[0].time, expectedNextT, 1e-9),
    `got ${after[0]?.time} want ${expectedNextT} (err ${Math.abs(after[0]?.time - expectedNextT)})`);
  ok('C seam gap <= one period (no LED hop at reset)',
    after[0]?.time - last.time <= Math.max(masterPeriod, 60 / clock.bpm()) + 1e-9, `gap=${after[0]?.time - last.time}`);
  ok('C bar index continuous across the reset', after[0]?.beat === (lastN + 1) % 4);
  ok('C free-run cadence after the reset is 60/bpm', after.every((b, i) => i === 0 || approx(b.time - after[i - 1].time, 60 / clock.bpm())));
  ok('C BPM unlocked by the reset', clock.bpmLocked() === false);
}

console.log('=== D. Count abort falls back clean: no forced click, no count numeral after it ===');
{
  const rig = await bootLooper({ startTime: 100 });
  const { looper, clock } = rig;
  ok('D metronome is off', clock.metronomeOn() === false);
  await looper.recDub(0); // first take: one-bar count-in, forced audible
  await rig.advance(1.1); // beats 0, 1 and 2 of the count queued
  const countClicks = rig.clicks().length;
  ok('D the count clicked with the metronome off', countClicks >= 2, `clicks=${countClicks}`);
  await looper.recDub(0); // abort during the count
  ok('D abort returns the lane to EMPTY', looper.trackInfo(0).state === 'EMPTY');
  const mark = rig.draws().length;
  // A count click already dispatched into the lookahead may still sound; nothing may be scheduled later.
  const cutoff = rig.now() + 0.1;
  await rig.advance(3);
  ok('D no click scheduled after the abort', rig.clicks().every((c) => c.time < cutoff),
    JSON.stringify(rig.clicks().map((c) => c.time)));
  ok('D no count numeral after the abort', since(rig.draws(), mark).every((b) => b.countLeft === 0));
  ok('D the LED keeps beating on free-run', since(rig.draws(), mark).length >= 5);
}

console.log('=== E. setBpm re-anchors ONLY a live free-run pulse (count/master periods stay frozen) ===');
{
  // A count-in pulse (unlocked here, to reach the pulse gate itself): setBpm must not touch its grid.
  const rig = await bootLooper({ init: false, startTime: 100 });
  const { clock } = rig;
  const anchor = rig.now() + HBL;
  clock.startCountIn(anchor, 0.5, 4);
  await rig.advance(0.3);
  clock.setBpm(90);
  await rig.advance(3);
  const beats = rig.draws();
  ok('E count pulse NOT re-anchored by setBpm', beats.length >= 6 && beats.every((b, n) => b.time === anchor + n * 0.5),
    JSON.stringify(beats.map((b) => b.time)));
  ok('E count numerals 4..1 intact', [4, 3, 2, 1].every((left, n) => beats[n]?.countLeft === left));
  ok('E bpm value itself did change (unlocked)', clock.bpm() === 90);
}
{
  // A master pulse while BPM is locked: neither the bpm nor the pulse moves.
  const rig = await bootLooper({ init: false, startTime: 5 });
  const { clock } = rig;
  const anchor = rig.now() + HBL;
  clock.startMasterPulse(anchor, 0.4999);
  clock.setBpmLocked(true);
  await rig.advance(1);
  clock.setBpm(90);
  await rig.advance(2);
  ok('E master pulse untouched + bpm frozen while locked',
    clock.bpm() === 120 && rig.draws().every((b, n) => b.time === anchor + n * 0.4999));
}
{
  // A live free-run pulse DOES re-anchor.
  const rig = await bootLooper({ init: false, startTime: 10 });
  const { clock } = rig;
  clock.ensureRunning();
  await rig.advance(0.3);
  clock.setBpm(90);
  const mark = rig.draws().length;
  await rig.advance(3);
  const after = since(rig.draws(), mark);
  ok('E free-run pulse re-anchored to the new period', after.length > 2 && after.every((b, i) => i === 0 || approx(b.time - after[i - 1].time, 60 / 90)));
}
{
  // No pulse running yet: setBpm must not fabricate a grid.
  const rig = await bootLooper({ init: false });
  rig.clock.setBpm(90);
  await rig.advance(1);
  ok('E no pulse started when none was live', rig.draws().length === 0 && rig.timers.pending() === 0);
}
{
  // A value-identical setBpm is a FULL no-op: streaming callers (MIDI clock, tap tempo) would otherwise
  // tear down and restart the waker for nothing.
  const rig = await bootLooper({ init: false, startTime: 10 });
  const { clock, timers } = rig;
  clock.ensureRunning();
  await rig.advance(0.4);
  const created = timers.created, cleared = timers.cleared;
  clock.setBpm(120);
  clock.setBpm(120.3); // rounds to 120
  ok('E value-identical setBpm does not re-anchor', timers.created === created && timers.cleared === cleared,
    `created ${created}->${timers.created} cleared ${cleared}->${timers.cleared}`);
  const mark = rig.draws().length;
  clock.setBpm(121);
  await rig.advance(2);
  const after = since(rig.draws(), mark);
  ok('E changed setBpm still re-anchors', timers.created === created + 1 &&
    after.length > 2 && after.every((b, i) => i === 0 || approx(b.time - after[i - 1].time, 60 / 121)));
}

console.log('=== F. The carried beat survives a re-anchor at a large beat index (hours-long session) ===');
for (const N of [1, 999, 40000, 123457]) {
  const rig = await bootLooper({ init: false, startTime: 3 });
  const { clock } = rig;
  const period = 0.5;
  // A grid whose next beat index is ~N: anchored N periods before the next beat just past the horizon.
  const want = rig.now() + 0.3;
  const anchor = want - N * period;
  clock.startMasterPulse(anchor, period);
  clock.stopMasterPulse(); // -> startFreeRunPulse carries beat N onto the free-run grid
  const mark = rig.draws().length;
  await rig.advance(0.4);
  const got = rig.draws()[mark];
  ok(`F round-trip error < 1ns at N=${N}`, got && approx(got.time, want, 1e-9), `err=${Math.abs(got?.time - want)}`);
  ok(`F bar index carried at N=${N}`, got?.beat === N % 4, `beat=${got?.beat}`);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

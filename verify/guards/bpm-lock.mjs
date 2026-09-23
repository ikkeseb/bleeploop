// The count-in BPM-lock lifecycle: the REAL looper state machine (src/audio/looper/machine.ts) and the
// clock's lock gate (src/audio/clock.ts) under the verify rig.
//
// The contract: BPM locks at EVERY first-track count-in press (free or fixed), so the count the player
// hears and the committed loop share one tempo; every abort of a first take unlocks it; a committed
// loop keeps it locked; a later-track arm never touches it; and only the recording lane can release
// the capture window or the lock.

import { bootLooper } from '../harness/rig.ts';
import { framesPerBar } from '../../src/audio/quantize.ts';

let fails = 0, checks = 0;
const approx = (a, b, eps = 1e-9) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

/** The recorder window and lock, as the machine holds them. */
function recorder(rig) {
  const rec = rig.state.engineState.recording;
  return { active: rec?.track ?? -1, start: rec?.startFrame ?? null, end: rec?.endFrame ?? null, locked: rig.clock.bpmLocked() };
}
async function pressRec(rig, lane, { fixed = false, bars = 2 } = {}) {
  rig.looper.setFixedLengthEnabled(fixed);
  rig.looper.setFixedLengthBars(bars);
  await rig.looper.recDub(lane);
}

console.log('=== A. Free-record count-in: the press LOCKS BPM ===');
{
  const rig = await bootLooper();
  const mark = rig.draws().length;
  await pressRec(rig, 0);
  await rig.advance(1.2);
  ok('A free-record count-in press locks BPM', rig.clock.bpmLocked() === true);
  const count = rig.draws().slice(mark).filter((b) => b.countLeft > 0);
  ok('A count grid runs at the press tempo', count.length >= 3 && count.every((b, n) => n === 0 || approx(b.time - count[n - 1].time, 0.5)));
}

console.log('=== B. A mid-take retune is a NO-OP while locked: count grid == commit grid ===');
{
  const rig = await bootLooper();
  rig.setInput(0.5);
  await pressRec(rig, 0);
  await rig.advance(3); // through the count, into the take
  rig.clock.setBpm(100); // the user drags the tempo mid-take
  ok('B setBpm is a no-op while locked', rig.clock.bpm() === 120);
  await rig.advance(3.05);
  await rig.looper.recDub(0);
  await rig.advance(0.3);
  const master = rig.looper.masterLengthFrames();
  ok('B commit uses the count tempo: master = 2 bars at 120 bpm', master === 2 * framesPerBar(120, rig.sr), `master=${master}`);
  ok('B committed beat period == count beat period', approx(master / rig.sr / 8, 0.5));
}

console.log('=== C. Every abort path UNLOCKS a first-track count-in (free + fixed) ===');
const aborts = {
  'REC again during the count': (rig) => rig.looper.recDub(0),
  'PLAY/STOP during the count': (rig) => rig.looper.playStop(0),
  'STOP': (rig) => rig.looper.stop(0),
  'CLEAR': (rig) => rig.looper.clear(0),
  'STOP ALL': (rig) => rig.looper.stopAll(),
};
for (const fixed of [false, true]) {
  for (const [label, abort] of Object.entries(aborts)) {
    const rig = await bootLooper();
    await pressRec(rig, 0, { fixed });
    await rig.advance(0.8);
    const tag = `[${fixed ? 'fixed' : 'free'}, ${label}]`;
    ok(`C ${tag} locked after the press`, rig.clock.bpmLocked() === true);
    await abort(rig);
    await rig.advance(0.1);
    const r = recorder(rig);
    ok(`C ${tag} abort UNLOCKS BPM`, r.locked === false);
    ok(`C ${tag} abort clears the capture window and the recorder`, r.active === -1 && r.start === null && r.end === null, JSON.stringify(r));
    ok(`C ${tag} the lane is EMPTY`, rig.looper.trackInfo(0).state === 'EMPTY');
    rig.clock.setBpm(90);
    ok(`C ${tag} tempo is editable again`, rig.clock.bpm() === 90);
  }
}
for (const [label, abort] of [['STOP', (rig) => rig.looper.stop(0)], ['CLEAR', (rig) => rig.looper.clear(0)]]) {
  // Past the come-in, mid-take: stop()/clear() discard an uncommitted first take and unlock too.
  const rig = await bootLooper();
  rig.setInput(0.5);
  await pressRec(rig, 0, { fixed: true, bars: 4 });
  await rig.advance(3.5);
  ok(`C [mid-take, ${label}] recording past the come-in`, rig.looper.trackInfo(0).state === 'RECORDING' && !rig.looper.trackInfo(0).armed);
  abort(rig);
  const r = recorder(rig);
  ok(`C [mid-take, ${label}] unlocks and releases`, !r.locked && r.active === -1 && r.start === null && r.end === null, JSON.stringify(r));
}

console.log('=== D. A committed loop STAYS locked ===');
{
  const rig = await bootLooper();
  const master = await rig.recordFirstTake({ bars: 2 });
  ok('D master defined', master > 0);
  ok('D committed loop keeps BPM locked', rig.clock.bpmLocked() === true);
  rig.clock.setBpm(140);
  ok('D BPM still frozen after commit', rig.clock.bpm() === 120);
}

console.log('=== E. A LATER-track arm abort must NOT unlock (the loop owns the tempo) ===');
for (const [label, abort] of [['REC again', (rig) => rig.looper.recDub(1)], ['STOP', (rig) => rig.looper.stop(1)], ['CLEAR', (rig) => rig.looper.clear(1)]]) {
  const rig = await bootLooper();
  await rig.recordFirstTake({ bars: 2 });
  await rig.looper.recDub(1); // later-track arm: waits for the next master boundary
  ok(`E [${label}] later lane armed`, rig.looper.trackInfo(1).armed === true);
  await abort(rig);
  ok(`E [${label}] later-track arm abort leaves BPM LOCKED`, rig.clock.bpmLocked() === true);
  ok(`E [${label}] the loop keeps playing`, rig.looper.trackInfo(0).state === 'PLAYING' && rig.looper.trackInfo(1).state === 'EMPTY');
}

console.log('=== F. Only the recording lane can release its capture window or the BPM lock ===');
{
  const rig = await bootLooper();
  await pressRec(rig, 2, { fixed: true, bars: 2 });
  await rig.advance(0.5);
  const before = recorder(rig);
  rig.looper.clear(1);
  rig.looper.stop(3);
  rig.looper.playStop(4);
  const after = recorder(rig);
  ok('F other lanes cannot release the recorder', after.active === 2);
  ok('F other lanes preserve both window edges', after.start === before.start && after.end === before.end && before.start !== null);
  ok('F other lanes cannot unlock BPM', after.locked === true);
  rig.looper.stop(2);
  const released = recorder(rig);
  ok('F the owner releases both edges and unlocks',
    released.active === -1 && released.start === null && released.end === null && !released.locked, JSON.stringify(released));
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

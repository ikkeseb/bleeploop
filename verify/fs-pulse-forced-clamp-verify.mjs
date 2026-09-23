// The count-in FORCED-beat clamp, the master/free-run drop, the anti-flam guard and the click's
// transport gate: the REAL clock (src/audio/clock.ts) and looper under the verify rig.
//
// What this PROVES:
//   - HAPPY PATH: every count beat fires at its true ctx time; beats 0..3 click with the metronome
//     off, the come-in beat 4 is on the LED but silent                                        [pulseTick]
//   - STALLED waker, MASTER pulse: past beats are DROPPED (no LED, no click); the grid resumes on the
//     next live beat with its bar index intact                                                [pulseTick]
//   - STALLED waker, COUNT-IN: past forced beats fire CLAMPED to the wake (LED 3-2-1 complete) and
//     the anti-flam guard collapses them to ONE click; the next live beat still sounds       [pulseTick, triggerClick]
//   - a clamped catch-up never eats the true-time come-in downbeat, at any tempo              [triggerClick]
//   - a whole metronome-on first take, committed at several phases, never double-strikes
//     (no two audible clicks < 0.12 s apart) and clicks every grid beat once                 [commit flam]
//   - the click is a transport mode: a stopped lane is silent while the LED keeps beating; forced
//     count beats bypass the gate; a loop-end stop silences the click from its deadline      [setTransportActive]

import { bootLooper } from './harness/rig.ts';
import { COUNT_IN_BEATS } from '../src/audio/looper/grid-math.ts';

let fails = 0, checks = 0;
const approx = (a, b, eps = 1e-9) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

const since = (list, mark) => list.slice(mark);
const audible = (clicks) => clicks.filter((c) => c.audible);
const times = (list) => JSON.stringify(list.map((x) => +x.time.toFixed(4)));

/** A clock-only rig (no capture worklet). */
async function clockRig({ metronome = false, transport = false, startTime = 100 } = {}) {
  const rig = await bootLooper({ init: false, startTime });
  if (metronome) rig.clock.setMetronome(true);
  rig.clock.setTransportActive(transport);
  return rig;
}

console.log('=== A. HAPPY PATH: the looper count-in fires every beat at its true time ===');
{
  const rig = await bootLooper({ startTime: 100 });
  const drawMark = rig.draws().length, clickMark = rig.clicks().length;
  await rig.looper.recDub(0); // the first take counts in one bar at 120 bpm
  await rig.advance(2.4);
  const beats = since(rig.draws(), drawMark);
  const anchor = beats[0]?.time;
  const clicks = audible(since(rig.clicks(), clickMark));
  ok('A every count beat at its exact ctx time (no clamp on the happy path)',
    beats.length >= 5 && beats.every((b, n) => approx(b.time, anchor + n * 0.5)), times(beats));
  ok('A count numerals 4-3-2-1, then 0 at the come-in', [4, 3, 2, 1, 0].every((left, n) => beats[n]?.countLeft === left));
  ok('A count beats 0..3 click with the metronome off',
    clicks.length === COUNT_IN_BEATS && clicks.every((c, n) => approx(c.time, anchor + n * 0.5)), times(clicks));
  ok('A only the count "1" is accented', clicks.every((c, n) => c.accent === (n === 0)));
  ok('A come-in beat 4 on the LED but silent', beats[4]?.beat === 0 && !clicks.some((c) => approx(c.time, anchor + 2)));
}

console.log('=== B. STALLED waker + MASTER pulse: past beats are DROPPED ===');
{
  const rig = await clockRig({ metronome: true, transport: true });
  const anchor = rig.now();
  const drawMark = rig.draws().length, clickMark = rig.clicks().length;
  rig.clock.startMasterPulse(anchor, 0.5);
  const first = since(rig.draws(), drawMark);
  ok('B first tick fires beat 0', first.length === 1 && first[0].time === anchor);
  await rig.stall(1.6); // beats 1 (0.5), 2 (1.0) and 3 (1.5) pass unserviced
  await rig.advance(0.5);
  const beats = since(rig.draws(), drawMark);
  const clicks = since(rig.clicks(), clickMark);
  const past = [1, 2, 3].map((n) => anchor + n * 0.5);
  ok('B beats 1,2,3 dropped: no LED', !beats.some((b) => past.some((t) => approx(b.time, t))), times(beats));
  ok('B beats 1,2,3 dropped: no click', !clicks.some((c) => past.some((t) => approx(c.time, t))), times(clicks));
  ok('B no master beat is clamped to the wake', beats.every((b) => Number.isInteger((b.time - anchor) / 0.5)), times(beats));
  ok('B resumes on beat 4 with its bar index intact', approx(beats[1]?.time, anchor + 2) && beats[1]?.beat === 0);
}

console.log('=== C. STALLED waker + COUNT-IN: past FORCED beats fire CLAMPED, one click ===');
{
  const rig = await clockRig();
  const anchor = rig.now();
  const drawMark = rig.draws().length, clickMark = rig.clicks().length;
  rig.clock.startCountIn(anchor, 0.5, COUNT_IN_BEATS);
  await rig.stall(1.6);
  const wake = rig.now();
  const beats = since(rig.draws(), drawMark);
  const clicks = audible(since(rig.clicks(), clickMark));
  const clamped = beats.filter((b) => b.time === wake);
  ok('C beats 1,2,3 fired clamped to the wake', clamped.length === 3, times(beats));
  ok('C every count LED 3-2-1 queued (count visually complete)', clamped.map((b) => b.countLeft).join() === '3,2,1');
  ok('C the clamped catch-up is ONE click, not a burst', clicks.filter((c) => c.time === wake).length === 1, times(clicks));
  ok('C beat 0 sounded on time before the stall', clicks[0]?.time === anchor);
  await rig.advance(0.5);
  ok('C come-in beat 4 on time on the LED', since(rig.draws(), drawMark).some((b) => approx(b.time, anchor + 2) && b.countLeft === 0));
}

console.log('=== D. The live beat after a clamp still sounds ===');
{
  const rig = await clockRig();
  const anchor = rig.now();
  const clickMark = rig.clicks().length;
  rig.clock.startCountIn(anchor, 0.5, COUNT_IN_BEATS);
  await rig.stall(1.3); // beats 1 and 2 clamp to the wake; beat 3 (1.5) is still ahead
  const wake = rig.now();
  await rig.advance(0.4);
  const clicks = audible(since(rig.clicks(), clickMark));
  ok('D one clamped catch-up click', clicks.filter((c) => c.time === wake).length === 1, times(clicks));
  ok('D the following live beat (0.2 s later) fires', clicks.some((c) => approx(c.time, anchor + 1.5)), times(clicks));
}

console.log('=== F. A clamped catch-up must NOT suppress the true-time come-in downbeat ===');
for (const bpm of [40, 100, 120, 200]) {
  // Metronome ON: the come-in "1" clicks. The stall ends 80 ms before it, inside the 0.12 s window of
  // the clamped beats 1-3; the downbeat must still sound at its true time.
  const period = 60 / bpm;
  const rig = await clockRig({ metronome: true, transport: true });
  const anchor = rig.now();
  const clickMark = rig.clicks().length;
  rig.clock.startCountIn(anchor, period, COUNT_IN_BEATS);
  await rig.stall(4 * period - 0.08);
  await rig.advance(0.2);
  const clicks = audible(since(rig.clicks(), clickMark));
  const wake = anchor + 4 * period - 0.08;
  ok(`F one clamped catch-up click bpm=${bpm}`, clicks.filter((c) => approx(c.time, wake, 1e-6)).length === 1, times(clicks));
  ok(`F come-in downbeat CLICKS at its true time bpm=${bpm}`,
    clicks.some((c) => approx(c.time, anchor + 4 * period) && c.accent), times(clicks));
}

console.log('=== G. A metronome-on first take never double-strikes, at any commit phase ===');
for (const stopAfterBeats of [8.0, 8.02, 8.2, 8.5, 8.9, 9.97]) {
  // Count-in, a free take with the click on, the commit re-anchor, then two loops of playback.
  const rig = await bootLooper({ startTime: 20 });
  const { looper, clock, state } = rig;
  clock.setMetronome(true);
  rig.setInput(0.25);
  await looper.recDub(0);
  // Stop `stopAfterBeats` beats after the come-in downbeat (count anchor + one bar).
  await rig.advanceTo(rig.draws().find((b) => b.countLeft === 4).time + 2 + stopAfterBeats * 0.5);
  await looper.recDub(0);
  await rig.advance(9);
  const master = looper.masterLengthFrames();
  ok(`G take committed (stop after ${stopAfterBeats} beats)`, master > 0 && looper.trackInfo(0).state === 'PLAYING', `master=${master}`);
  const clicks = audible(rig.clicks()).sort((a, b) => a.time - b.time);
  const flams = clicks.filter((c, i) => i > 0 && c.time - clicks[i - 1].time < 0.12);
  ok(`G no two audible clicks < 0.12 s apart (stop after ${stopAfterBeats} beats)`, flams.length === 0, times(flams));
  // After the commit every grid beat sounds exactly once.
  const beat = master / rig.sr / (4 * Math.round(master / 96000));
  const anchor = state.engineState.masterStartTime;
  const from = rig.now() - 6, to = rig.now() - 1;
  const expected = [];
  for (let n = Math.ceil((from - anchor) / beat); anchor + n * beat < to; n++) expected.push(anchor + n * beat);
  const missing = expected.filter((t) => !clicks.some((c) => approx(c.time, t, 1e-6)));
  ok(`G every post-commit grid beat clicks once (stop after ${stopAfterBeats} beats)`, missing.length === 0 && expected.length >= 8,
    `missing ${times(missing.map((time) => ({ time })))}`);
}

console.log('=== H. The click is a transport mode ===');
{
  // A playing loop clicks every beat; STOP silences it while the LED keeps beating; PLAY resumes it.
  const rig = await bootLooper({ startTime: 10 });
  const { looper, clock } = rig;
  clock.setMetronome(true);
  rig.setInput(0.25);
  await looper.recDub(0);
  await rig.advance(2.1 + 2);
  await looper.recDub(0);
  await rig.advance(3);
  looper.playStop(0);
  ok('H precondition: the lane is STOPPED', looper.trackInfo(0).state === 'STOPPED');
  const cutoff = rig.now() + 0.1; // a blip already dispatched may still be cancelled or sound
  let drawMark = rig.draws().length;
  await rig.advance(3);
  ok('H stopped transport: LED keeps beating', since(rig.draws(), drawMark).length >= 5);
  ok('H stopped transport: no audible click after the stop', audible(rig.clicks()).every((c) => c.time < cutoff),
    times(audible(rig.clicks()).filter((c) => c.time >= cutoff)));
  looper.playStop(0);
  drawMark = rig.draws().length;
  const clickMark = rig.clicks().length;
  await rig.advance(3);
  const beats = since(rig.draws(), drawMark), clicks = audible(since(rig.clicks(), clickMark));
  ok('H live transport again: every beat clicks', beats.length >= 5 && beats.every((b) => clicks.some((c) => approx(c.time, b.time))),
    `beats=${beats.length} clicks=${clicks.length}`);
}
{
  // Forced count beats bypass the gate: transport inactive, metronome off.
  const rig = await clockRig();
  const anchor = rig.now() + 0.02;
  const clickMark = rig.clicks().length;
  rig.clock.startCountIn(anchor, 0.5, COUNT_IN_BEATS);
  await rig.advance(2.5);
  const clicks = audible(since(rig.clicks(), clickMark));
  ok('H forced count beats 0..3 click despite an inactive transport and the metronome off',
    clicks.length === 4 && clicks.every((c, n) => approx(c.time, anchor + n * 0.5)), times(clicks));
}
{
  // A loop-end stop: the beat on the stop deadline stays on the LED but is silent.
  const rig = await bootLooper({ startTime: 10 });
  const { looper, clock } = rig;
  clock.setMetronome(true);
  rig.setInput(0.25);
  await looper.recDub(0);
  await rig.advance(2.1 + 2);
  await looper.recDub(0);
  await rig.advance(1.3);
  looper.setLoopEndStopEnabled(true);
  looper.playStop(0);
  const stopAt = looper.trackInfo(0).stopAt;
  ok('I loop-end stop pending on a future boundary', stopAt !== null && stopAt > rig.now());
  const clickMark = rig.clicks().length;
  await rig.advance(3);
  const beats = rig.draws().filter((b) => approx(b.time, stopAt, 1e-6));
  const clicks = since(rig.clicks(), clickMark);
  ok('I the beat at the deadline stays on the LED', beats.length === 1);
  ok('I the beat at the deadline is silent', !audible(rig.clicks()).some((c) => c.time >= stopAt - 1e-9), times(audible(clicks)));
  ok('I the lane stopped at the deadline', looper.trackInfo(0).state === 'STOPPED');
}
{
  // Forced count beats bypass a pending stop cutoff.
  const rig = await clockRig({ transport: true });
  const anchor = rig.now() + 0.02;
  rig.clock.setTransportActive(true, anchor + 0.25);
  const clickMark = rig.clicks().length;
  rig.clock.startCountIn(anchor, 0.5, 2);
  await rig.advance(1);
  const clicks = audible(since(rig.clicks(), clickMark));
  ok('I forced count beats still bypass a pending stop cutoff', clicks.length === 2 && approx(clicks[1].time, anchor + 0.5), times(clicks));
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

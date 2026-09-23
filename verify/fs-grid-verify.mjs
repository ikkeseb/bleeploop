// The looper<->click "one grid": the REAL clock pulse (src/audio/clock.ts) driven by the verify rig on
// a committed master from the REAL planCommit (src/audio/looper/grid-math.ts).
//
// What this PROVES:
//   - beatPeriod is exact (= fpb/(4*sr)) and 4*bars beats span one loop period            [planCommit]
//   - startMasterPulse schedules beat 0 exactly on the anchor at the call; a past anchor resumes on
//     the next future integer beat                                                         [startMasterPulse]
//   - over 12 minutes of a jittered 25 ms waker every beat is scheduled once, none skipped, none in
//     the past, each at anchor + N*master/(4*bars*sr) to < 1 ns, and every loop boundary is an
//     accented click                                                                        [pulseTick]
//   - a stalled waker drops the past beats and resumes on-grid without a catch-up burst     [pulseTick]
//   - the anti-flam guard suppresses only a true sub-0.12 s coincidence                     [triggerClick]
// What it does NOT prove (by ear on the PC): audible flam, the analog feel, real-hardware clock drift.

import { bootLooper } from './harness/rig.ts';
import { framesPerBar } from '../src/audio/quantize.ts';
import { HEARTBEAT_INTERNAL_LATENCY as HBL, planCommit } from '../src/audio/looper/grid-math.ts';

let fails = 0, checks = 0;
const approx = (a, b, eps) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); }
}

function commitGrid(bpmReq, sr, bars) {
  const fpb = framesPerBar(bpmReq, sr);
  const plan = planCommit(bars * fpb, bpmReq, sr, Number.MAX_SAFE_INTEGER); // buffer never clamps here
  return { fpb: plan.fpb, bars: plan.bars, master: plan.master, beatPeriod: plan.beatPeriod, loopPeriod: plan.period };
}

/** A clock-only rig (no capture worklet) that clicks every beat: metronome on, transport live. */
async function clickingRig(options) {
  const rig = await bootLooper({ init: false, ...options });
  rig.clock.setMetronome(true);
  rig.clock.setTransportActive(true);
  return rig;
}
const since = (list, mark) => list.slice(mark);

console.log('=== A. beatPeriod is exact (= fpb/(4*sr)) and bars-independent ===');
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 90, 137, 100, 200, 73.5]) {
    for (const bars of [1, 2, 4, 8, 3, 7]) {
      const g = commitGrid(bpm, sr, bars);
      // master/sr/(4*bars) can differ from fpb/(4*sr) by ~1 ulp; the boundary coincidence (C) is what binds.
      ok(`A period~=fpb/4sr sr=${sr} bpm=${bpm} bars=${bars}`,
        approx(g.beatPeriod, g.fpb / (4 * sr), g.beatPeriod * 1e-12), `got ${g.beatPeriod} vs ${g.fpb / (4 * sr)}`);
      ok(`A 4*bars*period==loopPeriod sr=${sr} bpm=${bpm} bars=${bars}`,
        approx(4 * bars * g.beatPeriod, g.loopPeriod, 1e-9), `${4 * bars * g.beatPeriod} vs ${g.loopPeriod}`);
    }
  }
}

console.log('=== B. startMasterPulse: beat 0 on the anchor at once; a past anchor resumes on the next beat ===');
for (const sr of [48000, 44100]) {
  for (const [bpm, bars] of [[120, 2], [137, 1], [90, 4], [200, 3]]) {
    const g = commitGrid(bpm, sr, bars);
    const rig = await bootLooper({ sampleRate: sr, init: false });
    const now = rig.now();
    const anchor = now + HBL; // the commit lead: well inside the 0.1 s lookahead
    const mark = rig.draws().length;
    rig.clock.startMasterPulse(anchor, g.beatPeriod);
    const first = rig.draws()[mark];
    ok(`B beat 0 queued at the call sr=${sr} bpm=${bpm}`, first?.at === now, `first=${JSON.stringify(first)}`);
    ok(`B beat 0 exactly on the anchor, accented sr=${sr} bpm=${bpm}`,
      first?.time === anchor && first?.beat === 0, `first=${JSON.stringify(first)}`);
  }
}
{
  const g = commitGrid(120, 48000, 1); // beat period 0.5
  const rig = await bootLooper({ startTime: 10, init: false });
  const anchor = rig.now() - 1.3; // 2.6 periods in the past
  const mark = rig.draws().length;
  rig.clock.startMasterPulse(anchor, g.beatPeriod);
  for (let w = 0; w < 100 && rig.draws().length === mark; w++) await rig.advance(0.025);
  const first = rig.draws()[mark];
  ok('B past anchor resumes at beat 3 on the grid',
    first?.time === anchor + 3 * g.beatPeriod && first?.beat === 3, `first=${JSON.stringify(first)}`);
  ok('B past anchor never schedules a beat in the past', first && first.time >= first.at, `first=${JSON.stringify(first)}`);
}

console.log('=== C. ZERO phase walk: 12 minutes of a jittered waker, every loop boundary an accented click ===');
for (const [bpm, sr, bars] of [[120, 48000, 2], [137, 44100, 1], [100, 48000, 4], [73.5, 44100, 3], [200, 48000, 8]]) {
  const g = commitGrid(bpm, sr, bars);
  const rig = await clickingRig({ sampleRate: sr });
  const anchor = rig.now() + HBL;
  const drawMark = rig.draws().length, clickMark = rig.clicks().length;
  rig.clock.startMasterPulse(anchor, g.beatPeriod);
  // The waker's cadence slips by up to 15 ms (GC / scheduling slop), never beyond the 0.1 s lookahead.
  const slip = [0, 0.006, 0, 0.002, 0, 0.015, 0];
  const end = anchor + 12 * 60;
  for (let i = 0; rig.now() < end; i++) {
    await rig.advance(0.2);
    if (slip[i % slip.length] > 0) await rig.stall(slip[i % slip.length]);
  }
  const beats = since(rig.draws(), drawMark);
  const clicks = since(rig.clicks(), clickMark);
  let late = 0, maxBeatErr = 0, badIndex = 0;
  beats.forEach((b, n) => {
    if (b.time < b.at) late++;
    if (b.beat !== n % 4) badIndex++;
    // Derived INDEPENDENTLY from the integer-frame master: a period taken from the requested bpm is
    // off by microseconds per beat and walks to milliseconds over 12 minutes.
    maxBeatErr = Math.max(maxBeatErr, Math.abs(b.time - (anchor + (n * g.master) / (4 * bars * sr))));
  });
  const expected = Math.floor((rig.now() + 0.1 - anchor) / g.beatPeriod) + 1; // every beat inside the horizon
  ok(`C every beat scheduled exactly once bpm=${bpm}`, Math.abs(beats.length - expected) <= 1,
    `scheduled=${beats.length} expected≈${expected}`);
  ok(`C beats strictly in order, none twice bpm=${bpm}`, beats.every((b, n) => n === 0 || b.time > beats[n - 1].time));
  ok(`C never scheduled late bpm=${bpm}`, late === 0, `late=${late}`);
  ok(`C bar index = N % 4 bpm=${bpm}`, badIndex === 0, `bad=${badIndex}`);
  ok(`C beat time == integer-frame derivation bpm=${bpm}`, maxBeatErr < 1e-9, `maxBeatErr=${maxBeatErr}`);
  // The load-bearing one: every loop boundary (anchor + k*loopPeriod, the integer-frame audio loop) is an
  // accented CLICK within a sample, with no growth across the run.
  const perLoop = 4 * bars;
  let maxOff = 0, missing = 0;
  for (let k = 0; k * perLoop < beats.length; k++) {
    const boundary = anchor + k * g.loopPeriod;
    const click = clicks.find((c) => Math.abs(c.time - boundary) < 1 / sr);
    if (!click || !click.accent || !click.audible) { missing++; continue; }
    maxOff = Math.max(maxOff, Math.abs(click.time - boundary));
  }
  ok(`C every loop boundary is an audible accented click bpm=${bpm}`, missing === 0, `missing=${missing}`);
  ok(`C downbeat<->boundary offset < 1 us over 12 min bpm=${bpm}`, maxOff < 1e-6, `maxOff=${(maxOff * 1e9).toFixed(3)}ns`);
  ok(`C every beat clicked (metronome on, transport live) bpm=${bpm}`, clicks.length === beats.length,
    `clicks=${clicks.length} beats=${beats.length}`);
}

console.log('=== D. A stalled waker drops past beats (no catch-up burst) and resumes on-grid ===');
{
  const g = commitGrid(120, 48000, 1); // beat period 0.5
  const rig = await bootLooper({ startTime: 10, init: false });
  const anchor = rig.now();
  const mark = rig.draws().length;
  rig.clock.startMasterPulse(anchor, g.beatPeriod);
  await rig.advance(0.01);
  const before = since(rig.draws(), mark).length;
  // No waker for ~1.94 s (GC / buffer-size change / ASIO<->WASAPI switch): beats 1..3 pass unserviced and
  // the late wake lands just before beat 4.
  await rig.stall(1.94);
  const woke = since(rig.draws(), mark + before);
  const wakeAt = woke[0]?.at;
  const burst = woke.filter((b) => b.at === wakeAt);
  ok('D the late wake schedules only future beats', burst.length > 0 && burst.every((b) => b.time >= b.at),
    JSON.stringify(burst));
  ok('D resumes exactly on-grid', burst.every((b) => Number.isInteger((b.time - anchor) / g.beatPeriod)));
  ok('D did not machine-gun', burst.length <= 1 + Math.ceil(0.1 / g.beatPeriod), `burst=${burst.length}`);
  ok('D the stale beats were dropped, not replayed: the wake resumes at beat 4',
    before === 1 && Math.round((burst[0]?.time - anchor) / g.beatPeriod) === 4 && burst[0]?.beat === 0,
    `before=${before} first resumed beat=${(burst[0]?.time - anchor) / g.beatPeriod}`);
}

console.log('=== E. Anti-flam: suppress ONLY a true sub-0.12 s coincidence ===');
{
  // E1: quarter notes at 300 bpm are 0.2 s apart: none may be suppressed.
  const rig = await clickingRig();
  const drawMark = rig.draws().length, clickMark = rig.clicks().length;
  rig.clock.startMasterPulse(rig.now() + HBL, 0.2);
  await rig.advance(200);
  const beats = since(rig.draws(), drawMark).length, clicks = since(rig.clicks(), clickMark);
  ok('E1 no legit 300 bpm beat suppressed', beats >= 1000 && clicks.length === beats && clicks.every((c) => c.audible),
    `beats=${beats} clicks=${clicks.length}`);
}
{
  // E2/E3: the commit flam — the outgoing pulse already dispatched a blip at A; the re-anchored pulse's
  // beat 0 lands 10 ms later. One strike only; the next real beat still sounds.
  const rig = await clickingRig();
  const A = rig.now() + 0.05;
  rig.clock.startMasterPulse(A, 0.5);
  const clickMark = rig.clicks().length;
  rig.clock.startMasterPulse(A + 0.01, 0.5);
  await rig.advance(0.7);
  const after = since(rig.clicks(), clickMark);
  ok('E2 the outgoing blip was scheduled', rig.clicks().some((c) => c.time === A));
  ok('E2 the re-anchored beat 0 10 ms later is suppressed', !after.some((c) => c.time === A + 0.01),
    JSON.stringify(after.map((c) => c.time)));
  ok('E3 the next real beat sounds', after.some((c) => c.time === A + 0.01 + 0.5 && c.audible));
}
{
  // E4: a long silent stretch never makes a stale spacing reference suppress the next click.
  const rig = await clickingRig();
  rig.clock.startMasterPulse(rig.now() + HBL, 0.5);
  await rig.advance(1);
  rig.clock.setMetronome(false);
  await rig.advance(300);
  const mark = rig.clicks().length;
  rig.clock.setMetronome(true);
  await rig.advance(1);
  ok('E4 long idle, then the click fires', since(rig.clicks(), mark).some((c) => c.audible));
}
{
  // E5: exactly 0.12 s apart is NOT inside the window (strict <). 0.62 - 0.5 is exactly the double 0.12.
  ok('E5 precondition: (0.5 + 0.12) - 0.5 === 0.12', 0.5 + 0.12 - 0.5 === 0.12);
  const rig = await clickingRig({ startTime: 0.4 });
  const mark = rig.clicks().length;
  rig.clock.startMasterPulse(0.5, 0.12);
  await rig.advance(0.3);
  const times = since(rig.clicks(), mark).map((c) => c.time);
  ok('E5 exactly 0.12 apart fires', times.includes(0.5) && times.includes(0.5 + 0.12), JSON.stringify(times));
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

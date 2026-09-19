// Executable verification of the looper<->click "one grid" math.
// Ports the EXACT formulae from src/audio/clock.ts and src/audio/looper/machine.ts commitMasterLoop
// (looper.ts was split into src/audio/looper/{state,capture,peaks,playback,machine,mixer}.ts + a facade
// on 2026-07-01; cited by line) and runs them across many
// configs + adversarial edge cases.
//
// What this PROVES (the deterministic core the docs say "static review cannot confirm"):
//   - beat 0 lands exactly on masterStartTime (the loop downbeat)        [clock.ts:319]
//   - every click downbeat coincides with the audio loop boundary, and
//     the offset does NOT accumulate over 10+ minutes (zero phase walk)  [the core claim]
//   - the lookahead scheduler schedules every beat exactly once — none
//     repeated, none skipped, none in the past — each at a ctx time that
//     matches the integer-frame derivation to < 1 ns                     [clock.ts:281-303]
//   - a stalled/late waker skips past beats (no catch-up machine-gun)    [clock.ts:286-291]
//   - the anti-flam guard suppresses ONLY a true sub-0.12s coincidence,
//     and never a legitimate >=0.2s beat                                 [clock.ts:181-214]
// What it does NOT prove (genuinely needs a by-ear check on the PC): audible flam, the analog
// feel, real-hardware clock drift over a long live session.

let fails = 0, checks = 0;
const approx = (a, b, eps) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); }
}

// ---- commitMasterLoop period: the REAL planCommit (src/audio/looper/grid-math.ts, pure) — no port ----
import { framesPerBar } from '../src/audio/quantize.ts';
import { planCommit } from '../src/audio/looper/grid-math.ts';
function commitGrid(bpmReq, sr, barsReq) {
  const bars = Math.max(1, barsReq); // raw already whole bars here
  const fpb = framesPerBar(bpmReq, sr);
  const plan = planCommit(bars * fpb, bpmReq, sr, Number.MAX_SAFE_INTEGER); // buffer never clamps here
  return { fpb: plan.fpb, bars: plan.bars, master: plan.master, beatPeriod: plan.beatPeriod, loopPeriod: plan.period };
}

// MIRRORS: src/audio/clock.ts@335-373 sha256:2eb1ec34ed38d50d  (pulseTick — master-anchored lookahead pulse)
// ---- the master-anchored lookahead pulse, faithfully ported ----
const PULSE_LOOKAHEAD = 0.1; // clock.ts:238
function makePulse(anchor, period, now0) {
  // startMasterPulse init, clock.ts:319
  const nextN0 = Math.max(0, Math.ceil((now0 - anchor) / period));
  return { anchor, period, nextN: nextN0 };
}
// Models scheduled grid/LED beats, not click audibility. The loop-end transport cutoff leaves
// this grid running; fs-pulse-forced-clamp-verify and loop-end-stop cover its audible gate.
// pulseTick, clock.ts:281-303 — returns the beats it SCHEDULED at this wake (time, beatInBar).
function pulseTick(st, now) {
  const horizon = now + PULSE_LOOKAHEAD;
  const fired = [];
  let t = st.anchor + st.nextN * st.period;       // clock.ts:285
  let guard = 0;
  while (t < horizon) {                            // clock.ts:286
    if (++guard > 1e7) throw new Error('pulseTick runaway (would machine-gun)');
    if (t >= now) {                                // clock.ts:291  skip past beats
      fired.push({ N: st.nextN, time: t, beatInBar: st.nextN % 4 });
    }
    st.nextN++;                                     // clock.ts:300
    t = st.anchor + st.nextN * st.period;          // clock.ts:301
  }
  return fired;
}
const PULSE_INTERVAL = 0.025; // clock.ts:239 — the real 25ms waker cadence
// Mirror the real system: the waker fires every 25ms; a beat exactly AT the horizon edge is simply
// picked up on the next wake (never missed, never late). Returns the first beat actually scheduled.
function runWakerUntilFired(st, startNow, maxWakes = 100) {
  let now = startNow;
  for (let w = 0; w < maxWakes; w++) {
    const fired = pulseTick(st, now);
    if (fired.length) return { first: fired[0], wake: w, now };
    now += PULSE_INTERVAL;
  }
  return { first: undefined, wake: maxWakes, now };
}
// ulp distance for the relaxed exactness check

// ---- clock.ts:181-214 anti-flam guard (just the gating decision) ----
const MIN_CLICK_SPACING = 0.12; // clock.ts:181
function makeClickGate() { return { last: -1 }; }  // clock.ts:182 lastClickTime = -1
function triggerClick(g, time) {                   // clock.ts:190
  if (g.last >= 0 && Math.abs(time - g.last) < MIN_CLICK_SPACING) return false; // :194 suppressed
  g.last = time;                                   // :195
  return true;                                     // fired
}

console.log('=== A. beatPeriod is exact (= fpb/(4*sr)) and bars-independent ===');
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 90, 137, 100, 200, 73.5]) {
    for (const bars of [1, 2, 4, 8, 3, 7]) {
      const g = commitGrid(bpm, sr, bars);
      // beatPeriod equals fpb/(4*sr) to float precision (bars-independent). The code computes it as
      // master/sr/(4*bars); that can differ from fpb/(4*sr) by ~1 ulp (~1e-16) — irrelevant. The
      // load-bearing property is the boundary coincidence (section C), not bit-identity here.
      ok(`A period~=fpb/4sr sr=${sr} bpm=${bpm} bars=${bars}`,
        approx(g.beatPeriod, g.fpb / (4 * sr), g.beatPeriod * 1e-12),
        `got ${g.beatPeriod} vs ${g.fpb/(4*sr)}`);
      // 4*bars beats must span exactly one loop period (to < 1 nanosecond of float residual).
      ok(`A 4*bars*period==loopPeriod sr=${sr} bpm=${bpm} bars=${bars}`,
        approx(4 * bars * g.beatPeriod, g.loopPeriod, 1e-9),
        `${4*bars*g.beatPeriod} vs ${g.loopPeriod}`);
    }
  }
}

console.log('=== B. beat 0 lands exactly on masterStartTime at commit ===');
{
  // commit: anchor = now + HEARTBEAT_INTERNAL_LATENCY (0.02, looper/state.ts:27) -> well inside the 0.1
  // lookahead, so beat 0 fires on the FIRST wake at exactly the anchor (the loop downbeat).
  const HBL = 0.02;
  for (const sr of [48000, 44100]) {
    for (const [bpm, bars] of [[120,2],[137,1],[90,4],[200,3]]) {
      const g = commitGrid(bpm, sr, bars);
      const now = 1234.5678;
      const anchor = now + HBL;
      const st = makePulse(anchor, g.beatPeriod, now);
      ok(`B nextN==0 sr=${sr} bpm=${bpm}`, st.nextN === 0, `nextN=${st.nextN}`);
      const { first } = runWakerUntilFired(st, now);
      ok(`B beat0 at anchor sr=${sr} bpm=${bpm}`,
        first && first.N === 0 && first.time === anchor && first.beatInBar === 0,
        `first=${JSON.stringify(first)}`);
    }
  }
  // Defensive: anchor already in the PAST by 2.6 periods -> next scheduled beat is the next FUTURE
  // integer beat (3), never a past one. The waker advances until it enters the lookahead horizon.
  const g = commitGrid(120, 48000, 1); // period 0.5
  const now = 1000, anchor = now - 1.3; // 2.6 periods in the past
  const st = makePulse(anchor, g.beatPeriod, now);
  ok('B past-anchor nextN=ceil(2.6)=3', st.nextN === 3, `nextN=${st.nextN}`);
  const { first } = runWakerUntilFired(st, now);
  ok('B past-anchor first future beat is on-grid & >= now',
    first && first.N === 3 && first.time === anchor + 3 * g.beatPeriod && first.time >= now,
    `first=${JSON.stringify(first)}`);
}

console.log('=== C. ZERO phase walk: click downbeats == loop boundaries over 10+ min ===');
for (const [bpm, sr, bars] of [[120,48000,2],[137,44100,1],[100,48000,4],[73.5,44100,3],[200,48000,8]]) {
  const g = commitGrid(bpm, sr, bars);
  const anchor = 5.0;
  // Simulate ~12 minutes of wakes at the real 25ms cadence (clock.ts:239), with realistic jitter.
  const st = makePulse(anchor, g.beatPeriod, anchor);
  const horizonEnd = anchor + 12 * 60; // 12 minutes
  let now = anchor;
  let scheduledN = -1, doubleSched = false, skipped = false, late = false, maxBeatErr = 0;
  // jitter the waker so it isn't a perfect 25ms — emulate GC/scheduling slop, never pacing the audio.
  const jit = [0.025, 0.031, 0.018, 0.027, 0.024, 0.040, 0.012];
  let ji = 0;
  while (now < horizonEnd) {
    const fired = pulseTick(st, now);
    for (const f of fired) {
      if (f.N <= scheduledN) doubleSched = true;     // each beat scheduled at most once
      if (f.N > scheduledN + 1) skipped = true;      // ...and at least once: the jitter below stays under
      scheduledN = f.N;                              //    the 0.1s lookahead, so no beat may be dropped
      if (f.time < now) late = true;                 // never scheduled in the past
      // Re-derive the beat time INDEPENDENTLY from the integer-frame domain (master frames over
      // 4*bars*sr) rather than from the scheduler's own beatPeriod: a period taken from the REQUESTED
      // bpm instead of the committed integer master is off by ~us per beat and walks to ms over 12 min.
      const fromFrames = anchor + (f.N * g.master) / (4 * bars * sr);
      maxBeatErr = Math.max(maxBeatErr, Math.abs(f.time - fromFrames));
    }
    now += jit[ji++ % jit.length];
  }
  ok(`C each beat once bpm=${bpm}`, !doubleSched);
  ok(`C no beat skipped bpm=${bpm}`, !skipped, `lastN=${scheduledN}`);
  ok(`C never late bpm=${bpm}`, !late);
  ok(`C beat time == integer-frame derivation bpm=${bpm}`, maxBeatErr < 1e-9, `maxBeatErr=${maxBeatErr}`);

  // The load-bearing one: every loop boundary (anchor + k*loopPeriod, the integer-frame audio loop)
  // must coincide with a click DOWNBEAT (beat index 4*bars*k), and the offset must stay < 1 sample
  // AND not grow across the whole run (no accumulation = no phase walk).
  const loops = Math.floor((horizonEnd - anchor) / g.loopPeriod);
  let maxBoundaryOffset = 0, firstOffset = null, lastOffset = 0;
  for (let k = 0; k <= loops; k++) {
    const loopBoundary = anchor + k * g.loopPeriod;            // audio engine, integer frames
    const clickDownbeat = anchor + (4 * bars * k) * g.beatPeriod; // the click's downbeat for loop k
    const off = Math.abs(loopBoundary - clickDownbeat);
    maxBoundaryOffset = Math.max(maxBoundaryOffset, off);
    if (firstOffset === null) firstOffset = off;
    lastOffset = off;
  }
  const oneSample = 1 / sr;
  ok(`C downbeat==boundary < 1 sample bpm=${bpm} (${loops} loops)`,
    maxBoundaryOffset < oneSample,
    `maxOff=${(maxBoundaryOffset*1e9).toFixed(3)}ns  1sample=${(oneSample*1e9).toFixed(0)}ns`);
  // No accumulation: offset at the LAST loop is within a few ulp of the FIRST (not growing linearly).
  ok(`C offset does not accumulate bpm=${bpm}`,
    maxBoundaryOffset < 1e-6,  // < 1 microsecond across 12 min is "does not walk"
    `first=${(firstOffset*1e9).toFixed(3)}ns last=${(lastOffset*1e9).toFixed(3)}ns max=${(maxBoundaryOffset*1e9).toFixed(3)}ns`);
  console.log(`    bpm=${bpm} sr=${sr} bars=${bars}: ${loops} loops/12min, max downbeat<->boundary offset = ${(maxBoundaryOffset*1e9).toFixed(2)} ns (1 sample = ${(oneSample*1e6).toFixed(1)} us)`);
}

console.log('=== D. Stalled/late waker skips past beats (no catch-up burst) ===');
{
  const g = commitGrid(120, 48000, 1); // period 0.5
  const anchor = 10.0;
  const st = makePulse(anchor, g.beatPeriod, anchor);
  pulseTick(st, anchor);          // normal first wake -> schedules beat 0 (+ horizon)
  const nBefore = st.nextN;
  // Now the waker stalls for 2 seconds (GC / buffer-size change / ASIO<->WASAPI switch).
  const wakeNow = anchor + 2.0;   // 4 beats' worth of time elapsed unserviced
  const fired = pulseTick(st, wakeNow);
  // None of the newly fired beats may be in the past; the burst of ~4 stale beats is dropped.
  ok('D no past beat fired after stall', fired.every(f => f.time >= wakeNow));
  ok('D resumes exactly on-grid', fired.every(f => f.time === anchor + f.N * g.beatPeriod));
  ok('D did not machine-gun', fired.length <= 1 + Math.ceil(PULSE_LOOKAHEAD / g.beatPeriod),
    `fired ${fired.length}`);
  console.log(`    stalled 2s: dropped stale beats ${nBefore}..${fired[0].N - 1}, resumed at beat ${fired[0].N} on-grid`);
}

console.log('=== E. Anti-flam: suppress ONLY a true sub-0.12s coincidence ===');
{
  // E1: a normal beat stream at the fastest legit tempo (300bpm 4n = 0.05s? no -> quarter notes).
  // Quarter-note spacing at 300 bpm = 0.2s > 0.12, so NONE are suppressed.
  const g = makeClickGate();
  let suppressed = 0;
  for (let N = 0; N < 1000; N++) if (!triggerClick(g, 100 + N * 0.2)) suppressed++;
  ok('E1 no legit 300bpm beat suppressed', suppressed === 0, `suppressed=${suppressed}`);

  // E2: the commit flam — a blip the outgoing pulse (count-in / clicking free-run) already dispatched
  // into its lookahead at t=200.010, then the new master pulse's beat 0 at t=200.020 (10ms later).
  // The second must be suppressed (single strike).
  const g2 = makeClickGate();
  const a = triggerClick(g2, 200.010); // stale leftover blip fires
  const b = triggerClick(g2, 200.020); // master pulse beat0, 10ms later -> SUPPRESSED
  ok('E2 commit flam: first fires', a === true);
  ok('E2 commit flam: second (10ms) suppressed', b === false);

  // E3: but the very next real beat (>=0.12s later) fires fine.
  const c = triggerClick(g2, 200.020 + 0.5);
  ok('E3 next real beat fires after flam', c === true);

  // E4: metronome OFF for a while then ON — stale lastClickTime far in the past never wrongly
  // suppresses (abs difference is large).
  const g4 = makeClickGate();
  triggerClick(g4, 50.0);            // last = 50
  const fired = triggerClick(g4, 350.0); // 5 minutes later
  ok('E4 long-idle then click fires', fired === true);

  // E5: exact boundary — spacing exactly 0.12 is NOT < 0.12, so it fires (no off-by-one suppression).
  const g5 = makeClickGate();
  triggerClick(g5, 0);
  ok('E5 exactly 0.12 apart fires', triggerClick(g5, 0.12) === true);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

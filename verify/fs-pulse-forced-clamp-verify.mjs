// Executable verification of the count-in FORCED-BEAT clamp + the master/free-run drop (bug-hunt
// 2026-06-20, #8) — a faithful Node port of the pulseTick loop in src/audio/clock.ts. clock.ts can't
// be imported in Node (Tone/Web Audio deps), so this mirrors the exact loop body by line. Same idiom
// as fs-count-in-verify.mjs.
//
// What this PROVES (the deterministic core):
//   - HAPPY PATH (waker on time, every beat >= now): every beat fires at its TRUE ctx time t, never
//     clamped — the fix changes NOTHING on the normal path.                                    [pulseTick]
//   - STALLED waker, MASTER/free-run pulse (forcedUntilN=0): a beat now in the past is DROPPED
//     (no click, no LED) — phase stays exact, click/LED resume on the next live beat (unchanged). [pulseTick]
//   - STALLED waker, COUNT-IN (forced beats): a past forced beat is NOT dropped — it fires CLAMPED
//     to `now` (LED + click), so the "ONE-two-three-four" count stays COMPLETE.                  [pulseTick #8]
//   - The anti-flam guard collapses several same-instant catch-up clicks to ONE (no machine-gun).  [triggerClick]
//   - pulseNextN advances over every beat in BOTH old and new logic (phase index never desynced).  [pulseTick]
//   - transportActive GATE (2026-07-04): with the metronome ON but every track stopped, no beat
//     wants a click (the click is a transport mode, not a free tick); FORCED count beats bypass
//     the gate; the LED schedule is unaffected either way.                                      [pulseTick]

let fails = 0, checks = 0;
const approx = (a, b, eps = 1e-9) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

const PULSE_LOOKAHEAD = 0.1; // clock.ts
const MIN_CLICK_SPACING = 0.12; // clock.ts

// MIRRORS: src/audio/clock.ts@347-385 sha256:2eb1ec34ed38d50d  (pulseTick — forced-clamp + transportActive gate)
// ── NEW pulseTick loop body (clock.ts after #8 + the 2026-07-04 transportActive click gate). ──────
// state: { anchor, beatPeriod, nextN, forcedUntilN, metronomeOn, transportActive, transportActiveUntil }.
// An omitted transportActiveUntil is Infinity, matching the clock setter's default. Mutates state.nextN
// like the source. transportActive mirrors clock.ts's looper-pushed activity flag: past the count, a
// click needs metronomeOn AND a live transport (any track recording/playing) — the click is a transport
// mode, not a free tick. FORCED count beats bypass the gate (the count must always be heard).
function pulseTickNew(state, now) {
  const out = [];
  if (state.beatPeriod <= 0) return out;
  const horizon = now + PULSE_LOOKAHEAD;
  let t = state.anchor + state.nextN * state.beatPeriod;
  while (t < horizon) {
    const forced = state.nextN < state.forcedUntilN;
    const fireAt = t >= now ? t : forced ? now : -1; // #8: clamp a late FORCED beat to now; else drop
    const beatInBar = state.nextN % 4;
    if (fireAt >= 0) {
      out.push({ n: state.nextN, beatInBar, fireAt, ledScheduled: true, wantsClick: forced || (state.metronomeOn && state.transportActive && fireAt < (state.transportActiveUntil ?? Infinity)), dropped: false });
    } else {
      out.push({ n: state.nextN, beatInBar, fireAt: -1, ledScheduled: false, wantsClick: false, dropped: true });
    }
    state.nextN++;
    t = state.anchor + state.nextN * state.beatPeriod;
  }
  return out;
}

// ── OLD pulseTick loop body (pre-#8: a past beat is ALWAYS dropped, forced or not). For contrast. ──
function pulseTickOld(state, now) {
  const out = [];
  if (state.beatPeriod <= 0) return out;
  const horizon = now + PULSE_LOOKAHEAD;
  let t = state.anchor + state.nextN * state.beatPeriod;
  while (t < horizon) {
    if (t >= now) {
      const forced = state.nextN < state.forcedUntilN;
      out.push({ n: state.nextN, beatInBar: state.nextN % 4, fireAt: t, ledScheduled: true, wantsClick: forced || state.metronomeOn, dropped: false });
    } else {
      out.push({ n: state.nextN, beatInBar: state.nextN % 4, fireAt: -1, ledScheduled: false, wantsClick: false, dropped: true });
    }
    state.nextN++;
    t = state.anchor + state.nextN * state.beatPeriod;
  }
  return out;
}

// MIRRORS: src/audio/clock.ts@255-259 sha256:6335225ab6e43e70  (triggerClick — anti-flam / lastClickWasClamped)
// anti-flam gate (clock.ts triggerClick, after R1 bug-hunt 2026-06-23): a blip within MIN_CLICK_SPACING
// of the last is suppressed — EXCEPT a genuine on-time beat (clamped=false) is never eaten by a CLAMPED
// catch-up (lastWasClamped=true): the clamp is an artificial `now` instant, not a real sounding beat, so
// it must not swallow the real come-in/master downbeat. Mirrors the source's lastClickWasClamped flag.
function makeGate(last = -1, lastWasClamped = false) { return { last, lastWasClamped }; }
function triggerClick(gate, time, clamped = false) {
  if (gate.last >= 0 && Math.abs(time - gate.last) < MIN_CLICK_SPACING) {
    if (!(gate.lastWasClamped && !clamped)) return false; // suppress, unless a true beat after a clamp
  }
  gate.last = time;
  gate.lastWasClamped = clamped;
  return true;
}
// OLD anti-flam (pre-R1): a single lastClickTime, no clamped distinction. For contrast in section F.
function triggerClickOld(gate, time) {
  if (gate.last >= 0 && Math.abs(time - gate.last) < MIN_CLICK_SPACING) return false;
  gate.last = time;
  return true;
}

// ════════════════════════════════════════════════════════════════════════════════════════════
console.log('=== A. HAPPY PATH: waker on time → every beat fires at its TRUE time, never clamped ===');
{
  // Count-in at 120bpm: anchor=now+HBL, period 0.5. Drive the waker every 25ms with NO stall; collect
  // every beat across the whole count + a couple beats past it. Every fire must equal the exact t.
  const period = 0.5, anchor = 100.02, forcedUntilN = 4;
  const state = { anchor, beatPeriod: period, nextN: 0, forcedUntilN, metronomeOn: false, transportActive: true };
  const allFires = [];
  let now = 100.0;
  for (let step = 0; step < 400 && state.nextN <= 8; step++) { // 400*25ms = 10s, covers 8 beats
    for (const f of pulseTickNew(state, now)) if (!f.dropped) allFires.push(f);
    now += 0.025;
  }
  // Every scheduled beat fired exactly at anchor + n*period (no clamp ever, since the waker never stalled).
  let allExact = true;
  for (const f of allFires) if (!approx(f.fireAt, anchor + f.n * period)) allExact = false;
  ok('A every beat fires at its exact ctx time (no clamp on the happy path)', allExact);
  // Beats 0..3 are forced (click even metronome-off); beats >=4 are not (metronome off → silent).
  const b = (n) => allFires.find((f) => f.n === n);
  ok('A count beats 0..3 all scheduled + want click', [0, 1, 2, 3].every((n) => b(n) && b(n).wantsClick));
  ok('A come-in beat 4 scheduled but silent (metronome off, not forced)', b(4) && !b(4).wantsClick);
  ok('A accents on bar downbeats (n%4==0)', allFires.every((f) => (f.beatInBar === 0) === (f.n % 4 === 0)));
  ok('A nextN advanced past every beat (no skip)', state.nextN >= 5);
}

console.log('=== B. STALLED waker + MASTER pulse: past beats are DROPPED (unchanged behaviour) ===');
{
  // Master pulse (forcedUntilN=0), metronome ON. First tick at now=100.0 fires beat 0. Then the waker
  // STALLS to now=101.6 (horizon 101.7) — beats 1 (@100.5), 2 (@101.0), 3 (@101.5) are all in the past.
  const period = 0.5, anchor = 100.0;
  const state = { anchor, beatPeriod: period, nextN: 0, forcedUntilN: 0, metronomeOn: true, transportActive: true };
  const first = pulseTickNew(state, 100.0);
  ok('B first tick fires beat 0', first.length === 1 && first[0].n === 0 && !first[0].dropped);
  const stalled = pulseTickNew(state, 101.6);
  const byN = Object.fromEntries(stalled.map((f) => [f.n, f]));
  ok('B beats 1,2,3 (all past) DROPPED', [1, 2, 3].every((n) => byN[n] && byN[n].dropped === true));
  ok('B no master beat is clamped to now', stalled.every((f) => f.dropped || approx(f.fireAt, anchor + f.n * period)));
  ok('B nextN still advanced over the dropped beats (phase index intact)', state.nextN === 4);
}

console.log('=== C. STALLED waker + COUNT-IN: past FORCED beats fire CLAMPED (count stays complete) ===');
{
  // Same stall (now=101.6), but a count-in (forcedUntilN=4), metronome OFF. The OLD logic DROPS count
  // beats 1,2,3 (silent "EN .. .. .."); the NEW logic clamps them to now so the count is complete.
  const period = 0.5, anchor = 100.0;
  const mk = () => ({ anchor, beatPeriod: period, nextN: 0, forcedUntilN: 4, metronomeOn: false, transportActive: true });

  const sNew = mk();
  pulseTickNew(sNew, 100.0); // beat 0 on time
  const stalledNew = pulseTickNew(sNew, 101.6);
  const newByN = Object.fromEntries(stalledNew.map((f) => [f.n, f]));
  ok('C NEW count beats 1,2,3 (past, forced) NOT dropped — fired clamped to now',
     [1, 2, 3].every((n) => newByN[n] && !newByN[n].dropped && approx(newByN[n].fireAt, 101.6)));
  ok('C NEW every count LED 0..3 was scheduled (count visually complete)',
     [1, 2, 3].every((n) => newByN[n] && newByN[n].ledScheduled));
  ok('C NEW all count beats want a click (forced audible)', [1, 2, 3].every((n) => newByN[n].wantsClick));

  // OLD (contrast): the same stall drops the past count beats — proving the bug the fix removes.
  const sOld = mk();
  pulseTickOld(sOld, 100.0);
  const stalledOld = pulseTickOld(sOld, 101.6);
  const oldByN = Object.fromEntries(stalledOld.map((f) => [f.n, f]));
  ok('C OLD count beats 1,2,3 were DROPPED (the bug: silent/broken count)',
     [1, 2, 3].every((n) => oldByN[n] && oldByN[n].dropped === true));
  ok('C fix is load-bearing: OLD dropped what NEW now fires', oldByN[1].dropped && !newByN[1].dropped);
}

console.log('=== D. Anti-flam collapses same-instant catch-up clicks to ONE ===');
{
  // Two forced beats both clamped to now=101.3 → the gate fires the first, suppresses the second
  // (within 0.12s), so the catch-up is a single click, not a machine-gun burst. The next live beat at
  // 101.5 (0.2s later) fires normally.
  const gate = makeGate(100.0); // beat 0 sounded at 100.0 (a real beat)
  ok('D first clamped catch-up click fires', triggerClick(gate, 101.3, true) === true);
  ok('D second clamped catch-up at same instant suppressed', triggerClick(gate, 101.3, true) === false);
  ok('D the following live beat (0.2s later) fires', triggerClick(gate, 101.5, false) === true);
}

console.log('=== E. No stall, count-in incremental: beats fire one-per-wake at exact times ===');
{
  // Sanity that under the normal 25ms waker the count beats come out at their true grid times in order.
  const period = 0.5, anchor = 50.02;
  const state = { anchor, beatPeriod: period, nextN: 0, forcedUntilN: 4, metronomeOn: false, transportActive: true };
  const order = [];
  let now = 50.0;
  for (let step = 0; step < 200 && state.nextN < 4; step++) {
    for (const f of pulseTickNew(state, now)) if (!f.dropped) order.push(f.fireAt);
    now += 0.025;
  }
  let monotonic = true;
  for (let i = 1; i < order.length; i++) if (order[i] < order[i - 1]) monotonic = false;
  ok('E count beats fire in time order', monotonic && order.length === 4, `order=${order.map((x) => x.toFixed(3))}`);
  ok('E each at its exact grid time', order.every((x, i) => approx(x, anchor + i * period)));
}

console.log('=== F. R1: a clamped catch-up must NOT suppress the true-time come-in/master downbeat ===');
{
  // Faithful INTEGRATED pulseTick + triggerClick (the source calls triggerClick inside the loop). A
  // count-in, metronome ON. anchor + n*period are the beats; forcedUntilN = countBeats; beat `countBeats`
  // is the come-in downbeat the take begins on (clicked because metronome ON). clamped = (t < now).
  function runWake(period, forcedUntilN, state, gate, now, triggerFn) {
    const fires = [];
    const horizon = now + PULSE_LOOKAHEAD;
    let t = state.anchor + state.nextN * period;
    while (t < horizon) {
      const forced = state.nextN < forcedUntilN;
      const fireAt = t >= now ? t : forced ? now : -1;
      if (fireAt >= 0) {
        const wantsClick = forced || (state.metronomeOn && state.transportActive && fireAt < (state.transportActiveUntil ?? Infinity));
        const clamped = t < now;
        const clicked = wantsClick ? triggerFn(gate, fireAt, clamped) : false;
        fires.push({ n: state.nextN, fireAt, clamped, clicked });
      }
      state.nextN++;
      t = state.anchor + state.nextN * period;
    }
    return fires;
  }

  // Main scenario (120bpm, period 0.5, anchor 100.0): beat 0 @100.0; a stall to now=101.92 clamps beats
  // 1,2,3 to 101.92; the come-in downbeat (beat 4) is at its TRUE time 102.0 — only 0.08s after the clamp,
  // INSIDE MIN_CLICK_SPACING (0.12). NEW logic: the downbeat fires (clamp can't eat a true beat). OLD: it
  // was suppressed (the R1 bug — the loop's "1" went silent).
  {
    const period = 0.5, anchor = 100.0, forcedUntilN = 4;
    const sNew = { anchor, nextN: 0, metronomeOn: true, transportActive: true };
    const gNew = makeGate(-1);
    runWake(period, forcedUntilN, sNew, gNew, 100.0, triggerClick);
    const fNew = runWake(period, forcedUntilN, sNew, gNew, 101.92, triggerClick);
    const nNew = Object.fromEntries(fNew.map((x) => [x.n, x]));
    ok('F NEW beat 1 clamp-fires (one catch-up click)', nNew[1] && nNew[1].clamped && nNew[1].clicked);
    ok('F NEW beats 2,3 same-instant clamps suppressed', nNew[2] && !nNew[2].clicked && nNew[3] && !nNew[3].clicked);
    ok('F NEW come-in downbeat 4 (102.0) CLICKS — not eaten by the clamp', nNew[4] && !nNew[4].clamped && nNew[4].clicked,
       `gap=${(102.0 - 101.92).toFixed(3)}s < ${MIN_CLICK_SPACING}`);

    const sOld = { anchor, nextN: 0, metronomeOn: true, transportActive: true };
    const gOld = makeGate(-1);
    runWake(period, forcedUntilN, sOld, gOld, 100.0, triggerClickOld);
    const fOld = runWake(period, forcedUntilN, sOld, gOld, 101.92, triggerClickOld);
    const nOld = Object.fromEntries(fOld.map((x) => [x.n, x]));
    ok('F OLD come-in downbeat 4 SUPPRESSED (the R1 bug: loop "1" silent)', nOld[4] && !nOld[4].clicked);
    ok('F fix is load-bearing: OLD ate the downbeat, NEW fires it', !nOld[4].clicked && nNew[4].clicked);
  }

  // Generalize: tempo-independent. now = comeIn - 0.08 (inside the window, past beat 3) at each bpm.
  let allFireNew = true;
  for (const bpm of [40, 100, 120, 200]) {
    const period = 60 / bpm, anchor = 100.0, forcedUntilN = 4;
    const comeIn = anchor + 4 * period;
    const s = { anchor, nextN: 0, metronomeOn: true, transportActive: true };
    const g = makeGate(-1);
    runWake(period, forcedUntilN, s, g, 100.0, triggerClick);
    const f = runWake(period, forcedUntilN, s, g, comeIn - 0.08, triggerClick);
    const byN = Object.fromEntries(f.map((x) => [x.n, x]));
    if (!(byN[4] && byN[4].clicked)) allFireNew = false;
  }
  ok('F NEW come-in downbeat fires at every bpm (40/100/120/200) — fix is tempo-independent', allFireNew);
}

console.log('=== G. R1 must PRESERVE the commit-flam + count-doubling guards (real leftover blip) ===');
{
  // The commit-flam / count-doubling guards rely on a REAL leftover count/free-run blip (clamped=false)
  // sitting in lastClickTime, so the colliding new pulse beat 0 at ~the same time is dropped. R1 must NOT
  // relax that — only the CLAMP case is exempt. Here the previous click is a real beat (clamped=false).
  const gate = makeGate(50.0, false); // a real leftover blip from the outgoing pulse sounded at 50.0
  ok('G master/count beat 0 at ~same time (50.02) is STILL suppressed (commit-flam guard intact)',
     triggerClick(gate, 50.02, false) === false);
  // And a clamp DOES still get exempted (paired sanity with F): real blip @60.0, then a clamp @60.05.
  const g2 = makeGate(60.0, false);
  // a clamped catch-up landing near a real blip: the clamp is suppressed by the real blip (clamped doesn't
  // get special treatment as the INCOMING blip — only a true beat after a CLAMP does), so no machine-gun.
  ok('G a clamp near a real blip is still suppressed (clamp is not privileged as incoming)',
     triggerClick(g2, 60.05, true) === false);
}

console.log('=== H. transportActive gate: metronome ON + all tracks stopped → silent; forced beats bypass ===');
{
  const period = 0.5, anchor = 100.0;
  // (1) Master pulse, metronome ON, transport INACTIVE (all tracks stopped): every beat schedules its
  // LED but none wants a click — the click is a transport mode, not a free tick.
  const sIdle = { anchor, beatPeriod: period, nextN: 0, forcedUntilN: 0, metronomeOn: true, transportActive: false };
  const idleFires = [];
  let now = 100.0;
  for (let step = 0; step < 200 && sIdle.nextN <= 6; step++) {
    for (const f of pulseTickNew(sIdle, now)) if (!f.dropped) idleFires.push(f);
    now += 0.025;
  }
  ok('H stopped transport: beats still scheduled (LED alive)', idleFires.length >= 6 && idleFires.every((f) => f.ledScheduled));
  ok('H stopped transport: NO beat wants a click', idleFires.every((f) => !f.wantsClick));
  // (2) Same pulse with the transport ACTIVE: every beat clicks (metronome ON honored again).
  const sLive = { anchor, beatPeriod: period, nextN: 0, forcedUntilN: 0, metronomeOn: true, transportActive: true };
  const liveFires = [];
  now = 100.0;
  for (let step = 0; step < 200 && sLive.nextN <= 6; step++) {
    for (const f of pulseTickNew(sLive, now)) if (!f.dropped) liveFires.push(f);
    now += 0.025;
  }
  ok('H live transport: every beat clicks', liveFires.length >= 6 && liveFires.every((f) => f.wantsClick));
  // (3) FORCED count beats bypass the gate (defensive: a count-in implies a RECORDING track, but the
  // count must be heard even if the activity flag were momentarily stale).
  const sCount = { anchor, beatPeriod: period, nextN: 0, forcedUntilN: 4, metronomeOn: false, transportActive: false };
  const countFires = [];
  now = 100.0;
  for (let step = 0; step < 200 && sCount.nextN <= 4; step++) {
    for (const f of pulseTickNew(sCount, now)) if (!f.dropped) countFires.push(f);
    now += 0.025;
  }
  const byN = Object.fromEntries(countFires.map((f) => [f.n, f]));
  ok('H forced count beats 0..3 click despite inactive transport + metronome off',
     [0, 1, 2, 3].every((n) => byN[n] && byN[n].wantsClick));
  ok('H post-count beat 4 is silent (not forced, gate applies)', byN[4] && !byN[4].wantsClick);
}

console.log('=== I. Pending stop cuts click audibility at the edge, while grid LEDs keep running ===');
{
  const state = { anchor: 100, beatPeriod: 0.5, nextN: 0, forcedUntilN: 0,
    metronomeOn: true, transportActive: true, transportActiveUntil: 100.5 };
  const before = pulseTickNew(state, 99.95);
  const edge = pulseTickNew(state, 100.45);
  ok('I beat before stop clicks', before[0]?.wantsClick === true);
  ok('I beat at stop stays visually scheduled but silent', edge[0]?.ledScheduled === true && edge[0]?.wantsClick === false);
  const forced = { ...state, nextN: 1, forcedUntilN: 2 };
  ok('I forced count beats still bypass pending stop cutoff', pulseTickNew(forced, 100.45)[0]?.wantsClick === true);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

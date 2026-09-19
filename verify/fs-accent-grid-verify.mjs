// Executable verification of the FREE-RUN beat pulse (the beat pulse is ONE
// ctx-time lookahead mechanism — startFreeRunPulse / startCountIn / startMasterPulse are three anchors
// of the SAME pulseTick scheduler). Node port of startFreeRunPulse + the pulseTick loop from
// src/audio/clock.ts (Tone/Web Audio deps — can't import; mirrored by line, same idiom as the other
// fs-*-verify.mjs).
//
// HISTORY: this file used to guard the OLD free-run mechanism — a Tone scheduleRepeat('4n') grid whose
// accent label was derived from getTicksAtTime(time)/PPQ (bug-hunt 2026-06-20: a JS beat counter reset
// on every (re)schedule mislabelled the accent after a count abort / master reset). That mechanism was
// DELETED 2026-07-07 (§3.1) and the bug class died with it: the unified pulse labels every beat
// beatInBar = N % 4 with N derived from the anchor (pulseNextN), so a counter/grid phase mismatch is
// structurally impossible. The old sections proving the tick-derivation are gone WITH the code they
// verified; what replaces them is the new mechanism's own contract:
//
// What this PROVES:
//   - fresh start: anchor = now, beat 0 fires immediately, cadence = 60/bpm exact, accent iff N%4==0 [startFreeRunPulse]
//   - a bpm change mid-free-run re-anchors PHASE-CARRYING: the next beat keeps the OUTGOING grid's
//     time AND bar index; subsequent beats space at the NEW period; no beat fires twice, none in the
//     past — so the LED cadence bends with no hop and no double-fire                        [startFreeRunPulse]
//   - master reset / count abort falls back to free-run carrying the outgoing grid's phase (the LED
//     keeps beating; the gap at the seam is <= one period; a count's forcedUntilN never leaks)
//   - setBpm re-anchors ONLY a live free-run pulse — a count-in / master pulse keeps its frozen
//     period (the committed grid never re-derives from the bpm signal)                      [setBpm gate]
//   - the anchor arithmetic round-trips: at a large beat index (hours-long session) the carried
//     next-beat time is preserved to sub-nanosecond error

let fails = 0, checks = 0;
const approx = (a, b, eps = 1e-9) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

const PULSE_LOOKAHEAD = 0.1; // clock.ts
const PULSE_INTERVAL = 0.025; // clock.ts PULSE_INTERVAL_MS

// MIRRORS: src/audio/clock.ts@386-398 sha256:beba6e9edef75c27  (startFreeRunPulse — phase-carrying re-anchor)
// state: { anchor, beatPeriod, nextN, forcedUntilN, freeRun, timerLive }. Mutates like the source.
// The source reads bpm() (already clamped+rounded) and engine.ctx.currentTime; passed in here.
function startFreeRunPulse(st, bpmNow, now) {
  const period = 60 / bpmNow;
  const nextBeatTime = st.beatPeriod > 0 ? st.anchor + st.nextN * st.beatPeriod : now;
  st.timerLive = true; // teardownBeatPulse(false): waker restarts, queued LED writes stay valid (phase-carrying)
  st.reanchors = (st.reanchors ?? 0) + 1; // model-only invocation counter (for the E no-op check)
  st.freeRun = true;
  st.forcedUntilN = 0;
  st.beatPeriod = period;
  st.anchor = nextBeatTime - st.nextN * period; // beat nextN fires where the old grid had it
}

// MIRRORS: src/audio/clock.ts@335-373 sha256:2eb1ec34ed38d50d  (pulseTick — the one lookahead scheduler)
// Same port as fs-grid-verify.mjs / fs-pulse-forced-clamp-verify.mjs: schedules every beat inside the
// horizon at its exact ctx time; past beats DROP unless forced (forced clamp to now, #8).
// Returns grid/LED beats regardless of the click's transport cutoff. Audio gating is covered by
// fs-pulse-forced-clamp-verify and the real-browser loop-end-stop probe.
function pulseTick(st, now) {
  const out = [];
  if (st.beatPeriod <= 0) return out;
  const horizon = now + PULSE_LOOKAHEAD;
  let t = st.anchor + st.nextN * st.beatPeriod;
  while (t < horizon) {
    const forced = st.nextN < st.forcedUntilN;
    const fireAt = t >= now ? t : forced ? now : -1;
    if (fireAt >= 0) out.push({ n: st.nextN, beatInBar: st.nextN % 4, fireAt, forced });
    st.nextN++;
    t = st.anchor + st.nextN * st.beatPeriod;
  }
  return out;
}

// MIRRORS: src/audio/clock.ts@41-53 sha256:8a06763d68be9472  (setBpm — lock guard + free-run-only re-anchor)
function setBpm(clockState, st, n, now) {
  if (clockState.locked) return;
  const clamped = Math.max(40, Math.min(300, Math.round(n)));
  if (clamped === clockState.bpm) return; // value-identical set = full no-op (no re-anchor churn)
  clockState.bpm = clamped;
  if (st.freeRun && st.timerLive) startFreeRunPulse(st, clockState.bpm, now);
}

const freshState = () => ({ anchor: 0, beatPeriod: 0, nextN: 0, forcedUntilN: 0, freeRun: false, timerLive: false });
// Drive the 25ms waker from `now` until `endNow`, collecting fired beats.
function runWaker(st, now, endNow) {
  const fired = [];
  while (now < endNow) {
    fired.push(...pulseTick(st, now));
    now += PULSE_INTERVAL;
  }
  return { fired, now };
}

// ════════════════════════════════════════════════════════════════════════════════════════════
console.log('=== A. Fresh start: anchor = now, beat 0 immediate, exact cadence, accent iff N%4==0 ===');
for (const bpm of [40, 90, 120, 137, 200, 300]) {
  const st = freshState();
  const now0 = 100.0;
  startFreeRunPulse(st, bpm, now0);
  const period = 60 / bpm;
  ok(`A anchor==now bpm=${bpm}`, st.anchor === now0, `anchor=${st.anchor}`);
  ok(`A period==60/bpm bpm=${bpm}`, st.beatPeriod === period);
  const { fired } = runWaker(st, now0, now0 + 8 * period + 0.05);
  ok(`A beat 0 fires at now bpm=${bpm}`, fired.length > 0 && fired[0].n === 0 && fired[0].fireAt === now0);
  ok(`A every beat at anchor+n*period bpm=${bpm}`, fired.every((f) => approx(f.fireAt, now0 + f.n * period, 0)));
  ok(`A accent iff n%4==0 bpm=${bpm}`, fired.every((f) => (f.beatInBar === 0) === (f.n % 4 === 0)));
  ok(`A no beat scheduled twice bpm=${bpm}`, fired.every((f, i) => i === 0 || f.n === fired[i - 1].n + 1));
}

console.log('=== B. bpm change mid-free-run: phase-carrying re-anchor (no hop, no double-fire) ===');
for (const [bpmA, bpmB] of [[120, 90], [90, 200], [200, 40], [120, 121]]) {
  const st = freshState();
  const now0 = 50.0;
  startFreeRunPulse(st, bpmA, now0);
  const pA = 60 / bpmA, pB = 60 / bpmB;
  // run a while on grid A, then change tempo at an arbitrary wake moment
  const { fired: before, now: nowChange } = runWaker(st, now0, now0 + 5 * pA + 0.013);
  const expectedNextT = st.anchor + st.nextN * st.beatPeriod; // where grid A would put the next beat
  const nAtChange = st.nextN;
  setBpm({ bpm: bpmA, locked: false }, st, bpmB, nowChange);
  ok(`B period switched ${bpmA}->${bpmB}`, st.beatPeriod === pB);
  const { fired: after } = runWaker(st, nowChange, nowChange + 6 * pB + 0.05);
  ok(`B next beat keeps the OUTGOING grid's time ${bpmA}->${bpmB}`,
     after.length > 0 && after[0].n === nAtChange && approx(after[0].fireAt, expectedNextT),
     `got ${after[0]?.fireAt} want ${expectedNextT}`);
  ok(`B bar index continuous across the re-anchor ${bpmA}->${bpmB}`,
     after[0].beatInBar === nAtChange % 4 && before.every((f) => f.beatInBar === f.n % 4));
  // subsequent spacing = the NEW period exactly
  let spacingOk = true;
  for (let i = 1; i < after.length; i++) if (!approx(after[i].fireAt - after[i - 1].fireAt, pB)) spacingOk = false;
  ok(`B post-change spacing == new period ${bpmA}->${bpmB}`, spacingOk);
  // no double-fire: indices strictly increasing across the whole run, none fired twice
  const all = [...before, ...after];
  ok(`B no beat index fired twice ${bpmA}->${bpmB}`, all.every((f, i) => i === 0 || f.n > all[i - 1].n));
  ok(`B no beat fired in the past ${bpmA}->${bpmB}`, after.every((f) => f.fireAt >= nowChange));
}

console.log('=== C. Master reset falls back to free-run CARRYING the master grid phase ===');
{
  // A committed master grid: exact integer-frame period (slightly off the nominal 60/bpm), long-lived.
  const sr = 48000, bpm = 137, bars = 2;
  const fpb = Math.round((sr * 60 / bpm) * 4);
  const masterPeriod = (bars * fpb) / sr / (4 * bars); // the exact committed beat period
  const st = freshState();
  st.anchor = 10.0; st.beatPeriod = masterPeriod; st.freeRun = false; st.timerLive = true;
  // run the master pulse ~20 minutes in (large nextN), then reset at an arbitrary moment
  const { fired: masterFired, now: nowReset } = runWaker(st, 10.0, 10.0 + 20 * 60);
  const lastMaster = masterFired[masterFired.length - 1];
  const expectedNextT = st.anchor + st.nextN * st.beatPeriod;
  const nAtReset = st.nextN;
  startFreeRunPulse(st, bpm, nowReset); // stopMasterPulse() delegates to exactly this
  ok('C free-run flag set on fallback', st.freeRun === true && st.forcedUntilN === 0);
  const { fired: after } = runWaker(st, nowReset, nowReset + 3);
  ok('C first free-run beat lands where the master grid had it (sub-ns)',
     after.length > 0 && after[0].n === nAtReset && approx(after[0].fireAt, expectedNextT, 1e-9),
     `got ${after[0]?.fireAt} want ${expectedNextT} (err ${Math.abs(after[0]?.fireAt - expectedNextT)})`);
  ok('C seam gap <= one period (no LED hop at reset)',
     after[0].fireAt - lastMaster.fireAt <= Math.max(masterPeriod, 60 / bpm) + 1e-9,
     `gap=${after[0].fireAt - lastMaster.fireAt}`);
  ok('C bar index continuous across the reset', after[0].beatInBar === nAtReset % 4);
}

console.log('=== D. Count abort falls back clean: forcedUntilN cleared, no forced click leaks ===');
{
  const st = freshState();
  // a count-in pulse: anchored, 4 forced beats, aborted after 2 of them fired
  st.anchor = 100.02; st.beatPeriod = 0.5; st.forcedUntilN = 4; st.freeRun = false; st.timerLive = true;
  runWaker(st, 100.0, 101.1); // beats 0,1,2 scheduled (some forced)
  startFreeRunPulse(st, 120, 101.1); // stopCountIn -> stopMasterPulse -> free-run
  ok('D forcedUntilN cleared on fallback', st.forcedUntilN === 0);
  const { fired } = runWaker(st, 101.1, 103.0);
  ok('D no post-abort beat is forced', fired.every((f) => !f.forced));
}

console.log('=== E. setBpm re-anchors ONLY a live free-run pulse (count/master periods stay frozen) ===');
{
  // count-in live (freeRun=false): setBpm must not touch the pulse (the count grid is frozen at press;
  // see fs-bpm-lock-verify.mjs for the R4 lock that usually prevents even the bpm write).
  const st = freshState();
  st.anchor = 100.02; st.beatPeriod = 0.5; st.forcedUntilN = 4; st.freeRun = false; st.timerLive = true;
  const c = { bpm: 120, locked: false };
  setBpm(c, st, 90, 100.3);
  ok('E count pulse NOT re-anchored by setBpm', st.beatPeriod === 0.5 && st.anchor === 100.02 && st.forcedUntilN === 4);
  ok('E bpm value itself did change (unlocked)', c.bpm === 90);
  // master pulse live (freeRun=false, locked=true): neither bpm nor the pulse moves.
  const st2 = freshState();
  st2.anchor = 5.0; st2.beatPeriod = 0.4999; st2.freeRun = false; st2.timerLive = true;
  const c2 = { bpm: 120, locked: true };
  setBpm(c2, st2, 90, 40.0);
  ok('E master pulse untouched + bpm frozen while locked', st2.beatPeriod === 0.4999 && c2.bpm === 120);
  // free-run live: setBpm DOES re-anchor.
  const st3 = freshState();
  startFreeRunPulse(st3, 120, 10.0);
  setBpm({ bpm: 120, locked: false }, st3, 90, 10.3);
  ok('E free-run pulse re-anchored to the new period', st3.beatPeriod === 60 / 90);
  // pulse not started yet (timerLive=false): setBpm must not fabricate a grid.
  const st4 = freshState();
  setBpm({ bpm: 120, locked: false }, st4, 90, 0.0);
  ok('E no pulse started when none was live', st4.timerLive === false && st4.beatPeriod === 0);
  // value-identical setBpm is a FULL no-op: no re-anchor (streaming callers — MIDI clock ~24 ticks/beat,
  // tap tempo — would otherwise churn teardown/setInterval and re-anchor for nothing; a re-anchor also
  // tears down + restarts the waker, which the source pays as clearInterval/setInterval churn).
  const st5 = freshState();
  startFreeRunPulse(st5, 120, 10.0);
  runWaker(st5, 10.0, 10.4);
  const reanchorsBefore = st5.reanchors;
  setBpm({ bpm: 120, locked: false }, st5, 120, 10.4); // same value (after clamp/round) — must not re-anchor
  setBpm({ bpm: 120, locked: false }, st5, 120.3, 10.42); // rounds to 120 — still identical, still a no-op
  ok('E value-identical setBpm does not re-anchor', st5.reanchors === reanchorsBefore,
     `reanchors ${reanchorsBefore} -> ${st5.reanchors}`);
  setBpm({ bpm: 120, locked: false }, st5, 121, 10.44); // a REAL change still re-anchors
  ok('E changed setBpm still re-anchors', st5.reanchors === reanchorsBefore + 1 && st5.beatPeriod === 60 / 121);
}

console.log('=== F. Anchor arithmetic round-trips at a large beat index (hours-long session) ===');
{
  // anchor = nextBeatTime - N*period, then pulseTick recomputes anchor + N*period: the float error of
  // that round-trip must stay sub-nanosecond even at N ~ 40000 (a ~5.5h free-run at 120bpm).
  for (const N of [1, 999, 40000, 123457]) {
    const st = freshState();
    st.anchor = 3.0; st.beatPeriod = 0.5; st.nextN = N; st.freeRun = true; st.timerLive = true;
    const want = st.anchor + N * st.beatPeriod;
    startFreeRunPulse(st, 137, want - 0.05); // re-anchor just before the carried beat
    const got = st.anchor + st.nextN * st.beatPeriod;
    ok(`F round-trip error < 1ns at N=${N}`, approx(got, want, 1e-9), `err=${Math.abs(got - want)}`);
  }
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);

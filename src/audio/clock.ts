import { createSignal } from 'solid-js';
import { getDraw, getTransport } from 'tone';
import { engine } from './engine';
import { averageInterval } from './quantize';
import { readStoredNumber, writeStoredNumber } from './persist';

/**
 * OWNS: tempo + the beat pulse — BPM (and its LOCK: `setBpmLocked`, driven by the looper at count-in press /
 * commit / master reset; `setBpm` is a no-op while locked), tap tempo, the ONE ctx-time lookahead pulse
 * that serves free-run / count-in / master anchors (click + LED), the metronome click and its anti-flam
 * guard. The single transport / tempo authority: all synths, the looper and tempo-synced FX read from here.
 *
 * Tone.getTransport() holds the BPM (tempo-synced Tone FX resolve their time values against it);
 * the beat pulse itself is a ctx-time lookahead scheduler (pulseTick) on the shared AudioContext
 * established by engine.ts — one clock.
 */

/**
 * Touch engine.ctx first so Tone.setContext(sharedCtx) runs BEFORE getTransport() is ever
 * called. Otherwise the transport binds to Tone's default context (created lazily by the
 * first getTransport() call) and never advances when we start the shared context.
 */
function tp() {
  void engine.ctx;
  return getTransport();
}

// ---------------------------------------------------------------------------
// BPM signal
// ---------------------------------------------------------------------------
const [bpm, setBpmSignal] = createSignal(120);

const MIN_BPM = 40;
const MAX_BPM = 300;

function clampBpm(n: number): number {
  return Math.max(MIN_BPM, Math.min(MAX_BPM, Math.round(n)));
}

/** Clamp to [40, 300], write to transport, and update the reactive signal. No-op while locked. */
function setBpm(n: number): void {
  if (bpmLocked()) return; // tempo is frozen once a loop defines the master length
  const clamped = clampBpm(n);
  // Value-identical set = full no-op. Streaming callers hit this constantly (MIDI clock ingests
  // ~24 ticks/beat, tap tempo re-averages per tap) — without the early return each one re-anchored
  // the free-run pulse below, churning teardown/setInterval dozens of times a second for nothing.
  if (clamped === bpm()) return;
  tp().bpm.value = clamped;
  setBpmSignal(clamped);
  // A live FREE-RUN pulse follows the tempo: re-anchor it to the new period. Count-in / master
  // pulses keep their frozen period by design — the committed grid never re-derives from this signal.
  if (pulseFreeRun && pulseTimer !== null) startFreeRunPulse();
}

// ---------------------------------------------------------------------------
// Tap tempo
// ---------------------------------------------------------------------------
const TAP_RESET_MS = 2000; // gap > this = start a new window
const TAP_MAX_HISTORY = 8;

let tapTimes: number[] = [];

/**
 * Record a tap. Pass an explicit `now` (ms) for deterministic testing;
 * defaults to `performance.now()`.
 *
 * With >= 2 taps in the current window, computes BPM from the average
 * inter-tap interval and calls setBpm. Returns the current bpm.
 */
function tap(now?: number): number {
  ensureRunning();
  const t = now ?? performance.now();
  const last = tapTimes[tapTimes.length - 1];

  if (last !== undefined && t - last > TAP_RESET_MS) {
    // Gap too large — start fresh window from this tap.
    tapTimes = [t];
    return bpm();
  }

  tapTimes.push(t);
  if (tapTimes.length > TAP_MAX_HISTORY) {
    tapTimes = tapTimes.slice(-TAP_MAX_HISTORY);
  }

  if (tapTimes.length >= 2) {
    setBpm(60000 / averageInterval(tapTimes));
  }

  return bpm();
}

// ---------------------------------------------------------------------------
// Transport run state — the transport free-runs once started; there is no global play/stop.
// ---------------------------------------------------------------------------
/** True once the audio clock is live (transport free-running). Drives the always-on beat LED. */
const [running, setRunning] = createSignal(false);

/**
 * True once a master loop length exists: BPM is then derived from the integer frame count and must
 * not change (else the metronome would drift from the loops). Set by the looper at the count-in press /
 * commit, cleared at abort / master reset via setBpmLocked().
 */
const [bpmLocked, setBpmLockedSignal] = createSignal(false);

function setBpmLocked(on: boolean): void {
  setBpmLockedSignal(on);
}

/** True from a gesture's engine.start until it rejects: also the in-flight guard (no double start). */
let _running = false;
/** The free-run pulse + Tone transport are started once; a retried engine.start does not restart them. */
let transportStarted = false;

/**
 * Start the transport ONCE and leave it running, so the beat LED + metronome are live whenever the
 * audio clock is — not gated on a global play button. Idempotent; safe to call on every
 * looper/transport gesture. Must run on a user gesture (engine.start resumes the AudioContext).
 * Synchronous for callers: the pulse + transport start now (callers such as session restore re-anchor
 * it right after). A REJECTED engine.start (Tone adoption, toneStart, the master-limiter latency
 * measurement) clears the running flag, so the next gesture retries instead of keeping a dead clock.
 */
function ensureRunning(): void {
  if (_running) return;
  _running = true;
  setRunning(true);
  const retryNextGesture = (): void => { _running = false; setRunning(false); };
  engine.start().catch((err: unknown) => {
    retryNextGesture();
    console.error('[clock] audio engine failed to start; the next gesture retries', err);
  });
  if (transportStarted) return;
  try { startFreeRunPulse(); tp().start(); } // startFreeRunPulse tears down first, so a retry is safe
  catch (err) { retryNextGesture(); throw err; } // a synchronous throw must not block the next gesture
  transportStarted = true; // latched last: only once the pulse and the transport both started
}

// ---------------------------------------------------------------------------
// Beat pulse (for UI LED) — updated via Draw so it fires on the render frame
// closest to the beat, not the audio thread.
// ---------------------------------------------------------------------------
const [beat, setBeat] = createSignal(0);
/** Count-in beats still to come INCLUDING the one that just fired (4,3,2,1 during a forced count; 0 otherwise). */
const [countLeft, setCountLeft] = createSignal(0);

// ---------------------------------------------------------------------------
// Metronome — a short two-tone blip in the audible register, connected to masterGain.
// Accent beat 1 (every 4th quarter note) at a higher frequency / louder.
// Default OFF.
//
// A click reads as a "click" via 1–4 kHz transient energy, which most laptop/monitor speakers
// reproduce cleanly (a sub-bass kick-drum blip lands as a muddy thud on them). Each beat gets a fresh
// OscillatorNode + GainNode (trivial at a 2–5 Hz beat rate); the gain ramp is the load-bearing
// anti-click shaping.
// ---------------------------------------------------------------------------
const [metronomeOn, setMetronomeSignal] = createSignal(false);

/**
 * True while any looper track is RECORDING / PLAYING / OVERDUBBING — pushed by the looper on every
 * state transition (looper/state.ts publish). Gates the AUDIBLE click: the metronome is a transport
 * MODE ("click with the count-in / recording / playback"), not a free-running tick — with nothing
 * recording or playing it is silent (a product call; it also removes the audible
 * "accent on the wrong beat" of the idle free-run grid, whose bar phase is anchored to an arbitrary
 * transport-start moment). Count-in beats stay FORCED regardless. A plain var, not a signal: it is
 * read inside scheduler callbacks per beat; no reactivity needed.
 */
let transportActive = false;
let transportActiveUntil = Infinity;
const pendingClicks = new Map<OscillatorNode, { time: number; forced: boolean }>();
function setTransportActive(on: boolean, until = Infinity): void {
  const previousUntil = transportActiveUntil;
  transportActive = on;
  transportActiveUntil = on ? until : 0;
  // A stop can arrive after pulseTick has queued its next beat. Cancel that future oscillator
  // as well as gating future ticks; forced count-in clicks retain their existing behavior.
  for (const [osc, click] of pendingClicks) {
    if (click.forced || click.time < transportActiveUntil || click.time < engine.ctx.currentTime) continue;
    osc.stop(engine.ctx.currentTime);
    pendingClicks.delete(osc);
    if (lastClickTime === click.time) {
      lastClickTime = -1;
      lastClickWasClamped = false;
    }
  }
  // Starting another lane can extend activity beyond a queued stop. Its next beat may already
  // have been cancelled, or skipped by pulseTick. Revisit only the live lookahead window; the
  // anti-flam guard keeps any beat still scheduled from sounding twice. LED scheduling is unchanged.
  if (transportActiveUntil > previousUntil && metronomeOn() && pulseBeatPeriod > 0) {
    const now = engine.ctx.currentTime;
    const horizon = Math.min(now + PULSE_LOOKAHEAD, transportActiveUntil);
    const first = Math.max(0, Math.ceil((now - pulseAnchor) / pulseBeatPeriod));
    for (let n = first, time = pulseAnchor + n * pulseBeatPeriod; time < horizon; n++, time = pulseAnchor + n * pulseBeatPeriod) {
      triggerClick(time, n % 4 === 0, false, n < pulseForcedUntilN);
    }
  }
}

// Click volume (0..1) — its own level so the metronome can be balanced against your playing without
// touching the master. Persisted to localStorage (mirrors master.ts), so it survives a reload. Scales
// the per-blip peak in triggerClick; 0 ⇒ no blip is scheduled. Default 0.7: present but not overpowering.
const CLICK_STORAGE_KEY = 'lf.clickVolume';
const DEFAULT_CLICK_VOLUME = 0.7;

const [clickVolume, setClickVolumeSignal] = createSignal(
  readStoredNumber(CLICK_STORAGE_KEY, DEFAULT_CLICK_VOLUME, 0, 1),
);

/** Set the metronome click volume (0..1), clamped + persisted. Read live by triggerClick per blip. */
function setClickVolume(v: number): void {
  const clamped = Math.max(0, Math.min(1, v));
  setClickVolumeSignal(clamped);
  writeStoredNumber(CLICK_STORAGE_KEY, clamped);
}

/**
 * Minimum spacing (s) between two scheduled click blips. Legit quarter-note beats are >= 0.2 s apart
 * (<= 300 bpm), so this only ever suppresses a TRUE coincidence — chiefly the commit-beat flam: a blip
 * the outgoing pulse already dispatched into its lookahead (osc.start called, up to PULSE_LOOKAHEAD
 * ahead) survives pulse teardown, and when startMasterPulse re-anchors at commit the new
 * grid's first beat lands ~now+0.02; without this guard those two can double-strike (a flam, since
 * each blip ramps from 0). This is live whenever the click sounds across a re-anchor — e.g. a count-in
 * blip at commit, or a metronome-on free take (transportActive) whose free-run pulse clicked. Tracking
 * the last scheduled blip time and skipping any new blip within this window makes the click provably
 * flam-free.
 */
const MIN_CLICK_SPACING = 0.12;
let lastClickTime = -1;
/**
 * Whether the click that set lastClickTime was a CLAMPED catch-up — a late FORCED count beat fired at
 * `now` instead of its true grid time (pulseTick, when a stalled waker woke past it). The anti-flam guard
 * must NOT let that artificial `now` instant suppress a genuine on-time beat — chiefly the come-in / master
 * first downbeat, the loop's "1" — that happens to land within MIN_CLICK_SPACING of it. Real grid beats are
 * >= one period apart (0.2 s at <= 300 bpm), so an un-clamped beat can only collide with a clamp or a
 * leftover blip from the outgoing pulse; we still suppress the latter (commit-flam / count-doubling) but
 * never the former, so the downbeat always sounds.
 */
let lastClickWasClamped = false;

/**
 * Fire one metronome click at absolute AudioContext time `time`. `isAccent` is beat 1 of every 4.
 * Connects to masterGain — downstream of the looper's record tap, so the click is never captured into
 * a recorded loop. The accent/beat peaks are scaled by the user's clickVolume; a zero volume (or a
 * blip within MIN_CLICK_SPACING of the last one) schedules nothing. `clamped` marks a late forced count
 * beat collapsed to `now` (pulseTick catch-up): such blips collapse among themselves but never suppress a
 * later true-time beat.
 */
function triggerClick(time: number, isAccent: boolean, clamped = false, forced = false): void {
  const vol = clickVolume();
  if (vol <= 0) return; // click muted — nothing to schedule (also avoids an exp-ramp from zero)
  // Anti-flam: drop a blip that would double-strike against the previous one (see MIN_CLICK_SPACING) —
  // EXCEPT a genuine on-time beat is never eaten by a CLAMPED catch-up (suppressing the real downbeat
  // against an artificial `now` clamp would drop the loop's "1"). The commit-flam / count-doubling guards
  // are preserved: there the previous click is a real leftover count/free-run blip (lastClickWasClamped ===
  // false), so a colliding true-time beat 0 is still dropped exactly as before.
  if (lastClickTime >= 0 && Math.abs(time - lastClickTime) < MIN_CLICK_SPACING) {
    if (!(lastClickWasClamped && !clamped)) return; // suppress, unless this is a true beat after a clamp
  }
  lastClickTime = time;
  lastClickWasClamped = clamped;
  const ctx = engine.ctx;
  const osc = ctx.createOscillator();
  const gain = ctx.createGain();
  osc.type = 'triangle'; // clean, no aliasing
  osc.frequency.value = isAccent ? 1500 : 1000;
  const peak = (isAccent ? 0.5 : 0.28) * vol;
  // 2 ms attack (no DC click) → 40 ms exponential decay (exp avoids an off-click; can't ramp to 0).
  gain.gain.setValueAtTime(0, time);
  gain.gain.linearRampToValueAtTime(peak, time + 0.002);
  gain.gain.exponentialRampToValueAtTime(0.0001, time + 0.04);
  osc.connect(gain);
  gain.connect(engine.masterGain);
  osc.start(time);
  osc.stop(time + 0.05);
  pendingClicks.set(osc, { time, forced });
  osc.onended = () => {
    pendingClicks.delete(osc);
    osc.disconnect();
    gain.disconnect();
  };
}

// ---------------------------------------------------------------------------
// Beat pulse — ONE mechanism: a ctx-time lookahead pulse. The waker (setInterval) only WAKES the
// scheduler; the blip/LED times are pure ctx time (pulseAnchor + N*pulseBeatPeriod) — the timer never
// paces the audio, so this is NOT a second clock (invariant #1: the click derives straight from ctx).
// The SAME pulse serves three anchors:
//   - FREE-RUN (startFreeRunPulse; from ensureRunning, and again after a count abort / master reset):
//     a wall-anchored beat 0 + the bpm-derived period. Drives the LED; with the click gated on
//     transport activity it is silent while idle, but clicks correctly during a metronome-on free take.
//   - COUNT-IN (startCountIn): anchored at the count downbeat; the first N beats are forced audible.
//   - MASTER (startMasterPulse): anchored at masterStartTime with the exact integer-frame-derived
//     period, so click + LED extrapolate from the SAME ctx anchor and the SAME exact period the loop
//     plays on — they cannot drift, they re-align on every transport op for free, and the accent lands
//     on every loop/bar downbeat by construction.
// ---------------------------------------------------------------------------

/**
 * LED-write generation, bumped by teardownBeatPulse. The pulse pushes the beat-LED update out-of-band
 * through Tone's Draw queue (up to a lookahead ahead of ctx time), and teardown only clears the
 * scheduling mechanism — it can't un-queue an already-scheduled Draw callback. So on a master reset /
 * count abort a stale beat index could paint one render frame past the restored grid. Each Draw callback
 * captures the gen live and no-ops if teardown has since bumped it — cosmetic, but it honors the
 * teardown contract.
 */
let pulseGen = 0;

let pulseTimer: ReturnType<typeof setInterval> | null = null;
let pulseAnchor = 0; // absolute ctx time of beat 0 (masterStartTime / count downbeat / free-run start)
let pulseBeatPeriod = 0; // seconds per quarter note (exact from the integer frame count under a master)
let pulseNextN = 0; // next absolute beat index to schedule
/** True while the pulse free-runs (no count-in / master anchor) — only then may setBpm re-anchor it. */
let pulseFreeRun = false;
/**
 * Count-in: beats with index < pulseForcedUntilN are clicked even when the metronome is OFF, so the
 * "ONE-two-three-four" count is always audible. 0 (the default, and reset by startMasterPulse /
 * startFreeRunPulse) means "honor metronomeOn() for every beat" — the normal master/free-run
 * behaviour.
 */
let pulseForcedUntilN = 0;
const PULSE_LOOKAHEAD = 0.1; // schedule blips up to 100 ms ahead of ctx.currentTime (never late)
const PULSE_INTERVAL_MS = 25; // waker cadence — well inside the lookahead horizon

/**
 * Stop the pulse waker and (by default) invalidate any LED writes it already queued into Draw.
 * `invalidateQueued: false` is for the PHASE-CARRYING free-run re-anchor only: there the next beat
 * keeps the outgoing grid's time and bar index, so LED writes already dispatched into the lookahead
 * are still correct — bumping the gen would eat them (their indices are < pulseNextN, so the new
 * pulse never reschedules them) and the LED would skip a beat on every mid-free-run bpm change.
 * Count-in / master re-anchors establish a genuinely NEW grid and must keep the bump.
 */
function teardownBeatPulse(invalidateQueued = true): void {
  if (invalidateQueued) pulseGen++;
  setCountLeft(0); // a superseded/aborted count must not leave a stale numeral in the lane well
  if (pulseTimer !== null) {
    clearInterval(pulseTimer);
    pulseTimer = null;
  }
}

/**
 * Lookahead scheduler for the ctx pulse. Woken every PULSE_INTERVAL_MS, it
 * schedules every beat whose absolute ctx time falls inside the lookahead horizon. Each beat drives
 * the LED (via Draw, on the render frame nearest the beat) and, if the metronome is on, a click —
 * both at the EXACT ctx time `pulseAnchor + N*pulseBeatPeriod`, so they stay sample-locked to the
 * loop. `pulseNextN` persists across wakes, so beats are scheduled exactly once and never skipped.
 */
function pulseTick(): void {
  if (pulseBeatPeriod <= 0) return;
  const now = engine.ctx.currentTime;
  const horizon = now + PULSE_LOOKAHEAD;
  let t = pulseAnchor + pulseNextN * pulseBeatPeriod;
  while (t < horizon) {
    const forced = pulseNextN < pulseForcedUntilN; // a count-in beat ("ONE-two-three-four") — must be heard
    // A beat already in the past (a throttled/stalled waker — a GC pause, a live buffer-size change, an
    // ASIO↔WASAPI switch — woke late): for the MASTER/free-run pulse, DROP it rather than machine-gun a
    // catch-up burst — phase stays exact (the index is anchor-derived from pulseNextN), so click + LED
    // resume cleanly on the next live beat. But a FORCED count beat is a load-bearing fixed sequence:
    // silently dropping it breaks the audible/visual count while the take's frame-0 arm (counted
    // independently in looper consume()) still lands on the original downbeat — count and loop "1" then
    // disagree. So fire a late forced beat clamped to `now` (the anti-flam guard collapses several
    // same-instant catch-ups to one click), keeping the count complete.
    const fireAt = t >= now ? t : forced ? now : -1;
    if (fireAt >= 0) {
      const beatInBar = pulseNextN % 4; // 0 = accent (loop/bar/count downbeat)
      const left = forced ? pulseForcedUntilN - pulseNextN : 0; // the lane well's 4-3-2-1
      const gen = pulseGen; // a queued LED write no-ops if teardown supersedes this pulse before it fires
      getDraw().schedule(() => {
        if (gen === pulseGen) {
          setBeat(beatInBar);
          setCountLeft(left);
        }
      }, fireAt);
      // Count-in beats are forced audible (you MUST hear the count); past the count we honor the
      // user's metronome on/off AND transport activity (the click is a transport mode: silent when all
      // tracks are stopped, resumes with them). The accent (beat 0 of the bar) covers both the count "1"
      // and the come-in "1" that the take begins on. `t < now` ⇒ this is a clamped catch-up (fired at
      // `now`, not its true time): mark it so it can't suppress a later true-time downbeat.
      if (forced || (metronomeOn() && transportActive && fireAt < transportActiveUntil)) {
        triggerClick(fireAt, beatInBar === 0, t < now, forced);
      }
    }
    pulseNextN++;
    t = pulseAnchor + pulseNextN * pulseBeatPeriod;
  }
}

/**
 * Start (or re-anchor) the FREE-RUN pulse — the same lookahead pulse, wall-anchored with the
 * bpm-derived period. Runs from ensureRunning() until a count-in / master anchor takes over, and again
 * after a count abort or a master reset. When a grid is already live (a bpm change mid-free-run, or the
 * fall-back from a torn-down count/master grid), the next beat keeps the OUTGOING grid's time and bar
 * index — the cadence bends to the new period with no LED hop and no double-fire (beats already
 * dispatched into the lookahead all have index < pulseNextN, and a stale next-beat time in the past is
 * dropped by pulseTick as usual). The free-run bar phase is anchored to an arbitrary wall moment by
 * nature (the parked "stable downbeat" line in docs/backlog-taste.md); the invariants here are the cadence and that the
 * accent stays on N % 4 === 0.
 */
function startFreeRunPulse(): void {
  const period = 60 / bpm();
  const nextBeatTime =
    pulseBeatPeriod > 0 ? pulseAnchor + pulseNextN * pulseBeatPeriod : engine.ctx.currentTime;
  // Phase-carrying: queued LED writes stay valid on the carried grid, so don't invalidate them.
  teardownBeatPulse(false);
  pulseFreeRun = true;
  pulseForcedUntilN = 0;
  pulseBeatPeriod = period;
  pulseAnchor = nextBeatTime - pulseNextN * period; // beat pulseNextN fires where the old grid had it
  pulseTick(); // fill the first horizon immediately — don't wait a full interval
  pulseTimer = setInterval(pulseTick, PULSE_INTERVAL_MS);
}

/**
 * Start a COUNT-IN before the first track's take (the looper calls this on first-track record press).
 * Drives the SAME ctx-time lookahead pulse as the master pulse, anchored at `anchorCtxTime` (the count
 * downbeat, ~one internal latency ahead) with `beatPeriodSec` = 60/bpm at press. The first `countBeats`
 * beats are FORCED audible (the "ONE-two-three-four" count, accent on beat 1) regardless of metronomeOn;
 * every beat also drives the LED, so it visually counts you in. Beats at/after `countBeats` honor the
 * user's metronome on/off — so the click stays coherent (same grid, same phase) through the free take
 * if the metronome is on. The take's frame 0 lands on beat `countBeats` (the looper arms frame-exact to
 * it). Takes over the pulse from the free-run anchor. At commit, the looper calls startMasterPulse()
 * which re-anchors this same pulse to the quantized grid; on abort it calls stopCountIn().
 */
function startCountIn(anchorCtxTime: number, beatPeriodSec: number, countBeats: number): void {
  teardownBeatPulse();
  pulseFreeRun = false;
  // Anti-flam reference: clear it UNCONDITIONALLY. The forced count "1" must never be suppressed by a
  // stale value (a prior count's "EN" left in lastClickTime on a fast abort→re-record would eat this
  // count's "EN"). Safe: the idle free-run pulse is silent
  // (transportActive gate) and the count anchors at minLead, so no sounding blip can coincide with the
  // anchor. NOT reset in startMasterPulse — the commit path RELIES on the carried-over value to
  // suppress the commit-beat flam (an already-dispatched blip carried across pulse re-anchoring).
  lastClickTime = -1;
  if (beatPeriodSec <= 0) return; // guard: no valid period -> no count
  pulseAnchor = anchorCtxTime;
  pulseBeatPeriod = beatPeriodSec;
  pulseForcedUntilN = Math.max(0, countBeats); // count beats click even with the metronome off
  const now = engine.ctx.currentTime;
  pulseNextN = Math.max(0, Math.ceil((now - anchorCtxTime) / beatPeriodSec)); // = 0 (anchor ~HBL ahead)
  pulseTick(); // fill the first horizon immediately (beat 0 fires on the first wake at the anchor)
  pulseTimer = setInterval(pulseTick, PULSE_INTERVAL_MS);
}

/**
 * Abort an in-progress count-in (stop/clear before the come-in downbeat): re-anchor the pulse to
 * free-run. Identical to stopMasterPulse — no master loop exists yet, so the free-run pulse is the
 * right thing to leave behind.
 */
function stopCountIn(): void {
  stopMasterPulse();
}

/**
 * Start the master-anchored ctx pulse (the looper calls this at master-loop commit). `anchorCtxTime`
 * is the loop downbeat (masterStartTime); `beatPeriodSec` is the exact quarter-note period derived
 * from the integer frame count. Re-anchors the pulse (from the count-in or free-run grid) so click +
 * LED lock to the loop. Mode-agnostic: works for free-recorded-then-quantized and future fixed-length
 * loops.
 */
function startMasterPulse(anchorCtxTime: number, beatPeriodSec: number): void {
  teardownBeatPulse();
  pulseFreeRun = false;
  if (beatPeriodSec <= 0) return; // guard: no valid period -> leave the LED/click idle
  pulseAnchor = anchorCtxTime;
  pulseBeatPeriod = beatPeriodSec;
  pulseForcedUntilN = 0; // committed pulse: every beat honors metronomeOn (the count is over)
  // First beat index whose ctx time is at/after now. The anchor may be up to a full loop period in
  // the PAST (seamless commit passes masterStartTime, the original downbeat), so ceil() picks the
  // next UPCOMING beat (index > 0); the max(0, …) covers the count-in-commit case where the anchor
  // sits at/ahead of now and the expression goes <= 0.
  const now = engine.ctx.currentTime;
  pulseNextN = Math.max(0, Math.ceil((now - anchorCtxTime) / beatPeriodSec));
  pulseTick(); // fill the first horizon immediately — don't wait a full interval
  pulseTimer = setInterval(pulseTick, PULSE_INTERVAL_MS);
}

/**
 * AUTO REC has no audible count beat to carry into its grid. Clear the anti-flam reference, then use
 * the ordinary past-anchor master pulse so the next future click follows the detected onset at the
 * current BPM. The later commit calls startMasterPulse directly and preserves its anti-flam state.
 */
function startAutoRecordPulse(anchorCtxTime: number, beatPeriodSec: number): void {
  lastClickTime = -1;
  startMasterPulse(anchorCtxTime, beatPeriodSec);
}

/** Fall back to the free-run pulse (the looper calls this at master reset; stopCountIn delegates). */
function stopMasterPulse(): void {
  // startFreeRunPulse carries the outgoing grid's phase, so the LED keeps beating with no loop and
  // without a hop at the reset moment.
  startFreeRunPulse();
}

function setMetronome(on: boolean): void {
  ensureRunning(); // bring the transport alive so the click sounds without a global play button
  setMetronomeSignal(on);
  // No need to reschedule; pulseTick reads metronomeOn() per scheduled beat.
}

// ---------------------------------------------------------------------------
// Exported singleton
// ---------------------------------------------------------------------------
export const clock = {
  /** Reactive: current BPM (Solid signal accessor). */
  bpm,
  /** Set BPM, clamped to [40, 300]. */
  setBpm,
  /**
   * Tap tempo. Pass explicit `now` (ms) for deterministic testing.
   * Returns current bpm after processing.
   */
  tap,
  /** Start the transport once and leave it running (beat LED + metronome free-run). Idempotent. */
  ensureRunning,
  /** Reactive: whether the audio clock is live (transport free-running). */
  running,
  /** Reactive: whether BPM is frozen (a master loop exists). */
  bpmLocked,
  /** Freeze/unfreeze BPM (called by the looper at master commit / reset). */
  setBpmLocked,
  /** Reactive: current beat index (0–3). Updated via Draw on the render frame. */
  beat,
  /** Reactive: count-in beats left incl. the one that just fired (4→1), 0 when no forced count runs. */
  countLeft,
  /** Start a count-in before the first track's take (forced-audible count clicks + LED). */
  startCountIn,
  /** Abort an in-progress count-in (stop/clear before the come-in) -> restore the free-run grid. */
  stopCountIn,
  /** Start the master-anchored ctx pulse (click + LED locked to the loop). Looper calls at commit. */
  startMasterPulse,
  /** Start the current-BPM pulse from an AUTO REC onset without a forced count. */
  startAutoRecordPulse,
  /** Stop the master pulse and restore the free-run grid. Looper calls at master reset. */
  stopMasterPulse,
  /** Reactive: whether metronome click is on. */
  metronomeOn,
  /** Enable or disable the metronome click. */
  setMetronome,
  /** Looper pushes transport activity (any track live) — gates the audible click (mode, not free tick). */
  setTransportActive,
  /** Reactive: the metronome click volume (0..1). */
  clickVolume,
  /** Set the metronome click volume (0..1), clamped + persisted. */
  setClickVolume,
} as const;

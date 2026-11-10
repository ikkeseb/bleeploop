import { looper, type TunerReading } from '../state/audio';
import { centsText, inTuneStep, noteOf } from './pitch';

/**
 * OWNS: the tuner's one requestAnimationFrame loop. Started when the tuner overlay (`TunerView.tsx`)
 * mounts and stopped when it unmounts; one tuner at a time.
 *
 * Each frame it reads the store's plain mirror of the last reading (`looper.tunerInto`) and writes the
 * DOM itself: the indicator's place on the cents scale, eased toward each reading by a critically damped
 * follower (it reports, so it moves; with reduced motion it steps), the cents and the frequency. It
 * decides what the reading means: a reading older than `STALE_MS` is no pitch, the in-tune state keeps
 * its hysteresis (`pitch.ts`), and a second of fresh readings under `QUIET_DB` is no signal: a stale one
 * (a gap in the feed is no measurement of the input) drops the pitch and ends that second. The cents and
 * the frequency text is written only when its string changes. The note, the in-tune state and the no-signal state go to the Solid side through `onState`,
 * called only when one of them changes (a few times a second at most).
 *
 * Invariant 6: no Solid import, no signal read; `verify/guards/stage-draw.mjs` holds this file and what
 * it reaches to that, as it holds the stage's loop. Allocation-free while the reading stands still.
 */

/** The DOM the loop writes. */
export interface TunerEls {
  /** A layer as wide as the scale with the indicator at its centre: moved by `translateX(<cents>%)`, so a
   * cent is a hundredth of the scale and nothing is measured. */
  needle: HTMLElement;
  cents: HTMLElement;
  hz: HTMLElement;
}

/** What the overlay shows in words and tone: the note (its MIDI number, null for no pitch), whether it is
 * in tune, and whether no signal reaches the input. */
export type TunerOnState = (midi: number | null, inTune: boolean, noSignal: boolean) => void;

export interface TunerHandle {
  /** Reduced motion: the indicator steps to each reading instead of easing there. */
  setReduced(on: boolean): void;
  stop(): void;
}

/** A reading older than this is no pitch: the host sends 20 to 25 a second while it hears one. */
const STALE_MS = 300;
/** Quieter than this (dBFS), the input carries no signal worth tuning ... */
const QUIET_DB = -60;
/** ... and after this long that quiet, the overlay says so. */
const NO_SIGNAL_MS = 1000;
/** The indicator's follower reaches 90 % of a step in about this long: it smooths the steps between
 * readings without trailing them. */
const FOLLOW_MS = 80;
/** Its natural frequency (rad/s): a critically damped step response reaches 90 % at ωt ≈ 3.89. */
const OMEGA = 3.89 / (FOLLOW_MS / 1000);
/** The longest frame step the follower takes (s): a tab coming back from the background lands, not flies. */
const MAX_STEP_S = 0.1;

interface Run {
  els: TunerEls;
  onState: TunerOnState;
  reduced: boolean;
  view: TunerReading;
  /** The reading last read (its arrival), its note and cents, its level. */
  at: number;
  midi: number | null;
  cents: number;
  level: number;
  /** The cents and frequency strings last written. */
  centsShown: string;
  hzShown: string;
  /** The state last reported. */
  shownMidi: number | null;
  inTune: boolean;
  noSignal: boolean;
  /** When the current quiet began (`performance.now()`), -1 while there is signal or no fresh reading. */
  quietSince: number;
  /** The indicator: its place (cents) and speed (cents/s), and the place last written. */
  x: number;
  v: number;
  written: number;
  last: number;
}

let run: Run | null = null;
let rafId = 0;

/** Ease the indicator toward `target` over `dt` seconds: the exact step of a critically damped spring,
 * stable at any frame time. */
function follow(r: Run, target: number, dt: number): void {
  const e = r.x - target;
  const k = r.v + OMEGA * e;
  const decay = Math.exp(-OMEGA * dt);
  r.x = target + (e + k * dt) * decay;
  r.v = (r.v - OMEGA * k * dt) * decay;
}

function tunerFrame(now: number): void {
  rafId = requestAnimationFrame(tunerFrame);
  const r = run;
  if (r === null) return;
  const view = looper.tunerInto(r.view);
  if (view.at !== r.at) {
    // A new reading, or the mirror dropped the last one (the tuner went off, a reset).
    r.at = view.at;
    r.level = view.level;
    const note = view.hz === null ? null : noteOf(view.hz);
    r.midi = note ? note.midi : null;
    if (note) {
      r.cents = note.cents;
      // Text only on change (invariant 6): a steady reading arrives 20 to 25 times a second.
      const c = centsText(note.cents);
      if (c !== r.centsShown) r.els.cents.textContent = r.centsShown = c;
      const f = (view.hz as number).toFixed(2);
      if (f !== r.hzShown) r.els.hz.textContent = r.hzShown = f;
    }
  }
  const fresh = r.at > 0 && now - r.at <= STALE_MS;
  const midi = fresh ? r.midi : null;
  const inTune = midi !== null && inTuneStep(r.inTune, r.cents);
  if (fresh && r.level < QUIET_DB) {
    if (r.quietSince < 0) r.quietSince = now;
  } else r.quietSince = -1;
  const noSignal = r.quietSince >= 0 && now - r.quietSince >= NO_SIGNAL_MS;

  // No pitch parks the indicator at the centre.
  const target = midi === null ? 0 : r.cents;
  const dt = r.last < 0 ? 0 : Math.min(MAX_STEP_S, (now - r.last) / 1000);
  r.last = now;
  if (r.reduced) {
    r.x = target;
    r.v = 0;
  } else follow(r, target, dt);
  if (Math.abs(r.x - r.written) >= 0.01) {
    r.written = r.x;
    r.els.needle.style.transform = `translateX(${r.x.toFixed(2)}%)`;
  }

  if (midi !== r.shownMidi || inTune !== r.inTune || noSignal !== r.noSignal) {
    r.shownMidi = midi;
    r.inTune = inTune;
    r.noSignal = noSignal;
    r.onState(midi, inTune, noSignal);
  }
}

/** Start the tuner's loop on `els`. A second start replaces the first. The state starts as no pitch,
 * in tune false, signal unknown: `onState` hears the first change. */
export function startTuner(els: TunerEls, onState: TunerOnState, reduced: boolean): TunerHandle {
  const mine: Run = {
    els,
    onState,
    reduced,
    view: { hz: null, level: -120, at: 0 },
    at: -1,
    midi: null,
    cents: 0,
    level: -120,
    centsShown: '',
    hzShown: '',
    shownMidi: null,
    inTune: false,
    noSignal: false,
    quietSince: -1,
    x: 0,
    v: 0,
    written: 0,
    last: -1,
  };
  run = mine;
  els.needle.style.transform = 'translateX(0%)';
  if (rafId === 0) rafId = requestAnimationFrame(tunerFrame);
  return {
    setReduced(on) {
      mine.reduced = on;
    },
    stop() {
      if (run !== mine) return;
      run = null;
      cancelAnimationFrame(rafId);
      rafId = 0;
    },
  };
}

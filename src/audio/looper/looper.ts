/**
 * OWNS: the public `looper` facade — the one object the UI, `__lf` and the verify ports go through. No
 * logic lives here; the module map below says which file owns which decision.
 *
 * The looper engine: an RC-505-style 5-track looper that records from `engine.recordTap` (the
 * record-only mirror of `looperInputBus`), loops with sample-accurate master-loop quantization, and
 * supports overdub.
 *
 * ── CAPTURE PATH ──────────────────────────────────────────────────────────────────────
 * `capture.ts` owns the AudioWorklet, SAB ring, main-thread drain and mic/line input arm. Its header
 * documents the complete capture path and keep-alive wiring.
 *
 * ── MASTER LOOP ───────────────────────────────────────────────────────────────────────
 * The FIRST track to finish its initial recording defines the master loop. Its raw recorded
 * length is quantized to a whole number of bars at the current clock.bpm() (>= 1 bar). The
 * displayed tempo stays FROZEN at the count-in press (BPM is locked there for every count-in),
 * and the loop period is taken purely from the integer frame count — never re-derived from bpm.
 * masterLengthFrames is stored as an INTEGER and every later track COMMITS exactly that many frames.
 * A shorter whole-bar take tiles across that region, so tracks stay frame-identical without a silent tail.
 *
 * ── PLAYBACK ──────────────────────────────────────────────────────────────────────────
 * Per track: an AudioBufferSourceNode (loop=true, loopStart=0, loopEnd=loopPeriodSec) started
 * at a quantized absolute ctx.currentTime boundary, routed track -> per-track GainNode ->
 * engine.masterGain (NOT back into looperInputBus — that would feed back).
 *
 * ── OVERDUB ───────────────────────────────────────────────────────────────────────────
 * While OVERDUBBING we sum incoming PCM into a COPY of the track buffer (overdubBuf). The
 * record write head wraps modulo masterLengthFrames so the new layer lands sample-aligned over
 * the existing loop. At the next loop boundary we build a fresh AudioBuffer from the summed
 * data and swap the playing source for one started exactly on the boundary. Phase stays aligned, but
 * the no-crossfade swap can still step when the outgoing and incoming samples differ. See
 * scheduleOverdubSwap().
 *
 * ── MODULE MAP ────────────────────────────────────────────────────────────────────────
 * This file is a thin FACADE: it re-exports the public types and assembles the `looper` object from
 * the modules below. Dependency direction is one-directional except one flagged edge:
 *   grid-math.ts — the PURE grid arithmetic (commit quantise/BPM derive, anchors, arm split). No engine
 *                   import, so verify/ imports it. Depends on nothing.
 *   state.ts     — constants, Track/TrackPublic/PeakView types, the reactive Solid signals, the plain
 *                   engine-singleton `engineState` mutable object, and the read-only accessors
 *                   (publish/trackPeak/trackInfo/captureQuanta/captureOverruns/peaksInto/phaseValue/
 *                   stateOf/fillFramesOf/recHeadFrac/masterFramesValue/sr). The base every other module
 *                   imports.
 *   peaks.ts     — waveform peak precompute (updateLivePeaks/recomputePeaks/resetPeaks). Depends on state.
 *   playback.ts  — AudioBufferSourceNode scheduling, boundary math, the overdub boundary-swap timer.
 *                   Depends on state + peaks.
 *   mixer.ts     — per-track gain/mute/volume + FX param control. Depends on state.
 *   machine.ts   — the EMPTY→RECORDING→PLAYING⇄OVERDUBBING(+STOPPED) state machine. Depends on
 *                   state + peaks + playback + mixer, AND on capture.ts's `init()` (recDub awaits it).
 *   capture.ts   — the capture worklet wiring, SAB ring, drain tick, consume()/arm-split, the mic/line
 *                   input arm. Depends on state + peaks + machine.ts (consume() commits via
 *                   finishCapture).
 *   session.ts   — validates and loads exported PCM + mixer/FX state. Depends on capture, state, peaks,
 *                   playback and mixer.
 *   transport-actions.ts — selected-track REC/DUB and PLAY/STOP adapters for keyboard/MIDI dispatch.
 *                   Depends on state + machine.
 * capture.ts ↔ machine.ts is therefore a circular import (capture needs machine's commit functions;
 * machine needs capture's `init`). This is safe here — both directions are only used inside function
 * bodies at runtime, never at module-evaluation time — but keep it the only one.
 */

import { armInput, disarmInput, init, inputArmRequested, toggleInput } from './capture';
import {
  autoRecordEnabled,
  autoRecordSensitivity,
  captureOverruns,
  captureQuanta,
  exportSnapshot,
  fillFramesOf,
  fixedLengthBars,
  fixedLengthEnabled,
  inputArmed,
  masterFramesValue,
  masterLengthFrames,
  mutedOf,
  loopEndStopEnabled,
  setLoopEndStopEnabled,
  levelValue,
  peaksInto,
  phaseValue,
  recHeadFrac,
  retakeEnabled,
  setRetakeEnabled,
  stateOf,
  TRACK_COUNT,
  trackInfo,
  trackPeak,
  trackSignals,
} from './state';
import {
  clear,
  clearAll,
  copy,
  playAll,
  playStop,
  nextTakeMaxBars,
  recDub,
  reverse,
  setAutoRecordEnabled,
  setAutoRecordSensitivity,
  setFixedLengthBars,
  setFixedLengthEnabled,
  stop,
  stopAll,
  undoLastOverdub,
} from './machine';
import { loadSession } from './session';
import {
  fxState,
  setFxBypass,
  setFxParam,
  setMute,
  setVolume,
  trackMuted,
  trackVolume,
} from './mixer';
import { playStopSelected, recDubSelected, selectTrack, selectedTrack } from './transport-actions';

export type { TrackState, PeakView } from './state';

// ── Exported singleton ───────────────────────────────────────────────────────────────────
export const looper = {
  /** Lazily build the capture worklet + ring + tracks. Awaitable; idempotent. */
  init,
  /** Number of tracks (5). */
  trackCount: TRACK_COUNT,
  /** [REC/DUB] intent for track `i` (record / overdub only). */
  recDub,
  /** [▶/■] intent for track `i` (commit+stop / stop / resume — keeps the take). */
  playStop,
  /** STOP track `i` (keeps the buffer). */
  stop,
  /** One-level UNDO/REDO of track `i`'s last overdub (boundary-aligned buffer swap; toggle). */
  undoLastOverdub,
  /** Toggle per-track REVERSE of track `i`'s committed loop (boundary-aligned buffer swap; toggle). */
  reverse,
  /** COPY track `i`'s whole lane (loop, volume, mute, FX) into the first EMPTY lane; returns its index or -1. */
  copy,
  /** CLEAR track `i` (frees buffer -> EMPTY; resets master if it was the last track). */
  clear,
  /** Bring all live tracks to STOPPED (commits recordings). */
  stopAll,
  /** Resume all STOPPED tracks -> PLAYING. */
  playAll,
  /** Stop PLAYING loops on the next loop edge; off by default. Captures still commit+stop. */
  loopEndStopEnabled,
  setLoopEndStopEnabled,
  /** RETAKE: a known-length take keeps rolling; the stop gesture keeps the last complete pass. Off by default. */
  retakeEnabled,
  setRetakeEnabled,
  /** Clear all tracks and reset the master loop length. */
  clearAll,
  /** Reactive index of the keyboard/MIDI-selected track (0-based; default 0). */
  selectedTrack,
  /** Select track `i` for keyboard/MIDI transport (0-based, clamped). */
  selectTrack,
  /** REC/DUB toggle on the selected track (keyboard/MIDI entry; same path as the lane core). */
  recDubSelected,
  /** PLAY/STOP on the selected track (keyboard/MIDI entry; same path as the lane PLAY/STOP cap). */
  playStopSelected,
  /** Reactive per-track state and committed loop length. */
  track: (i: number) => trackSignals[i][0],
  /** Non-reactive snapshot of a track's public info. */
  trackInfo,
  /** Read-only snapshot (copies) of committed tracks for WAV export. */
  exportSnapshot,
  /** SESSION IMPORT v0: load exported PCM + mixer/FX state into an all-EMPTY looper (async; throws). */
  loadSession,
  /** Max abs sample of track `i`'s recorded region (peak; for non-silent verification). */
  trackPeak,
  /** Reactive master loop length in frames (integer; 0 until the first track commits). */
  masterLengthFrames,
  /** Reactive: whether the mic/line input is armed. */
  inputArmed,
  /** Arm the mic/line input into looperInputBus. Returns false if none available. */
  armInput,
  /** Disarm the mic/line input. */
  disarmInput,
  inputArmRequested,
  /** Toggle the mic/line input; returns the new armed state. */
  toggleInput,
  /** Worklet heartbeat: process() quantum count (proves capture is running). */
  captureQuanta,
  /** Cumulative capture overruns (dropped frames on a full ring; observability). */
  captureOverruns,
  /** Fill a caller-owned PeakView with track `i`'s waveform peaks (non-reactive; for the rAF loop). */
  peaksInto,
  /** Plain loop phase 0..1 (non-reactive) for the draw loop. */
  phaseValue,
  /** Plain peak-held record level 0..1 (non-reactive) for the draw loop's meter. */
  levelValue,
  /** Plain track state (non-reactive) for the draw loop. */
  stateOf,
  /** Plain track mute flag (non-reactive) for the draw loop. */
  mutedOf,
  /** Plain captured-frame count (non-reactive) for probes and the draw loop. */
  fillFramesOf,
  /** Later-track record-head fraction 0..1, or -1 if no master yet (non-reactive). */
  recHeadFrac,
  /** Plain master loop length in frames (non-reactive) for the draw loop. */
  masterFramesValue,
  /** Reactive per-track FX state array (five entries, chain order). */
  fxState,
  /** Toggle bypass for an FX of a track (click-free). */
  setFxBypass,
  /** Set an FX param of a track. */
  setFxParam,
  /** Set per-track output volume (0..1.5). */
  setVolume,
  /** Mute/unmute a track in-sync (loop keeps running). */
  setMute,
  /** Reactive per-track volume. */
  trackVolume,
  /** Reactive per-track mute state. */
  trackMuted,
  /** Reactive: whether the next take captures a fixed number of bars. */
  fixedLengthEnabled,
  /** Enable/disable fixed-length record mode for the next take. */
  setFixedLengthEnabled,
  /** Reactive: the fixed-length bar count (>= 1). */
  fixedLengthBars,
  /** Effective upper bar limit for the next take: master bars when set, otherwise 32. */
  nextTakeMaxBars,
  /** Set the fixed-length bar count (clamped to [1, 32]). */
  setFixedLengthBars,
  /** Reactive: whether the first master-defining take waits for a level trigger instead of count-in. */
  autoRecordEnabled,
  /** Enable/disable first-track AUTO REC. Starts disabled on each app launch. */
  setAutoRecordEnabled,
  /** Reactive AUTO trigger sensitivity on the 1..100 product scale. */
  autoRecordSensitivity,
  /** Set AUTO trigger sensitivity, clamped to a whole number in [1, 100]. */
  setAutoRecordSensitivity,
} as const;

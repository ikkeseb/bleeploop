import { createSignal } from 'solid-js';
import type { RingBuffer } from 'ringbuf.js';
import { engine } from '../engine';
import { clock } from '../clock';
import { type FxChain, type FxState } from '../fx/fx';
import { AUTO_RECORD_DEFAULT_SENSITIVITY } from './auto-record';

/**
 * OWNS: shared looper state — constants, the per-track `Track` record shape, the reactive Solid signals
 * the UI reads (+ `publish`, the one choke point every transition funnels through), the plain (non-signal)
 * engine-singleton state the capture/playback/machine modules read and write, and the read-only accessors
 * (`__lf` debug hook + the non-reactive draw loop) that only need a plain view of that state. No decisions
 * are made here. This is the base layer every other `looper/*` module depends on — kept one-directional
 * (state ← peaks, playback, mixer ← machine ← capture ← the `looper.ts` facade) so there are no import
 * cycles between the modules.
 */

// ── Constants ──────────────────────────────────────────────────────────────────────────
export const TRACK_COUNT = 5;
export const MAX_LOOP_SECONDS = 60;
export const DRAIN_INTERVAL_MS = 25;
/**
 * Ring capacity: 2^19 frames ≈ 10.9 s @48k / 11.9 s @44.1k, so a stalled drain tick (main-thread
 * jank, a multi-second freeze) has room for that much audio. A longer stall drops whole packets. Cost:
 * ~4.1 MiB each for the timestamped SAB ring and encoded scratch, plus ~2 MiB decoded scratch.
 */
export const RING_CAPACITY_FRAMES = 524288;
// The scheduling lead + count-in length live with the pure grid math (verify/ imports them from there).
export { COUNT_IN_BEATS, HEARTBEAT_INTERNAL_LATENCY } from './grid-math';
/**
 * Waveform peak resolution: one min/max pair per this many frames (~21ms @48kHz). Peaks are
 * pre-computed (incrementally during capture, fully on commit) so the 60fps draw loop never
 * scans raw PCM — it only down-samples this coarse array to the canvas width.
 */
export const PEAK_FRAMES = 1024;
/** Upper bound on the bars selector (a 32-bar loop at 40 bpm is ~3 min — already past MAX_LOOP_SECONDS;
 *  the per-record target is clamped DOWN to whole bars that fit the record buffer, so an over-long pick
 *  records the largest whole-bar count that fits rather than overrunning the buffer). */
export const MAX_FIXED_BARS = 32;

export type TrackState = 'EMPTY' | 'RECORDING' | 'OVERDUBBING' | 'PLAYING' | 'STOPPED';

interface TrackPublic {
  readonly state: TrackState;
  /** Final loop length in frames (= masterLengthFrames once the master is set, else 0). */
  readonly lengthFrames: number;
  /**
   * A later track is RECORDING but still waiting for the master boundary (the arm window) — no
   * audio is kept yet. The UI shows this as a distinct "waiting for downbeat" state rather than
   * live recording, since the strip would otherwise scream RECORDING for up to a full loop while
   * nothing is captured.
   */
  readonly armed: boolean;
  /** First-track AUTO REC is listening for an input onset; no audio is kept until it triggers. */
  readonly autoArmed: boolean;
  /** A one-level overdub undo is available (PLAYING/STOPPED with a pre-dub snapshot) — drives the UI cap. */
  readonly canUndo: boolean;
  /** Per-track reverse is available (PLAYING/STOPPED with a committed loop) — drives the UI cap. */
  readonly canReverse: boolean;
  /** Whether the loop is currently playing reversed (a toggle) — drives the UI cap's active state. */
  readonly reversed: boolean;
  /** Absolute audio time of a requested loop-end stop; null while no stop is pending. */
  readonly stopAt: number | null;
  /** RETAKE: the 1-based pass being recorded while the take rolls; 0 when this take is not rolling. */
  readonly retakePass: number;
}

/**
 * A non-reactive view of a track's waveform peaks, filled into a caller-owned object by
 * `peaksInto()` so the rAF draw loop can read peaks with zero allocation and no Solid
 * subscription. `min`/`max` are refs into the track's pre-allocated arrays; valid indices are
 * `[0, count)`. `version` bumps whenever the peaks change (the renderer's dirty flag).
 */
export interface PeakView {
  min: Float32Array | null;
  max: Float32Array | null;
  count: number;
  version: number;
}

// ── Reactive state (Solid signals) ───────────────────────────────────────────────────────
/**
 * Field-wise equality for the per-track payload. `publish()` runs on EVERY capture-drain tick
 * (~40/s while recording/overdubbing) and always writes a FRESH object, so Solid's default identity
 * check propagates a "change" even when nothing changed — during OVERDUBBING the payload is provably
 * constant (consume()'s overdub branch touches only `writeHead`, which is not published), yet every
 * subscriber re-ran and the DOM was rewritten 40×/s on the same thread as the capture drain and the
 * overdub boundary swap (hundreds of full-document layouts per 5 s of capture). All fields are
 * primitives, so this compare is exact — a real transition still propagates.
 */
function sameTrack(a: TrackPublic, b: TrackPublic): boolean {
  return (
    a.state === b.state &&
    a.lengthFrames === b.lengthFrames &&
    a.armed === b.armed &&
    a.autoArmed === b.autoArmed &&
    a.canUndo === b.canUndo &&
    a.canReverse === b.canReverse &&
    a.reversed === b.reversed &&
    a.stopAt === b.stopAt &&
    a.retakePass === b.retakePass
  );
}

export const trackSignals = Array.from({ length: TRACK_COUNT }, () =>
  createSignal<TrackPublic>(
    {
      state: 'EMPTY',
      lengthFrames: 0,
      armed: false,
      autoArmed: false,
      canUndo: false,
      canReverse: false,
      reversed: false,
      stopAt: null,
      retakePass: 0,
    },
    { equals: sameTrack },
  ),
);
export const [masterLengthFrames, setMasterLengthFrames] = createSignal(0);
export const [inputArmed, setInputArmed] = createSignal(false);
/** Session-only playback stop preference. Capture stop/commit behavior stays immediate. */
export const [loopEndStopEnabled, setLoopEndStopEnabled] = createSignal(false);
/** Per-track FX-state version bump — lets the UI re-read fxState reactively on any FX edit. */
export const fxVersion = Array.from({ length: TRACK_COUNT }, () => createSignal(0));
/** Per-track volume (0..1.5) + mute, as signals so the mixer UI re-reads reactively. */
export const volumeSignals = Array.from({ length: TRACK_COUNT }, () => createSignal(1));
export const muteSignals = Array.from({ length: TRACK_COUNT }, () => createSignal(false));
/**
 * Fixed-length record mode: when enabled, the next take captures `fixedLengthBars` bars and auto-stops
 * on the downbeat, clamped to the master for a later take. A first take still runs the count-in and locks
 * tempo at the press. RETAKE ignores FIXED once a master exists because its passes keep master length.
 */
export const [fixedLengthEnabled, setFixedLengthEnabledSignal] = createSignal(false);
export const [fixedLengthBars, setFixedLengthBarsSignal] = createSignal(4);
/**
 * RETAKE (session-only, off by default): a take whose length is known at arm keeps rolling pass after pass
 * instead of committing at its end; the stop gesture keeps the last COMPLETE pass. Later-take passes use
 * the master length even with FIXED enabled. A free first take has no length to roll around.
 */
export const [retakeEnabled, setRetakeEnabled] = createSignal(false);
/** Optional first-track level trigger. Off by default, so the existing count-in remains the entry path. */
export const [autoRecordEnabled, setAutoRecordEnabledSignal] = createSignal(false);
/** Session-only trigger sensitivity, 1 (loud) .. 100 (quiet), surfaced beside the AUTO toggle. */
export const [autoRecordSensitivity, setAutoRecordSensitivitySignal] = createSignal(
  AUTO_RECORD_DEFAULT_SENSITIVITY,
);

// ── Internal per-track record ──────────────────────────────────────────────────────────
export interface Track {
  state: TrackState;
  /** Pending playback stop on the master grid. The source owns the actual audio deadline. */
  stopAt: number | null;
  /** Pre-allocated record buffer (mono), MAX_LOOP_SECONDS * sampleRate frames. */
  record: Float32Array;
  /** Write head into `record` (frames). Wraps mod masterLengthFrames while overdubbing. */
  writeHead: number;
  /** Frames captured in the current pass; a committed later take is tiled and then reports master length. */
  fillFrames: number;
  /** The committed loop length (frames) — equals masterLengthFrames once playing. */
  lengthFrames: number;
  /** Live playback source + gain. */
  source: AudioBufferSourceNode | null;
  /** Sources still audible until a scheduled boundary swap; retained so STOP/CLEAR can silence them. */
  retiringSources: Set<AudioBufferSourceNode>;
  gain: GainNode | null;
  /** Per-track output volume 0..1.5 (unity 1.0). Applied to `gain` (store-and-apply if lazy). */
  volume: number;
  /** Muted: effective gain forced to 0 in-sync (the loop keeps running). */
  muted: boolean;
  /** Per-track FX chain (lazily built on first playback); routes gain -> FX -> masterGain. */
  fx: FxChain | null;
  /** FX state snapshot (source of truth for the UI; applied to `fx` when built). */
  fxState: FxState[];
  /** Working copy used while overdubbing (summed layers). */
  overdubBuf: Float32Array | null;
  /**
   * Two master-length playback AudioBuffers double-buffered for the overdub boundary swap: each boundary
   * writes the freshly-summed loop into the buffer NOT currently being read and hands it to a new source
   * (the outgoing source stopped a full loop period earlier), so the swap allocates nothing per period.
   * Allocated at startOverdub, freed (null) at finishOverdub / abort / clear. `overdubSwapIdx` picks the next.
   */
  overdubSwapBufs: [AudioBuffer, AudioBuffer] | null;
  overdubSwapIdx: number;
  /**
   * One-level undo/redo buffer for the LAST overdub: the loop as it was BEFORE the current/most-recent
   * overdub session, snapshotted at startOverdub (a SEPARATE copy — overdubBuf gets summed into, this one
   * doesn't). `undoLastOverdub` swaps it with `record`, so a second call redoes. Sized to master; null
   * when there's nothing to undo (a fresh first take, or after clear). Survives STOP; cleared on clear().
   */
  undoBuf: Float32Array | null;
  /** Previous one-level undo target retained only while a new overdub is in flight. If plugin PCM is
   * lost, the new layer is rejected and this restores the undo history that existed before it began. */
  overdubPreviousUndoBuf: Float32Array | null;
  /**
   * Whether the track's committed buffer is currently REVERSED relative to how it was recorded. Reversal
   * is its own inverse (a second `reverse(i)` restores forward), so this is a pure display flag. undo/redo
   * SWAPS this with `undoBufReversed` in lockstep with record<->undoBuf, so it ALWAYS describes the buffer
   * currently playing (never desyncs after dub->reverse->undo). Reset on clear().
   */
  reversed: boolean;
  /**
   * Orientation of the `undoBuf` snapshot, paired with `reversed` so the two swap together in
   * `undoLastOverdub`. Set = `reversed` at startOverdub (the snapshot inherits the live orientation); after a
   * swap it holds the orientation of the buffer undoBuf now points at (the redo target). Meaningless while
   * `undoBuf === null`. Reset on clear().
   */
  undoBufReversed: boolean;
  /** Orientation paired with `overdubPreviousUndoBuf`; meaningless while that buffer is null. */
  overdubPreviousUndoBufReversed: boolean;
  /** One pending boundary swap, cancelled when the overdub ends or playback stops. */
  overdubTimer: ReturnType<typeof setTimeout> | null;
  /**
   * A later track is RECORDING but still waiting for the master boundary (the arm window).
   * While armed we suppress the waveform/playhead so the briefly-captured pre-boundary audio
   * isn't shown — the real take begins (writeHead reset to 0) when the boundary fires.
   */
  armed: boolean;
  /** AUTO REC is waiting for signal. Separate from `armed`, which counts frames to a known grid edge. */
  autoArmed: boolean;
  /** Down-sampled waveform peaks (one min/max pair per PEAK_FRAMES frames). Pre-allocated. */
  peakMin: Float32Array;
  peakMax: Float32Array;
  /** Number of peak bins currently valid for display (incl. a trailing partial bin). */
  peakCount: number;
  /** Number of fully-completed bins (internal append cursor for incremental capture). */
  peakComplete: number;
  /** Dirty counter: bumped whenever the peaks change (the renderer's redraw trigger). */
  peakVersion: number;
}

// ── Engine singletons (lazy) ─────────────────────────────────────────────────────────────
/**
 * Mutable engine-singleton + transport state shared across the capture/playback/machine modules.
 * Plumbing note: ES module imports are live READ-only bindings — a module can't reassign a `let` it only
 * imported, so the fields below that are written from more than one of {capture.ts, machine.ts,
 * playback.ts} are grouped into this single mutable object rather than left as bare module-level `let`s
 * (which would force either a setter-function per field or a circular import between capture.ts and
 * machine.ts). Fields referenced from exactly one file stay a local `let` in that file (see capture.ts /
 * machine.ts) rather than living here.
 *
 * ⚠ INTERNAL to src/audio/looper/ — do NOT import from outside this directory. Everything external
 * (UI, __lf, verify ports) goes through the `looper` facade in looper.ts; importing this object
 * directly would bypass the state machine's invariants.
 */
export const engineState = {
  initialized: false,
  /** The five per-track records (buffers, heads, sources); built by capture.ts buildEngine. */
  tracks: [] as Track[],
  ring: null as RingBuffer | null,
  heartbeat: null as Int32Array | null,
  /** Scratch buffer the drain loop pops into (reused; not in the audio thread). */
  drainScratch: null as Float32Array | null,
  packetScratch: null as Float64Array | null,
  /** Absolute render-frame window for the single active recorder. */
  captureStartFrame: null as number | null,
  /** Exclusive absolute end, shared by automatic completion and manual stop; null for open overdub/AUTO. */
  captureEndFrame: null as number | null,
  captureCompensationFrames: 0,
  /** PLAY/STOP intent retained while audio performed before the press is still arriving. */
  captureStopPlayback: false,
  captureFrontierFrame: 0,
  /**
   * RETAKE roll of the active take. While `retakeRolling`, reaching `captureEndFrame` slides the capture
   * window one pass forward instead of committing. `retakeBuf` holds the last complete CLEAN pass (valid
   * when `retakeKept`); `retakePass` is the 1-based pass in flight. All reset with the recorder slot.
   */
  retakeRolling: false,
  retakeBuf: null as Float32Array | null,
  retakeKept: false,
  retakePass: 0,
  /** The active recording track index, or -1. Owns the single capture ring while set. */
  activeRecordIndex: -1,
  /** ctx.currentTime at which the master loop's downbeat occurs (anchor for all boundaries). */
  masterStartTime: 0,
  /** Diagnostic remaining arm frames. Actual capture uses captureStartFrame against packet timestamps. */
  pendingRecordStartFrame: 0,
  /** Max peak bins per track (sized from the record buffer length; set in init). */
  maxPeaks: 0,
  /**
   * Plain (non-signal) mirrors for the 60fps draw loop. Solid signals must not be read in the rAF
   * loop (invariant 6), so the looper keeps bare copies the renderer polls via cheap getters.
   */
  loopPhasePlain: 0,
  masterFramesPlain: 0,
  /** Peak-held record level 0..1 at recordTap (what a take would capture), decayed per drain tick. */
  inputLevelPlain: 0,
};

export function sr(): number {
  return engine.ctx.sampleRate;
}

export function publish(i: number): void {
  const t = engineState.tracks[i];
  trackSignals[i][1]({
    state: t.state,
    lengthFrames: t.lengthFrames,
    armed: t.armed,
    autoArmed: t.autoArmed,
    canUndo: t.undoBuf !== null && (t.state === 'PLAYING' || t.state === 'STOPPED'),
    canReverse: t.lengthFrames > 0 && (t.state === 'PLAYING' || t.state === 'STOPPED'),
    reversed: t.reversed,
    stopAt: t.stopAt,
    retakePass: engineState.retakeRolling && engineState.activeRecordIndex === i && !t.armed ? engineState.retakePass : 0,
  });
  // Every state transition funnels through here, so this is the one choke point that knows whether the
  // transport is audibly alive. This call is DELIBERATELY outside the signal write above and must stay
  // unconditional: the signal is change-gated (sameTrack), the click gate is not — skipping it when the
  // payload happens to be unchanged would leave the metronome reading a stale transport mode.
  // The clock gates the metronome click on it (click = a transport mode:
  // count-in + recording/playback, silent when everything is stopped/empty).
  let activeUntil = 0;
  for (const tr of engineState.tracks) {
    if (engineState.captureStopPlayback && masterLengthFrames() > 0 && tr === engineState.tracks[engineState.activeRecordIndex]) continue;
    if ((tr.state === 'RECORDING' && !tr.autoArmed) || tr.state === 'OVERDUBBING') {
      activeUntil = Infinity;
      break;
    }
    if (tr.state === 'PLAYING') activeUntil = Math.max(activeUntil, tr.stopAt ?? Infinity);
  }
  clock.setTransportActive(activeUntil > 0, activeUntil);
}

// ── Read-only accessors for the __lf debug hook / UI ─────────────────────────────────────
/** Max abs sample over the committed loop region (or fill region while recording). */
export function trackPeak(i: number): number {
  const t = engineState.tracks[i];
  const n = t.lengthFrames > 0 ? t.lengthFrames : t.fillFrames;
  let peak = 0;
  const buf = t.record;
  for (let k = 0; k < n; k++) {
    const a = buf[k] < 0 ? -buf[k] : buf[k];
    if (a > peak) peak = a;
  }
  return peak;
}

export function trackInfo(i: number): TrackPublic {
  return trackSignals[i][0]();
}

// ── WAV-export snapshot (read-only; WAV-export v0, FX state added for the v1 wet master) ──
export interface ExportTrack {
  /** 0-based engine track index. */
  index: number;
  /** Copy of the committed loop region [0, lengthFrames), MONO. */
  pcm: Float32Array;
  volume: number;
  muted: boolean;
  reversed: boolean;
  /** Deep copy of the five per-track FX states (chain order) — drives the v1 offline master render
   *  and lands in session.json. Plain JSON-serializable data. */
  fx: FxState[];
}
export interface ExportSnapshot {
  sampleRate: number;
  masterLengthFrames: number;
  tracks: ExportTrack[];
}

/**
 * Read-only snapshot for WAV export (WAV-export v0). Returns COPIES of each committed track's mono
 * loop region. A track is committed once it has a loop length (lengthFrames > 0), which admits
 * PLAYING, STOPPED and OVERDUBBING. An OVERDUBBING track's `record` buffer always holds a complete,
 * coherent loop: startOverdub sums incoming PCM into a SEPARATE overdubBuf (capture.ts) and each
 * boundary swap writes the freshly-committed layer back into `record` (playback.ts scheduleOverdubSwap),
 * so `record` is never torn mid-write — it is exactly the already-committed audio the user is hearing.
 * Export runs synchronously on the same main thread as the boundary swap, so a slice() cannot race it.
 * Only a first take still RECORDING (no loop defined yet, lengthFrames === 0) is excluded. engineState
 * never leaves the directory; callers get plain data.
 */
export function exportSnapshot(): ExportSnapshot {
  const tracks: ExportTrack[] = [];
  for (let i = 0; i < engineState.tracks.length; i++) {
    const t = engineState.tracks[i];
    if (t.lengthFrames > 0 && (t.state === 'PLAYING' || t.state === 'STOPPED' || t.state === 'OVERDUBBING')) {
      tracks.push({
        index: i,
        pcm: t.record.slice(0, t.lengthFrames),
        volume: t.volume,
        muted: t.muted,
        reversed: t.reversed,
        fx: t.fxState.map((s) => ({ bypassed: s.bypassed, params: { ...s.params } })),
      });
    }
  }
  return { sampleRate: sr(), masterLengthFrames: masterLengthFrames(), tracks };
}

/** Number of process() quanta the worklet has run (proves the capture node is pulled). */
export function captureQuanta(): number {
  return engineState.heartbeat ? Atomics.load(engineState.heartbeat, 0) : 0;
}

/**
 * Cumulative capture overrun: total FRAMES dropped on a full ring (a stalled/backgrounded main-thread
 * drain). Timestamped packets expose the gap; the increased total rejects a take or rolls back
 * an overdub at commit. The earlier loop and its previous undo target are retained.
 */
export function captureOverruns(): number {
  return engineState.heartbeat ? Atomics.load(engineState.heartbeat, 1) : 0;
}

// ── Non-reactive draw-loop accessors (invariant 6: no signals in the rAF loop) ───────────
/**
 * Fill `out` with track `i`'s current peak view (no allocation, no Solid subscription). `min`/
 * `max` are refs into the track's arrays — read indices `[0, out.count)` only. Safe before init
 * (returns an empty view with version -1).
 */
export function peaksInto(i: number, out: PeakView): PeakView {
  const t = engineState.tracks[i];
  if (!t) {
    out.min = null;
    out.max = null;
    out.count = 0;
    out.version = -1;
    return out;
  }
  out.min = t.peakMin;
  out.max = t.peakMax;
  out.count = t.peakCount;
  out.version = t.peakVersion;
  return out;
}

/** Plain current loop phase 0..1 (written by capture.ts's drain tick) for the draw loop. */
export function phaseValue(): number {
  return engineState.loopPhasePlain;
}

/** Plain record level 0..1 (peak-held, written by capture.ts's drain tick) for the draw loop. */
export function levelValue(): number {
  return engineState.inputLevelPlain;
}

/** Plain current state of track `i` (no signal read). */
export function stateOf(i: number): TrackState {
  return engineState.tracks[i]?.state ?? 'EMPTY';
}

/** Plain mute flag of track `i` (no signal read) for the draw loop. */
export function mutedOf(i: number): boolean {
  return engineState.tracks[i]?.muted ?? false;
}

/**
 * Plain "RECORDING but not laying down a take yet" flag of track `i` (no signal read): armed for the
 * downbeat / count-in, or AUTO LISTEN waiting for an onset. The draw loop keeps the well free of
 * rec-red while this is true, matching the ARMED/LISTENING chrome.
 */
export function waitingOf(i: number): boolean {
  const t = engineState.tracks[i];
  return !!t && (t.armed || t.autoArmed);
}

/** Plain number of frames captured so far, or 0 before the track exists. */
export function fillFramesOf(i: number): number {
  return engineState.tracks[i]?.fillFrames ?? 0;
}

/**
 * Record-head position 0..1 for a later track currently RECORDING (writeHead / master). Returns
 * -1 when the master loop isn't defined yet (first track records in grow-from-left mode, where
 * the head is simply the right edge of the drawn waveform).
 */
export function recHeadFrac(i: number): number {
  const t = engineState.tracks[i];
  if (!t || engineState.masterFramesPlain <= 0) return -1;
  // While armed (waiting for the boundary) the rec head rides the master loop phase, in sync
  // with the other tracks; once the real take starts it follows the write head.
  if (t.armed) return engineState.loopPhasePlain;
  return Math.min(1, t.writeHead / engineState.masterFramesPlain);
}

/** Plain master loop length in frames (mirror of the signal) for the draw loop. */
export function masterFramesValue(): number {
  return engineState.masterFramesPlain;
}

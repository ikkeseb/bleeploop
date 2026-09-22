import { engine } from '../engine';
import { clock } from '../clock';
import { validateFxStates, type FxState } from '../fx/metadata';
import { framesPerBar } from '../quantize';
import { init } from './capture';
import {
  engineState,
  fxVersion,
  publish,
  setMasterLengthFrames,
  sr,
  TRACK_COUNT,
} from './state';
import { recomputePeaks, resetPeaks } from './peaks';
import { makeLoopBuffer, preparePlaybackGraph, startPlayback } from './playback';
import { setMute, setVolume } from './mixer';

// ── Session import (SESSION IMPORT v0) ───────────────────────────────────────────────────
// OWNS: loading an exported session into an all-EMPTY looper. loadSession touches ONLY cross-module
// exported helpers — zero machine-private state — so the import surface lives in its own module and
// the state machine stays focused. It deliberately re-applies finishRecording's commit recipe rather
// than calling it (there is no counted downbeat to phase-preserve on an import).

/** One imported track: 0-based engine index + EXACTLY masterLengthFrames of mono PCM. */
export interface LoadSessionTrack {
  index: number;
  pcm: Float32Array;
  volume: number;
  muted: boolean;
  reversed: boolean;
  /** Missing stays compatible with older direct fixtures and normalizes to PLAYING. */
  state?: 'PLAYING' | 'STOPPED';
  fx: FxState[];
}
export interface LoadSessionPayload {
  bpm: number;
  bars: number;
  masterLengthFrames: number;
  tracks: LoadSessionTrack[];
}

/** Grid-anchor lead (s) for loadSession: far enough in the future that the lightweight scheduling
 *  pass starts every prepared track cleanly ahead of ctx.currentTime (never clamped), small enough
 *  to feel instant. Heavy PCM copies, peak scans, and AudioBuffer builds happen before the read. */
const IMPORT_START_LEAD = 0.08;

/**
 * Load a previously-exported session into an ALL-EMPTY looper: establish the master grid from the
 * imported INTEGER frame count and start every imported PLAYING track on ONE shared grid anchor;
 * STOPPED tracks restore without a source. All tracks share the same integer frame count, so they are
 * frame-identical by construction (the same invariant recording enforces). This is finishRecording's
 * commit recipe re-applied to pre-decoded PCM: same master-state writes, same masterStartTime anchoring,
 * same integer-frame-derived beat period into clock.startMasterPulse — no parallel grid math. There is
 * no counted downbeat to phase-preserve (nothing was recorded), so the anchor is simply now + a small
 * lead and every PLAYING track starts at buffer offset 0 ON that anchor. An all-STOPPED import remains
 * transport-idle; PLAY ALL later re-anchors the group from frame 0 through machine.ts.
 *
 * The core does not trust the UI: it re-validates every precondition and THROWS (descriptive Error)
 * before touching any state — import never overwrites a session in progress.
 */
export async function loadSession(payload: LoadSessionPayload): Promise<void> {
  await init();
  if (!engineState.initialized) throw new Error('loadSession: looper failed to initialize');

  // ── Preconditions — validate EVERYTHING before mutating any state ──
  const bpm = payload.bpm;
  const bars = payload.bars;
  const master = payload.masterLengthFrames;
  const tracks = payload.tracks;
  if (!Array.isArray(tracks) || tracks.length < 1) throw new Error('loadSession: payload has no tracks');
  if (!Number.isInteger(bpm) || bpm < 40 || bpm > 300) {
    throw new Error(`loadSession: bpm must be an integer in 40..300, got ${bpm}`);
  }
  if (!Number.isInteger(bars) || bars < 1) {
    throw new Error(`loadSession: bars must be a positive integer, got ${bars}`);
  }
  for (let i = 0; i < TRACK_COUNT; i++) {
    const s = engineState.tracks[i].state;
    if (s !== 'EMPTY') {
      throw new Error(`loadSession: track ${i + 1} is ${s} — import never overwrites a session; clear all tracks first`);
    }
  }
  if (!Number.isInteger(master) || master <= 0) {
    throw new Error(`loadSession: masterLengthFrames must be a positive integer, got ${master}`);
  }
  const expectedFrames = bars * framesPerBar(bpm, sr());
  if (!Number.isSafeInteger(expectedFrames) || expectedFrames !== master) {
    throw new Error(`loadSession: BPM, bars and masterLengthFrames disagree (expected ${expectedFrames}, got ${master})`);
  }
  const capacity = engineState.tracks[0].record.length;
  if (master > capacity) {
    throw new Error(`loadSession: masterLengthFrames ${master} exceeds the record buffer (${capacity} frames at this sample rate)`);
  }
  const seen = new Set<number>();
  const validatedTracks: (LoadSessionTrack & { state: 'PLAYING' | 'STOPPED' })[] = [];
  for (const s of tracks) {
    if (!Number.isInteger(s.index) || s.index < 0 || s.index >= TRACK_COUNT) {
      throw new Error(`loadSession: track index ${s.index} out of range 0..${TRACK_COUNT - 1}`);
    }
    if (seen.has(s.index)) throw new Error(`loadSession: duplicate track index ${s.index}`);
    seen.add(s.index);
    if (s.pcm.length !== master) {
      throw new Error(`loadSession: track ${s.index + 1} pcm is ${s.pcm.length} frames, expected masterLengthFrames ${master}`);
    }
    if (typeof s.reversed !== 'boolean') {
      throw new Error(`loadSession: track ${s.index + 1} reversed must be a boolean, got ${String(s.reversed)}`);
    }
    const state = s.state ?? 'PLAYING';
    if (state !== 'PLAYING' && state !== 'STOPPED') {
      throw new Error(`loadSession: track ${s.index + 1} state must be PLAYING or STOPPED, got ${String(s.state)}`);
    }
    const fx = validateFxStates(s.fx, `loadSession: track ${s.index + 1}`);
    validatedTracks.push({ ...s, state, fx });
  }

  // Allocate and fill every AudioBuffer before touching the live grid. A later allocation failure
  // must leave the all-EMPTY precondition intact so recovery can retry the same archive.
  const prepared = validatedTracks.map((s) => ({
    index: s.index,
    state: s.state,
    audioBuf: makeLoopBuffer(s.pcm, master),
  }));
  const previousBpm = clock.bpm();
  const previousTracks = validatedTracks.map((s) => {
    const t = engineState.tracks[s.index];
    return {
      index: s.index,
      volume: t.volume,
      muted: t.muted,
      reversed: t.reversed,
      fxState: t.fxState.map((f) => ({ bypassed: f.bypassed, params: { ...f.params } })),
    };
  });

  try {
    // Same transport bring-up every record gesture gets from the UI (idempotent): resume the shared
    // AudioContext + start the free-run pulse, so ctx.currentTime advances and the pulse exists for
    // startMasterPulse to re-anchor. It runs only after the complete public-payload validation above.
    clock.ensureRunning();

    // ── Tempo: adopt the session bpm, then freeze it (finishRecording's lock discipline) ──
    // The all-EMPTY precondition means no committed loop holds a lock, and no count-in is in flight
    // (that track would be RECORDING) — the unlock is belt-and-braces so setBpm below always applies.
    clock.setBpmLocked(false);
    clock.setBpm(bpm);
    clock.setBpmLocked(true);

    // ── Master state — exactly finishRecording's writes ──
    setMasterLengthFrames(master);
    engineState.masterFramesPlain = master;

    // beatPeriod exactly as finishRecording derives it, using the already-validated exported bar count
    // directly. The period itself stays integer-frame-derived and cannot disagree with the locked bpm.
    const beatPeriod = master / sr() / (4 * bars);

    // ── Heavy per-track preparation: complete every O(master) operation before reading the anchor ──
    for (const s of validatedTracks) {
      const t = engineState.tracks[s.index];
      t.record.set(s.pcm, 0); // pcm.length === master, validated above
      t.writeHead = master; // where a finished take leaves it (finishRecording parity)
      t.lengthFrames = master;
      t.fillFrames = master;
      t.reversed = s.reversed;
      // Replace the FX state with a DEEP COPY of the imported one BEFORE startPlayback — FxChain
      // builds lazily from t.fxState on first playback. If a chain already exists (this track played
      // earlier this session and was cleared; clear() keeps gain + fx alive), push the imported state
      // into the live nodes too, and bump fxVersion so the FX UI re-reads either way.
      t.fxState = s.fx.map((f) => ({ bypassed: f.bypassed, params: { ...f.params } }));
      t.fx?.setState(t.fxState);
      fxVersion[s.index][1]((v) => v + 1);
      recomputePeaks(t, master);
      // Volume/mute through the mixer's own paths (clamping, click-free live ramp, signals) — never
      // hand-rolled gain math. Applied before startPlayback so a lazily-created gain node is born at
      // the right level (startPlayback reads t.muted/t.volume on creation).
      setVolume(s.index, s.volume);
      setMute(s.index, s.muted);
      // Pay the lazy graph construction cost before reading the one shared anchor. This is required
      // even for STOPPED imports: their later PLAY ALL must not consume its 20 ms lead building five
      // cold FX chains and then clamp lanes to different ctx.currentTime values.
      preparePlaybackGraph(s.index);
    }

    // ONE shared grid anchor, read only after preparation so the 80ms lead cannot expire during PCM
    // copies/peak scans/AudioBuffer construction. All boundaries/click/LED extrapolate from this time.
    const gridAnchor = engine.ctx.currentTime + IMPORT_START_LEAD;
    engineState.masterStartTime = gridAnchor;

    // ── Lightweight scheduling pass: PLAYING tracks start at frame 0 on the same anchor; STOPPED
    //    tracks publish their restored state without ever creating or starting a source. ──
    for (const p of prepared) {
      const t = engineState.tracks[p.index];
      t.state = p.state;
      if (p.state === 'PLAYING') startPlayback(p.index, p.audioBuf, gridAnchor);
      publish(p.index);
    }

    // Pulse starts AFTER the track loop: a PLAYING publish above flips transportActive before pulseTick
    // gates the click. Starting it before the loop (with the anchor inside the lookahead) would silently
    // drop the imported session's first downbeat. All-STOPPED imports leave transportActive false; PLAY
    // ALL later re-anchors and opens the gate through the normal idle-restart path.
    clock.startMasterPulse(gridAnchor, beatPeriod);
  } catch (error) {
    // The import commit is one transaction. A graph/source failure after state writes must restore
    // the original all-EMPTY lanes so a valid recovery archive remains retryable.
    for (const before of previousTracks) {
      const t = engineState.tracks[before.index];
      for (const source of t.retiringSources) {
        try { source.stop(); } catch { /* already stopped */ }
        try { source.disconnect(); } catch { /* already disconnected */ }
      }
      t.retiringSources.clear();
      if (t.source) {
        try { t.source.stop(); } catch { /* already stopped */ }
        try { t.source.disconnect(); } catch { /* already disconnected */ }
        t.source = null;
      }
      t.record.fill(0, 0, master);
      t.stopAt = null;
      t.writeHead = 0;
      t.fillFrames = 0;
      t.lengthFrames = 0;
      t.reversed = before.reversed;
      t.state = 'EMPTY';
      t.fxState = before.fxState;
      t.fx?.setState(t.fxState);
      fxVersion[before.index][1]((v) => v + 1);
      setVolume(before.index, before.volume);
      setMute(before.index, before.muted);
      resetPeaks(t);
    }
    setMasterLengthFrames(0);
    engineState.masterFramesPlain = 0;
    engineState.masterStartTime = 0;
    engineState.loopPhasePlain = 0;
    clock.setBpmLocked(false);
    clock.setBpm(previousBpm);
    clock.stopMasterPulse();
    for (const before of previousTracks) publish(before.index);
    throw error;
  }
}

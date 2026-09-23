/**
 * OWNS: per-track playback scheduling — the looping AudioBufferSourceNode start/handoff at absolute ctx
 * times, the lazily-built per-track gain → FX → masterGain wiring, and the overdub boundary-swap timer.
 * Boundary times come from `grid-math.ts`; which transition happens is decided in `machine.ts`.
 */
import { connect as toneConnect } from 'tone';
import { engine } from '../engine';
import { clock } from '../clock';
import { framesPerBar } from '../quantize';
import { FxChain } from '../fx/fx';
import { engineState, masterLengthFrames, publish, sr } from './state';
import { nextBoundaryTime } from './grid-math';
import { recomputePeaks } from './peaks';

// A retiring source may already stop BEFORE a requested loop-end stop. Never extend its life:
// AudioScheduledSourceNode.stop replaces an earlier scheduled stop, which would overlap the swap.
const sourceStopTimes = new WeakMap<AudioBufferSourceNode, number>();

function stopSourceBy(source: AudioBufferSourceNode, when: number): void {
  const deadline = Math.min(sourceStopTimes.get(source) ?? Infinity, when);
  source.stop(deadline);
  sourceStopTimes.set(source, deadline);
}

/** Schedule silence in the audio graph. onEnded only publishes the completed transition. */
export function schedulePlaybackStop(i: number, when: number, onEnded: () => void): boolean {
  const t = engineState.tracks[i];
  const source = t.source;
  if (!source) return false;
  source.addEventListener('ended', onEnded, { once: true });
  for (const retiring of t.retiringSources) stopSourceBy(retiring, when);
  stopSourceBy(source, when);
  return true;
}

// ── AudioBuffer helpers ──────────────────────────────────────────────────────────────────
/** Build a mono AudioBuffer of exactly `frames` from `src` (frames region, from 0). */
export function makeLoopBuffer(src: Float32Array, frames: number): AudioBuffer {
  const ctx = engine.ctx;
  const buf = ctx.createBuffer(1, frames, ctx.sampleRate);
  buf.getChannelData(0).set(src.subarray(0, frames));
  return buf;
}

/**
 * Idempotently build a track's playback gain + FX graph without creating or starting a playback
 * source. Session import calls this for every prepared lane before reading its shared grid anchor:
 * otherwise an all-STOPPED import pays the lazy graph cost inside PLAY ALL after its 20 ms anchor was
 * chosen, and startPlayback's safety clamp can give later lanes a different start time.
 */
export function preparePlaybackGraph(i: number): void {
  const ctx = engine.ctx;
  const t = engineState.tracks[i];
  if (!t.gain) {
    t.gain = ctx.createGain();
    t.gain.gain.value = t.muted ? 0 : t.volume;
  }
  if (!t.fx) {
    const fx = new FxChain(t.fxState); // graph only: source scheduling stays in startPlayback
    toneConnect(t.gain, fx.input);
    t.fx = fx; // only once wired: a retry that finds t.fx set skips the connect, the lane stays silent
  }
}

/**
 * Start (or restart) a track's looping playback at absolute time `when`.
 * `offset` (seconds into the loop, default 0) lets the FIRST wrap begin part-way through the buffer so
 * the loop is phase-aligned to the count-in grid even though playback starts at an arbitrary `when`
 * (phase-preserving commit). It loops cleanly: at loopEnd the source wraps to loopStart (0), so every
 * subsequent wrap lands on the grid. Later/overdub restarts pass offset 0 (they start on a boundary).
 */
export function startPlayback(i: number, audioBuf: AudioBuffer, when: number, offset = 0): void {
  const ctx = engine.ctx;
  const t = engineState.tracks[i];
  // Build the graph BEFORE the clamp anchor is read. A cold lane constructs its whole FxChain here
  // (Tone nodes, allocation), so a `ctx.currentTime` sampled before that work is already stale by the
  // time `src.start` runs: the clamp would pass `when` through unchanged, the source would start late
  // and the correction below would be skipped — a phase hop. Reading `now` after the expensive work
  // keeps the clamp honest, so a late start is compensated in `startOffset` instead.
  preparePlaybackGraph(i);
  const startAt = Math.max(when, ctx.currentTime);
  // If `when` had to be clamped up to now, advance the offset by the same amount so the buffer position
  // that sounds at startAt stays phase-correct. Wrap into [0, duration) (offset is 0 on the boundary paths).
  const dur = audioBuf.duration;
  const startOffset = dur > 0 ? (offset + (startAt - when)) % dur : 0;
  const prev = t.source;
  const gain = t.gain;
  const fx = t.fx;
  if (!gain || !fx) throw new Error(`Track ${i + 1} playback graph was not prepared`);
  fx.setTiming({
    anchor: engineState.masterStartTime,
    beatPeriod: framesPerBar(clock.bpm(), ctx.sampleRate) / ctx.sampleRate / 4,
  });
  const src = ctx.createBufferSource();
  src.buffer = audioBuf;
  src.loop = true;
  src.loopStart = 0;
  src.loopEnd = audioBuf.duration;
  src.connect(gain);
  try {
    src.start(startAt, startOffset);
  } catch (error) {
    src.disconnect();
    throw error;
  }
  t.source = src;
  // Hand the OLD source off seamlessly: let it keep playing until exactly `startAt` (always a
  // loop boundary), then stop + free it. Stopping it immediately would leave a silent gap from
  // now until `startAt` — inaudible when `startAt ≈ now` (the overdub swap fires on the boundary)
  // but up to a FULL loop of silence for finishOverdub(), which calls this at an arbitrary phase.
  if (prev) {
    t.retiringSources.add(prev);
    const release = () => {
      t.retiringSources.delete(prev);
      try {
        prev.disconnect();
      } catch {
        /* already disconnected */
      }
    };
    prev.onended = release;
    try {
      stopSourceBy(prev, startAt);
    } catch {
      release();
    }
  }
}

// ── Boundary math ────────────────────────────────────────────────────────────────────────
/** Absolute ctx time of the next master-loop boundary at/after now. */
export function nextBoundary(): number {
  return nextBoundaryTime(engineState.masterStartTime, masterLengthFrames() / sr(), engine.ctx.currentTime);
}

/** Cancel the track's pending boundary callback when its overdub ends or playback stops. */
export function cancelOverdubSwap(i: number): void {
  const t = engineState.tracks[i];
  if (!t || t.overdubTimer === null) return;
  clearTimeout(t.overdubTimer);
  t.overdubTimer = null;
}

/**
 * At the next loop boundary, commit the summed overdub layer: copy overdubBuf back into the
 * record buffer and swap the playing source for a fresh AudioBuffer started exactly on the
 * boundary with matching phase, so there is no phase hop. The swap has no crossfade, so differing
 * samples at the crossover can still step. Re-arms itself each boundary while still OVERDUBBING.
 *
 * Each track owns one timeout. Replacing it cancels the previous callback; capture release and
 * PLAY/STOP cancel it too. A completed session leaves no callback waiting for its old boundary.
 *
 * `boundary` is the absolute ctx time of the boundary THIS firing commits at. The re-arm passes the
 * next boundary explicitly (anchor-derived, so no float accumulation) instead of recomputing
 * nextBoundary() at callback time: the wall-clock timer can fire while ctx.currentTime (which advances
 * in render quanta) still reads a hair BEFORE the boundary, and a recompute would then return the SAME
 * boundary — a duplicate swap whose double-buffer pick would write into the AudioBuffer the outgoing
 * source is still reading (the alias-safety argument below assumes one firing per boundary).
 *
 * A swap that throws from the commit onward (record copy, peaks, buffer fill, source start) is logged here
 * and handed to `onSwapFailed` (machine.ts decides the transition); it never re-arms, so a failed boundary
 * cannot leave the lane OVERDUBBING with no swap left. The layer stays in `overdubBuf`, so STOP commits it.
 */
export function scheduleOverdubSwap(i: number, onSwapFailed: (error: unknown) => void, boundary?: number): void {
  cancelOverdubSwap(i);
  const t = engineState.tracks[i];
  const master = masterLengthFrames();
  const when = boundary ?? nextBoundary();
  const delayMs = Math.max(0, (when - engine.ctx.currentTime) * 1000);
  const timer = setTimeout(() => {
    if (t.overdubTimer !== timer) return;
    t.overdubTimer = null;
    if (t.state !== 'OVERDUBBING' || !t.overdubBuf) return;
    try {
      // Commit summed layer into the record buffer.
      t.record.set(t.overdubBuf.subarray(0, master), 0);
      recomputePeaks(t, master);
      // Double-buffer the playback AudioBuffer: write the summed loop into whichever of the two buffers is
      // NOT live. The buffer picked here was last handed to a source two boundaries ago, and that source was
      // stopped one boundary ago (startPlayback's prev.stop(when)) — a full loop period before now — so no
      // active source is reading it. This avoids a fresh ~master-length AudioBuffer every loop period.
      let audioBuf: AudioBuffer;
      if (t.overdubSwapBufs) {
        audioBuf = t.overdubSwapBufs[t.overdubSwapIdx];
        audioBuf.getChannelData(0).set(t.record.subarray(0, master));
        t.overdubSwapIdx ^= 1;
      } else {
        audioBuf = makeLoopBuffer(t.record, master);
      }
      startPlayback(i, audioBuf, when);
    } catch (error) {
      console.error(`[looper] track ${i + 1}: overdub boundary swap failed`, error);
      onSwapFailed(error);
      return;
    }
    // No refill needed: line "t.record.set(t.overdubBuf…)" above made record and overdubBuf
    // byte-identical, and nothing in between mutates either — the working copy already holds exactly
    // the committed content, so the layer keeps accumulating into it correctly.
    publish(i);
    // Re-arm for the scheduled successor in the normal case; after a stall, skip every missed boundary
    // and target the first future one. Both paths stay derived from the master anchor.
    if (engineState.tracks[i].state === 'OVERDUBBING') {
      const period = master / sr();
      const scheduledNext = Math.round((when - engineState.masterStartTime) / period) + 1;
      const firstFuture = Math.floor((engine.ctx.currentTime - engineState.masterStartTime) / period) + 1;
      const n = Math.max(scheduledNext, firstFuture);
      scheduleOverdubSwap(i, onSwapFailed, engineState.masterStartTime + n * period);
    }
  }, delayMs);
  t.overdubTimer = timer;
}

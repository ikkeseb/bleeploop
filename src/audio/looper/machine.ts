/**
 * OWNS: the track state machine — every EMPTY → RECORDING → PLAYING ⇄ OVERDUBBING (+ STOPPED) transition,
 * the single-recorder slot, and the master-loop decisions: MASTER LENGTH is decided in `finishRecording`
 * (first take, via grid-math `planCommit`) and `stopCapture` (the wall-clock bar target, `planFreeStop`);
 * BPM IS LOCKED at the count-in press in `startRecording` (`beginAutoRecording` for AUTO) and re-asserted at
 * commit, unlocked only by `releaseRecorderState` (abort) / `resetMaster`; record-latency compensation C is
 * APPLIED as absolute capture windows in `startRecording`, `beginAutoRecording` and `startOverdub`;
 * `stopCapture` tightens their end, and `finishCapture` owns completion/cleanup. RETAKE lives here too:
 * `completeRetakePass` slides a rolling take one pass forward, `stopCapture` decides which pass is kept
 * (grid-math `planRetakeStop`), and `finishCapture` hands the recorder to the lane whose REC approved. Capture
 * maps timestamped overdub samples back onto the uncompensated master grid.
 * The pure grid arithmetic lives in `grid-math.ts`; capture-side frame handling in `capture.ts`.
 */
import { notifyError } from '../../notify';
import { engine } from '../engine';
import { clock } from '../clock';
import { defaultFxStates } from '../fx/fx';
import { clampBars, framesPerBar, maxWholeBars } from '../quantize';
import {
  commitAnchor,
  countInArm,
  nextBoundaryTime,
  phaseOffset,
  planCommit,
  planFreeStop,
  planRetakeStop,
  type RetakeStop,
} from './grid-math';
import { recordCompensationFrames } from '../record-latency';
import { pluginBridge, type PluginRecordLossSnapshot } from '../plugin-bridge';
import { cancelAutoRecord, drainStaleFrames, flushActiveCapture, init, prepareAutoRecord } from './capture';
import {
  autoRecordEnabled,
  autoRecordSensitivity,
  captureOverruns,
  COUNT_IN_BEATS,
  engineState,
  fixedLengthBars,
  fixedLengthEnabled,
  fxVersion,
  HEARTBEAT_INTERNAL_LATENCY,
  MAX_FIXED_BARS,
  masterLengthFrames,
  loopEndStopEnabled,
  muteSignals,
  publish,
  retakeEnabled,
  setAutoRecordEnabledSignal,
  setAutoRecordSensitivitySignal,
  setFixedLengthBarsSignal,
  setFixedLengthEnabledSignal,
  setMasterLengthFrames,
  sr,
  TRACK_COUNT,
  type Track,
  volumeSignals,
} from './state';
import { recomputePeaks, resetPeaks } from './peaks';
import {
  cancelOverdubSwap,
  makeLoopBuffer,
  nextBoundary,
  scheduleOverdubSwap,
  schedulePlaybackStop,
  startPlayback,
} from './playback';
import { applyTrackGain, setMute, setVolume } from './mixer';

/**
 * ctx.currentTime of the FIRST track's take frame 0 = the counted come-in downbeat (recordStart in
 * startRecording). The whole loop grid (loop-audio wraps + the metronome click + beat-LED) is phase-
 * anchored to THIS, not to the commit instant — so the looper click stays on the SAME grid as the
 * count-in click with no phase hop at commit. Set on first-track record press, read in finishRecording
 * (the phase-preserving anchor). 0 = no counted take in flight.
 */
let firstTakeDownbeatCtx = 0;
/**
 * Capture-loss total at each record or overdub arm. Timestamped packets expose gaps but cannot
 * recover missing audio. A larger counter at commit rejects the take or restores the pre-dub loop.
 * This conservative check also counts losses during discarded pre-roll.
 */
let armOverrunBaseline = 0;
/** Plugin PCM loss totals at the current take/layer's arm. Both capture and plugin loss reject
 * the take: a clean native monitor does not establish that recordTap was continuous. */
let armPluginLossBaseline: PluginRecordLossSnapshot = { droppedFrames: 0, underruns: 0 };
/** RETAKE: the loss counters are sampled per drain, so a loss seen when a pass completes cannot be placed
 * on one side of the pass edge. It discards that pass AND taints the next one. */
let retakeTainted = false;
/** RETAKE: the lane whose REC press approved the rolling take; it records next. -1 = none. */
let handoffLane = -1;
// ── Master-loop derivation ───────────────────────────────────────────────────────────────
function pluginLossSinceArm(): PluginRecordLossSnapshot {
  const now = pluginBridge.recordLossSnapshot();
  return {
    droppedFrames: Math.max(0, now.droppedFrames - armPluginLossBaseline.droppedFrames),
    underruns: Math.max(0, now.underruns - armPluginLossBaseline.underruns),
  };
}

/** Refresh after a fully discarded pre-roll batch; the straddling frame-0 batch is never excluded. */
export function beginPluginRecordIntegrityWindow(): void {
  armPluginLossBaseline = pluginBridge.recordLossSnapshot();
}

/** AUTO may listen for minutes. Discarded quiet history must not poison the eventual take. */
export function refreshAutoRecordIntegrityWindow(): void {
  armOverrunBaseline = captureOverruns();
  beginPluginRecordIntegrityWindow();
}

function describePluginLoss(loss: PluginRecordLossSnapshot): string {
  const parts: string[] = [];
  if (loss.droppedFrames > 0) parts.push(`${loss.droppedFrames} bridge frames dropped`);
  if (loss.underruns > 0) parts.push(`${loss.underruns} silent worklet underruns`);
  return parts.join(' and ');
}

/**
 * Reject a take/layer if either timestamped capture or the native-plugin PCM bridge lost audio after arm. A clean native monitor
 * does not prove the record branch was continuous: the monitor bypasses this JS bridge entirely.
 */
/** What the record path lost since the integrity window opened; '' = the window is clean. */
function recordLossDetail(): string {
  const captureDropped = captureOverruns() - armOverrunBaseline;
  return [
    describePluginLoss(pluginLossSinceArm()),
    captureDropped > 0 ? `${captureDropped} capture frames dropped` : '',
  ].filter(Boolean).join(' and ');
}

function rejectRecordLoss(i: number): boolean {
  const detail = recordLossDetail() || (retakeTainted ? 'an interruption at the edge of this pass' : '');
  if (!detail) return false;

  const t = engineState.tracks[i];
  const overdub = t.state === 'OVERDUBBING';
  console.error(`[looper] rejected track ${i}'s ${overdub ? 'overdub layer' : 'take'}: ${detail}`);
  notifyError(
    `Track ${i + 1}: ${overdub ? 'overdub layer' : 'take'} discarded`,
    `Recorded audio was interrupted (${detail}). No damaged audio was kept; try again.`,
  );

  if (overdub) {
    const master = masterLengthFrames();
    if (t.undoBuf) t.record.set(t.undoBuf.subarray(0, master), 0); // restore the pre-layer loop
    t.undoBuf = t.overdubPreviousUndoBuf;
    t.undoBufReversed = t.overdubPreviousUndoBufReversed;
    t.overdubPreviousUndoBuf = null;
    t.overdubPreviousUndoBufReversed = false;
    t.overdubBuf = null;
    t.overdubSwapBufs = null;
    recomputePeaks(t, master);
    const stopAfter = engineState.captureStopPlayback;
    t.state = stopAfter ? 'STOPPED' : 'PLAYING';
    if (!stopAfter) startPlayback(i, makeLoopBuffer(t.record, master), nextBoundary());
    return true;
  }

  const wasFirstTake = masterLengthFrames() === 0;
  t.armed = false;
  t.record.fill(0);
  t.writeHead = 0;
  t.fillFrames = 0;
  t.lengthFrames = 0;
  resetPeaks(t);
  t.state = 'EMPTY';
  if (wasFirstTake) clock.stopCountIn();
  return true;
}

/**
 * Report graph construction or source-scheduling failures at commit and buffer-swap sites. The
 * callers still release the recorder and publish state in their cleanup paths, so a failed playback
 * transition cannot leave the recording slot claimed. Export owns a separate context and no longer
 * redirects live node construction into its offline graph.
 *
 * `console.error`, not `console.warn`: logging.ts pipes only console.error into the release log file.
 */
function reportCommitPlaybackFailure(i: number, err: unknown, kind: 'commit' | 'swap' = 'commit'): void {
  if (kind === 'swap') {
    // undo/reverse: the buffer edit is in place, only the live-source swap failed — the loop keeps playing
    // the pre-edit audio. STOP + PLAY restarts from the edited buffer.
    console.error(`[looper] track ${i}: playback failed to restart after a buffer edit`, err);
    notifyError(
      `Track ${i + 1}: playback failed to restart`,
      'The edit was applied to the loop but playback could not switch to it — stop and play the track again.',
    );
    return;
  }
  console.error(`[looper] track ${i}: playback failed to start at commit — the loop is retained`, err);
  notifyError(
    `Track ${i + 1}: the take failed to start`,
    'The loop was saved, but playback could not start. Press play to retry.',
  );
}

/** Commit a first or later take. Only the first take chooses a length and anchors the grid. */
function finishRecording(i: number): void {
  const t = engineState.tracks[i];
  // Keep the unpadded length: a short first take waits for the next counted downbeat.
  const raw = Math.min(t.writeHead, (engineState.captureEndFrame ?? Infinity) - (engineState.captureStartFrame ?? 0));
  let master = masterLengthFrames();
  let when = engine.ctx.currentTime + HEARTBEAT_INTERNAL_LATENCY;
  let offset = 0;
  let firstBeatPeriod = 0;
  if (master === 0) {
    const plan = planCommit(raw, clock.bpm(), sr(), t.record.length);
    master = plan.master;
    const timing = commitAnchor(firstTakeDownbeatCtx, master, sr(), when, raw);
    when = timing.playWhen;
    offset = timing.startOffset;
    clock.setBpm(plan.derivedBpm);
    clock.setBpmLocked(true);
    setMasterLengthFrames(master);
    engineState.masterFramesPlain = master;
    engineState.masterStartTime = timing.gridAnchor;
    firstBeatPeriod = plan.beatPeriod;
  } else {
    offset = phaseOffset(when, engineState.masterStartTime, master / sr());
  }
  if (raw < master) t.record.fill(0, raw, master);
  t.writeHead = master;
  t.lengthFrames = master;
  t.fillFrames = master;
  t.armed = false;
  t.state = engineState.captureStopPlayback ? 'STOPPED' : 'PLAYING';
  if (firstBeatPeriod > 0) clock.startMasterPulse(engineState.masterStartTime, firstBeatPeriod);
  recomputePeaks(t, master);
  if (t.state === 'PLAYING') startPlayback(i, makeLoopBuffer(t.record, master), when, offset);
}

/** Every completed window releases its owner and publishes once, including loss and playback failure. */
export function finishCapture(i: number): void {
  if (engineState.activeRecordIndex !== i) return;
  const t = engineState.tracks[i];
  const next = handoffLane;
  const seam = engineState.captureEndFrame; // an approved retake ends ON a pass edge (= a master boundary + C)
  handoffLane = -1;
  try {
    if (rejectRecordLoss(i)) return;
    if (t.state === 'OVERDUBBING') finishOverdub(i);
    else finishRecording(i);
  } catch (e) {
    stopAndFreeSource(t);
    t.state = t.lengthFrames > 0 ? 'STOPPED' : 'EMPTY';
    reportCommitPlaybackFailure(i, e);
  } finally {
    releaseRecorderState(i);
    resetMasterIfBlank();
    publish(i);
    // RETAKE handoff: only a take that actually committed passes the recorder on.
    if (next >= 0 && t.state === 'PLAYING' && seam !== null) startRecording(next, seam);
  }
}

/** Enable/disable fixed-length record mode (governs the first-track record only). */
function setFixedLengthEnabled(on: boolean): void {
  setFixedLengthEnabledSignal(on);
}

/** Set the fixed-length bar count, clamped to [1, MAX_FIXED_BARS] and rounded to a whole number. */
function setFixedLengthBars(n: number): void {
  setFixedLengthBarsSignal(Math.max(1, Math.min(MAX_FIXED_BARS, Math.round(n))));
}

/** Enable/disable first-track AUTO REC. Off by default; later-track arms never read this flag. */
function setAutoRecordEnabled(on: boolean): void {
  setAutoRecordEnabledSignal(on);
}

/** Set AUTO sensitivity on the product-facing 1..100 integer scale. */
function setAutoRecordSensitivity(n: number): void {
  const finite = Number.isFinite(n) ? n : autoRecordSensitivity();
  setAutoRecordSensitivitySignal(Math.max(1, Math.min(100, Math.round(finite))));
}

/** Bound a take at its existing master, FIXED bar count, or the free-record capacity. */
function configureRecordingEnd(t: Track): void {
  let frames = masterLengthFrames() || t.record.length;
  if (masterLengthFrames() === 0 && fixedLengthEnabled()) {
    const fpb = framesPerBar(clock.bpm(), sr());
    frames = clampBars(fixedLengthBars(), maxWholeBars(t.record.length, fpb)) * fpb;
  }
  engineState.captureEndFrame = engineState.captureStartFrame! + frames;
  // RETAKE rolls any take whose length is decided here (a free first take is bounded only by capacity).
  if (retakeEnabled() && (masterLengthFrames() > 0 || fixedLengthEnabled())) {
    engineState.retakeRolling = true;
    engineState.retakeBuf = new Float32Array(frames); // allocated at arm, never in the drain
    engineState.retakeKept = false;
    engineState.retakePass = 1;
    retakeTainted = false;
  }
}

/**
 * RETAKE: the rolling take reached its window end. Set the finished pass aside (a pass that lost audio is
 * dropped, and takes the older kept pass with it — approving must never hand back a pass the player did
 * not just hear themselves play) and slide the whole take one pass forward: the capture window, the
 * first-take phase anchor and the integrity window all move by exactly one pass, so every later decision
 * (`stopCapture`, `finishRecording`) sees an ordinary take that began at this pass's downbeat.
 */
export function completeRetakePass(i: number): void {
  const t = engineState.tracks[i];
  const frames = engineState.captureEndFrame! - engineState.captureStartFrame!;
  const detail = recordLossDetail();
  engineState.retakeKept = !detail && !retakeTainted;
  if (engineState.retakeKept) engineState.retakeBuf!.set(t.record.subarray(0, frames));
  if (detail) {
    console.error(`[looper] retake: dropped track ${i}'s pass ${engineState.retakePass}: ${detail}`);
    notifyError(
      `Track ${i + 1}: pass ${engineState.retakePass} dropped`,
      `Recorded audio was interrupted (${detail}). This pass and the next are skipped — keep playing.`,
    );
  }
  retakeTainted = !!detail; // the loss cannot be placed on one side of the edge: the next pass is skipped too
  armOverrunBaseline = captureOverruns();
  beginPluginRecordIntegrityWindow();
  engineState.captureStartFrame = engineState.captureEndFrame;
  engineState.captureEndFrame = engineState.captureStartFrame! + frames;
  if (firstTakeDownbeatCtx > 0) firstTakeDownbeatCtx += frames / sr();
  engineState.retakePass++;
  t.writeHead = 0;
  t.fillFrames = 0;
  resetPeaks(t);
  publish(i);
}

/**
 * AUTO detector callback: the retained onset is already in t.record[0..writeHead). Anchor the grid to
 * when that guitar onset was performed. Native wet reached recordTap C frames later than the monitored
 * performance, so subtract C from the captured timeline instead of discarding onset audio.
 */
export function beginAutoRecording(i: number, capturedStartCtx: number): void {
  const t = engineState.tracks[i];
  if (!t || !t.autoArmed || masterLengthFrames() !== 0) return;
  engineState.captureCompensationFrames = recordCompensationFrames();
  const compensationSec = engineState.captureCompensationFrames / sr();
  firstTakeDownbeatCtx = Math.max(0, capturedStartCtx - compensationSec);
  engineState.captureStartFrame = Math.round(capturedStartCtx * sr());
  t.autoArmed = false;
  clock.setBpmLocked(true);
  configureRecordingEnd(t);
  publish(i); // makes transportActive true before the pulse schedules a sounding future beat
  clock.startAutoRecordPulse(firstTakeDownbeatCtx, 60 / clock.bpm());
}

// ── Shared teardown helpers ──────────────────────────────────────────────────────────────
/**
 * Release the single-recorder slot + every piece of cross-cutting arm state a torn-down capture
 * must not leak into the next record: the absolute capture window, its diagnostic remaining count,
 * compensation and pending playback-stop intent, plus the first-take anchor and count-in BPM lock.
 * Callers keep their own state-transition specifics (EMPTY-vs-STOPPED, source teardown, buffer
 * wipe, count-pulse teardown). Only the owner may release the shared state.
 */
function releaseRecorderState(i: number): void {
  if (engineState.activeRecordIndex !== i) return;
  cancelOverdubSwap(i);
  const t = engineState.tracks[i];
  const wasAutoArmed = t?.autoArmed === true;
  engineState.activeRecordIndex = -1;
  if (t) t.autoArmed = false;
  if (wasAutoArmed) cancelAutoRecord();
  engineState.pendingRecordStartFrame = 0;
  engineState.captureStartFrame = null;
  engineState.captureEndFrame = null;
  engineState.captureCompensationFrames = 0;
  engineState.captureStopPlayback = false;
  engineState.retakeRolling = false;
  engineState.retakeBuf = null;
  engineState.retakeKept = false;
  engineState.retakePass = 0;
  retakeTainted = false;
  handoffLane = -1;
  firstTakeDownbeatCtx = 0;
  if (masterLengthFrames() === 0) clock.setBpmLocked(false);
}

/** Stop, disconnect and drop a track's live playback source (idempotent; safe when none). */
function stopAndFreeSource(t: Track): void {
  for (const src of t.retiringSources) {
    try {
      src.stop();
    } catch {
      /* already stopped */
    }
    try {
      src.disconnect();
    } catch {
      /* already disconnected */
    }
  }
  t.retiringSources.clear();
  if (!t.source) return;
  try {
    t.source.stop();
  } catch {
    /* already stopped */
  }
  t.source.disconnect();
  t.source = null;
}

// ── Public per-track actions ─────────────────────────────────────────────────────────────
/**
 * `[REC/DUB]` button for track `i` — the record/overdub job only.
 *   EMPTY        -> start recording (track 1 defines the downbeat; later tracks arm to the boundary)
 *   RECORDING    -> commit, KEEP the take -> PLAYING (track 1 also defines master length)
 *   PLAYING      -> start overdub on the next boundary
 *   OVERDUBBING  -> commit the layer -> PLAYING
 *   STOPPED      -> no-op (the UI disables this button in STOPPED; resume via the play button — a)
 */
async function recDub(i: number): Promise<void> {
  await init();
  if (!engineState.initialized) return;
  if (engineState.tracks[i]?.stopAt !== null) return;
  switch (engineState.tracks[i].state) {
    case 'EMPTY': {
      // RETAKE: REC on another lane approves the rolling take; this lane then records from that take's
      // pass end (finishCapture hands over). With nothing to keep yet the press is ignored, as it is for
      // any second recorder.
      const active = engineState.activeRecordIndex;
      if (active >= 0 && engineState.retakeRolling && !engineState.tracks[active].armed) {
        if (retakeStopPlan() === 'stop-now') break;
        handoffLane = i;
        stopCapture(active);
      } else startRecording(i);
      break;
    }
    case 'RECORDING':
      stopCapture(i);
      break;
    case 'PLAYING':
      startOverdub(i);
      break;
    case 'OVERDUBBING':
      stopCapture(i);
      break;
    case 'STOPPED':
      break; // disabled in UI
  }
}

/**
 * `[▶/■]` button for track `i` — the audibility job only.
 *   EMPTY        -> no-op (disabled in UI)
 *   RECORDING    -> commit + stop -> STOPPED  (the take is KEPT)
 *   OVERDUBBING  -> commit + stop -> STOPPED
 *   PLAYING      -> stop -> STOPPED (or schedule the loop-end stop when enabled)
 *   STOPPED      -> resume -> PLAYING
 */
function playStop(i: number): void {
  const t = engineState.tracks[i];
  if (!t) return;
  if (t.stopAt !== null) {
    // A second press forces silence now. If the audio deadline already passed but onended has
    // not reached the main thread, finish the old transition first and treat this press as PLAY.
    const elapsed = engine.ctx.currentTime >= t.stopAt;
    stop(i);
    if (elapsed) resume(i);
    return;
  }
  switch (t.state) {
    case 'EMPTY':
      break; // disabled in UI
    case 'RECORDING':
    case 'OVERDUBBING':
      engineState.captureStopPlayback = true;
      if (t.state === 'OVERDUBBING') {
        cancelOverdubSwap(i); // A boundary swap must not restart sound during the capture tail.
        stopAndFreeSource(t);
      }
      stopCapture(i);
      publish(i);
      break;
    case 'PLAYING':
      if (loopEndStopEnabled()) requestLoopEndStop(i);
      else stop(i);
      break;
    case 'STOPPED':
      resume(i);
      break;
  }
}

/** PLAYING stays audible until its next loop edge. Capture commands retain their commit behavior. */
function requestLoopEndStop(i: number, when = nextBoundary()): void {
  const t = engineState.tracks[i];
  const source = t.source;
  t.stopAt = when;
  const scheduled = schedulePlaybackStop(i, when, () => {
    // CLEAR, immediate STOP or a newer playback source makes this completion obsolete.
    if (t.stopAt !== when || t.source !== source) return;
    stop(i);
  });
  if (!scheduled) stop(i);
  else publish(i);
}

/** `seamFrame`: a RETAKE handoff — begin exactly where the approved take's pass ended (a later take). */
function startRecording(i: number, seamFrame: number | null = null): void {
  if (engineState.activeRecordIndex >= 0) return; // single-recorder v1: ignore if another is recording
  const t = engineState.tracks[i];
  t.writeHead = 0;
  t.fillFrames = 0;
  t.armed = false;
  t.autoArmed = false;
  resetPeaks(t);
  // Capture begins clean from this press. A handoff keeps the ring: its seam may sit inside the batch
  // being drained right now, and the timestamped arm split discards everything before it anyway.
  if (seamFrame === null) drainStaleFrames();
  engineState.activeRecordIndex = i;
  armOverrunBaseline = captureOverruns(); // detect a capture drop between here and commit
  beginPluginRecordIntegrityWindow();
  engineState.captureStartFrame = null;
  engineState.captureEndFrame = null;
  engineState.captureCompensationFrames = 0;
  engineState.captureStopPlayback = false;

  if (masterLengthFrames() === 0) {
    if (autoRecordEnabled()) {
      // AUTO REC replaces only the first-track count-in. The capture ring keeps draining silence while
      // the pre-allocated detector listens; no click sounds and no BPM/length choice freezes until input
      // actually triggers. The same REC/DUB or PLAY/STOP gesture cancels through stopCapture().
      prepareAutoRecord();
      engineState.pendingRecordStartFrame = 0;
      firstTakeDownbeatCtx = 0;
      t.autoArmed = true;
      t.state = 'RECORDING';
      publish(i);
      return;
    }
    // FIRST track: COUNT IN one bar, then capture frame-exact on the downbeat AFTER the count, so the
    // loop's "1" is a counted beat (not the button-press moment) and the head holds no recorded dead
    // air. Same frame-exact arm the later tracks use, applied to the grid-defining first track: arm
    // now, discard exactly the lead-in + count frames in consume(), begin the take at frame 0 on the
    // straddling batch. The count clicks (forced audible) + LED ride a ctx pulse anchored to the count
    // (clock.startCountIn); finishRecording re-anchors that same pulse to the quantized grid at stop.
    // Anchor the count-in at now + lead (snappiest possible start) for BOTH metronome states: the idle
    // free-run grid is SILENT (the click is a transport mode — clock.setTransportActive), so there is no
    // audible grid to stay in phase with and grid-anchoring would only delay the count by up to a beat.
    // The count's accent on beat 0 announces itself ("ONE-two-three-four"); the loop downbeat (recordStart =
    // anchor + one bar) anchors the master pulse at commit. Math: grid-math.ts countInArm.
    const { beatPeriod, anchor, recordStart, pendingFrames } = countInArm(engine.ctx.currentTime, clock.bpm(), sr());
    clock.startCountIn(anchor, beatPeriod, COUNT_IN_BEATS);
    clock.setBpmLocked(true);
    t.armed = true; // counting in; the take (and waveform) begins at the come-in downbeat
    t.state = 'RECORDING';
    firstTakeDownbeatCtx = recordStart; // the counted downbeat the loop grid is phase-anchored to at commit
    // Discard the lead-in + count frames, PLUS the record-latency compensation C: the natively-monitored
    // guitar's recorded transient lands C frames late at the record tap, so starting the take C frames
    // later puts that transient ON frame 0 (= the counted downbeat the grid anchors to ⇒ on playback the
    // guitar lands on the click). C is 0 unless a native monitor is armed, so the synth/mic + verified
    // baselines are untouched; the whole capture window shifts uniformly so the loop length is unchanged.
    // firstTakeDownbeatCtx STAYS at recordStart (the heard click/grid anchor), NOT the shifted take start.
    engineState.captureCompensationFrames = recordCompensationFrames();
    engineState.captureStartFrame = Math.round(recordStart * sr()) + engineState.captureCompensationFrames;
    engineState.pendingRecordStartFrame = pendingFrames + engineState.captureCompensationFrames;
  } else {
    // LATER track: choose one absolute master boundary. Capture compares packet timestamps
    // with that deadline, so producer progress during this gesture cannot shift the take.
    t.armed = true; // waiting for the boundary; waveform/playhead suppressed until then
    t.state = 'RECORDING';
    // + record-latency compensation C (same as the first-track arm): the take begins C frames after the
    // boundary so the late wet transient lands on the take's frame 0, which plays back ON the master grid
    // boundary (= track-1 frame 0 = the click). C is 0 unless a native monitor is armed; loop length is
    // master frames either way (the window shifts uniformly).
    const now = engine.ctx.currentTime;
    const boundary = nextBoundaryTime(engineState.masterStartTime, masterLengthFrames() / sr(), now);
    engineState.captureCompensationFrames = recordCompensationFrames();
    engineState.captureStartFrame = seamFrame ?? Math.round(boundary * sr()) + engineState.captureCompensationFrames;
    engineState.pendingRecordStartFrame = Math.max(0, engineState.captureStartFrame - Math.round(now * sr()));
  }
  configureRecordingEnd(t);
  publish(i);
}

/** How a stop gesture pressed NOW resolves the rolling retake (pure rule: grid-math `planRetakeStop`). */
function retakeStopPlan(): RetakeStop {
  const press = Math.round(engine.ctx.currentTime * sr()) + engineState.captureCompensationFrames;
  return planRetakeStop(press, engineState.captureEndFrame!, framesPerBar(clock.bpm(), sr()), engineState.retakeKept);
}

/** Tighten the capture window once; repeated gestures cannot extend a take or layer. */
function stopCapture(i: number): void {
  const t = engineState.tracks[i];
  if (t.autoArmed || t.armed) {
    stop(i); // Nothing retained yet: cancel count-in, boundary arm or AUTO listening.
    return;
  }
  const now = engine.ctx.currentTime;
  let end = Math.round(now * sr()) + engineState.captureCompensationFrames;
  if (engineState.retakeRolling) {
    // The roll ends with this gesture, whichever way it resolves (grid-math `planRetakeStop`).
    engineState.retakeRolling = false;
    const passEnd = engineState.captureEndFrame!;
    const plan = retakeStopPlan();
    if (plan === 'keep-last') {
      // The kept pass replaces the one in flight; that pass's losses die with it.
      t.record.set(engineState.retakeBuf!);
      t.writeHead = passEnd - engineState.captureStartFrame!;
      armOverrunBaseline = captureOverruns();
      beginPluginRecordIntegrityWindow();
      retakeTainted = false;
      finishCapture(i);
      return;
    }
    if (plan === 'finish-pass') end = passEnd; // runs to its own end, then commits as an ordinary take
  }
  if (t.state === 'RECORDING' && masterLengthFrames() === 0 && end !== engineState.captureEndFrame) {
    // Whole bars come from musical time, including the quarter-beat grace. A shorter take
    // retains audio through the press and is padded to one bar only after its tail arrives.
    const elapsed = firstTakeDownbeatCtx > 0 ? now - firstTakeDownbeatCtx : 0;
    const { bars, target } = planFreeStop(elapsed, clock.bpm(), sr(), t.record.length);
    if (bars >= 1) end = engineState.captureStartFrame! + target;
  }
  engineState.captureEndFrame = Math.min(engineState.captureEndFrame ?? Infinity, end);
  flushActiveCapture();
  if (engineState.activeRecordIndex === i && engineState.captureFrontierFrame >= engineState.captureEndFrame!) {
    finishCapture(i);
  }
}

function startOverdub(i: number): void {
  if (engineState.activeRecordIndex >= 0) return; // only one recorder at a time
  const t = engineState.tracks[i];
  if (t.reversed) return; // RC-505: reverse BLOCKS overdub — flip back to forward first
  const master = masterLengthFrames();
  if (master === 0) return;
  drainStaleFrames();
  armOverrunBaseline = captureOverruns();
  beginPluginRecordIntegrityWindow();
  const punchInFrame = Math.round(engine.ctx.currentTime * sr());
  const compensation = recordCompensationFrames();
  // Work on a summed copy of the current loop. Kept for the whole session (each boundary swap commits
  // it into `record` and keeps accumulating into the same copy), so the per-period allocation is gone.
  t.overdubBuf = t.record.slice(0, master);
  // Pre-allocate the two playback AudioBuffers the boundary swap alternates between (double-buffer), so
  // scheduleOverdubSwap writes into an existing buffer instead of minting a fresh one every loop period.
  const octx = engine.ctx;
  t.overdubSwapBufs = [octx.createBuffer(1, master, octx.sampleRate), octx.createBuffer(1, master, octx.sampleRate)];
  t.overdubSwapIdx = 0;
  // Snapshot the pre-dub loop for one-level undo — a SEPARATE copy (overdubBuf gets summed into). Taken
  // here, before the first scheduleOverdubSwap commits the summed layer back into `record`.
  t.overdubPreviousUndoBuf = t.undoBuf;
  t.overdubPreviousUndoBufReversed = t.undoBufReversed;
  t.undoBuf = t.record.slice(0, master);
  // Pair the snapshot's orientation so undo/redo keeps reversed honest.
  t.undoBufReversed = t.reversed;
  t.state = 'OVERDUBBING';
  engineState.activeRecordIndex = i;
  // Keep only audio performed after this punch-in. Wet arriving before punchIn+C belongs to
  // earlier playing; capture maps retained packet frames back by C onto the master grid.
  const gridFrame = Math.round(engineState.masterStartTime * sr());
  t.writeHead = ((punchInFrame - gridFrame) % master + master) % master;
  engineState.captureStartFrame = punchInFrame + compensation;
  engineState.captureEndFrame = null;
  engineState.captureCompensationFrames = compensation;
  engineState.captureStopPlayback = false;
  scheduleOverdubSwap(i);
  publish(i);
}

/** Called by timestamped capture after the complete compensated punch-out window has arrived. */
function finishOverdub(i: number): void {
  const t = engineState.tracks[i];
  const master = masterLengthFrames();
  if (t.overdubBuf) {
    t.record.set(t.overdubBuf.subarray(0, master), 0);
    t.overdubBuf = null;
  }
  t.overdubSwapBufs = null; // session over: release the double-buffer (the live source keeps its own ref)
  t.overdubPreviousUndoBuf = null; // the new successful overdub supersedes the older undo target
  t.overdubPreviousUndoBufReversed = false;
  recomputePeaks(t, master);
  const stopAfter = engineState.captureStopPlayback;
  t.state = stopAfter ? 'STOPPED' : 'PLAYING';
  // The final summed loop becomes audible on the next boundary.
  if (!stopAfter) startPlayback(i, makeLoopBuffer(t.record, master), nextBoundary());
}

/**
 * One-level UNDO/REDO of the last overdub. Swaps `record` with the pre-dub snapshot in `undoBuf` (so a
 * second call redoes), recomputes peaks, and — if PLAYING — swaps the live source for a fresh buffer on
 * the next loop boundary (the same sample-aligned swap finishOverdub uses, so no click and no grid drift; the
 * loop stays frame-identical to master). If STOPPED, the buffer is swapped in place and `resume` will play
 * it. No-op unless the track is PLAYING/STOPPED with an undo buffer available. Does not capture, so it's
 * safe while another track records. Also swaps `reversed`<->`undoBufReversed` so the displayed orientation
 * always matches the buffer now playing (a reverse between the dub and the undo must not leave the cap
 * lying about direction).
 */
function undoLastOverdub(i: number): void {
  const t = engineState.tracks[i];
  if (!t || !t.undoBuf) return;
  if (t.stopAt !== null) return;
  if (t.state !== 'PLAYING' && t.state !== 'STOPPED') return;
  const master = masterLengthFrames();
  if (master === 0) return;
  // Swap record <-> undoBuf (toggle); frame count is preserved either way.
  const prev = t.record.slice(0, master);
  t.record.set(t.undoBuf.subarray(0, master), 0);
  t.undoBuf = prev;
  // Swap orientation in lockstep so reversed tracks the now-active buffer (undo after a reverse must not
  // leave the cap lit while the forward pre-dub loop plays). publish(i) below emits the corrected value.
  const wasReversed = t.reversed;
  t.reversed = t.undoBufReversed;
  t.undoBufReversed = wasReversed;
  recomputePeaks(t, master);
  swapLiveSource(i, master);
}

/**
 * Per-track REVERSE: flip the committed loop so it plays backwards (a toggle — a second call restores
 * forward, since reversal is its own inverse). Reverses the master-length region of `record` IN PLACE,
 * recomputes peaks, and — if PLAYING — swaps the live source for the reversed buffer on the next loop
 * boundary (the same click-free, sample-aligned swap undoLastOverdub/stopCapture use, so the loop stays
 * frame-identical to master with no audible seam). If STOPPED, the buffer is flipped in place and resume()
 * plays it reversed. No-op unless the track is PLAYING/STOPPED with a committed loop.
 *
 * Reverse operates on the live `record`. undo/redo restores the pre-dub loop in the orientation it had when
 * snapshotted and `reversed` follows it via `undoBufReversed`, so the cap never lies. Overdub is BLOCKED
 * while reversed (RC-505 behaviour) — flip forward to dub (see startOverdub's `reversed` guard), so no layer
 * is ever recorded onto a reversed loop.
 */
function reverse(i: number): void {
  const t = engineState.tracks[i];
  if (!t) return;
  if (t.stopAt !== null) return;
  if (t.state !== 'PLAYING' && t.state !== 'STOPPED') return;
  const master = masterLengthFrames();
  if (master === 0) return;
  // In-place reversal of the master-length region (frames past master are untouched). Two-pointer swap;
  // for an odd length the middle frame stays put. Reversal is its own inverse, so this same call toggles.
  const buf = t.record;
  for (let a = 0, b = master - 1; a < b; a++, b--) {
    const tmp = buf[a];
    buf[a] = buf[b];
    buf[b] = tmp;
  }
  t.reversed = !t.reversed;
  recomputePeaks(t, master);
  swapLiveSource(i, master);
}

/**
 * undo/reverse tail: if PLAYING, swap the live source for a fresh buffer of `record` on the next loop
 * boundary (sample-aligned, click-free); if STOPPED the buffer edit is already in place and resume() picks
 * it up. The publish is in a `finally` so a startPlayback throw (reportCommitPlaybackFailure) still emits
 * the corrected `reversed`/peaks — the buffer edit has happened either way.
 */
function swapLiveSource(i: number, master: number): void {
  const t = engineState.tracks[i];
  try {
    if (t.state === 'PLAYING') startPlayback(i, makeLoopBuffer(t.record, master), nextBoundary());
  } catch (e) {
    reportCommitPlaybackFailure(i, e, 'swap');
  } finally {
    publish(i);
  }
}

/** STOP: silence + stop the track's playback but keep its buffer. EMPTY tracks are unaffected. */
function stop(i: number): void {
  const t = engineState.tracks[i];
  if (!t || t.state === 'EMPTY') return; // not yet initialized, or nothing to stop
  t.stopAt = null;
  const discardUncommitted = t.state === 'RECORDING' && t.lengthFrames === 0;
  if (t.state === 'RECORDING' || t.state === 'OVERDUBBING') {
    // Abort the in-progress capture cleanly.
    // A first-track record with no master yet — whether still counting in (armed) OR mid-take (fixed or
    // free) — owns the count pulse and will NOT commit on this abort, so tear it down + restore the
    // free-run grid. A later-track arm (master > 0) must NOT touch the pulse: the master loop still
    // exists and owns it.
    const wasCountIn = t.state === 'RECORDING' && masterLengthFrames() === 0;
    t.overdubBuf = null;
    t.overdubSwapBufs = null; // release the double-buffer if this aborted an overdub
    t.overdubPreviousUndoBuf = null;
    t.overdubPreviousUndoBufReversed = false;
    t.armed = false;
    firstTakeDownbeatCtx = 0; // symmetric with stopCapture's abort: drop the phase anchor of the aborted take
    releaseRecorderState(i); // shared arm-state clears + fixed-length/BPM unlock
    if (wasCountIn) clock.stopCountIn();
  }
  if (discardUncommitted) {
    t.record.fill(0);
    t.writeHead = 0;
    t.fillFrames = 0;
    resetPeaks(t);
  }
  stopAndFreeSource(t);
  t.state = t.lengthFrames > 0 ? 'STOPPED' : 'EMPTY';
  if (discardUncommitted) resetMasterIfBlank();
  publish(i);
}

/** Resume a STOPPED track's playback, phase-locked to the master grid. */
function resume(i: number): void {
  const t = engineState.tracks[i];
  if (t.lengthFrames === 0) return;
  t.stopAt = null;
  const audioBuf = makeLoopBuffer(t.record, t.lengthFrames);
  t.state = 'PLAYING';
  // Come back in IMMEDIATELY at the CURRENT loop phase (audible at once, phase-locked to the running grid)
  // rather than waiting up to a full loop of silence for nextBoundary(). The track is already mid-loop when
  // you press play, so starting at the live phase is the natural resume — it slots straight into the grid
  // and the other tracks. The buffer still wraps to frame 0 on a grid boundary (gridAnchor + k*period), so
  // it stays phase-locked.
  const period = t.lengthFrames / sr();
  const when = engine.ctx.currentTime + HEARTBEAT_INTERNAL_LATENCY;
  const offset = period > 0 ? phaseOffset(when, engineState.masterStartTime, period) : 0;
  startPlayback(i, audioBuf, when, offset);
  publish(i);
}

// ── Global fan-out ───────────────────────────────────────────────────────────────────────
/** Stop live tracks: PLAYING honors loop-end mode; recordings/overdubs commit and stop immediately. */
function stopAll(): void {
  // One audio-time deadline for the whole gesture, even if the loop edge passes during fan-out.
  const when = loopEndStopEnabled() && masterLengthFrames() > 0 ? nextBoundary() : null;
  const forceNow = engineState.tracks.some((t) => t.stopAt !== null);
  for (let i = 0; i < TRACK_COUNT; i++) {
    const s = engineState.tracks[i]?.state;
    if (s === 'PLAYING') {
      if (when !== null && !forceNow) requestLoopEndStop(i, when);
      else stop(i);
      continue;
    }
    if (s === 'RECORDING' || s === 'OVERDUBBING') playStop(i);
  }
}

/** Resume every STOPPED track -> PLAYING (phase-locked to the master grid). */
function playAll(): void {
  for (let i = 0; i < TRACK_COUNT; i++) {
    if (engineState.tracks[i]?.state === 'STOPPED') resume(i);
  }
}

/** Clear every track AND reset the master loop length + unlock BPM. UI confirms first. */
function clearAll(): void {
  for (let i = 0; i < TRACK_COUNT; i++) clear(i);
  resetMaster();
}

/** Reset the master loop (length, grid anchor, phase) and unlock BPM. The blank-slate state. */
function resetMaster(): void {
  setMasterLengthFrames(0);
  engineState.masterFramesPlain = 0;
  engineState.masterStartTime = 0;
  firstTakeDownbeatCtx = 0; // blank slate: no counted take in flight
  engineState.loopPhasePlain = 0;
  clock.setBpmLocked(false);
  clock.stopMasterPulse(); // re-anchor the ctx pulse to free-run (LED keeps beating, no loop)
}

/** Reset the grid once no committed or in-flight lane remains. */
function resetMasterIfBlank(): void {
  if (engineState.activeRecordIndex < 0 && engineState.tracks.every((t) => t.state === 'EMPTY')) {
    resetMaster();
  }
}

/**
 * CLEAR: free the track's buffer and return it to EMPTY. If this was the last non-empty track, the
 * master loop is reset too (so one-by-one clears reach the same blank slate as Clear-all).
 */
function clear(i: number): void {
  const t = engineState.tracks[i];
  if (!t) return; // not yet initialized — nothing to clear
  t.stopAt = null;
  // Clearing the track that owns the capture ring releases it and the shared arm-count (so a stale
  // pendingRecordStartFrame from an aborted count-in / arm can't bleed into the next record). If this
  // was a first-track count-in it's also the last non-empty track, so resetMaster() below tears down
  // the count pulse + restores the free-run grid.
  if (engineState.activeRecordIndex === i) {
    releaseRecorderState(i); // shared arm-state clears + fixed-length/BPM unlock
  }
  stopAndFreeSource(t);
  t.retiringSources.clear();
  t.record.fill(0);
  t.overdubBuf = null;
  t.overdubSwapBufs = null;
  t.undoBuf = null;
  t.overdubPreviousUndoBuf = null;
  t.undoBufReversed = false;
  t.overdubPreviousUndoBufReversed = false;
  t.reversed = false;
  t.armed = false;
  t.autoArmed = false;
  t.writeHead = 0;
  t.fillFrames = 0;
  t.lengthFrames = 0;
  t.volume = 1;
  t.muted = false;
  volumeSignals[i][1](1);
  muteSignals[i][1](false);
  // Reset the FX chain to defaults in lockstep with the vol/mute reset above — CLEAR is the blank
  // slate for the WHOLE lane (a product call, see the AGENTS.md gotcha; without it a re-record plays
  // through the previous take's invisible filter/pitch/delay while the slider/mute read defaults).
  // Same recipe as session.ts's import: replace fxState, push into the live chain if one exists
  // (clear() keeps t.fx alive — the wiring rides t.gain), bump fxVersion so the FX UI re-reads.
  t.fxState = defaultFxStates();
  t.fx?.setState(t.fxState);
  fxVersion[i][1]((v) => v + 1);
  // Re-sync the LIVE gain node to the reset state. clear() keeps t.gain alive (the FX wiring rides it),
  // so without this the node holds its last ramped value (e.g. 0.3, or 0 if muted) and a re-record of
  // this track plays at the old level/mute while the slider+icon read unity/unmuted — state and audio
  // disagree until the user nudges them. No-op if the node is lazy (not built yet).
  applyTrackGain(i);
  resetPeaks(t);
  t.state = 'EMPTY';
  // If that cleared the last non-empty track, reset the master loop so one-by-one clears reach the
  // same blank slate as Clear-all.
  resetMasterIfBlank();
  publish(i);
}

/**
 * Track COPY: duplicate track `i`'s committed loop into the first free EMPTY lane and return that lane's
 * index (-1 = no-op: no committed loop, or no free lane). The whole lane follows — PCM, orientation,
 * volume, mute and FX (owner's call) — but not the undo history: the copy is a fresh take. The PCM is a
 * DEEP copy (the two lanes never share a backing array), frame-identical over `[0, master)`. A PLAYING
 * source yields a PLAYING copy via resume() (live master phase, so both lanes stay phase-locked); a
 * STOPPED source yields a STOPPED copy. Does not capture, so it is safe while another track records.
 */
function copy(i: number): number {
  const src = engineState.tracks[i];
  if (!src) return -1;
  if (src.state !== 'PLAYING' && src.state !== 'STOPPED') return -1;
  const master = masterLengthFrames();
  if (master === 0 || src.lengthFrames !== master) return -1;
  // A free lane is EMPTY and does not hold the recorder slot.
  const j = engineState.tracks.findIndex((t, k) => t.state === 'EMPTY' && k !== engineState.activeRecordIndex);
  if (j < 0) return -1;
  const dst = engineState.tracks[j];
  dst.record.set(src.record.subarray(0, master), 0);
  dst.writeHead = master; // where a finished take leaves it (finishRecording parity)
  dst.fillFrames = master;
  dst.lengthFrames = master;
  dst.reversed = src.reversed;
  // Same FX recipe as clear()/loadSession: replace fxState with a deep copy, push it into the live
  // chain if one exists (clear() keeps t.fx alive), bump fxVersion so the FX UI re-reads.
  dst.fxState = src.fxState.map((f) => ({ bypassed: f.bypassed, params: { ...f.params } }));
  dst.fx?.setState(dst.fxState);
  fxVersion[j][1]((v) => v + 1);
  recomputePeaks(dst, master);
  // Volume/mute through the mixer's own paths, before playback so a lazy gain node is born right.
  setVolume(j, src.volume);
  setMute(j, src.muted);
  dst.state = 'STOPPED';
  if (src.state === 'PLAYING' && src.stopAt === null) resume(j);
  else publish(j);
  return j;
}

export {
  recDub,
  playStop,
  undoLastOverdub,
  reverse,
  copy,
  stop,
  stopAll,
  playAll,
  clearAll,
  clear,
  setFixedLengthEnabled,
  setFixedLengthBars,
  setAutoRecordEnabled,
  setAutoRecordSensitivity,
};

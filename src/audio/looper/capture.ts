import { RingBuffer } from 'ringbuf.js';
import captureUrl from '../worklets/capture-processor.ts?worker&url';
import { engine } from '../engine';
import { clock } from '../clock';
import { defaultFxStates } from '../fx/fx';
import { readAudioDeviceSettings } from '../audio-settings';
import { platform } from '../../platform';
import type { OpenedInput } from '../../platform';
import { notifyError } from '../../notify';
import { CAPTURE_PACKET_HEADER, CAPTURE_PACKET_SIZE, capturePacketCapacity } from '../capture-packet';
import {
  autoRecordSensitivity,
  DRAIN_INTERVAL_MS,
  engineState,
  inputArmed,
  MAX_LOOP_SECONDS,
  masterLengthFrames,
  PEAK_FRAMES,
  publish,
  RING_CAPACITY_FRAMES,
  setInputArmed,
  sr,
  TRACK_COUNT,
  type RecordSession,
  type Track,
  type TrackState,
} from './state';
import { armSplitAt, compensatedLoopFrame } from './grid-math';
import { resetPeaks, updateLivePeaks } from './peaks';
import {
  beginAutoRecording,
  beginPluginRecordIntegrityWindow,
  completeRetakePass,
  finishCapture,
  refreshAutoRecordIntegrityWindow,
} from './machine';
import { AutoRecordDetector, autoRecordThreshold } from './auto-record';

/**
 * OWNS: the capture path — the capture AudioWorkletNode + SAB ring + per-track buffers, the main-thread
 * drain into the actively-recording track (`consume`: the frame-exact arm split, the first-take cap that
 * auto-commits, the overdub sum), the plain loop-phase field the draw loop reads, and the platform mic/line
 * input arm. Commits are decided in `machine.ts` (it calls back into finishCapture).
 * See `looper.ts` (the facade) for the full subsystem overview.
 *
 * ── CAPTURE PATH ──────────────────────────────────────────────────────────────────────
 * One capture AudioWorkletNode taps `engine.recordTap` (everything on looperInputBus flows
 * there, plus any record-only source like a native-monitored plugin's wet). It pushes every 128-frame
 * quantum with its absolute render-frame timestamp into ONE lock-free ringbuf.js RingBuffer. A
 * main-thread drain (setInterval ~25ms) decodes complete packets and writes the selected window into the
 * track's pre-allocated Float32Array at its write head. Only ONE track records at a time
 * (the single shared ring belongs to whichever track is RECORDING/OVERDUBBING).
 *
 * KEEP-ALIVE: an AudioWorkletNode whose output is unconnected may not have process() pulled.
 * We connect captureNode -> a muted GainNode (gain 0) -> ctx.destination so the graph always
 * pulls it, without leaking the dry tap to the speakers. A heartbeat counter in shared memory
 * (bumped every quantum) lets callers confirm process() is really running.
 */

// ── Engine singletons (lazy) ─────────────────────────────────────────────────────────────
/**
 * In-flight init promise — memoized so two near-simultaneous first interactions (e.g. an input-arm tap
 * racing a track REC press, both `await init()`) SHARE one build instead of both passing the
 * `if (initialized)` guard (only set true after two awaits) and double-building the ring/worklet/tracks
 * (the second clobbering the first's `tracks` + leaking a captureNode permanently tapping recordTap).
 * Nulled on a transient failure so a retry can rebuild.
 */
let initPromise: Promise<void> | null = null;
let captureNode: AudioWorkletNode | null = null;
let keepAliveGain: GainNode | null = null;
let drainTimer: ReturnType<typeof setInterval> | null = null;
let openedInput: OpenedInput | null = null;
/** Main-thread level detector + look-back history, allocated once with the capture engine. */
let autoRecordDetector: AutoRecordDetector | null = null;
/**
 * Mono-sum node inserted between the opened mic/line source and looperInputBus (see `makeMicMonoSum`).
 * Held so disarmInput can disconnect it when the input closes.
 */
let micMonoSum: GainNode | null = null;
/**
 * In-flight input-arm promise — memoized so a double-tap of the mic-arm toggle (two armInput() calls
 * before the first getUserMedia resolves, since inputArmed flips only after open()) SHARE one open()
 * instead of both passing the `if (openedInput)` guard and opening two MediaStreams — the first of which
 * leaks (its close() is unreachable: a live mic that can't be disarmed) while both feed looperInputBus,
 * doubling the recorded + monitored input level.
 */
let armingInFlight: Promise<boolean> | null = null;
/**
 * What the LAST arm/disarm gesture asked for, held separately from the adopted `openedInput`. A disarm
 * that lands while `open()` is still pending has no stream to close yet, so it clears this flag instead;
 * the open then resolves into a cancelled arm and closes its own stream rather than wiring a mic the user
 * already turned off. A re-arm during the same pending open sets it back, so a burst of toggles ends in
 * the state of the last gesture whatever order the promises settle in.
 */
let inputArmDesired = false;

// ── Initialization ───────────────────────────────────────────────────────────────────────
/** The shared build (worklet + ring + tracks + drain), memoized while in flight and after success. */
function build(): Promise<void> {
  if (engineState.initialized) return Promise.resolve();
  if (initPromise) return initPromise; // a concurrent first interaction is already building — share it
  initPromise = buildEngine().catch((e) => {
    initPromise = null; // transient failure (engine.start / addModule) — let a later call retry the build
    throw e;
  });
  return initPromise;
}

/**
 * Lazily build the capture worklet + ring + tracks and bring the clock alive (beat LED + metronome go
 * live the moment the looper is first USED). Idempotent. Must be awaited before any record call
 * (engine.start already resumed the context).
 */
export async function init(): Promise<void> {
  await build();
  if (engineState.initialized) clock.ensureRunning();
}

/**
 * Build the capture path WITHOUT starting the clock — the play path calls this on the first note
 * (`instrument.ensureActive`) so the record-level meter reads the synth/plugin/mic before the looper
 * is touched. Fire-and-forget: a failure here is retried and reported by the next `init()`.
 */
export function warm(): void {
  if (engineState.initialized || initPromise) return;
  build().catch((e) => console.error('[capture] warm-up failed', e));
}

async function buildEngine(): Promise<void> {
  if (!self.crossOriginIsolated) {
    // SharedArrayBuffer + Atomics are unavailable — the capture ring cannot work.
    console.error(
      '[looper] self.crossOriginIsolated is false: SharedArrayBuffer unavailable, looper capture disabled. ' +
        'Ensure COOP/COEP headers are set.',
    );
    return;
  }
  await engine.start();
  const ctx = engine.ctx;
  autoRecordDetector = new AutoRecordDetector(ctx.sampleRate);

  await ctx.audioWorklet.addModule(captureUrl);

  const packetCapacity = capturePacketCapacity(RING_CAPACITY_FRAMES);
  const ringSab = RingBuffer.getStorageForCapacity(packetCapacity, Float64Array);
  engineState.ring = new RingBuffer(ringSab, Float64Array);
  // [0] = quantum count (liveness), [1] = overrun count (frames dropped on a full ring).
  const heartbeatSab = new SharedArrayBuffer(2 * Int32Array.BYTES_PER_ELEMENT);
  engineState.heartbeat = new Int32Array(heartbeatSab);

  captureNode = new AudioWorkletNode(ctx, 'capture-processor', {
    numberOfInputs: 1,
    numberOfOutputs: 1,
    outputChannelCount: [1],
    channelCount: 1,
    channelCountMode: 'explicit',
    processorOptions: { ringSab, heartbeatSab },
  });

  // Record tap: everything routed to looperInputBus flows to recordTap, plus any
  // record-only source (a native-monitored plugin's wet). We capture recordTap, not looperInputBus.
  engine.recordTap.connect(captureNode);
  // Keep-alive: muted path to destination so process() is always pulled.
  keepAliveGain = ctx.createGain();
  keepAliveGain.gain.value = 0;
  captureNode.connect(keepAliveGain);
  keepAliveGain.connect(ctx.destination);

  const maxFrames = Math.ceil(MAX_LOOP_SECONDS * ctx.sampleRate);
  engineState.maxPeaks = Math.ceil(maxFrames / PEAK_FRAMES);
  engineState.tracks = Array.from({ length: TRACK_COUNT }, () => ({
    state: 'EMPTY' as TrackState,
    stopAt: null,
    record: new Float32Array(maxFrames),
    writeHead: 0,
    fillFrames: 0,
    lengthFrames: 0,
    source: null,
    retiringSources: new Set<AudioBufferSourceNode>(),
    gain: null,
    volume: 1,
    muted: false,
    overdub: null,
    undoBuf: null,
    undoBufReversed: false,
    reversed: false,
    fx: null,
    fxState: defaultFxStates(),
    armed: false,
    autoArmed: false,
    peakMin: new Float32Array(engineState.maxPeaks),
    peakMax: new Float32Array(engineState.maxPeaks),
    peakCount: 0,
    peakComplete: 0,
    peakVersion: 0,
  }));

  engineState.drainScratch = new Float32Array(RING_CAPACITY_FRAMES);
  engineState.packetScratch = new Float64Array(packetCapacity);
  if (drainTimer !== null) clearInterval(drainTimer);
  drainTimer = setInterval(drainTick, DRAIN_INTERVAL_MS);
  engineState.initialized = true;
}

// ── Drain loop (main thread) ─────────────────────────────────────────────────────────────
/**
 * Pop everything available from the ring and feed it to the active recording track.
 * Also advances the loop-phase accessor for the UI.
 */
function drainTick(): void {
  const peak = drainCapturedPackets();
  // Record-level meter: peak of this batch, held with a fast decay (~110 ms from full to −20 dB at the
  // 25 ms tick). Plain field, read by the rAF loop (invariant 6).
  engineState.inputLevelPlain = Math.max(peak, engineState.inputLevelPlain * 0.8);

  // Update the loop phase — a PLAIN field read by the single rAF loop (waveform.ts: lane playheads +
  // the command-bar ring dial). Never a signal: this runs on the 25 ms drain timer (invariant 6).
  const len = masterLengthFrames();
  if (len > 0) {
    const elapsed = engine.ctx.currentTime - engineState.masterStartTime;
    const period = len / sr();
    if (period > 0) {
      engineState.loopPhasePlain = ((elapsed % period) + period) % period / period;
    }
  }
}

/** Decode one ring snapshot, preserving each contiguous run's exact render-frame origin. */
function drainCapturedPackets(): number {
  const { ring, packetScratch, drainScratch } = engineState;
  if (!ring || !packetScratch || !drainScratch) return 0;
  const available = Math.floor(ring.available_read() / CAPTURE_PACKET_SIZE) * CAPTURE_PACKET_SIZE;
  if (available === 0) return 0;
  const got = ring.pop(packetScratch, Math.min(available, packetScratch.length));
  let firstFrame = 0, frames = 0, peak = 0;
  const deliver = () => {
    if (frames === 0) return;
    engineState.captureFrontierFrame = firstFrame + frames;
    const rec = engineState.recording;
    if (rec) consume(rec, drainScratch, frames, firstFrame);
    frames = 0;
  };
  for (let p = 0; p < got; p += CAPTURE_PACKET_SIZE) {
    const start = packetScratch[p];
    const count = packetScratch[p + 1];
    if (frames > 0 && start !== firstFrame + frames) deliver();
    if (frames === 0) firstFrame = start;
    for (let k = 0; k < count; k++) {
      const value = packetScratch[p + CAPTURE_PACKET_HEADER + k];
      drainScratch[frames + k] = value;
      peak = Math.max(peak, Math.abs(value));
    }
    frames += count;
  }
  deliver();
  return peak;
}

/**
 * Discard whatever the ring holds right now (the pre-roll before a record/overdub press), so the take
 * begins clean from the press. Shared by machine.ts's startRecording + startOverdub.
 */
export function drainStaleFrames(): void {
  const { ring, packetScratch } = engineState;
  if (!ring || !packetScratch) return;
  const available = Math.floor(ring.available_read() / CAPTURE_PACKET_SIZE) * CAPTURE_PACKET_SIZE;
  if (available > 0) {
    const got = ring.pop(packetScratch, Math.min(available, packetScratch.length));
    engineState.captureFrontierFrame = packetScratch[got - CAPTURE_PACKET_SIZE] + packetScratch[got - CAPTURE_PACKET_SIZE + 1];
  }
}

export function flushActiveCapture(): void {
  if (engineState.recording) drainCapturedPackets();
}

/**
 * Frame-exact arm split, shared by the first-track count-in and the later-track boundary arm:
 * compare the absolute capture deadline with this batch's render-frame timestamp. The main-thread
 * clock may advance while a stale ring snapshot is discarded; that cannot move the window.
 * `pendingStartFrame` is only a diagnostic remaining count, never the timing authority.
 *
 * Returns -1 while still counting (caller discards the whole pre-downbeat batch), otherwise the
 * offset into `data` where the real take begins (0 when not armed / not straddling). On the
 * straddling batch it clears the arm and resets the write head + peaks so the take starts clean.
 */
function armSplitOffset(t: Track, rec: RecordSession, count: number, firstFrame: number): number {
  if (!t.armed) return 0;
  const split = armSplitAt(rec.startFrame ?? firstFrame, firstFrame, count);
  rec.pendingStartFrame = split.pending;
  if (split.offset < 0) {
    // This whole batch is discarded pre-roll, so losses through it cannot corrupt the take. Refresh
    // here, but not on the straddling batch below: that batch already contains the take's frame 0.
    beginPluginRecordIntegrityWindow(rec);
    return -1; // still before the downbeat/boundary — discard these provisional frames
  }
  t.armed = false; // target reached: the real take begins (writeHead 0), waveform may grow
  t.writeHead = 0;
  t.fillFrames = 0;
  resetPeaks(t);
  return split.offset; // this batch straddles the target: the take begins here
}

/**
 * Write `count` freshly-captured frames into the recording track `rec.track`.
 * - Recording: append the retained window; the machine decides the committed length.
 * - Overdub: sum the retained window modulo the master length.
 */
function consume(rec: RecordSession, data: Float32Array, count: number, firstFrame: number): void {
  const i = rec.track;
  const t = engineState.tracks[i];
  const master = masterLengthFrames();

  if (t.state === 'RECORDING') {
    let offset = 0;
    if (t.autoArmed) {
      const detector = autoRecordDetector;
      if (!detector) return;
      const threshold = autoRecordThreshold(autoRecordSensitivity());
      offset = detector.scan(data, count, threshold, t.record);
      if (offset < 0) {
        // Loss before the retained look-back cannot damage the future take. Advance both baselines only
        // while the entire history is quiet; once possible onset audio exists, damage in it must count.
        if (detector.historyIsQuiet(threshold)) refreshAutoRecordIntegrityWindow(rec);
        return;
      }
      const retained = detector.copiedFrames();
      t.writeHead = retained;
      t.fillFrames = retained;
      resetPeaks(t);
      // The retained onset may begin in detector history before this batch. Its frame origin is
      // recovered from the producer timestamp, independent of when the main-thread drain runs.
      const capturedStartCtx = (firstFrame + offset - retained) / sr();
      beginAutoRecording(i, capturedStartCtx);
    } else {
      // Count-in or later-track boundary arm: both keep their exact timestamped start.
      offset = armSplitOffset(t, rec, count, firstFrame);
    }
    if (offset < 0) return; // still counting in — discard the pre-downbeat frames (recorded dead air)
    // First and later takes append the same exclusive timestamp window. AUTO's detector
    // may already have copied its look-back prefix; append only the rest of this batch.
    const end = Math.min(count, (rec.endFrame ?? Infinity) - firstFrame);
    const n = Math.max(0, Math.min(end - offset, t.record.length - t.writeHead));
    t.record.set(data.subarray(offset, offset + n), t.writeHead);
    t.writeHead += n;
    t.fillFrames = t.writeHead;
    updateLivePeaks(t);
  }

  if (t.state === 'OVERDUBBING' && master > 0 && t.overdub) {
    // Sum incoming PCM into the working copy, wrapping mod master. Split into contiguous runs at each
    // wrap so the hot inner loop carries no per-sample `%` (the write region is contiguous within a run).
    // The outer step handles a batch that spans the wrap — or, after a stall, multiple loop periods —
    // with each overlapping pass summing onto the same positions.
    const buf = t.overdub.buf;
    let k = Math.max(0, (rec.startFrame ?? firstFrame) - firstFrame);
    const end = Math.min(count, (rec.endFrame ?? Infinity) - firstFrame);
    const gridFrame = Math.round(engineState.masterStartTime * sr());
    let head = compensatedLoopFrame(firstFrame + k, rec.compensationFrames, gridFrame, master);
    while (k < end) {
      const run = Math.min(end - k, master - head); // frames until the next wrap
      for (let j = 0; j < run; j++) buf[head + j] += data[k + j];
      k += run;
      head += run;
      if (head >= master) head = 0;
    }
    t.writeHead = head;
  }
  const endFrame = rec.endFrame;
  if (endFrame !== null && firstFrame + count >= endFrame) {
    // A rolling RETAKE slides to its next pass; anything else commits.
    if (rec.retakeRolling) completeRetakePass(rec);
    else finishCapture(i);
    // Frames past the window end belong to whoever records next: the retake's next pass, or the lane a
    // retake handoff just started on this very frame. An ordinary commit leaves no recorder.
    const next = engineState.recording;
    const tail = Math.max(0, endFrame - firstFrame);
    if (next && tail < count) consume(next, data.subarray(tail, count), count - tail, firstFrame + tail);
  } else {
    publish(i);
  }
}

/** Reset the pre-allocated detector when a fresh AUTO arm claims the capture ring. */
export function prepareAutoRecord(): void {
  autoRecordDetector?.reset();
}

/** Drop any retained onset when AUTO is cancelled, cleared or superseded. */
export function cancelAutoRecord(): void {
  autoRecordDetector?.reset();
}

// ── Mic / line input arm ─────────────────────────────────────────────────────────────────
/**
 * Build the centred mono node the mic/line source passes through before it reaches looperInputBus.
 * With an explicit Audio Settings channel, host.web has already isolated one ChannelSplitter output;
 * this node keeps that lane mono and reconnecting it to the stereo looperInputBus centres it in BOTH
 * speakers. With `auto`, an interface may still return multiple channels despite the advisory mono
 * request; speakers interpretation then sums them to mono (0.5·(L+R)), avoiding one-eared playback. A
 * genuinely stereo line source is summed to mono in auto mode too — accepted for now (standard looper
 * behaviour).
 */
function makeMicMonoSum(ctx: BaseAudioContext): GainNode {
  const node = ctx.createGain();
  node.channelCount = 1;
  node.channelCountMode = 'explicit';
  node.channelInterpretation = 'speakers';
  return node;
}

/**
 * The armed input's device died (interface unplugged, device disabled). Nothing else notices: the source
 * node stays wired to looperInputBus and feeds silence, so the mic reads ARMED while the take records
 * nothing. Tear down through the ordinary disarm path — same cleanup as a user-driven disarm — and say so.
 */
function loseInput(): void {
  disarmInput();
  console.error('[looper] audio input device lost — mic disarmed');
  notifyError('Input device lost — mic disarmed');
}

/** Arm the platform audio input (mic/line) into looperInputBus so it gets recorded + heard. */
export function armInput(): Promise<boolean> {
  inputArmDesired = true; // the latest gesture wants the mic on — a shared pending open adopts again
  if (openedInput) return Promise.resolve(true);
  if (armingInFlight) return armingInFlight; // a concurrent arm (double-tap) is already opening — share it
  armingInFlight = (async () => {
    await init();
    if (openedInput) return true;
    if (!engineState.initialized) {
      // buildEngine bailed (no crossOriginIsolated → no capture ring). Opening the mic here would show
      // an ARMED lane that records nothing; refuse loudly instead.
      console.error('[looper] input arm refused: looper capture is unavailable (crossOriginIsolated=false)');
      notifyError('Input not armed — recording is unavailable', 'The capture engine could not start (cross-origin isolation is off).');
      return false;
    }
    // The device can die between open() resolving and the wiring below, so the loss report is LATCHED
    // rather than acted on blind: `mine` identifies this arm's input once it is live (a loss report from
    // a superseded arm must not disarm the current one), and a loss that lands before that is replayed
    // right after wiring. Events can't interleave with the synchronous wiring block, so exactly one of
    // the two paths runs.
    let mine: OpenedInput | null = null;
    let lost = false;
    const storedChannel = readAudioDeviceSettings().inputChannel;
    const parsedChannel = storedChannel === '' ? null : Number(storedChannel);
    const channel =
      parsedChannel !== null && Number.isInteger(parsedChannel) && parsedChannel >= 0
        ? parsedChannel
        : null;
    const opened = await platform.audioInput.open(engine.ctx, {
      channel,
      onLost: () => {
        lost = true;
        if (mine && openedInput === mine) loseInput();
      },
    });
    if (!opened) {
      console.warn('[looper] no audio input available to arm');
      return false;
    }
    if (!inputArmDesired) {
      // Disarmed while getUserMedia was still open. disarmInput() had no stream to close, so close this
      // one here instead of wiring a mic the user already turned off; inputArmed stays false.
      opened.close();
      console.info('[looper] audio input open completed after disarm — stream closed');
      return false;
    }
    if (openedInput) {
      // A concurrent arm won the race while we awaited open() — close this redundant stream rather than
      // leaking it + double-connecting to looperInputBus. (Defensive; the latch already serializes.)
      opened.close();
      return true;
    }
    // Mono-sum before the graph so a one-channel input isn't heard/recorded in one ear only.
    micMonoSum = makeMicMonoSum(engine.ctx);
    opened.node.connect(micMonoSum);
    micMonoSum.connect(engine.looperInputBus);
    openedInput = opened;
    mine = opened;
    setInputArmed(true);
    if (lost) {
      loseInput(); // died while we were opening/wiring — the handler above found `mine` unset
      return false;
    }
    return true;
  })();
  return armingInFlight.finally(() => {
    armingInFlight = null;
  });
}

/** Disarm the audio input. A disarm during a pending open cancels that open (see `inputArmDesired`). */
export function disarmInput(): void {
  inputArmDesired = false;
  if (!openedInput) return;
  openedInput.close(); // stops the MediaStream tracks + disconnects opened.node
  if (micMonoSum) {
    micMonoSum.disconnect();
    micMonoSum = null;
  }
  openedInput = null;
  setInputArmed(false);
}

export async function toggleInput(): Promise<boolean> {
  // A pending open counts as ON for the gesture: the second tap of a double-tap is a DISARM that cancels
  // the open (see `inputArmDesired`), not a second arm sharing it. `inputArmed()` alone stays false until
  // the open is adopted, which would turn every tap during the open into another arm.
  if (inputArmed() || (armingInFlight !== null && inputArmDesired)) {
    disarmInput();
    return false;
  }
  return armInput();
}

/**
 * What the last arm/disarm gesture asked for, independent of whether a stream is open yet. A false
 * `armInput()` result while this still reads true means the arm FAILED (no device); while it reads
 * false the arm was cancelled by a later disarm and needs no failure report.
 */
export function inputArmRequested(): boolean {
  return inputArmDesired;
}

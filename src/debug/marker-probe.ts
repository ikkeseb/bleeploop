/** DEV runner for an isolated, empty native session. Load one effect, go LIVE and settle first.
 * No looper actions, settings writes or gain changes. No physical-loopback claim: the presentation
 * estimate depends on driver/browser reports and an UNKNOWN ASIO callback-entry delay. */
import { RingBuffer } from 'ringbuf.js';
import { markerProbeNative } from '../platform';
import { engine } from '../audio/engine';
import { looper } from '../audio/looper/looper';
import { slotPlugins } from '../audio/instrument-slots';
import { pluginBridge } from '../audio/plugin-bridge';
import { recordLatency } from '../audio/record-latency';
import { computeC } from '../audio/record-latency-math';
import { CAPTURE_PACKET_SIZE, CAPTURE_PACKET_HEADER, capturePacketCapacity } from '../audio/capture-packet';
import captureUrl from '../audio/worklets/capture-processor.ts?worker&url';
import { clockOffset, findMarkers } from './marker-math';
import renderClockUrl from './render-clock-processor.ts?worker&url';

let running = false;

export async function runMarkerProbe({ slot = 0, signal }: { slot?: 0 | 1; signal?: AbortSignal } = {}) {
  if (!import.meta.env.DEV || running) throw Error('Marker probe unavailable or already running');
  const assertEmpty = () => {
    if (signal?.aborted) throw Error('Marker probe cancelled');
    if (looper.inputArmed()) throw Error('Disarm the web mic before the marker probe');
    for (let i = 0; i < looper.trackCount; i++) if (looper.stateOf(i) !== 'EMPTY') throw Error('Marker probe requires an empty jam');
    if (!slotPlugins()[slot] || slotPlugins()[1 - slot] || recordLatency.armedSlot() !== slot) {
      throw Error('Marker probe requires exactly one loaded, natively monitored slot');
    }
    if (engine.ctx.state !== 'running') throw Error('AudioContext must already be running');
  };
  assertEmpty();
  running = true;
  let started = false;
  let node: AudioWorkletNode | undefined;
  let renderClock: AudioWorkletNode | undefined;
  let keepAlive: GainNode | undefined;
  try {
    const ctx = engine.ctx;
    const sr = ctx.sampleRate;
    const graphMs = engine.outputGraphLatencySeconds * 1000;
    if (sr < 44100 || sr > 96000) throw Error('Probe supports 44.1–96 kHz');
    await ctx.audioWorklet.addModule(captureUrl);
    await ctx.audioWorklet.addModule(renderClockUrl);
    const ringSab = RingBuffer.getStorageForCapacity(capturePacketCapacity(sr), Float64Array);
    const ring = new RingBuffer(ringSab, Float64Array);
    const heartbeatSab = new SharedArrayBuffer(8);
    const heartbeat = new Int32Array(heartbeatSab);
    const packets = new Float64Array(capturePacketCapacity(sr));
    const pcm = new Float32Array(sr * 9);
    let frames = 0, firstFrame = -1;
    const formulaSample = () => {
      const snapshot = recordLatency.snapshot();
      const terms = {
        hopFrames: snapshot.hopFrames,
        cpalOutSeconds: recordLatency.cpalOutSeconds(), baseLatency: snapshot.baseLatencyMs / 1000,
        outputLatency: snapshot.outputLatencyMs / 1000, outputGraphLatencySeconds: graphMs / 1000,
        trimMs: 0, floorEnabled: recordLatency.isFloorEnabled(),
      };
      return { snapshot, terms, formulaWithoutTrimMs: computeC(terms, sr).frames / sr * 1000,
        productionWithoutTrimMs: computeC({ ...terms, renderCursorSeconds: snapshot.renderCursorSeconds }, sr).frames / sr * 1000,
        instantaneousBridge: pluginBridge.stats(slot) };
    };
    const anchors: ({ contextTime: number; performanceTime: number; observedPerformanceMs: number;
      renderContextTime: number; readSpanMs: number; reportedBaseLatencyMs: number;
      reportedOutputLatencyMs: number; renderMinusPresentationContextMs: number; cursorCompensationMs: number } & ReturnType<typeof formulaSample>)[] = [];
    node = new AudioWorkletNode(ctx, 'capture-processor', {
      numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1], channelCount: 1,
      channelCountMode: 'explicit', processorOptions: { ringSab, heartbeatSab },
    });
    keepAlive = ctx.createGain(); keepAlive.gain.value = 0;
    const clockSab = new SharedArrayBuffer(8 + 8192 * 16);
    const clockPublished = new Int32Array(clockSab, 0, 2);
    const clockRecords = new Float64Array(clockSab, 8);
    renderClock = new AudioWorkletNode(ctx, 'render-clock-probe', {
      numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1], channelCount: 1,
      channelCountMode: 'explicit', processorOptions: { clockSab },
    });
    engine.recordTap.connect(renderClock); renderClock.connect(node);
    node.connect(keepAlive); keepAlive.connect(ctx.destination);
    const wallPings: { before: number; native: number; after: number }[] = [];
    const pings: { before: number; native: number; after: number }[] = [];
    const ping = async () => {
      for (let i = 0; i < 24; i++) {
        const before = performance.now();
        const native = await markerProbeNative.clock();
        pings.push({ before, native, after: performance.now() });
      }
    };
    const drain = () => {
      const before = performance.now();
      const wall = Date.now();
      // Explicit ±1 ms quantization allowance. Clock jumps must still leave consistent bounds.
      wallPings.push({ before: before - 1, native: wall, after: performance.now() + 1 });
      const got = ring.pop(packets, Math.min(packets.length, Math.floor(ring.available_read() / CAPTURE_PACKET_SIZE) * CAPTURE_PACKET_SIZE));
      for (let i = 0; i < got; i += CAPTURE_PACKET_SIZE) {
        const at = packets[i], count = packets[i + 1];
        if (firstFrame < 0) firstFrame = at;
        if (at !== firstFrame + frames || count !== 128 || frames + count > pcm.length) throw Error('Noncontiguous/oversized web capture');
        for (let k = 0; k < count; k++) pcm[frames++] = packets[i + CAPTURE_PACKET_HEADER + k];
      }
      const readStart = performance.now();
      const anchor = ctx.getOutputTimestamp();
      const renderContextTime = ctx.currentTime;
      const observedPerformanceMs = performance.now();
      if (!anchor.contextTime || !anchor.performanceTime || performance.now() - anchor.performanceTime > 500) {
        throw Error('Missing or stale browser output timestamp');
      }
      const previous = anchors.at(-1);
      if (previous && (anchor.contextTime < previous.contextTime || anchor.performanceTime < previous.performanceTime)) {
        throw Error('Browser output timestamp regressed');
      }
      const sampled = formulaSample();
      // Candidate accounting: map the next render frame and add ONLY audio still queued for it.
      // Queue depletion and render-cursor advance should cancel at a browser callback boundary.
      const queue = sampled.instantaneousBridge;
      const cursorCompensationMs = anchor.performanceTime
        + (renderContextTime - anchor.contextTime) * 1000 - observedPerformanceMs
        + (queue?.queue ?? 0) / sr * 1000
        - sampled.terms.cpalOutSeconds * 1000 + graphMs;
      anchors.push({ contextTime: anchor.contextTime, performanceTime: anchor.performanceTime,
        observedPerformanceMs, renderContextTime, readSpanMs: observedPerformanceMs - readStart,
        reportedBaseLatencyMs: ctx.baseLatency * 1000, reportedOutputLatencyMs: ctx.outputLatency * 1000,
        // Descriptive only: W3C explicitly warns this subtraction is not a reliable latency estimate.
        renderMinusPresentationContextMs: (renderContextTime - anchor.contextTime) * 1000,
        cursorCompensationMs, ...sampled });
      if (Atomics.load(heartbeat, 1)) throw Error('Web capture overflow');
    };
    await ping();
    drain();
    assertEmpty();
    const lossBefore = pluginBridge.recordLossSnapshot();
    await markerProbeNative.begin(slot); started = true;
    const until = performance.now() + 7300;
    while (performance.now() < until) {
      await new Promise(resolve => setTimeout(resolve, 20));
      assertEmpty(); drain();
    }
    const lossAfter = pluginBridge.recordLossSnapshot();
    if (lossAfter.droppedFrames !== lossBefore.droppedFrames || lossAfter.underruns !== lossBefore.underruns) {
      throw Error('Plugin bridge lost samples during measurement');
    }
    await ping(); drain();
    const nativeMarkers = await markerProbeNative.result();
    const webMarkers = findMarkers(pcm.subarray(0, frames), sr);
    if (webMarkers.length !== 8) throw Error(`Expected 8 web markers, detected ${webMarkers.length}`);
    if (Atomics.load(clockPublished, 1)) throw Error('Render clock capture overflow');
    const clockCount = Atomics.load(clockPublished, 0);
    const wallOffset = clockOffset(wallPings);
    const offset = clockOffset(pings);
    if (offset.uncertainty > 2) throw Error(`Clock mapping too wide: ±${offset.uncertainty.toFixed(3)} ms`);
    const measurements = webMarkers.map((marker, i) => {
      const contextTime = (firstFrame + marker.frame) / sr;
      const nearest = anchors.reduce((best, a) => Math.abs(a.contextTime - contextTime) < Math.abs(best.contextTime - contextTime) ? a : best);
      if (Math.abs(nearest.contextTime - contextTime) > 0.1) throw Error('No nearby output timestamp for marker');
      const webPresentationMs = nearest.performanceTime + (contextTime - nearest.contextTime) * 1000;
      const nativePresentationMs = nativeMarkers[i].presentationMs - offset.midpoint;
      // Presentation anchors refer to output playback. Formula terms instead belong near the
      // marker's RENDER frame, which precedes output presentation by the browser's buffering.
      const formulaAnchor = anchors.reduce((best, a) => Math.abs(a.renderContextTime - contextTime) < Math.abs(best.renderContextTime - contextTime) ? a : best);
      const formulaAnchorDistanceMs = (formulaAnchor.renderContextTime - contextTime) * 1000;
      if (Math.abs(formulaAnchorDistanceMs) > 30) throw Error('No contemporaneous formula sample for marker');
      const recordFrame = firstFrame + marker.frame;
      const quantumFrame = Math.floor(recordFrame / 128) * 128;
      const clockIndex = (quantumFrame - clockRecords[0]) / 128;
      if (!Number.isInteger(clockIndex) || clockIndex < 0 || clockIndex >= clockCount
        || clockRecords[clockIndex * 2] !== quantumFrame) throw Error('Missing marker render clock');
      const renderPerformanceMs = clockRecords[clockIndex * 2 + 1] - wallOffset.midpoint;
      const injectionPerformanceMs = nativeMarkers[i].injectionTimeMs - offset.midpoint;
      const estimatedCompensationMs = webPresentationMs - nativePresentationMs + graphMs;
      return { marker: i, nativeScore: nativeMarkers[i].score, webScore: marker.score,
        recordContextFrame: firstFrame + marker.frame,
        nativePresentationNativeMs: nativeMarkers[i].presentationMs, nativePresentationMs, webPresentationMs,
        presentationAnchor: nearest, formulaAnchor, formulaAnchorDistanceMs,
        renderPerformanceMs, injectionPerformanceMs,
        injectionOffsetFrames: nativeMarkers[i].injectionOffsetFrames,
        injectionSampleRate: nativeMarkers[i].injectionSampleRate,
        markerOffsetInRenderQuantum: recordFrame - quantumFrame,
        forkToCallbackMs: nativeMarkers[i].callbackEntryTimeMs - nativeMarkers[i].injectionTimeMs,
        reportedDriverDelayMs: nativeMarkers[i].reportedDriverDelayMs,
        callbackOffsetFrames: nativeMarkers[i].callbackOffsetFrames,
        nativeSampleRate: nativeMarkers[i].nativeSampleRate,
        forkToRenderMs: renderPerformanceMs - injectionPerformanceMs,
        renderToPresentationMs: webPresentationMs - renderPerformanceMs,
        forkToNativePresentationMs: nativePresentationMs - injectionPerformanceMs,
        estimatedCompensationMs, formulaWithoutTrimMs: formulaAnchor.formulaWithoutTrimMs,
        residualVsFormulaMs: estimatedCompensationMs - formulaAnchor.formulaWithoutTrimMs,
        residualVsCursorMs: estimatedCompensationMs - formulaAnchor.cursorCompensationMs,
        productionWithoutTrimMs: formulaAnchor.productionWithoutTrimMs,
        residualVsProductionMs: estimatedCompensationMs - formulaAnchor.productionWithoutTrimMs };
    });
    const sorted = measurements.map(m => m.estimatedCompensationMs).sort((a, b) => a - b);
    const estimate = (sorted[3] + sorted[4]) / 2;
    const formulaSorted = measurements.map(m => m.formulaWithoutTrimMs).sort((a, b) => a - b);
    const residualSorted = measurements.map(m => m.residualVsFormulaMs).sort((a, b) => a - b);
    const report = { sampleRate: sr, markers: measurements, clockOffsetMs: offset,
      wallClockOffsetMs: wallOffset, wallClockQuantizationAllowanceMs: 1,
      graphMs, formulaWithoutTrimMs: (formulaSorted[3] + formulaSorted[4]) / 2,
      estimatedCompensationMedianMs: estimate, estimatedSpreadMs: sorted[7] - sorted[0],
      residualVsFormulaMs: (residualSorted[3] + residualSorted[4]) / 2,
      residualSpreadMs: residualSorted[7] - residualSorted[0],
      savedTrimMs: recordLatency.offsetMs(), endSnapshot: recordLatency.snapshot(), lossBefore, lossAfter,
      limitation: 'Presentation estimate only. ASIO callback-entry delay is UNKNOWN and outside the IPC clock bound. Browser/driver reports and physical converter latency are not independently verified.',
    };
    await markerProbeNative.report(report);
    return report;
  } finally {
    if (started) await markerProbeNative.cancel().catch(error => console.error('[marker-probe] cancel IPC failed; native deadline remains active', error));
    try {
      if (renderClock) { engine.recordTap.disconnect(renderClock); renderClock.disconnect(); }
      node?.disconnect();
      keepAlive?.disconnect();
    } finally { running = false; }
  }
}

/**
 * Rendered-PCM proof for idle playback restart and live phase joins: measures rendered lane PCM for
 * single/ALL idle restarts from frame zero, simultaneous lane starts, live-phase joins beside a muted
 * lane pending END STOP, cold-graph starts (first PLAY and COPY into a lane that never played, under
 * an injected FX-build cost) and PLAY ALL with one lane failing at source start or at buffer prep
 * (the surviving lanes still start together, one toast, one release-log line per failure).
 * Run: pnpm probe playback-restart [--url=<server>]
 *
 * Captures each lane at its GainNode with absolute AudioWorklet render frames. Seeded ramps make
 * source frame zero and live phase observable in PCM. This does not establish native ASIO feel.
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open, url }) => {
  const { page, consoleErrors } = await open();

  const results = await page.evaluate(async () => {
    const lf = window.__lf;
    const { engineState } = await import('/src/audio/looper/state.ts');
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const { toasts, dismissToast } = await import('/src/notify.ts');
    await lf.looper.init();
    const ctx = lf.engine.ctx;
    await ctx.resume();

    const timeout = async (promise, ms, label) => {
      let timer;
      try {
        return await Promise.race([
          promise,
          new Promise((_, reject) => {
            timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms} ms`)), ms);
          }),
        ]);
      } finally {
        clearTimeout(timer);
      }
    };
    const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const waitFor = async (predicate, label, ms = 2000) => {
      const deadline = performance.now() + ms;
      while (!predicate()) {
        if (performance.now() >= deadline) throw new Error(`${label} timed out after ${ms} ms`);
        await wait(5);
      }
    };

    const workletCode = `
      class RestartPcmCapture extends AudioWorkletProcessor {
        next = -1; // Chromium can repeat a quantum's currentFrame (capture-processor.ts); a real input never repeats
        constructor(options) {
          super();
          this.frames = options.processorOptions.frames;
          this.channels = options.processorOptions.channels;
          this.samples = [];
          for (let channel = 0; channel < this.channels; channel++) {
            this.samples.push(new Float32Array(this.frames));
          }
          this.base = -1;
          this.written = 0;
          this.ready = false;
          this.armRequested = false;
          this.armed = false;
          this.sent = false;
          this.port.onmessage = ({ data }) => {
            if (data === 'arm') this.armRequested = true;
          };
        }
        process(inputs) {
          const frame = Math.max(currentFrame, this.next);
          this.next = frame + 128;
          if (!this.ready) {
            this.ready = true;
            this.port.postMessage({ kind: 'ready', frame });
          }
          if (this.armRequested && !this.armed) {
            this.armed = true;
            this.base = frame;
            this.port.postMessage({ kind: 'armed', frame });
          }
          if (!this.armed || this.sent) return true;
          const input = inputs[0] ?? [];
          const count = Math.min(128, this.frames - this.written);
          for (let channel = 0; channel < this.channels; channel++) {
            const source = input[channel];
            const target = this.samples[channel];
            for (let k = 0; k < count; k++) target[this.written + k] = source?.[k] ?? 0;
          }
          this.written += count;
          if (this.written >= this.frames) {
            this.sent = true;
            this.port.postMessage({ kind: 'done', base: this.base, samples: this.samples });
          }
          return true;
        }
      }
      registerProcessor('restart-pcm-capture', RestartPcmCapture);
    `;
    const moduleUrl = URL.createObjectURL(new Blob([workletCode], { type: 'text/javascript' }));
    await ctx.audioWorklet.addModule(moduleUrl);
    URL.revokeObjectURL(moduleUrl);

    const startCapture = async (laneIndexes, seconds = 0.32) => {
      const frames = Math.ceil(seconds * ctx.sampleRate);
      const merger = ctx.createChannelMerger(laneIndexes.length);
      merger.channelInterpretation = 'discrete';
      const node = new AudioWorkletNode(ctx, 'restart-pcm-capture', {
        numberOfInputs: 1,
        numberOfOutputs: 1,
        outputChannelCount: [1],
        channelCount: laneIndexes.length,
        channelCountMode: 'explicit',
        channelInterpretation: 'discrete',
        processorOptions: { frames, channels: laneIndexes.length },
      });
      let readyResolve;
      let armedResolve;
      let doneResolve;
      const ready = new Promise((resolve) => { readyResolve = resolve; });
      const armed = new Promise((resolve) => { armedResolve = resolve; });
      const done = new Promise((resolve) => { doneResolve = resolve; });
      node.port.onmessage = ({ data }) => {
        if (data.kind === 'ready') readyResolve(data);
        if (data.kind === 'armed') armedResolve(data);
        if (data.kind === 'done') doneResolve(data);
      };
      // A `null` entry reserves a channel for a lane whose gain node does not exist yet: a COLD lane
      // builds it on its first start. `attach` wires that lane the instant it appears — still ahead of
      // the scheduled start time — so the capture base frame stays valid for the absolute-frame math.
      const attached = [];
      const attach = (channel, lane) => {
        engineState.tracks[lane].gain.connect(merger, 0, channel);
        attached.push(lane);
      };
      laneIndexes.forEach((lane, channel) => { if (lane !== null) attach(channel, lane); });
      merger.connect(node);
      node.connect(ctx.destination);
      await timeout(ready, 1500, 'capture ready handshake');
      node.port.postMessage('arm');
      const armedAt = await timeout(armed, 1500, 'capture arm handshake');
      return {
        armedAt: armedAt.frame,
        attach,
        done: timeout(done, (seconds + 2) * 1000, 'PCM capture'),
        cleanup() {
          for (const lane of attached) engineState.tracks[lane].gain?.disconnect(merger);
          merger.disconnect();
          node.disconnect();
          node.port.close();
        },
      };
    };

    const loopSeconds = 0.8;
    const rampSpan = 0.08;
    const amplitudes = [0.11, 0.23, 0.35, 0.47, 0.59];
    let masterFrames = 0;
    const sampleAt = (lane, frame) => Math.fround(amplitudes[lane] + rampSpan * frame / masterFrames);
    const load = async (count) => {
      lf.looper.clearAll();
      lf.looper.setLoopEndStopEnabled(false);
      lf.clock.setMetronome(false);
      masterFrames = Math.round(ctx.sampleRate * loopSeconds);
      await lf.looper.loadSession({
        bpm: 300,
        bars: 1,
        masterLengthFrames: masterFrames,
        tracks: Array.from({ length: count }, (_, lane) => ({
          index: lane,
          pcm: Float32Array.from({ length: masterFrames }, (_, frame) => sampleAt(lane, frame)),
          volume: 1,
          muted: false,
          reversed: false,
          fx: defaultFxStates(),
        })),
      });
    };
    const mod = (value, period) => ((value % period) + period) % period;
    const waitForOldPlaybackPhase = () => waitFor(() => {
      const phase = mod(ctx.currentTime - engineState.masterStartTime, loopSeconds) / loopSeconds;
      return phase > 0.3 && phase < 0.45;
    }, 'imported playback phase');
    const firstAudible = (samples, threshold = 0.01) => {
      for (let i = 0; i < samples.length; i++) if (Math.abs(samples[i]) > threshold) return i;
      return -1;
    };
    const topMeasurement = (data, lane, anchor) => {
      const samples = data.samples[lane];
      const first = firstAudible(samples);
      let maxRampError = 0;
      const compared = first < 0 ? 0 : Math.min(1024, samples.length - first);
      for (let k = 0; k < compared; k++) {
        maxRampError = Math.max(maxRampError, Math.abs(samples[first + k] - sampleAt(lane, k)));
      }
      const firstFrame = first < 0 ? null : data.base + first;
      const anchorFrame = Math.round(anchor * ctx.sampleRate);
      return {
        firstFrame,
        anchorFrame,
        frameError: firstFrame === null ? null : firstFrame - anchorFrame,
        firstSample: first < 0 ? null : samples[first],
        expectedFirstSample: sampleAt(lane, 0),
        firstSampleError: first < 0 ? null : samples[first] - sampleAt(lane, 0),
        maxRampError,
        compared,
      };
    };
    const phaseMeasurement = (data, lane, anchor) => {
      const samples = data.samples[0];
      const first = firstAudible(samples);
      let maxRampError = 0;
      let compared = 0;
      for (let k = Math.max(0, first); k < samples.length && compared < 1024; k++) {
        const phaseFrame = mod((data.base + k) - anchor * ctx.sampleRate, masterFrames);
        if (phaseFrame < 2 || phaseFrame > masterFrames - 2) continue;
        maxRampError = Math.max(maxRampError, Math.abs(samples[k] - sampleAt(lane, phaseFrame)));
        compared++;
      }
      const firstFrame = first < 0 ? null : data.base + first;
      const firstPhaseFrame = firstFrame === null ? null : mod(firstFrame - anchor * ctx.sampleRate, masterFrames);
      return {
        firstFrame,
        firstPhaseFrame,
        firstSample: first < 0 ? null : samples[first],
        expectedFirstSample: firstPhaseFrame === null ? null : sampleAt(lane, firstPhaseFrame),
        maxRampError,
        compared,
      };
    };

    const rows = [];

    // ── Cold-graph starts (run FIRST: `clear()` keeps a lane's gain/FX alive, so a lane is only ever
    // cold before its first session) ───────────────────────────────────────────────────────────────
    // A lane that has never played owns no gain/FX nodes, and its first start constructs the whole
    // FxChain. Inject that cost ONCE, into the first createGain a cold lane asks for (the first act of
    // preparePlaybackGraph). 60 ms is three times the 20 ms start lead, so an anchor read before the
    // build is measurably stale. Every loop source start is recorded too, so the LEAD (scheduled time
    // minus the clock at the call) is observable: a negative lead means the graph build ate it.
    const nativeCreateGain = AudioContext.prototype.createGain;
    const nativeStart = AudioBufferSourceNode.prototype.start;
    let coldGraphCostMs = 0;
    let starts = [];
    AudioContext.prototype.createGain = function (...args) {
      if (coldGraphCostMs > 0) {
        const until = performance.now() + coldGraphCostMs;
        coldGraphCostMs = 0;
        while (performance.now() < until) { /* injected cold-graph build cost */ }
      }
      return nativeCreateGain.apply(this, args);
    };
    AudioBufferSourceNode.prototype.start = function (...args) {
      // Every looping source is recorded; `laneStarts` keeps the master-length loop buffers, which is
      // what separates a lane from the click pulse's own looping source.
      if (this.loop) starts.push({ when: args[0], now: ctx.currentTime, offset: args[1], length: this.buffer?.length });
      return nativeStart.apply(this, args);
    };
    const laneStarts = (observed) => observed.filter((entry) => entry.length === masterFrames);
    try {
      // A STOPPED copy of a STOPPED lane has never played: nothing of its graph exists yet.
      await load(1);
      await waitForOldPlaybackPhase();
      lf.looper.stopAll();
      const coldLane = lf.looper.copy(0);
      const wasCold = coldLane > 0 && !engineState.tracks[coldLane].gain && !engineState.tracks[coldLane].fx;
      const oldColdAnchor = engineState.masterStartTime;
      const coldCapture = await startCapture([null]);
      let coldData;
      try {
        starts = [];
        coldGraphCostMs = 60;
        lf.looper.playStop(coldLane);
        coldCapture.attach(0, coldLane);
        const anchor = engineState.masterStartTime;
        const observed = laneStarts(starts);
        const lead = observed.length === 1 ? observed[0].when - observed[0].now : null;
        coldData = await coldCapture.done;
        const measurement = topMeasurement(coldData, 0, anchor);
        rows.push({
          name: 'cold first PLAY restarts the idle transport at its anchor and at source frame zero',
          pass: wasCold
            && anchor > oldColdAnchor
            && lead !== null && lead > 0.01
            && measurement.compared === 1024
            && Math.abs(measurement.frameError) <= 1
            && Math.abs(measurement.firstSampleError) < 0.00002
            && measurement.maxRampError < 0.00002,
          coldLane,
          wasCold,
          lead,
          observed,
          oldAnchor: oldColdAnchor,
          anchor,
          captureBaseFrame: coldData.base,
          armedFrame: coldCapture.armedAt,
          ...measurement,
        });
      } finally {
        coldGraphCostMs = 0;
        coldCapture.cleanup();
      }

      // COPY of a PLAYING lane goes through resume() with a cold graph and must join at the live phase.
      // Two lanes are loaded so the free lane COPY picks (lane 3) is still one that never played.
      await load(2);
      await waitForOldPlaybackPhase();
      const copyAnchor = engineState.masterStartTime;
      const copyCapture = await startCapture([null]);
      let copyData;
      try {
        starts = [];
        coldGraphCostMs = 60;
        // COPY lands in the first free lane; read its coldness BEFORE the copy builds the graph.
        const freeLane = engineState.tracks.findIndex((track) => track.state === 'EMPTY');
        const copyWasCold = freeLane > 1 && !engineState.tracks[freeLane].gain && !engineState.tracks[freeLane].fx;
        const copyLane = lf.looper.copy(0);
        copyCapture.attach(0, copyLane);
        const anchorAfterCopy = engineState.masterStartTime;
        const observed = laneStarts(starts);
        const lead = observed.length === 1 ? observed[0].when - observed[0].now : null;
        copyData = await copyCapture.done;
        const measurement = phaseMeasurement(copyData, 0, copyAnchor);
        rows.push({
          name: 'COPY into a cold lane joins the live phase with its start lead intact',
          pass: copyWasCold
            && copyLane === freeLane
            && anchorAfterCopy === copyAnchor
            && lead !== null && lead > 0.01
            && measurement.compared === 1024
            && measurement.firstPhaseFrame > masterFrames * 0.1
            && measurement.maxRampError < 0.00003,
          copyLane,
          freeLane,
          copyWasCold,
          lead,
          observed,
          anchor: copyAnchor,
          anchorAfterCopy,
          captureBaseFrame: copyData.base,
          armedFrame: copyCapture.armedAt,
          ...measurement,
        });
      } finally {
        coldGraphCostMs = 0;
        copyCapture.cleanup();
      }

      // ── Cold PLAY ALL: a STOPPED copy of a STOPPED lane has never played. Its graph must be built
      // before the SHARED anchor is read, or the build eats the lead of every lane in the fan-out.
      // Lane 4 (index 3) is the first free lane after load(3) and has never played in this session.
      // Lane 2 is copied so capture channel 1 compares against lane 2's seeded ramp (`sampleAt(1, …)`).
      await load(3);
      await waitForOldPlaybackPhase();
      lf.looper.stopAll();
      const allColdLane = lf.looper.copy(1);
      const allWasCold = allColdLane === 3 && !engineState.tracks[allColdLane].gain && !engineState.tracks[allColdLane].fx;
      const oldAllAnchor = engineState.masterStartTime;
      const allCapture = await startCapture([0, null]);
      let allData;
      try {
        starts = [];
        coldGraphCostMs = 60;
        lf.looper.playAll();
        allCapture.attach(1, allColdLane);
        const anchor = engineState.masterStartTime;
        const observed = laneStarts(starts);
        const leads = observed.map((entry) => entry.when - entry.now);
        allData = await allCapture.done;
        const measurements = [0, 1].map((channel) => topMeasurement(allData, channel, anchor));
        rows.push({
          name: 'cold PLAY ALL builds the never-played lane before the shared anchor is read',
          pass: allWasCold
            && anchor > oldAllAnchor
            && observed.length === 4
            && leads.every((lead) => lead > 0.01)
            && measurements.every((measurement) => measurement.compared === 1024
              && Math.abs(measurement.frameError) <= 1
              && measurement.maxRampError < 0.00002)
            && new Set(measurements.map((measurement) => measurement.firstFrame)).size === 1,
          allColdLane,
          allWasCold,
          leads,
          observed,
          oldAnchor: oldAllAnchor,
          anchor,
          measurements,
          captureBaseFrame: allData.base,
          armedFrame: allCapture.armedAt,
        });
      } finally {
        coldGraphCostMs = 0;
        allCapture.cleanup();
      }

      // ── PLAY ALL is best-effort per lane ───────────────────────────────────────────────
      // Lane 2's source start is failed on purpose (its seeded first sample identifies the buffer).
      // Lanes 1 and 3 must still start together from frame zero, lane 2 must read STOPPED, and the
      // fan-out must report ONE toast rather than one per lane or an aborted gesture.
      await load(3);
      await waitForOldPlaybackPhase();
      lf.looper.stopAll();
      for (const toast of toasts()) dismissToast(toast.id);
      const oldFailAnchor = engineState.masterStartTime;
      const failCapture = await startCapture([0, 1, 2]);
      let failData;
      let injectedStarts = 0;
      AudioBufferSourceNode.prototype.start = function (...args) {
        if (this.loop && Math.abs((this.buffer?.getChannelData(0)[0] ?? 0) - amplitudes[1]) < 0.001) {
          injectedStarts++;
          throw new Error('injected source start failure');
        }
        return nativeStart.apply(this, args);
      };
      try {
        lf.looper.playAll();
        const anchor = engineState.masterStartTime;
        const states = [0, 1, 2].map((lane) => lf.looper.stateOf(lane));
        const reported = toasts().map((toast) => ({ message: toast.message, detail: toast.detail, kind: toast.kind, count: toast.count }));
        const failedLaneSource = engineState.tracks[1].source === null;
        failData = await failCapture.done;
        const measurements = [0, 2].map((lane) => topMeasurement(failData, lane, anchor));
        const failedLane = topMeasurement(failData, 1, anchor);
        rows.push({
          name: 'PLAY ALL keeps the surviving lanes and reports the failed lane once',
          pass: injectedStarts === 1
            && anchor > oldFailAnchor
            && states[0] === 'PLAYING' && states[1] === 'STOPPED' && states[2] === 'PLAYING'
            && failedLaneSource
            && measurements.every((measurement) => measurement.compared === 1024
              && Math.abs(measurement.frameError) <= 1
              && Math.abs(measurement.firstSampleError) < 0.00002
              && measurement.maxRampError < 0.00002)
            && new Set(measurements.map((measurement) => measurement.firstFrame)).size === 1
            && failedLane.firstFrame === null
            && reported.length === 1 && reported[0].count === 1 && reported[0].kind === 'error'
            && reported[0].message.includes('2')
            && reported[0].detail?.startsWith('The other tracks are playing.'),
          injectedStarts,
          oldAnchor: oldFailAnchor,
          anchor,
          states,
          failedLaneSource,
          reported,
          measurements,
          failedLane,
          captureBaseFrame: failData.base,
          armedFrame: failCapture.armedAt,
        });
      } finally {
        AudioBufferSourceNode.prototype.start = nativeStart;
        failCapture.cleanup();
        for (const toast of toasts()) dismissToast(toast.id);
      }

      // ── PLAY ALL survives a lane whose PREP fails ──────────────────────────────────────
      // Five lanes; the middle one (track 3) fails in makeLoopBuffer: its createBuffer call is the third
      // master-length buffer PLAY ALL builds (lanes are prepared in order). The other four must start
      // together from frame zero, track 3 must stay STOPPED, and one toast must name track 3.
      await load(5);
      await waitForOldPlaybackPhase();
      lf.looper.stopAll();
      for (const toast of toasts()) dismissToast(toast.id);
      const oldPrepAnchor = engineState.masterStartTime;
      const prepCapture = await startCapture([0, 1, 2, 3, 4]);
      const nativeCreateBuffer = AudioContext.prototype.createBuffer;
      let loopBuffers = 0;
      let injectedPreps = 0;
      AudioContext.prototype.createBuffer = function (...args) {
        if (this === ctx && args[1] === masterFrames && ++loopBuffers === 3) {
          injectedPreps++;
          throw new Error('injected loop buffer prep failure');
        }
        return nativeCreateBuffer.apply(this, args);
      };
      try {
        lf.looper.playAll();
        AudioContext.prototype.createBuffer = nativeCreateBuffer;
        const anchor = engineState.masterStartTime;
        const states = [0, 1, 2, 3, 4].map((lane) => lf.looper.stateOf(lane));
        const reported = toasts().map((toast) => ({ message: toast.message, detail: toast.detail, kind: toast.kind, count: toast.count }));
        const failedLaneSource = engineState.tracks[2].source === null;
        const prepData = await prepCapture.done;
        const survivors = [0, 1, 3, 4];
        const measurements = survivors.map((lane) => topMeasurement(prepData, lane, anchor));
        const failedLane = topMeasurement(prepData, 2, anchor);
        rows.push({
          name: 'PLAY ALL keeps the surviving lanes when the middle lane fails to prepare',
          pass: injectedPreps === 1
            && anchor > oldPrepAnchor
            && survivors.every((lane) => states[lane] === 'PLAYING') && states[2] === 'STOPPED'
            && failedLaneSource
            && measurements.every((measurement) => measurement.compared === 1024
              && Math.abs(measurement.frameError) <= 1
              && Math.abs(measurement.firstSampleError) < 0.00002
              && measurement.maxRampError < 0.00002)
            && new Set(measurements.map((measurement) => measurement.firstFrame)).size === 1
            && failedLane.firstFrame === null
            && reported.length === 1 && reported[0].count === 1 && reported[0].kind === 'error'
            && reported[0].message.endsWith('track 3'),
          injectedPreps,
          oldAnchor: oldPrepAnchor,
          anchor,
          states,
          failedLaneSource,
          reported,
          measurements,
          failedLane,
          captureBaseFrame: prepData.base,
          armedFrame: prepCapture.armedAt,
        });
      } finally {
        AudioContext.prototype.createBuffer = nativeCreateBuffer;
        prepCapture.cleanup();
        for (const toast of toasts()) dismissToast(toast.id);
      }

      // ── PLAY ALL where every lane fails ────────────────────────────────────────────────
      // The toast must not claim that other tracks are playing when none started.
      await load(2);
      await waitForOldPlaybackPhase();
      lf.looper.stopAll();
      for (const toast of toasts()) dismissToast(toast.id);
      let injectedAll = 0;
      AudioBufferSourceNode.prototype.start = function (...args) {
        if (this.loop && this.buffer?.length === masterFrames) {
          injectedAll++;
          throw new Error('injected source start failure');
        }
        return nativeStart.apply(this, args);
      };
      try {
        lf.looper.playAll();
        AudioBufferSourceNode.prototype.start = nativeStart;
        const states = [0, 1].map((lane) => lf.looper.stateOf(lane));
        const reported = toasts().map((toast) => ({ message: toast.message, detail: toast.detail, kind: toast.kind, count: toast.count }));
        rows.push({
          name: 'PLAY ALL with every lane failing reports that no track started',
          pass: injectedAll === 2
            && states.every((state) => state === 'STOPPED')
            && [0, 1].every((lane) => engineState.tracks[lane].source === null)
            && reported.length === 1 && reported[0].count === 1 && reported[0].kind === 'error'
            && reported[0].message.endsWith('tracks 1, 2')
            && reported[0].detail === 'No track started. Press play on a track to retry.',
          injectedAll,
          states,
          reported,
        });
      } finally {
        AudioBufferSourceNode.prototype.start = nativeStart;
        for (const toast of toasts()) dismissToast(toast.id);
      }
    } finally {
      AudioContext.prototype.createGain = nativeCreateGain;
      AudioBufferSourceNode.prototype.start = nativeStart;
    }

    await load(1);
    await waitForOldPlaybackPhase();
    lf.looper.stopAll();
    const oldSingleAnchor = engineState.masterStartTime;
    const singleCapture = await startCapture([0]);
    let singleData;
    try {
      lf.looper.playStop(0);
      const anchor = engineState.masterStartTime;
      singleData = await singleCapture.done;
      const measurement = topMeasurement(singleData, 0, anchor);
      rows.push({
        name: 'single PLAY restarts idle transport at source frame zero',
        pass: anchor > oldSingleAnchor
          && measurement.compared === 1024
          && Math.abs(measurement.frameError) <= 1
          && Math.abs(measurement.firstSampleError) < 0.00002
          && measurement.maxRampError < 0.00002,
        oldAnchor: oldSingleAnchor,
        anchor,
        captureBaseFrame: singleData.base,
        armedFrame: singleCapture.armedAt,
        ...measurement,
      });
    } finally {
      singleCapture.cleanup();
    }

    await load(3);
    await waitForOldPlaybackPhase();
    lf.looper.stopAll();
    const oldAllAnchor = engineState.masterStartTime;
    const allCapture = await startCapture([0, 1, 2]);
    let allData;
    try {
      lf.looper.playAll();
      const anchor = engineState.masterStartTime;
      allData = await allCapture.done;
      const measurements = [0, 1, 2].map((lane) => topMeasurement(allData, lane, anchor));
      const firstFrames = measurements.map((measurement) => measurement.firstFrame);
      rows.push({
        name: 'PLAY ALL restarts every lane at one frame and source frame zero',
        pass: anchor > oldAllAnchor
          && measurements.every((measurement) => measurement.compared === 1024
            && Math.abs(measurement.frameError) <= 1
            && Math.abs(measurement.firstSampleError) < 0.00002
            && measurement.maxRampError < 0.00002)
          && new Set(firstFrames).size === 1,
        oldAnchor: oldAllAnchor,
        anchor,
        captureBaseFrame: allData.base,
        armedFrame: allCapture.armedAt,
        firstFrames,
        measurements,
      });
    } finally {
      allCapture.cleanup();
    }

    await load(2);
    lf.looper.playStop(1);
    lf.looper.setMute(0, true);
    await waitFor(() => {
      const phase = mod(ctx.currentTime - engineState.masterStartTime, loopSeconds) / loopSeconds;
      return phase > 0.12 && phase < 0.28;
    }, 'early live phase');
    lf.looper.setLoopEndStopEnabled(true);
    lf.looper.playStop(0);
    const liveAnchor = engineState.masterStartTime;
    const pendingStopAt = lf.looper.trackInfo(0).stopAt;
    const existingLane = {
      state: lf.looper.trackInfo(0).state,
      muted: lf.looper.trackMuted(0),
      stopAt: pendingStopAt,
    };
    const liveCapture = await startCapture([1]);
    let liveData;
    try {
      const joinAt = ctx.currentTime;
      lf.looper.playStop(1);
      const anchorAfterJoin = engineState.masterStartTime;
      liveData = await liveCapture.done;
      const measurement = phaseMeasurement(liveData, 1, liveAnchor);
      const startDelayFrames = measurement.firstFrame - Math.round(joinAt * ctx.sampleRate);
      const expectedDelayFrames = Math.round(0.02 * ctx.sampleRate);
      rows.push({
        name: 'live join keeps phase with a muted lane pending END STOP',
        pass: existingLane.state === 'PLAYING'
          && existingLane.muted
          && pendingStopAt !== null
          && anchorAfterJoin === liveAnchor
          && measurement.compared === 1024
          && measurement.firstPhaseFrame > masterFrames * 0.1
          && Math.abs(measurement.firstSample - amplitudes[1]) > 0.005
          && Math.abs(startDelayFrames - expectedDelayFrames) <= 128
          && measurement.maxRampError < 0.00003,
        anchor: liveAnchor,
        anchorAfterJoin,
        joinAt,
        startDelayFrames,
        expectedDelayFrames,
        pendingStopAt,
        existingLane,
        captureBaseFrame: liveData.base,
        armedFrame: liveCapture.armedAt,
        ...measurement,
      });
    } finally {
      liveCapture.cleanup();
    }

    lf.looper.setLoopEndStopEnabled(false);
    lf.looper.clearAll();
    return { sampleRate: ctx.sampleRate, masterFrames, rows };
  });

  const summary = {
    url,
    sampleRate: results.sampleRate,
    masterFrames: results.masterFrames,
    passed: results.rows.filter((row) => row.pass).length,
    total: results.rows.length,
    playFailureLogs: consoleErrors.filter((text) => text.includes('[looper] play failed')),
    results: results.rows,
  };
  console.log(JSON.stringify(summary, null, 2));
  assert.ok(results.rows.length > 0, 'probe produced no measurements');
  assert.equal(summary.playFailureLogs.length, 4, 'each failed PLAY ALL lane (start 1 + prep 1 + all-fail 2) must reach the release log exactly once');
  for (const result of results.rows) assert.ok(result.pass, `${result.name}: ${JSON.stringify(result)}`);
});

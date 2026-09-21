/**
 * Rendered-PCM proof for idle playback restart and live phase joins. Run with Vite on :1420:
 *   pnpm exec node verify/playback-restart.mjs
 *   pnpm exec node verify/playback-restart.mjs --url=http://localhost:1421
 *
 * Captures each lane at its GainNode with absolute AudioWorklet render frames. Seeded ramps make
 * source frame zero and live phase observable in PCM. This does not establish native ASIO feel.
 */
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.env.LF_URL
  ?? process.argv.find((arg) => arg.startsWith('--url='))?.slice(6)
  ?? 'http://localhost:1420';
const browser = await chromium.launch({ headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });

try {
  const page = await browser.newPage();
  const browserErrors = [];
  page.on('pageerror', (error) => browserErrors.push(String(error)));
  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf);

  const results = await page.evaluate(async () => {
    const lf = window.__lf;
    const { engineState } = await import('/src/audio/looper/state.ts');
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
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
          if (!this.ready) {
            this.ready = true;
            this.port.postMessage({ kind: 'ready', frame: currentFrame });
          }
          if (this.armRequested && !this.armed) {
            this.armed = true;
            this.base = currentFrame;
            this.port.postMessage({ kind: 'armed', frame: currentFrame });
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
      laneIndexes.forEach((lane, channel) => engineState.tracks[lane].gain.connect(merger, 0, channel));
      merger.connect(node);
      node.connect(ctx.destination);
      await timeout(ready, 1500, 'capture ready handshake');
      node.port.postMessage('arm');
      const armedAt = await timeout(armed, 1500, 'capture arm handshake');
      return {
        armedAt: armedAt.frame,
        done: timeout(done, (seconds + 2) * 1000, 'PCM capture'),
        cleanup() {
          for (const lane of laneIndexes) engineState.tracks[lane].gain.disconnect(merger);
          merger.disconnect();
          node.disconnect();
          node.port.close();
        },
      };
    };

    const loopSeconds = 0.8;
    const rampSpan = 0.08;
    const amplitudes = [0.11, 0.23, 0.35];
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
      const measurements = amplitudes.map((_, lane) => topMeasurement(allData, lane, anchor));
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
    browserErrors,
    results: results.rows,
  };
  console.log(JSON.stringify(summary, null, 2));
  assert.equal(browserErrors.length, 0, `uncaught browser errors: ${browserErrors.join('\n')}`);
  assert.ok(results.rows.length > 0, 'probe produced no measurements');
  for (const result of results.rows) assert.ok(result.pass, `${result.name}: ${JSON.stringify(result)}`);
} finally {
  await browser.close();
}

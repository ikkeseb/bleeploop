/**
 * Checks first/later/AUTO/FIXED capture windows, padding, cancellation, recorder release,
 * playback-failure retry and the 60-second capacity cap. A delayed absolute-frame-coded AudioWorklet
 * models native wet arrival using production's own frozen compensation (C). A manual first-take stop
 * keeps its existing press/pad rule; a later stop chooses completed whole bars and tiles that window
 * across the master. `--first-only` restricts the run to first-take cases.
 * Run: pnpm probe record-stop-window [--first-only] [--churn=<nodes>] [--url=<server>] (about 165 s for
 * the full matrix). `--churn=60` makes Chromium hand quanta a stale currentFrame (capture-processor.ts).
 * No native driver or physical latency is measured here.
 *
 * @no-ci intermittent: since the stale-currentFrame fix (a8738fa), 1 of 8 full runs on the PC missed one 128-frame quantum (AUTO recDub; before it, ~1 of 3 in the 60 s capacity case); cause unknown; the next red run's firstMisses locates it
 */
import { probe, flag, arg } from '../harness/probe.ts';
import assert from 'node:assert/strict';

const firstOnly = flag('first-only');
const churn = Number(arg('churn') ?? 0);

await probe(async ({ open }) => {
  const { page } = await open();
  await page.evaluate(() => window.__lf.autosave.ready());
  const results = [];
  const cases = ['first', 'later'].flatMap((take) => [0, 150].flatMap((trim) =>
    ['recDub', 'playStop', 'stopAll', 'clear'].map((mode) => ({ take, mode, trim }))));
  cases.push(...['first', 'later'].flatMap((take) => ['repeat', 'loss'].map((mode) => ({ take, mode, trim: 150 }))));
  cases.push({ take: 'later', mode: 'afterLoop', trim: 150 });
  cases.push(...['fixed', 'auto', 'autoFixed'].flatMap((take) =>
    ['recDub', 'repeat', 'automatic'].map((mode) => ({ take, mode, trim: 150 }))));
  cases.push(...['clearOther', 'lossOther', 'bufferFailure', 'sourceFailure', 'wholeBar', 'earlyBar'].map((mode) => ({ take: mode.endsWith('Other') ? 'later' : 'first', mode, trim: 150 })));
  cases.push(...['wholeBar', 'earlyBar'].map((mode) => ({ take: 'fixed', mode, trim: 150 })));
  cases.push({ take: 'first', mode: 'capacity', trim: 150 });
  for (const testCase of cases.filter(({ take }) => !firstOnly || take === 'first')) {
    const result = await page.evaluate(async ({ take, mode, trim, churn }) => {
      // --churn: connect and drop graph nodes every 5 ms so the main thread holds the graph lock and
      // Chromium hands quanta a stale currentFrame (capture-processor.ts).
      const churnTimer = churn && setInterval(() => {
        const nodes = Array.from({ length: churn }, () => window.__lf.engine.ctx.createGain());
        for (const node of nodes) node.connect(window.__lf.engine.ctx.destination);
        for (const node of nodes) node.disconnect();
      }, 5);
      const lf = window.__lf;
      await lf.looper.init();
      const { engineState } = await import('/src/audio/looper/state.ts');
      if (!engineState.initialized) throw new Error('Restart Vite: the probe imported a different HMR state module');
      const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
      const ctx = lf.engine.ctx;
      const sr = ctx.sampleRate;
      lf.master.setMuted(true);
      const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
      const until = async (time) => {
        const deadline = performance.now() + Math.max(5000, (time - ctx.currentTime) * 1000 + 5000);
        while (ctx.currentTime < time) {
          if (performance.now() > deadline) throw new Error('Audio clock did not advance');
          await pause(1);
        }
      };
      lf.looper.clearAll();
      lf.clock.setBpm(120);
      const auto = take.startsWith('auto');
      lf.looper.setAutoRecordEnabled(auto);
      lf.looper.setAutoRecordSensitivity(50);
      lf.looper.setFixedLengthEnabled(take === 'fixed' || take === 'autoFixed' || mode === 'automatic');
      lf.looper.setFixedLengthBars(mode === 'automatic' ? 1 : 4);
      const index = take === 'later' ? 1 : 0;
      if (take === 'later') await lf.looper.loadSession({ bpm: 120, bars: 2, masterLengthFrames: sr * 4,
        tracks: [{ index: 0, pcm: new Float32Array(sr * 4), volume: 1, muted: false, reversed: false, fx: defaultFxStates() }] });
      lf.recordLatency.setEnabled(trim > 0);
      lf.recordLatency.setFloorEnabled(false);
      lf.recordLatency.setOffsetMs(trim);
      lf.recordLatency.beginMonitorGeneration(0, 0);
      const compensation = lf.recordLatency.recordCompensationFrames();
      const onset = auto ? Math.ceil((ctx.currentTime + 0.15) * sr) : 0;
      const expectedSample = (frame) => frame < onset ? 0 :
        auto && frame < onset + 128 ? Math.fround((frame - onset + 1) / 65536) : frame - compensation + 1;
      const starts = [];
      const createSource = ctx.createBufferSource;
      ctx.createBufferSource = function () {
        const node = createSource.call(this);
        const start = node.start;
        node.start = function (when, offset) { starts.push({ node, when, offset }); return start.call(this, when, offset); };
        return node;
      };
      const url = URL.createObjectURL(new Blob([`
        class Frames extends AudioWorkletProcessor {
          next = -1; // Chromium can repeat a quantum's currentFrame (capture-processor.ts); a real input never repeats
          process(_inputs, outputs) {
            const out = outputs[0][0];
            const base = Math.max(currentFrame, this.next);
            this.next = base + out.length;
            for (let k = 0; k < out.length; k++) {
              const frame = base + k;
              out[k] = frame < ${onset} ? 0 :
                ${auto} && frame < ${onset + 128} ? (frame - ${onset} + 1) / 65536 : frame - ${compensation} + 1;
            }
            return true;
          }
        }
        registerProcessor('record-stop-${take}-${mode}-${trim}', Frames);
      `], { type: 'text/javascript' }));
      await ctx.audioWorklet.addModule(url); URL.revokeObjectURL(url);
      const source = new AudioWorkletNode(ctx, `record-stop-${take}-${mode}-${trim}`, { numberOfInputs: 0, outputChannelCount: [1] });
      source.connect(lf.engine.recordTap); // Silent branch only; frame values never reach the speakers.
      await lf.looper.recDub(index);
      if (auto) {
        const timeout = performance.now() + 5000;
        while (lf.looper.trackInfo(index).autoArmed) {
          if (performance.now() > timeout) throw new Error('AUTO did not trigger');
          await pause(5);
        }
      }
      const captureStart = engineState.captureStartFrame;
      if (captureStart === null) throw new Error('No active capture window');
      const automaticEnd = engineState.captureEndFrame;
      const musicalStart = captureStart - compensation;
      const nearBar = mode === 'afterLoop' || mode === 'wholeBar' || mode === 'earlyBar';
      await until(mode === 'afterLoop' ? musicalStart / sr + 4.02 : nearBar ? musicalStart / sr + (mode === 'earlyBar' ? 1.94 : 2.02) : captureStart / sr + 0.30);
      const createBuffer = ctx.createBuffer;
      if (mode === 'bufferFailure') ctx.createBuffer = () => { throw new Error('injected buffer allocation failure'); };
      if (mode === 'sourceFailure') ctx.createBufferSource = () => { throw new Error('injected source allocation failure'); };
      // Hold this gesture's clock observation across recDub's init microtasks. The audio renderer
      // keeps running. Suspending the context is unsuitable: init resumes it during the gesture.
      const stopTime = ctx.currentTime;
      const stopFrame = Math.round(stopTime * sr);
      const overrunsBefore = lf.looper.captureOverruns();
      Object.defineProperty(ctx, 'currentTime', { configurable: true, get: () => stopTime });
      try {
        if (mode === 'automatic' || mode === 'capacity') { /* let the configured deadline finish */ }
        else if (['recDub', 'bufferFailure', 'sourceFailure', 'wholeBar', 'earlyBar'].includes(mode)) await lf.looper.recDub(index);
        else if (mode === 'stopAll') lf.looper.stopAll();
        else lf.looper.playStop(index);
      } finally { delete ctx.currentTime; }
      const stateAtStop = lf.looper.stateOf(index);
      const frameDeadline = engineState.captureEndFrame;
      if (mode === 'clearOther' || mode === 'lossOther') lf.looper.clear(0);
      if (mode === 'loss' || mode === 'lossOther') Atomics.add(engineState.heartbeat, 1, 128);
      if (mode === 'repeat' || mode === 'clear') {
        await until(stopFrame / sr + 0.04);
        if (mode === 'clear') lf.looper.clear(index);
        else lf.looper.playStop(index);
      }
      await until(mode === 'automatic' || mode === 'capacity' ? automaticEnd / sr + 0.12 : Math.max(stopFrame / sr + compensation / sr, (frameDeadline ?? 0) / sr) + 0.12);
      ctx.createBuffer = createBuffer;
      ctx.createBufferSource = createSource;
      const finalState = lf.looper.stateOf(index);
      const pcm = lf.looper.exportSnapshot().tracks.find((track) => track.index === index)?.pcm;
      const loopFrames = sr * (mode === 'capacity' ? 60 : take === 'later' ? 4 : 2);
      const expectedFrames = mode === 'capacity' ? loopFrames : take === 'later' ? (mode === 'afterLoop' ? loopFrames : sr * 2) : ['wholeBar', 'earlyBar', 'automatic'].includes(mode) ? sr * 2 : Math.min(sr * 2, stopFrame - musicalStart);
      let missing = 0, extra = 0;
      const firstMisses = []; // [k, got, want, absolute frame]: enough to tell a shifted window from one bad quantum
      if (pcm) {
        for (let k = 0; k < pcm.length; k++) {
          const expected = take === 'later'
            ? expectedSample(captureStart + k % expectedFrames)
            : k < expectedFrames ? expectedSample(captureStart + k) : 0;
          if (pcm[k] !== expected) {
            if (firstMisses.length < 3) firstMisses.push([k, pcm[k], expected, captureStart + k]);
            if (k < expectedFrames || take === 'later') missing++;
            else extra++;
          }
        }
      }
      let canRetryPlayback = true;
      if (mode === 'bufferFailure' || mode === 'sourceFailure') {
        lf.looper.playStop(index);
        canRetryPlayback = lf.looper.stateOf(index) === 'PLAYING' && engineState.tracks[index].source !== null;
      }
      const activeAfter = engineState.activeRecordIndex;
      const playback = starts.find((entry) => entry.node === engineState.tracks[index].source);
      const playbackPhase = playback ? ((playback.when - musicalStart / sr) % 2 + 2) % 2 : null;
      const playbackOffset = playback?.offset ?? null;
      const masterAfter = lf.looper.masterLengthFrames();
      const stopIntentAfter = engineState.captureStopPlayback;
      const windowAfter = engineState.captureStartFrame;
      const softOnsetRetained = !auto || (captureStart <= onset && pcm?.[onset - captureStart] === expectedSample(onset));
      await lf.looper.recDub(2);
      const canRecordNext = lf.looper.stateOf(2) === 'RECORDING';
      lf.looper.clear(2);
      const result = { take, mode, trim, sr, compensation, expectedFrames, loopFrames, stateAtStop, finalState,
        frames: pcm?.length ?? 0, missing, extra, activeAfter, canRecordNext, frameDeadline, automaticEnd, playbackPhase, playbackOffset, masterAfter, stopIntentAfter, windowAfter, softOnsetRetained, canRetryPlayback,
        head: pcm ? Array.from(pcm.subarray(0, 4)) : [], expectedHead: musicalStart + 1,
        overruns: lf.looper.captureOverruns() - overrunsBefore, firstMisses };
      clearInterval(churnTimer);
      source.disconnect(); lf.looper.clearAll(); lf.recordLatency.clearMonitor(0); lf.recordLatency.setOffsetMs(0);
      return result;
    }, { ...testCase, churn });
    console.log(JSON.stringify(result));
    results.push(result);
  }
  for (const result of results) {
    const discarded = ['clear', 'loss', 'lossOther'].includes(result.mode);
    const plays = ['recDub', 'automatic', 'capacity', 'wholeBar', 'earlyBar'].includes(result.mode);
    assert.equal(result.overruns, result.mode.startsWith('loss') ? 128 : 0, 'only the injected capture loss is allowed');
    assert.equal(result.finalState, discarded ? 'EMPTY' : plays ? 'PLAYING' : 'STOPPED');
    assert.equal(result.stopIntentAfter, false, 'completion clears stop intent');
    assert.equal(result.windowAfter, null, 'completion clears the capture window');
    if (result.mode === 'lossOther') assert.equal(result.masterAfter, 0, 'no remaining lane means a blank grid');
    if (result.take !== 'later' && result.mode === 'recDub') {
      assert.ok(Math.min(result.playbackPhase, 2 - result.playbackPhase) < 1 / result.sr, 'short first take waits for the counted downbeat');
      assert.equal(result.playbackOffset, 0, 'short first take begins at sample zero');
    }
    assert.equal(result.activeAfter, -1, 'completion must release the recorder');
    assert.ok(result.canRetryPlayback, 'a saved take can resume after transient playback failure');
    assert.ok(result.canRecordNext, 'another lane must be able to record after completion');
    if (discarded) assert.equal(result.frames, 0, 'CLEAR or capture loss must discard the pending take');
    else {
      assert.equal(result.frames, result.loopFrames, 'committed length must follow the existing musical rule');
      assert.equal(result.missing, 0, result.take === 'later' ? 'later stop must tile its whole-bar window across the master' : 'first stop must keep every frame performed before the press');
      assert.equal(result.extra, 0, 'first-take padding must remain silent');
      assert.ok(result.softOnsetRetained, 'AUTO must retain the soft onset before its trigger');
    }
  }
  console.log(`PASS: ${results.length} recording windows, zero missing or extra samples`);
});

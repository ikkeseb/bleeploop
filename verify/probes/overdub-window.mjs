// Browser rig on :1420. Synthetic wet audio is delayed by the compensation returned by production.
// Measures the musical punch-in/out window through real looper dispatchers, not a formula port.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';

const browser = await chromium.launch({ headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage();
  await page.goto(process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  await page.waitForFunction(() => !!window.__lf);
  const requestedMode = process.argv.find((arg) => arg.startsWith('--mode='))?.slice(7);
  for (const mode of requestedMode ? [requestedMode] : ['dub', 'playStop', 'stopAll', 'doubleStop', 'clear']) {
  const result = await page.evaluate(async (mode) => {
    const lf = window.__lf;
    await lf.looper.init();
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const { engineState } = await import('/src/audio/looper/state.ts');
    const compensation = lf.recordLatency;
    const ctx = lf.engine.ctx;
    const sr = ctx.sampleRate;
    const master = sr;
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const until = async (time) => {
      while (ctx.currentTime < time) await pause(1);
    };
    lf.looper.clearAll();
    await lf.looper.loadSession({
      bpm: 240, bars: 1, masterLengthFrames: master,
      tracks: [{ index: 0, pcm: new Float32Array(master).fill(0.05), volume: 1, muted: false, reversed: false, fx: defaultFxStates() }],
    });
    // Register synthetic native monitoring. No hardware path is involved: the impulses below model
    // its wet arrival exactly, using production's frozen C rather than assuming a driver's latency.
    compensation.setEnabled(true);
    compensation.setFloorEnabled(false);
    compensation.setOffsetMs(150);
    compensation.beginMonitorGeneration(0, 0);
    const cFrames = compensation.recordCompensationFrames();
    const c = cFrames / sr;
    if (c < 0.1 || c > 0.5) throw new Error(`Unexpected synthetic compensation ${c}s`);
    await until(ctx.currentTime + 0.15);
    await lf.looper.recDub(0);
    const punchIn = ctx.currentTime;
    const anchor = engineState.masterStartTime;
    const intended = [
      { name: 'before-punch-in', time: punchIn - 0.04, value: 0.25 },
      { name: 'inside-early', time: punchIn + 0.04, value: 0.5 },
      { name: 'inside-late', time: punchIn + c + 0.08, value: 0.75 },
    ];
    for (const event of intended) {
      const buffer = ctx.createBuffer(1, 1, sr);
      buffer.getChannelData(0)[0] = event.value;
      const source = ctx.createBufferSource();
      source.buffer = buffer;
      source.connect(lf.engine.recordTap);
      source.start(Math.round((event.time + c) * sr) / sr);
    }
    await until(punchIn + c + 0.10);
    const punchOut = ctx.currentTime;
    const analyser = ctx.createAnalyser();
    analyser.fftSize = 256;
    engineState.tracks[0].gain.connect(analyser);
    const silent = ctx.createGain(); silent.gain.value = 0;
    analyser.connect(silent); silent.connect(ctx.destination);
    if (mode === 'dub') await lf.looper.recDub(0);
    else if (mode === 'clear') lf.looper.clear(0);
    else if (mode === 'stopAll') lf.looper.stopAll();
    else { lf.looper.playStop(0); if (mode === 'doubleStop') lf.looper.playStop(0); }
    const stateAtPunchOut = lf.looper.stateOf(0);
    const after = { name: 'after-punch-out', time: punchOut + 0.04, value: 0.9 };
    intended.push(after);
    const afterBuffer = ctx.createBuffer(1, 1, sr);
    afterBuffer.getChannelData(0)[0] = after.value;
    const afterSource = ctx.createBufferSource(); afterSource.buffer = afterBuffer;
    afterSource.connect(lf.engine.recordTap);
    afterSource.start(Math.round((after.time + c) * sr) / sr);
    await until(punchOut + 0.02);
    const audible = new Float32Array(256); analyser.getFloatTimeDomainData(audible);
    const peakAfterStop = Math.max(...audible.map(Math.abs));
    engineState.tracks[0].gain.disconnect(analyser);
    analyser.disconnect(); silent.disconnect();
    await until(punchOut + c + 0.12);
    const pcm = lf.looper.exportSnapshot().tracks.find((track) => track.index === 0)?.pcm ?? new Float32Array(master);
    const finalState = lf.looper.stateOf(0);
    const observed = intended.map((event) => {
      const positions = [];
      for (let k = 0; k < pcm.length; k++) if (Math.abs(pcm[k] - (event.value + 0.05)) < 1e-5) positions.push(k);
      const expected = ((Math.round((event.time - anchor) * sr) % master) + master) % master;
      const errors = positions.map((position) => ((position - expected + master / 2) % master + master) % master - master / 2);
      return { ...event, inWindow: mode !== 'clear' && event.time >= punchIn && event.time < punchOut, positions, expected, errors };
    });
    compensation.clearMonitor(0);
    compensation.setOffsetMs(0);
    const overruns = lf.looper.captureOverruns();
    lf.looper.clearAll();
    return { mode, sr, cFrames, punchIn, punchOut, stateAtPunchOut, finalState, peakAfterStop, observed, overruns };
  }, mode);
  console.log(JSON.stringify(result));
  assert.equal(result.overruns, 0, 'capture overrun invalidates the timing probe');
  assert.equal(result.finalState, mode === 'dub' ? 'PLAYING' : mode === 'clear' ? 'EMPTY' : 'STOPPED');
  if (mode !== 'dub') assert.ok(result.peakAfterStop < 1e-6, 'STOP must silence playback while capture tail drains');
  for (const event of result.observed) {
    assert.equal(event.positions.length, event.inWindow ? 1 : 0,
      `${event.name}: compensated recording must preserve exactly the musical punch window`);
    if (event.inWindow) assert.ok(Math.abs(event.errors[0]) <= 1, `${event.name}: overdub must match the absolute grid`);
  }
  }
  const zeroCompensation = await page.evaluate(async () => {
    const lf = window.__lf;
    const ctx = lf.engine.ctx;
    const sr = ctx.sampleRate;
    const { engineState } = await import('/src/audio/looper/state.ts');
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    lf.looper.clearAll();
    lf.recordLatency.setEnabled(false);
    await lf.looper.loadSession({ bpm: 240, bars: 1, masterLengthFrames: sr,
      tracks: [{ index: 0, pcm: new Float32Array(sr), volume: 0, muted: false, reversed: false, fx: defaultFxStates() }] });
    const moduleUrl = URL.createObjectURL(new Blob([`
      class Frames extends AudioWorkletProcessor {
        process(_inputs, outputs) {
          const out = outputs[0][0];
          for (let k = 0; k < out.length; k++) out[k] = currentFrame + k + 1;
          return true;
        }
      }
      registerProcessor('overdub-head-frames', Frames);
    `], { type: 'text/javascript' }));
    await ctx.audioWorklet.addModule(moduleUrl); URL.revokeObjectURL(moduleUrl);
    const source = new AudioWorkletNode(ctx, 'overdub-head-frames', { numberOfInputs: 0, outputChannelCount: [1] });
    source.connect(lf.engine.recordTap);
    await pause(100);
    const createBuffer = ctx.createBuffer;
    let setupStalls = 0;
    ctx.createBuffer = function (...args) {
      const buffer = createBuffer.apply(this, args);
      const start = performance.now();
      while (performance.now() - start < 8) { /* producer continues during overdub buffer setup */ }
      setupStalls++;
      return buffer;
    };
    try { await lf.looper.recDub(0); } finally { ctx.createBuffer = createBuffer; }
    const firstFrame = engineState.captureStartFrame;
    const gridFrame = Math.round(engineState.masterStartTime * sr);
    while (ctx.currentTime < firstFrame / sr + 0.1) await pause(1);
    await lf.looper.recDub(0);
    const deadline = performance.now() + 2000;
    while (lf.looper.stateOf(0) === 'OVERDUBBING' && performance.now() < deadline) await pause(1);
    if (lf.looper.stateOf(0) === 'OVERDUBBING') throw new Error('overdub tail did not finish');
    const pcm = lf.looper.exportSnapshot().tracks[0].pcm;
    const checked = Math.round(sr * 0.03);
    let wrong = 0;
    for (let k = 0; k < checked; k++) {
      const index = ((firstFrame + k - gridFrame) % sr + sr) % sr;
      if (pcm[index] !== firstFrame + k + 1) wrong++;
    }
    source.disconnect(); lf.looper.clearAll();
    return { setupStalls, firstFrame, checked, wrong };
  });
  console.log(JSON.stringify({ zeroCompensation }));
  assert.ok(zeroCompensation.setupStalls >= 2, 'exercise both overdub buffer allocations');
  assert.equal(zeroCompensation.wrong, 0, 'C=0 must retain the complete punch-in head during setup stalls');
} finally {
  await browser.close();
}

/**
 * Absolute-frame-coded AudioWorklet proof of real capture alignment: measures absolute capture
 * frames under producer/consumer interleaving (`--stall-ms=` controls the injected pop stall, 0
 * disables it) and checks that FIXED/AUTO/free takes, with and without compensation, land on the
 * exact absolute sample grid and tile with zero drift. Requires the browser rig.
 * Run: pnpm probe capture-clock [--stall-ms=<ms>] [--url=<server>]
 * Proves frame-accurate capture in Chromium's Web Audio renderer; it does not establish native
 * ASIO timing, device jitter or anything audible on the rig.
 */
import { probe, arg } from '../harness/probe.ts';
import assert from 'node:assert/strict';

const stallMs = Number(arg('stall-ms') ?? 8);

await probe(async ({ open }) => {
  const { page } = await open();
  const result = await page.evaluate(async (stallMs) => {
    const lf = window.__lf;
    await lf.looper.init();
    lf.recordLatency.setEnabled(false);
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const { engineState } = await import('/src/audio/looper/state.ts');
    const ctx = lf.engine.ctx;
    const sr = ctx.sampleRate;
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const workletUrl = URL.createObjectURL(new Blob([`
      class AbsoluteFrame extends AudioWorkletProcessor {
        next = -1; // Chromium can repeat a quantum's currentFrame (capture-processor.ts); a real input never repeats
        process(_inputs, outputs) {
          const out = outputs[0][0];
          const base = Math.max(currentFrame, this.next);
          this.next = base + out.length;
          for (let k = 0; k < out.length; k++) out[k] = base + k + 1;
          return true;
        }
      }
      registerProcessor('absolute-frame', AbsoluteFrame);
    `], { type: 'text/javascript' }));
    await ctx.audioWorklet.addModule(workletUrl);
    URL.revokeObjectURL(workletUrl);
    const source = new AudioWorkletNode(ctx, 'absolute-frame', { numberOfInputs: 0, outputChannelCount: [1] });
    source.connect(lf.engine.recordTap);
    await pause(100);

    // Directly test the old premise that AudioContext.currentTime cannot change within one JS task.
    const taskClock = [];
    const wallStart = performance.now();
    let previous = ctx.currentTime;
    taskClock.push(previous);
    while (performance.now() - wallStart < 40) {
      const time = ctx.currentTime;
      if (time !== previous) { taskClock.push(time); previous = time; }
    }

    lf.looper.clearAll();
    await lf.looper.loadSession({
      bpm: 240, bars: 1, masterLengthFrames: sr,
      tracks: [{ index: 0, pcm: new Float32Array(sr), volume: 0, muted: false, reversed: false, fx: defaultFxStates() }],
    });
    const anchor = engineState.masterStartTime;
    const rows = [];
    for (const delayMs of [0, 0, stallMs, stallMs]) {
      lf.looper.clear(1);
      lf.looper.setVolume(1, 0);
      // Arm away from the boundary, so the injected stall cannot legitimately choose the next loop.
      while (((ctx.currentTime - anchor) % 1 + 1) % 1 > 0.4) await pause(5);
      await pause(30);
      const ring = engineState.ring;
      // The periodic drain can empty the ring immediately before this gesture. Wait for an actual
      // producer quantum so drainStaleFrames necessarily reaches the pop hook under test.
      const fillDeadline = performance.now() + 1000;
      while (ring.available_read() === 0 && performance.now() < fillDeadline) await pause(1);
      if (ring.available_read() === 0) throw new Error('Capture producer did not supply a quantum');
      const originalPop = ring.pop;
      let observation = null;
      ring.pop = function (target, count) {
        const before = ctx.currentTime;
        const got = originalPop.call(this, target, count);
        const frontier = got > 0 ? target[got - 1] : null;
        // A controllable real producer/consumer interleaving, after the actual stale PCM discard.
        const wall = performance.now();
        while (performance.now() - wall < delayMs) { /* audio rendering continues */ }
        observation = { before, after: ctx.currentTime, discardedEndFrame: frontier, got };
        ring.pop = originalPop;
        return got;
      };
      try {
        await lf.looper.recDub(1);
      } finally {
        ring.pop = originalPop;
      }
      if (!observation) throw new Error('The stale-drain hook was not reached');
      const intendedFrame = Math.round((anchor + Math.ceil(observation.after - anchor)) * sr);
      const pendingFrames = engineState.pendingRecordStartFrame;
      const deadline = performance.now() + 3500;
      while (lf.looper.stateOf(1) !== 'PLAYING' && performance.now() < deadline) await pause(5);
      if (lf.looper.stateOf(1) !== 'PLAYING') throw new Error('Later recording did not complete');
      const pcm = lf.looper.exportSnapshot().tracks.find((track) => track.index === 1).pcm;
      const capturedFrame = pcm[0] - 1;
      let discontinuities = 0;
      for (let k = 1; k < pcm.length; k++) if (pcm[k] !== pcm[k - 1] + 1) discontinuities++;
      rows.push({ delayMs, ...observation, pendingFrames, intendedFrame, capturedFrame,
        errorFrames: capturedFrame - intendedFrame, discontinuities });
    }
    // The first take defines the grid, so comparing two tracks alone cannot reveal a uniform slip.
    // Decode its absolute frame zero and compare both the capture deadline and the resulting grid.
    for (const kind of ['fixed', 'free', 'auto', 'fixed-compensated', 'free-compensated', 'auto-compensated']) {
      lf.looper.clearAll();
      lf.clock.setBpm(240);
      lf.looper.setVolume(0, 0);
      const auto = kind.startsWith('auto');
      const free = kind.startsWith('free');
      lf.looper.setAutoRecordEnabled(auto);
      lf.looper.setFixedLengthEnabled(!free);
      lf.looper.setFixedLengthBars(1);
      const compensated = kind.endsWith('-compensated');
      lf.recordLatency.setEnabled(compensated);
      lf.recordLatency.setOffsetMs(compensated ? 100 : 0);
      if (compensated) lf.recordLatency.beginMonitorGeneration(0, 0);
      const cFrames = lf.recordLatency.recordCompensationFrames();
      await lf.looper.recDub(0);
      const startDeadline = performance.now() + 2000;
      while (engineState.captureStartFrame === null && performance.now() < startDeadline) await pause(2);
      const expectedFrame = engineState.captureStartFrame;
      if (expectedFrame === null) throw new Error(`${kind}: did not establish a capture deadline`);
      if (free) {
        const punchOut = (expectedFrame - cFrames) / sr + 1.03;
        while (ctx.currentTime < punchOut) await pause(2);
        await lf.looper.recDub(0);
      }
      const deadline = performance.now() + 3500;
      while (lf.looper.stateOf(0) !== 'PLAYING' && performance.now() < deadline) await pause(5);
      if (lf.looper.stateOf(0) !== 'PLAYING') throw new Error(`${kind}: first take did not complete`);
      const snapshot = lf.looper.exportSnapshot();
      const pcm = snapshot.tracks[0].pcm;
      const capturedFrame = pcm[0] - 1;
      let discontinuities = 0;
      for (let k = 1; k < pcm.length; k++) if (pcm[k] !== pcm[k - 1] + 1) discontinuities++;
      const gridFrame = Math.round(engineState.masterStartTime * sr);
      const gridError = ((capturedFrame - cFrames - gridFrame) % pcm.length + pcm.length) % pcm.length;
      rows.push({ kind, expectedFrame, capturedFrame, cFrames, gridFrame, gridError,
        errorFrames: capturedFrame - expectedFrame, discontinuities, frames: pcm.length });
      lf.recordLatency.clearMonitor();
    }
    lf.recordLatency.setOffsetMs(0);
    lf.looper.setAutoRecordEnabled(false);
    lf.looper.setFixedLengthEnabled(false);
    source.disconnect();
    lf.looper.clearAll();
    return { sr, taskClock, taskClockAdvanced: taskClock.length > 1, rows, overruns: lf.looper.captureOverruns() };
  }, stallMs);
  console.log(JSON.stringify(result));
  assert.equal(result.overruns, 0);
  for (const row of result.rows) {
    assert.equal(row.discontinuities, 0, 'coded source must remain continuous');
    assert.ok(Math.abs(row.errorFrames) <= 1, `capture start misses absolute grid: ${JSON.stringify(row)}`);
    if (row.kind) {
      assert.equal(row.frames, result.sr, `${row.kind}: first take must be exactly one bar`);
      assert.equal(row.gridError, 0, `${row.kind}: committed grid must agree with the captured frame`);
    }
  }
});

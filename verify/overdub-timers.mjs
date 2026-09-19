// Vite on :1420. Count actual playback-owned timer registrations, callbacks and cancellations.
// Exercises the real looper with silent output. This measures retained callbacks, not CPU or memory.
// Restart Vite before running after source edits; the state identity guard rejects stale HMR modules.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';

const selected = process.argv.find((arg) => arg.startsWith('--case='))?.slice(7);
const cases = ['rapid', 'stopTail', 'clearReuse', 'rearm'];
assert.ok(!selected || cases.includes(selected), `Unknown case: ${selected}`);
const browser = await chromium.launch({ headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto(process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  await page.waitForFunction(() => !!window.__lf);
  await page.evaluate(() => window.__lf.autosave.ready());
  const results = [];
  for (const name of cases.filter((value) => !selected || value === selected)) {
    const result = await page.evaluate(async (name) => {
      const lf = window.__lf;
      await lf.looper.init();
      const { engineState } = await import('/src/audio/looper/state.ts');
      if (!engineState.initialized) throw new Error('Restart Vite: the probe imported a different HMR state module');
      const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
      const ctx = lf.engine.ctx;
      const sr = ctx.sampleRate;
      lf.master.setMuted(true);
      lf.looper.setLoopEndStopEnabled(false);
      lf.recordLatency.setEnabled(false);
      const originalSetTimeout = window.setTimeout;
      const originalClearTimeout = window.clearTimeout;
      const pending = new Map();
      const scheduled = [];
      let fired = 0, cancelled = 0, cancelledFromPlayback = 0, peakPending = 0;
      let probeCancelled = 0;
      // Only the direct caller owns a timer. A reactive subscriber reached through publish()
      // may schedule unrelated work while playback.ts is farther down the same stack.
      const fromPlayback = (stack) => /\/src\/audio\/looper\/playback\.ts(?:\?|:)/.test(stack?.split('\n')[2] ?? '');
      window.setTimeout = function (handler, delay, ...args) {
        if (typeof handler !== 'function' || !fromPlayback(new Error().stack)) return originalSetTimeout.call(window, handler, delay, ...args);
        const entry = { requestedAt: ctx.currentTime, delayMs: Number(delay), firedAt: null };
        const id = originalSetTimeout.call(window, function (...callbackArgs) {
          pending.delete(id);
          fired++;
          entry.firedAt = ctx.currentTime;
          handler.apply(this, callbackArgs);
        }, delay, ...args);
        pending.set(id, entry);
        scheduled.push(entry);
        peakPending = Math.max(peakPending, pending.size);
        return id;
      };
      window.clearTimeout = function (id) {
        if (pending.delete(id)) {
          cancelled++;
          if (fromPlayback(new Error().stack)) cancelledFromPlayback++;
        }
        return originalClearTimeout.call(window, id);
      };
      const pause = (ms) => new Promise((resolve) => originalSetTimeout.call(window, resolve, ms));
      const waitFor = async (predicate, label, timeout = 5000) => {
        const deadline = performance.now() + timeout;
        while (!predicate()) {
          if (performance.now() > deadline) throw new Error(`Timed out: ${label}`);
          await pause(2);
        }
      };
      const load = async (seconds = 8) => {
        lf.looper.clearAll();
        await lf.looper.loadSession({ bpm: seconds === 1 ? 240 : 120, bars: seconds === 1 ? 1 : 4,
          masterLengthFrames: sr * seconds,
          tracks: [{ index: 0, pcm: new Float32Array(sr * seconds).fill(0.025), volume: 1,
            muted: false, reversed: false, fx: defaultFxStates() }] });
        if (engineState.tracks[0].state !== lf.looper.stateOf(0)) throw new Error('Restart Vite: looper state identities disagree');
        // Pass the imported loop's first start, so the next timer targets its full-period boundary.
        await waitFor(() => ctx.currentTime >= engineState.masterStartTime + 0.15, 'imported playback start');
      };
      const snapshot = () => ({ pending: pending.size, scheduled: scheduled.length, fired, cancelled,
        cancelledFromPlayback, peakPending, state: lf.looper.stateOf(0), active: engineState.activeRecordIndex });
      try {
        await load(name === 'rearm' ? 1 : 8);
        const overrunsBefore = lf.looper.captureOverruns();
        const observations = {};
        if (name === 'rapid') {
          const cycles = [];
          for (let i = 0; i < 20; i++) {
            await lf.looper.recDub(0);
            const during = snapshot();
            lf.looper.stop(0); // direct abort, without a compensated completion wait
            const stopped = snapshot();
            lf.looper.playStop(0);
            cycles.push({ during: during.pending, duringState: during.state,
              stopped: stopped.pending, stoppedState: stopped.state, resumed: lf.looper.stateOf(0) });
          }
          observations.cycles = cycles;
          observations.finishedAt = ctx.currentTime;
          observations.firstDeadline = scheduled[0]?.requestedAt + scheduled[0]?.delayMs / 1000;
        } else if (name === 'stopTail') {
          lf.recordLatency.setEnabled(true);
          lf.recordLatency.setFloorEnabled(false);
          lf.recordLatency.setOffsetMs(150);
          lf.recordLatency.beginMonitorGeneration(0, 0);
          observations.compensationFrames = lf.recordLatency.recordCompensationFrames();
          await lf.looper.recDub(0);
          observations.beforeStop = snapshot();
          lf.looper.playStop(0);
          observations.duringTail = snapshot();
          observations.hasCaptureEnd = engineState.captureEndFrame !== null;
          await waitFor(() => lf.looper.stateOf(0) === 'STOPPED', 'compensated stop completion');
          observations.sourceAfter = engineState.tracks[0].source !== null;
        } else if (name === 'clearReuse') {
          await lf.looper.recDub(0);
          observations.beforeClear = snapshot();
          lf.looper.clear(0);
          observations.cleared = snapshot();
          await load();
          await lf.looper.recDub(0);
          observations.reused = snapshot();
          lf.looper.clear(0);
        } else {
          await lf.looper.recDub(0);
          await waitFor(() => fired >= 2, 'two normal boundary callbacks');
          observations.running = snapshot();
          observations.deadlines = scheduled.map((entry) => entry.requestedAt + entry.delayMs / 1000);
          lf.looper.stop(0);
        }
        return { name, ...observations, final: snapshot(),
          overruns: lf.looper.captureOverruns() - overrunsBefore };
      } finally {
        lf.looper.clearAll();
        // Baseline deliberately leaves timers queued. Remove them only after recording the result,
        // so each case remains isolated and the obsolete callback cannot affect another case.
        for (const id of pending.keys()) { originalClearTimeout.call(window, id); probeCancelled++; }
        pending.clear();
        window.setTimeout = originalSetTimeout;
        window.clearTimeout = originalClearTimeout;
        lf.recordLatency.clearMonitor(0);
        lf.recordLatency.setOffsetMs(0);
        lf.recordLatency.setEnabled(false);
        if (probeCancelled > 0) console.info(`[overdub-timers] probe teardown removed ${probeCancelled} retained callbacks`);
      }
    }, name);
    console.log(JSON.stringify(result));
    results.push(result);
  }
  for (const result of results) {
    assert.equal(result.overruns, 0, `${result.name}: capture loss invalidates the run`);
    assert.ok(result.final.scheduled > 0, `${result.name}: timer stack filter observed no playback scheduling`);
    assert.equal(result.final.pending, 0, `${result.name}: ended capture must cancel its pending callback`);
    assert.equal(result.final.peakPending, 1, `${result.name}: one track must own at most one boundary timeout`);
    assert.equal(result.final.cancelledFromPlayback, result.final.cancelled, `${result.name}: playback owns timer cancellation`);
    if (result.name === 'rapid') {
      assert.ok(result.finishedAt < result.firstDeadline, 'rapid cycles must finish before their first boundary');
      assert.equal(result.final.fired, 0, 'rapid cycles must cancel timers before any callback executes');
      assert.equal(result.final.cancelled, 20, 'all 20 completed sessions must cancel their timer');
      assert.ok(result.cycles.every((cycle) => cycle.during === 1 && cycle.stopped === 0 &&
        cycle.duringState === 'OVERDUBBING' && cycle.stoppedState === 'STOPPED' && cycle.resumed === 'PLAYING'));
    } else if (result.name === 'stopTail') {
      assert.ok(result.compensationFrames > 0, 'exercise a real pending capture tail');
      assert.equal(result.beforeStop.pending, 1);
      assert.equal(result.duringTail.state, 'OVERDUBBING', 'stop must wait for the compensated tail');
      assert.equal(result.duringTail.active, 0);
      assert.equal(result.duringTail.pending, 0, 'playback timer must cancel before recorder release');
      assert.ok(result.hasCaptureEnd);
      assert.equal(result.final.state, 'STOPPED');
      assert.equal(result.final.active, -1);
      assert.equal(result.final.fired, 0);
      assert.equal(result.sourceAfter, false);
    } else if (result.name === 'clearReuse') {
      assert.equal(result.beforeClear.pending, 1);
      assert.equal(result.cleared.state, 'EMPTY');
      assert.equal(result.cleared.pending, 0);
      assert.equal(result.reused.pending, 1, 'reused track owns only its new timer');
      assert.equal(result.final.cancelled, 2);
    } else {
      assert.equal(result.running.fired, 2);
      assert.equal(result.running.pending, 1, 'normal callbacks rearm one successor');
      assert.equal(result.running.scheduled, 3);
      assert.ok(result.deadlines.every((deadline, i) => i === 0 || deadline > result.deadlines[i - 1]),
        'successor deadlines must advance');
      assert.equal(result.final.cancelled, 1);
    }
  }
  assert.deepEqual(errors, [], 'unexpected browser errors');
} finally {
  await browser.close();
}

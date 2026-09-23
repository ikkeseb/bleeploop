/**
 * Counts actual playback-owned timer registrations, callbacks and cancellations through the real
 * looper with silent output: rapid stop/reuse (20 cycles), compensated STOP waiting out the capture
 * tail, CLEAR + reuse, normal boundary rearming, and a boundary swap whose source start fails (the
 * lane must land STOPPED, logged once, with no successor timer, and PLAY restarts the committed
 * loop). `--case=<name>` (rapid|stopTail|clearReuse|rearm|swapFail) restricts the run to one case.
 * Run: pnpm probe overdub-timers [--case=<name>] [--url=<server>]
 * This measures retained callbacks against the real looper, not CPU or memory; it does not exercise
 * native ASIO scheduling. Restart Vite before running after source edits: the state-identity guard
 * rejects a stale HMR module.
 */
import { probe, arg } from '../harness/probe.ts';
import assert from 'node:assert/strict';

// Every imported loop sample carries this level. Headless capture has no input signal, so an overdub
// layer adds silence and a kept loop still reads LOAD_AMP per sample.
const LOAD_AMP = 0.025;
const selected = arg('case');
const cases = ['rapid', 'stopTail', 'clearReuse', 'rearm', 'swapFail'];
assert.ok(!selected || cases.includes(selected), `Unknown case: ${selected}`);

await probe(async ({ open }) => {
  const { page } = await open();
  await page.evaluate(() => window.__lf.autosave.ready());
  const results = [];
  for (const name of cases.filter((value) => !selected || value === selected)) {
    const result = await page.evaluate(async ({ name, loadAmp }) => {
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
          tracks: [{ index: 0, pcm: new Float32Array(sr * seconds).fill(loadAmp), volume: 1,
            muted: false, reversed: false, fx: defaultFxStates() }] });
        if (engineState.tracks[0].state !== lf.looper.stateOf(0)) throw new Error('Restart Vite: looper state identities disagree');
        // Pass the imported loop's first start, so the next timer targets its full-period boundary.
        await waitFor(() => ctx.currentTime >= engineState.masterStartTime + 0.15, 'imported playback start');
      };
      const snapshot = () => ({ pending: pending.size, scheduled: scheduled.length, fired, cancelled,
        cancelledFromPlayback, peakPending, state: lf.looper.stateOf(0), active: engineState.activeRecordIndex });
      try {
        await load(name === 'rearm' || name === 'swapFail' ? 1 : 8);
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
        } else if (name === 'swapFail') {
          // D15: the first boundary swap's source start throws. The lane must leave OVERDUBBING into
          // STOPPED with the committed loop kept, log once, register no successor timer, and PLAY again.
          const { toasts, dismissToast } = await import('/src/notify.ts');
          for (const toast of toasts()) dismissToast(toast.id);
          const master = engineState.masterFramesPlain || sr;
          const nativeStart = AudioBufferSourceNode.prototype.start;
          const nativeError = console.error;
          let injected = 0, swapLogs = 0, recordAtFailure = null;
          console.error = function (...args) {
            if (String(args[0]).includes('overdub boundary swap failed')) swapLogs++;
            return nativeError.apply(this, args);
          };
          AudioBufferSourceNode.prototype.start = function (...args) {
            if (injected === 0 && this.loop && this.buffer?.length === engineState.tracks[0].lengthFrames) {
              injected++;
              recordAtFailure = engineState.tracks[0].record.slice(0, engineState.tracks[0].lengthFrames);
              throw new Error('injected overdub swap start failure');
            }
            return nativeStart.apply(this, args);
          };
          try {
            await lf.looper.recDub(0);
            observations.beforeSwap = snapshot();
            await waitFor(() => fired >= 1, 'first boundary callback');
            await waitFor(() => lf.looper.stateOf(0) === 'STOPPED', 'failed swap lands STOPPED');
          } finally {
            AudioBufferSourceNode.prototype.start = nativeStart;
            console.error = nativeError;
          }
          observations.afterFailure = snapshot();
          observations.injected = injected;
          observations.swapLogs = swapLogs;
          observations.toasts = toasts().map((toast) => toast.message);
          observations.sourceAfterFailure = engineState.tracks[0].source !== null;
          // Nothing re-arms: wait past the next boundary and count registrations again.
          const period = engineState.tracks[0].lengthFrames / sr;
          await pause(period * 1000 + 100);
          observations.afterNextBoundary = snapshot();
          const t = engineState.tracks[0];
          let recordDrift = 0;
          for (let k = 0; k < t.lengthFrames; k++) recordDrift = Math.max(recordDrift, Math.abs(t.record[k] - recordAtFailure[k]));
          observations.recordDrift = recordDrift;
          observations.recordMin = recordAtFailure.reduce((min, v) => Math.min(min, v), Infinity);
          observations.recordMax = recordAtFailure.reduce((max, v) => Math.max(max, v), -Infinity);
          lf.looper.playStop(0);
          observations.playState = lf.looper.stateOf(0);
          const played = t.source?.buffer?.getChannelData(0);
          let playedDrift = played ? 0 : Infinity;
          if (played) for (let k = 0; k < t.lengthFrames; k++) playedDrift = Math.max(playedDrift, Math.abs(played[k] - t.record[k]));
          observations.playedDrift = playedDrift;
          observations.master = master;
          for (const toast of toasts()) dismissToast(toast.id);
          lf.looper.stop(0);
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
    }, { name, loadAmp: LOAD_AMP });
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
    } else if (result.name === 'swapFail') {
      assert.equal(result.injected, 1, 'exactly one swap start was failed');
      assert.equal(result.beforeSwap.state, 'OVERDUBBING');
      assert.equal(result.afterFailure.state, 'STOPPED', 'a failed swap must leave OVERDUBBING');
      assert.equal(result.afterFailure.active, -1, 'the recorder must be released');
      assert.equal(result.afterFailure.pending, 0, 'no successor timer after a failed swap');
      assert.equal(result.afterNextBoundary.scheduled, 1, 'no timer registered after the failed swap');
      assert.equal(result.afterNextBoundary.fired, 1);
      assert.equal(result.swapLogs, 1, 'the failed swap is logged exactly once');
      assert.equal(result.toasts.length, 1, 'one toast reports the stopped overdub');
      assert.equal(result.sourceAfterFailure, false, 'the stale source is freed');
      // Half the seeded level either way: a lost loop (0) or a doubled commit (2x) both land outside.
      assert.ok(Math.abs(result.recordMin - LOAD_AMP) < LOAD_AMP / 2 && Math.abs(result.recordMax - LOAD_AMP) < LOAD_AMP / 2,
        `the committed loop is kept: samples ${result.recordMin}..${result.recordMax}, seeded ${LOAD_AMP}`);
      assert.ok(result.recordDrift < 1e-6, `committed content changed after the failure: ${result.recordDrift}`);
      assert.equal(result.playState, 'PLAYING', 'PLAY restarts the lane');
      assert.equal(result.playedDrift, 0, 'PLAY starts the committed content');
    } else {
      assert.equal(result.running.fired, 2);
      assert.equal(result.running.pending, 1, 'normal callbacks rearm one successor');
      assert.equal(result.running.scheduled, 3);
      assert.ok(result.deadlines.every((deadline, i) => i === 0 || deadline > result.deadlines[i - 1]),
        'successor deadlines must advance');
      assert.equal(result.final.cancelled, 1);
    }
  }
});

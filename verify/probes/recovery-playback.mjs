/**
 * Main-thread continuity during real recovery writes (`src/session/autosave.ts`, the encode in its
 * worker): measures the worst setInterval tick gap and any long-task duration while `autosave.flush()`
 * snapshots, encodes and writes a 1-track 1-bar and a 5-track 30-bar session, on the web engine fake
 * (`src/platform/host.web.ts`) whose snapshot answers those loops; the UI's controls must not stall.
 *
 * Cannot see the native engine or its audio during a save (the engine renders on its own thread), the
 * snapshot's cost over Tauri IPC (the fake hands back a copy of bytes already in the page) or WebView2
 * timing.
 * Run: pnpm probe recovery-playback
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open({ viewport: { width: 1280, height: 820 }, init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
  await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
  await page.evaluate(() => window.__lf.autosave.ready());
  const results = await page.evaluate(async () => {
    const lf = window.__lf;
    lf.autosave.start()(); // Boot started it: only the measured flush writes.
    const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
    const sr = 48000;
    let seq = 0;
    const emit = (frame) => lf.native.emit({ seq: ++seq, reset: false, events: [], ...frame });
    const lane = (state, length) => ({ state, length, armed: false, autoArmed: false, canUndo: false,
      canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
    const results = [];
    for (const [count, bars] of [[1, 1], [5, 30]]) {
      // A new engine (a reset frame), then its loops committed and playing; the snapshot answers them.
      emit({ reset: true, settings: [], anchor: { frame: 0, atMs: Date.now(), rate: sr, grid: 0 },
        events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
          ...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }))] });
      const frames = sr * bars * 2;
      const pcm = Array.from({ length: count }, () =>
        Float32Array.from({ length: frames }, (_, f) => 0.01 * Math.sin(2 * Math.PI * 220 * f / sr)));
      lf.native.snapshotBytes = encodeSessionBytes({ rate: sr, masterLengthFrames: frames, bpm: 120,
        tracks: pcm.map((_, index) => ({ index, frames, reversed: false, state: 'Playing' })) }, pcm).buffer;
      emit({ events: [{ Transport: { frame: 0, master: frames, bpm: 120, locked: true } },
        ...pcm.map((_, i) => ({ Lane: { frame: 0, lane: i, info: lane('Playing', frames) } }))] });
      await new Promise((r) => setTimeout(r, 500));
      const longTasks = [];
      const observer = new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) longTasks.push(entry.duration);
      });
      observer.observe({ type: 'longtask' });
      let maxTickGap = 0;
      let previous = performance.now();
      const timer = setInterval(() => {
        const now = performance.now();
        maxTickGap = Math.max(maxTickGap, now - previous);
        previous = now;
      }, 5);
      let puts = 0;
      const put = IDBObjectStore.prototype.put;
      IDBObjectStore.prototype.put = function (...args) {
        if (this.name === 'recovery') puts++;
        return put.apply(this, args);
      };
      const start = performance.now();
      await lf.autosave.flush().finally(() => (IDBObjectStore.prototype.put = put));
      const elapsed = performance.now() - start;
      await new Promise((r) => setTimeout(r, 100));
      clearInterval(timer);
      observer.disconnect();
      results.push({ count, seconds: bars * 2, elapsedMs: elapsed, maxTickGapMs: maxTickGap, longTasksMs: longTasks, puts });
    }
    return results;
  });
  console.log(JSON.stringify(results));
  assert.equal(results.length, 2);
  for (const result of results) {
    assert.ok(result.puts >= 1, 'the flush wrote the jam');
    // A local performance regression guard, with headroom for scheduling jitter. The old full-size
    // synchronous encoder blocked this machine for 257 ms; worker runs were below 80 ms.
    assert.ok(result.maxTickGapMs < 100, `Recovery blocked controls for ${result.maxTickGapMs} ms`);
  }
});

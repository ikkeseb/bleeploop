// pnpm exec node verify/probes/recovery-playback.mjs (Vite on 1420).
// Measures main-thread delay during real recovery writes. Does not measure native bridge audio.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';

const browser = await chromium.launch({ headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 820 } });
  await page.goto((process.argv.find(arg => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420'));
  await page.waitForFunction(() => !!window.__lf);
  await page.evaluate(() => window.__lf.autosave.ready());
  const results = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.looper.init();
    const results = [];
    for (const [count, bars] of [[1, 1], [5, 30]]) {
      lf.looper.clearAll();
      const sr = lf.engine.ctx.sampleRate;
      const frames = sr * bars * 2;
      const fx = lf.looper.fxState(0);
      const tracks = Array.from({ length: count }, (_, index) => ({
        index, pcm: Float32Array.from({ length: frames }, (_, f) => 0.01 * Math.sin(2 * Math.PI * 220 * f / sr)),
        volume: 0.5, muted: false, reversed: false, fx,
      }));
      await lf.looper.loadSession({ bpm: 120, bars, masterLengthFrames: frames, tracks });
      await new Promise(r => setTimeout(r, 500));
      const code = `class Continuity extends AudioWorkletProcessor {
        constructor() { super(); this.quanta = 0; this.silent = 0; this.port.onmessage = () => this.port.postMessage({quanta:this.quanta,silent:this.silent}); }
        process(inputs) { const ch = inputs[0]?.[0]; if(ch) { let energy=0; for(let i=0;i<ch.length;i++) energy+=ch[i]*ch[i]; this.quanta++; if(energy/ch.length<1e-8) this.silent++; } return true; }
      } registerProcessor('continuity-${count}', Continuity);`;
      const url = URL.createObjectURL(new Blob([code], { type: 'application/javascript' }));
      // Each case uses a distinct module/name in the same context.
      await lf.engine.ctx.audioWorklet.addModule(url);
      URL.revokeObjectURL(url);
      const meter = new AudioWorkletNode(lf.engine.ctx, `continuity-${count}`);
      lf.engine.masterGain.connect(meter);
      meter.connect(lf.engine.ctx.destination); // processor writes silence, no duplicate audible route
      const longTasks = [];
      const observer = new PerformanceObserver(list => {
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
      const start = performance.now();
      await lf.autosave.flush();
      const elapsed = performance.now() - start;
      await new Promise(r => setTimeout(r, 100));
      const continuity = await new Promise(resolve => { meter.port.onmessage = e => resolve(e.data); meter.port.postMessage('read'); });
      lf.engine.masterGain.disconnect(meter);
      meter.disconnect();
      clearInterval(timer);
      observer.disconnect();
      results.push({ count, seconds: bars * 2, elapsedMs: elapsed, maxTickGapMs: maxTickGap, longTasksMs: longTasks, continuity });
    }
    return results;
  });
  assert.equal(results.length, 2);
  for (const result of results) {
    assert.ok(result.continuity.quanta > 0);
    assert.equal(result.continuity.silent, 0, 'Web Audio playback must stay continuous during save');
    // A local performance regression guard, with headroom for scheduling jitter. The old full-size
    // synchronous encoder blocked this machine for 257 ms; worker runs were below 80 ms.
    assert.ok(result.maxTickGapMs < 100, `Recovery blocked controls for ${result.maxTickGapMs} ms`);
  }
  console.log(JSON.stringify(results));
} finally {
  await browser.close();
}

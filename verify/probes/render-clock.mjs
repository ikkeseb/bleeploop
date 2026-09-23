// Actual DEV observer worklet: identity PCM, absolute render frames, finite wall clock and overflow.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
const browser = await chromium.launch();
try {
  const page = await browser.newPage();
  await page.goto(process.argv.find(arg => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  const results = await page.evaluate(async () => {
    const { default: url } = await import('/src/debug/render-clock-processor.ts?worker&url');
    const results = [];
    for (const capacity of [64, 2]) {
      const length = 4096, sr = 44100;
      const ctx = new OfflineAudioContext(2, length, sr);
      await ctx.audioWorklet.addModule(url);
      const sab = new SharedArrayBuffer(8 + capacity * 16);
      const published = new Int32Array(sab, 0, 2);
      const clocks = new Float64Array(sab, 8);
      const observer = new AudioWorkletNode(ctx, 'render-clock-probe', {
        numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1], channelCount: 1,
        channelCountMode: 'explicit', processorOptions: { clockSab: sab },
      });
      const buffer = ctx.createBuffer(1, length, sr);
      const pcm = buffer.getChannelData(0);
      for (let i = 0; i < length; i++) pcm[i] = Math.sin(i * 0.17) * 0.2;
      const source = ctx.createBufferSource(); source.buffer = buffer;
      const merge = ctx.createChannelMerger(2);
      source.connect(merge, 0, 0); source.connect(observer); observer.connect(merge, 0, 1);
      merge.connect(ctx.destination); source.start(0);
      const before = Date.now();
      const rendered = await ctx.startRendering();
      const after = Date.now();
      const direct = rendered.getChannelData(0), observed = rendered.getChannelData(1);
      const count = Atomics.load(published, 0);
      const timestamps = Array.from({ length: count }, (_, i) => [clocks[i * 2], clocks[i * 2 + 1]]);
      results.push({ capacity, count, overflow: Atomics.load(published, 1),
        identical: direct.every((v, i) => v === observed[i]),
        exactFrames: timestamps.every(([frame], i) => frame === i * 128),
        wallClockBounded: timestamps.every(([, wall]) => wall >= before && wall <= after),
      });
    }
    return results;
  });
  console.log(JSON.stringify(results));
  assert.deepEqual(results, [
    { capacity: 64, count: 32, overflow: 0, identical: true, exactFrames: true, wallClockBounded: true },
    { capacity: 2, count: 2, overflow: 1, identical: true, exactFrames: true, wallClockBounded: true },
  ]);
} finally { await browser.close(); }

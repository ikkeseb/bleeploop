// Production compensation under paired queue/render-cursor movement, invalid timestamps and freeze.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage();
  await page.goto(process.argv.find(arg => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  await page.waitForFunction(() => !!window.__lf);
  const result = await page.evaluate(async () => {
    const { engine, recordLatency: latency } = window.__lf;
    const { pluginBridge } = await import('/src/audio/plugin-bridge.ts');
    await engine.start();
    const ctx = engine.ctx, sr = ctx.sampleRate;
    let queued = 441, phase = 0, valid = true;
    pluginBridge.stats = () => { queued = ++phase % 2 ? 441 : 882; return { hop1Lag: 0, hop2Fill: queued }; };
    Object.defineProperty(ctx, 'baseLatency', { configurable: true, value: 0.01 });
    Object.defineProperty(ctx, 'outputLatency', { configurable: true, value: 0.08 });
    ctx.getOutputTimestamp = () => valid
      ? { contextTime: ctx.currentTime - (0.09 - queued / sr), performanceTime: performance.now() }
      : { contextTime: 0, performanceTime: 0 };
    latency.beginMonitorGeneration(0, 0.02);
    await new Promise(r => setTimeout(r, 220));
    const frames = latency.recordCompensationFrames();
    const measured = latency.lastCompensation();
    const expected = Math.round((0.09 - 0.02 + engine.outputGraphLatencySeconds) * sr);
    valid = false;
    await new Promise(r => setTimeout(r, 120));
    const frozen = latency.recordCompensationFrames() === frames;
    const offset = latency.offsetMs();
    latency.setOffsetMs(offset + 10);
    const trimDelta = latency.recordCompensationFrames() - frames;
    latency.setOffsetMs(offset);
    latency.beginMonitorGeneration(0, 0.02);
    await new Promise(r => setTimeout(r, 120));
    latency.recordCompensationFrames();
    const fallback = latency.lastCompensation();
    latency.clearMonitor(0);
    return { source: measured?.source ?? null, frames, expected, errorFrames: frames - expected,
      frozen, trimDelta, expectedTrimDelta: Math.round(sr * 0.01), fallbackSource: fallback?.source ?? null,
      fallbackFinite: Number.isFinite(fallback?.frames), unarmed: latency.recordCompensationFrames() };
  });
  console.log(JSON.stringify(result));
  assert.equal(result.source, 'timestamp');
  assert.ok(Math.abs(result.errorFrames) <= 2, 'paired queue/cursor changes must cancel without a base/quantum allowance');
  assert.equal(result.frozen, true);
  assert.ok(Math.abs(result.trimDelta - result.expectedTrimDelta) <= 1);
  assert.equal(result.fallbackSource, 'reported');
  assert.equal(result.fallbackFinite, true);
  assert.equal(result.unarmed, 0);
} finally { await browser.close(); }

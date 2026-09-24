/**
 * Measures the production master limiter's DSP delay at 44.1/48 kHz against the real
 * `computeC`/`WORKLET_QUANTUM_FRAMES` formula, then checks that the production compensation path
 * feeds that measured graph latency through unchanged. Also injects one failed offline-latency
 * measurement at startup and checks that `engine.start()` rejects, leaves the engine not started,
 * retries on the next gesture and settles with exactly one successful measurement.
 * Run: pnpm probe master-latency [--url=<server>] (run with `pnpm dev` already open)
 * Offline PCM proves graph latency only, not device latency or guitar alignment on the native rig.
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open();
  const startup = await page.evaluate(async () => {
    const engine = window.__lf.engine;
    const Original = window.OfflineAudioContext;
    let attempts = 0;
    window.OfflineAudioContext = class extends Original {
      constructor(...args) {
        attempts++;
        if (attempts === 1) throw new Error('Injected offline latency measurement failure');
        super(...args);
      }
    };
    try {
      let rejected = false;
      try { await engine.start(); } catch { rejected = true; }
      const notStarted = !engine.started;
      await Promise.all([engine.start(), engine.start()]);
      const measured = engine.outputGraphLatencySeconds;
      await engine.start();
      return { rejected, notStarted, attempts, measured,
        pass: rejected && notStarted && attempts === 2 && measured > 0 && engine.started };
    } finally {
      window.OfflineAudioContext = Original;
    }
  });
  const result = await page.evaluate(async () => {
    const { makeMasterLimiter } = await import('/src/audio/engine.ts');
    const { computeC, WORKLET_QUANTUM_FRAMES } = await import('/src/audio/record-latency-math.ts');
    const results = [];
    for (const sampleRate of [44100, 48000]) {
      const firstSignal = async (limited) => {
        const ctx = new OfflineAudioContext(1, 4096, sampleRate);
        const buffer = ctx.createBuffer(1, 4096, sampleRate);
        buffer.getChannelData(0)[512] = 0.1;
        const source = ctx.createBufferSource(); source.buffer = buffer;
        if (limited) source.connect(makeMasterLimiter(ctx)).connect(ctx.destination);
        else source.connect(ctx.destination);
        source.start();
        const pcm = (await ctx.startRendering()).getChannelData(0);
        const first = pcm.findIndex((sample) => Math.abs(sample) > 0.000001);
        return { first, peak: pcm.reduce((peak, sample) => Math.max(peak, Math.abs(sample)), 0) };
      };
      const dry = await firstSignal(false), limited = await firstSignal(true);
      const delayFrames = limited.first - dry.first;
      const outputGraphLatencySeconds = delayFrames / sampleRate;
      const terms = { hopFrames: 2176, cpalOutSeconds: 0.01,
        baseLatency: 0.01, outputLatency: 0.02, trimMs: 0, floorEnabled: false,
        outputGraphLatencySeconds };
      const compensation = computeC(terms, sampleRate);
      const expectedSeconds = (terms.hopFrames + WORKLET_QUANTUM_FRAMES) / sampleRate
        - terms.cpalOutSeconds + terms.baseLatency + terms.outputLatency + outputGraphLatencySeconds;
      const missingFrames = Math.round(expectedSeconds * sampleRate) - compensation.frames;
      results.push({ sampleRate, dry, limited, delayFrames, delayMs: outputGraphLatencySeconds * 1000,
        compensationFrames: compensation.frames, expectedFrames: Math.round(expectedSeconds * sampleRate),
        missingFrames, pass: dry.first === 512 && limited.first >= dry.first && missingFrames === 0 });
    }
    const lf = window.__lf;
    await lf.looper.init();
    const sr = lf.engine.ctx.sampleRate;
    const measured = lf.engine.outputGraphLatencySeconds;
    const measuredOffline = results.find((entry) => entry.sampleRate === sr)?.delayMs;
    const originalTrim = lf.recordLatency.offsetMs();
    const originalFloor = lf.recordLatency.isFloorEnabled();
    try {
      lf.recordLatency.setOffsetMs(0);
      lf.recordLatency.setFloorEnabled(false);
      lf.recordLatency.beginMonitorGeneration(0, 0.01);
      const actual = lf.recordLatency.recordCompensationFrames();
      const breakdown = lf.recordLatency.lastCompensation();
      const expected = computeC({ hopFrames: breakdown.hopFrames,
        cpalOutSeconds: breakdown.cpalOutSeconds, baseLatency: breakdown.baseLatency,
        outputLatency: breakdown.outputLatencyReported - breakdown.baseLatency,
        outputGraphLatencySeconds: measured, trimMs: 0, floorEnabled: false }, sr).frames;
      lf.recordLatency.clearMonitor();
      const unarmed = lf.recordLatency.recordCompensationFrames();
      results.push({ name: 'Production measurement feeds native-only compensation', sampleRate: sr,
        measuredMs: measured * 1000, measuredOfflineMs: measuredOffline, actual, expected, unarmed,
        pass: measuredOffline !== undefined && Math.abs(measured * 1000 - measuredOffline) < 0.000001
          && breakdown.outputGraphLatencySeconds === measured && actual === expected && unarmed === 0 });
    } finally {
      lf.recordLatency.clearMonitor();
      lf.recordLatency.setOffsetMs(originalTrim);
      lf.recordLatency.setFloorEnabled(originalFloor);
    }
    return results;
  });
  console.log(JSON.stringify({ startup, measurements: result }, null, 2));
  assert.ok(startup.pass, `startup latency measurement failed: ${JSON.stringify(startup)}`);
  for (const entry of result) assert.ok(entry.pass, `${entry.name ?? entry.sampleRate}: ${JSON.stringify(entry)}`);
});

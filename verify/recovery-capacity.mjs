// Run with the browser rig on port 1420: pnpm exec node verify/recovery-capacity.mjs
// Disposable browser storage only. Tests maximum recovery size and Chromium's independent WAV decoder.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';

const browser = await chromium.launch({ headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 820 } });
  await page.goto('http://localhost:1420');
  await page.waitForFunction(() => !!window.__lf);
  await page.evaluate(() => window.__lf.autosave.ready());
  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.looper.init();
    const { encodeWav } = await import('/src/audio/export/wav.ts');
    const { maxImportArchiveBytes } = await import('/src/audio/export/import.ts');
    const fx = lf.looper.fxState(0);
    const sr = lf.engine.ctx.sampleRate;
    // Browser's own decoder provides an independent interoperability check of the float header.
    const wav = encodeWav([Float32Array.of(0, 2.25, -3.5, 1e-8)], sr, 'float32');
    const decoded = await lf.engine.ctx.decodeAudioData(wav.buffer);
    const externalSamples = Array.from(decoded.getChannelData(0));
    const frames = sr * 60;
    const tracks = Array.from({ length: 5 }, (_, index) => ({
      index, pcm: Float32Array.from({ length: frames }, (_, frame) =>
        frame % 1000 === 0 ? (index + 1) * 0.75 : 1e-8),
      volume: 0.5, muted: true, reversed: false, fx,
    }));
    await lf.looper.loadSession({ bpm: 120, bars: 30, masterLengthFrames: frames, tracks });
    const t0 = performance.now();
    await lf.autosave.flush();
    const saveMs = performance.now() - t0;
    const bundle = await lf.buildExportBundle({ bpm: 120, bars: 30 }, { includeMaster: false, stemFormat: 'float32' });
    lf.looper.clearAll();
    await lf.importSession(bundle.zipBytes);
    const snapshot = lf.looper.exportSnapshot();
    return {
      externalSamples, saveMs, bytes: bundle.zipBytes.length, limit: maxImportArchiveBytes(sr),
      exact: snapshot.tracks.length === 5 && snapshot.tracks.every((track, i) =>
        track.pcm.length === frames && track.pcm.every((sample, f) => Object.is(sample, tracks[i].pcm[f]))),
    };
  });
  assert.deepEqual(result.externalSamples, Array.from(Float32Array.of(0, 2.25, -3.5, 1e-8)));
  assert.ok(result.bytes <= result.limit);
  assert.ok(result.exact);
  console.log(JSON.stringify(result));
} finally {
  await browser.close();
}

/**
 * Recovery archive capacity and cross-decoder interoperability, in disposable browser storage. Loads
 * a 5-track, 30-bar session near the practical size ceiling, times the autosave flush, exports and
 * reimports it back to an exact PCM match, and decodes a hand-built float32 WAV through Chromium's own
 * independent decoder as an interoperability check of the float header. Cannot see native storage
 * limits or WebView2.
 * Run: pnpm probe recovery-capacity
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open({ viewport: { width: 1280, height: 820 } });
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
});

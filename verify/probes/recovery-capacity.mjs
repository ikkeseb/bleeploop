/**
 * Recovery archive capacity and cross-decoder interoperability, in disposable browser storage, on the
 * web engine fake (`src/platform/host.web.ts`): the engine's snapshot answers a 5-track, 30-bar session
 * (60 s, the longest loop) near the practical size ceiling. Times the autosave flush
 * (`src/session/autosave.ts`), exports it (`src/session/export.ts`, float32 stems, no master), checks the
 * archive fits the import limit, and imports it (`src/session/import.ts`) back to an exact PCM match in
 * the session the fake engine was asked to load. Decodes a hand-built float32 WAV
 * (`src/session/wav.ts`) through Chromium's own independent decoder (an OfflineAudioContext of the
 * probe's) as an interoperability check of the float header.
 *
 * Cannot see the native engine, its snapshot over Tauri IPC, native storage limits or WebView2.
 * Run: pnpm probe recovery-capacity
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open({ viewport: { width: 1280, height: 820 }, init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
  await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
  await page.evaluate(() => window.__lf.autosave.ready());
  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    const { session } = await import('/src/ui/state/audio.ts');
    const { encodeSessionBytes, splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    const { encodeWav } = await import('/src/session/wav.ts');
    const { maxImportArchiveBytes } = await import('/src/session/import.ts');
    const sr = 48000;
    // Browser's own decoder provides an independent interoperability check of the float header.
    const wav = encodeWav([Float32Array.of(0, 2.25, -3.5, 1e-8)], sr, 'float32');
    const decoded = await new OfflineAudioContext(1, 1, sr).decodeAudioData(wav.buffer);
    const externalSamples = Array.from(decoded.getChannelData(0));
    const frames = sr * 60;
    const pcm = Array.from({ length: 5 }, (_, index) => Float32Array.from({ length: frames }, (_, frame) =>
      frame % 1000 === 0 ? (index + 1) * 0.75 : 1e-8));
    let seq = 0;
    const emit = (frame) => lf.native.emit({ seq: ++seq, reset: false, events: [], ...frame });
    const lane = (state, length) => ({ state, length, armed: false, autoArmed: false, canUndo: false,
      canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
    emit({ reset: true, settings: [], anchor: { frame: 0, atMs: Date.now(), rate: sr, grid: 0 },
      events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
        ...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }))] });
    // The engine holds the five loops, stopped; its snapshot answers them.
    lf.native.snapshotBytes = encodeSessionBytes({ rate: sr, masterLengthFrames: frames, bpm: 120,
      tracks: pcm.map((_, index) => ({ index, frames, reversed: false, state: 'Stopped' })) }, pcm).buffer;
    emit({ events: [{ Transport: { frame: 0, master: frames, bpm: 120, locked: true } },
      ...pcm.map((_, i) => ({ Lane: { frame: 0, lane: i, info: lane('Stopped', frames) } }))] });
    const t0 = performance.now();
    await lf.autosave.flush();
    const saveMs = performance.now() - t0;
    const bundle = await lf.buildExportBundle({ bpm: 120, bars: 30 }, { includeMaster: false, stemFormat: 'float32' }, session);
    // The player's CLEAR ALL empties the engine; the import loads into it.
    lf.native.snapshotBytes = null;
    emit({ events: [...[0, 1, 2, 3, 4].flatMap((i) => [{ Cleared: { frame: 0, lane: i } },
      { Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }]),
    { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }] });
    await lf.importSession(bundle.zipBytes, session);
    const { header, pcm: loaded } = splitSessionBytes(lf.native.loadedSessions.at(-1).slice().buffer);
    return {
      externalSamples, saveMs, bytes: bundle.zipBytes.length, limit: maxImportArchiveBytes(sr),
      loads: lf.native.loadedSessions.length, master: header.masterLengthFrames,
      exact: loaded.length === 5 && loaded.every((track, i) =>
        track.length === frames && track.every((sample, f) => Object.is(sample, pcm[i][f]))),
    };
  });
  console.log(JSON.stringify(result));
  assert.deepEqual(result.externalSamples, Array.from(Float32Array.of(0, 2.25, -3.5, 1e-8)));
  assert.ok(result.bytes <= result.limit);
  assert.ok(result.exact);
});

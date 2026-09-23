/** Production wet-export isolation under delayed and rejected reverb generation.
 * Run against Vite: node verify/probes/export-context.mjs --url=http://localhost:1420
 * This checks browser graph ownership, not native download delivery or device sound.
 */
import { chromium } from 'playwright';

const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const results = [];
  for (const failure of [false, true]) {
    const page = await browser.newPage();
    await page.goto(process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
    await page.waitForFunction(() => !!window.__lf);
    results.push(await page.evaluate(async (failure) => {
      const { engine } = await import('/src/audio/engine.ts');
      const { FxChain, defaultFxStates } = await import('/src/audio/fx/fx.ts');
      const { renderWetMaster } = await import('/src/audio/export/render.ts');
      const transformed = await (await fetch('/src/audio/fx/fx.ts')).text();
      const tonePath = transformed.match(/from\s+["']([^"']*\/tone[^"']*)["']/)?.[1];
      if (!tonePath) throw new Error('Could not resolve application Tone module');
      const Tone = await import(tonePath);
      await engine.start();
      // Build the live shared reverb before delaying only the export's new reverb.
      const warm = new FxChain(defaultFxStates());
      warm.dispose();
      const originalContext = Tone.getContext();
      const originalGenerate = Tone.Reverb.prototype.generate;
      const originalDispose = Tone.Gain.prototype.dispose;
      let cleanupFailures = 0;
      Tone.Gain.prototype.dispose = function () {
        const result = originalDispose.call(this);
        if (failure && this.context.isOffline && cleanupFailures === 0) {
          cleanupFailures++;
          throw new Error('Injected cleanup failure must not replace render failure');
        }
        return result;
      };
      let release;
      const barrier = new Promise((resolve) => { release = resolve; });
      const generationCalls = new WeakMap();
      Tone.Reverb.prototype.generate = function () {
        const actual = originalGenerate.call(this);
        const count = (generationCalls.get(this) ?? 0) + 1;
        generationCalls.set(this, count);
        if (count === 1) return actual; // Constructor ignores its promise; makeReverbBus awaits call two.
        return actual.then(async (value) => {
          await barrier;
          if (failure) throw new Error('Injected export reverb failure');
          return value;
        });
      };
      const sr = engine.ctx.sampleRate;
      const pcm = new Float32Array(Math.round(sr * 2));
      pcm[128] = 0.1;
      const rendering = renderWetMaster({ sampleRate: sr, masterLengthFrames: pcm.length,
        tracks: [{ index: 0, pcm, volume: 1, muted: false, reversed: false, fx: defaultFxStates() }] }, 120, 1)
        .then(() => ({ rejected: false }), (error) => ({ rejected: true, error: String(error) }));
      await new Promise((resolve) => setTimeout(resolve, 100));
      const during = Tone.getContext() === originalContext;
      // A fresh lane takes exactly this production constructor path at first playback.
      let liveError = null;
      let live;
      try { live = new FxChain(defaultFxStates()); } catch (error) { liveError = String(error); }
      release();
      const outcome = await rendering;
      const after = Tone.getContext() === originalContext;
      Tone.Reverb.prototype.generate = originalGenerate;
      Tone.Gain.prototype.dispose = originalDispose;
      Tone.setContext(originalContext); // Keep cleanup valid even on the red baseline.
      live?.dispose();
      return { failure, during, after, liveError, cleanupFailures, ...outcome,
        pass: during && after && liveError === null && outcome.rejected === failure
          && (!failure || (cleanupFailures === 1 && outcome.error.includes('Injected export reverb failure'))) };
    }, failure));
    await page.close();
  }
  const page = await browser.newPage();
  await page.goto(process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  await page.waitForFunction(() => !!window.__lf);
  results.push(await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.looper.init();
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const { buildExportBundle } = await import('/src/audio/export/export.ts');
    const { importSession, maxImportArchiveBytes } = await import('/src/audio/export/import.ts');
    const { parseZip } = await import('/src/audio/export/unzip.ts');
    const { makeZip } = await import('/src/audio/export/zip.ts');
    const { decodeWav } = await import('/src/audio/export/wav.ts');
    const sr = lf.engine.ctx.sampleRate;
    const pcm = new Float32Array(sr * 2);
    pcm.set([1.5, -1.5, 1e-7], 1024);
    await lf.looper.loadSession({ bpm: 120, bars: 1, masterLengthFrames: pcm.length,
      tracks: [{ index: 0, pcm, volume: 0.25, muted: false, reversed: false, fx: defaultFxStates() }] });
    const bundle = await buildExportBundle({ bpm: 120, bars: 1 });
    if (!bundle) throw new Error('No export bundle');
    const entries = parseZip(bundle.zipBytes);
    const stem = entries.find((entry) => entry.name.endsWith('-track1.wav'));
    const decoded = decodeWav(stem.data).channels[0];
    lf.looper.clearAll();
    await importSession(bundle.zipBytes);
    const restored = lf.looper.exportSnapshot().tracks[0];
    let errors = 0;
    for (let k = 0; k < pcm.length; k++) {
      if (decoded[k] !== pcm[k] || restored.pcm[k] !== pcm[k]) errors++;
    }
    const transformed = await (await fetch('/src/audio/fx/fx.ts')).text();
    const tonePath = transformed.match(/from\s+["']([^"']*\/tone[^"']*)["']/)?.[1];
    if (!tonePath) throw new Error('Could not resolve application Tone module');
    const Tone = await import(tonePath);
    const originalContext = Tone.getContext();
    const originalGenerate = Tone.Reverb.prototype.generate;
    const calls = new WeakMap();
    Tone.Reverb.prototype.generate = function () {
      const actual = originalGenerate.call(this);
      const count = (calls.get(this) ?? 0) + 1;
      calls.set(this, count);
      return count === 1 ? actual : actual.then(() => { throw new Error('Injected bundle reverb failure'); });
    };
    let fallbackKind;
    try {
      const fallback = await buildExportBundle({ bpm: 120, bars: 1 });
      const metadata = parseZip(fallback.zipBytes).find((entry) => entry.name.endsWith('-session.json'));
      fallbackKind = JSON.parse(new TextDecoder().decode(metadata.data)).master.kind;
    } finally {
      Tone.Reverb.prototype.generate = originalGenerate;
    }
    const contextPreserved = Tone.getContext() === originalContext;
    const cap = maxImportArchiveBytes(sr);
    let oversizedRejected = false;
    try { await importSession(new Uint8Array(cap + 1)); }
    catch (error) { oversizedRejected = String(error).includes(`maximum is ${cap}`); }
    // A small valid ZIP can repeat one large local payload through many central-directory records.
    const one = makeZip([{ name: 'same.wav', data: new Uint8Array(1024 * 1024) }]);
    const view = new DataView(one.buffer);
    const eocd = one.length - 22;
    const start = view.getUint32(eocd + 16, true), size = view.getUint32(eocd + 12, true);
    const count = 128;
    const hostile = new Uint8Array(start + count * size + 22);
    hostile.set(one.subarray(0, start));
    for (let i = 0; i < count; i++) hostile.set(one.subarray(start, eocd), start + i * size);
    hostile.set(one.subarray(eocd), hostile.length - 22);
    const directory = new DataView(hostile.buffer), end = hostile.length - 22;
    directory.setUint16(end + 8, count, true);
    directory.setUint16(end + 10, count, true);
    directory.setUint32(end + 12, count * size, true);
    let repeatedPayloadRejected = false;
    const began = performance.now();
    try { await importSession(hostile); }
    catch (error) { repeatedPayloadRejected = String(error).includes('128 entries; maximum is 7'); }
    const rejectionMs = performance.now() - began;
    return { name: 'Download bundle preserves editable overdub headroom and quiet samples', errors,
      samples: Array.from(restored.pcm.slice(1024, 1027)), volume: restored.volume,
      fallbackKind, contextPreserved, oversizedRejected, cap, repeatedPayloadRejected, rejectionMs,
      pass: errors === 0 && restored.volume === 0.25 && fallbackKind === 'dry-fallback'
        && contextPreserved && oversizedRejected && repeatedPayloadRejected };
  }));
  await page.close();
  console.log(JSON.stringify(results, null, 2));
  if (results.some((result) => !result.pass)) process.exitCode = 1;
} finally {
  await browser.close();
}

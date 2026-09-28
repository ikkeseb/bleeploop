/**
 * The export's offline wet render and the archive paths around it, with the real TypeScript executing
 * (`src/session/render.ts` and `offline-fx.ts` on Tone's OfflineContext; `export.ts`, `import.ts`,
 * `unzip.ts`), the engine side on the web engine fake (`src/platform/host.web.ts`, the `engine-seam`
 * pattern: the probe scripts the feed and the snapshot the engine would answer through `__lf.native`):
 *
 * - The render leaves Tone's global context as it found it, whether it resolves or rejects, and its own
 *   rejection (an injected reverb-generation failure) surfaces even when a cleanup step throws too.
 * - An editable export keeps float32 stems exactly (samples past ±1 and a 1e-7), and importing the zip
 *   hands the engine that PCM exactly and restores the lane's volume. A reverb-generation failure still
 *   completes the export, with a dry master (`master.kind` 'dry-fallback'), and leaves Tone's context
 *   alone. Import refuses an archive over its size cap and a small archive whose central directory
 *   repeats one payload 128 times (entry cap 9).
 * - The master mixes a STOPPED lane and leaves out a muted one (tester-feedback F26).
 *
 * Cannot see the engine's own FX (lf-engine plays them; this render may sound unlike it), the native
 * snapshot and load (the fake answers them), download delivery in WebView2, or anything audible.
 * Run: pnpm probe export-context
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const MASTER = 2 * RATE; // one bar at 120 BPM

const lane = (state, extra = {}) => ({
  state,
  length: state === 'Empty' ? 0 : MASTER,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: state === 'Playing' || state === 'Stopped',
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
  ...extra,
});
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });
const transport = (master) => ({ Transport: { frame: 0, master, bpm: 120, locked: master > 0 } });

await probe(async ({ open }) => {
  const results = [];

  // ── The render: Tone's context, and its own rejection past a cleanup failure ──────────────────────
  for (const failure of [false, true]) {
    const { page } = await open();
    results.push(await page.evaluate(async (failure) => {
      // The Tone module the app's offline render imports (Vite's pre-bundled copy).
      const transformed = await (await fetch('/src/session/offline-fx.ts')).text();
      const tonePath = transformed.match(/from\s+["']([^"']*\/tone[^"']*)["']/)?.[1];
      if (!tonePath) throw new Error('Could not resolve the application Tone module');
      const Tone = await import(tonePath);
      const { renderWetMaster } = await import('/src/session/render.ts');
      const { defaultFxStates } = await import('/src/ui/state/fx-metadata.ts');
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
      const generationCalls = new WeakMap();
      Tone.Reverb.prototype.generate = function () {
        const actual = originalGenerate.call(this);
        const count = (generationCalls.get(this) ?? 0) + 1;
        generationCalls.set(this, count);
        // The constructor ignores its promise; makeReverbBus awaits call two.
        if (count === 1 || !failure) return actual;
        return actual.then(() => { throw new Error('Injected export reverb failure'); });
      };
      const pcm = new Float32Array(48000 * 2);
      pcm[128] = 0.1;
      let outcome;
      try {
        outcome = await renderWetMaster({ sampleRate: 48000, masterLengthFrames: pcm.length,
          tracks: [{ index: 0, pcm, volume: 1, muted: false, reversed: false, fx: defaultFxStates() }] }, 120, 1)
          .then(() => ({ rejected: false }), (error) => ({ rejected: true, error: String(error) }));
      } finally {
        Tone.Reverb.prototype.generate = originalGenerate;
        Tone.Gain.prototype.dispose = originalDispose;
      }
      const after = Tone.getContext() === originalContext;
      Tone.setContext(originalContext); // Keep the page usable even on a red run.
      return { name: `The render ${failure ? 'rejects with its own error' : 'resolves'} and restores Tone's context`,
        failure, after, cleanupFailures, ...outcome,
        pass: after && outcome.rejected === failure
          && (!failure || (cleanupFailures === 1 && outcome.error.includes('Injected export reverb failure'))) };
    }, failure));
    await page.close();
  }

  // ── The engine fake: a device, and every lane EMPTY ───────────────────────────────────────────────
  const { page, consoleErrors } = await open({ init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  await emit({
    reset: true,
    settings: [],
    events: [transport(0), ...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });
  await page.evaluate(() => window.__lf.autosave.ready());

  /** The engine now holds `pcm` on lane 0 in `state`: the feed says so and the snapshot answers it. */
  const engineHolds = async (pcm, state) => {
    await page.evaluate(async ([pcm, state, master]) => {
      const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
      window.__lf.native.snapshotBytes = encodeSessionBytes(
        { rate: 48000, masterLengthFrames: master, bpm: 120, tracks: [{ index: 0, frames: master, reversed: false, state }] },
        [Float32Array.from(pcm)],
      ).buffer;
    }, [pcm, state, MASTER]);
    await emit({ events: [transport(MASTER), laneEvent(0, lane(state))] });
  };

  // ── The editable download and its import ──────────────────────────────────────────────────────────
  const editable = Array.from({ length: MASTER }, () => 0);
  editable.splice(1024, 3, 1.5, -1.5, 1e-7);
  // Loaded as an import loads it (the engine takes the bytes, the store the mix), then the engine plays it.
  await page.evaluate(async ([pcm, master]) => {
    const { session } = await import('/src/ui/state/audio.ts');
    const { defaultFxStates } = await import('/src/ui/state/fx-metadata.ts');
    await session.loadSession({ bpm: 120, bars: 1, masterLengthFrames: master,
      tracks: [{ index: 0, pcm: Float32Array.from(pcm), volume: 0.25, muted: false, reversed: false, fx: defaultFxStates() }] });
  }, [editable, MASTER]);
  await engineHolds(editable, 'Playing');
  const exported = await page.evaluate(async (pcm) => {
    const lf = window.__lf;
    const { session } = await import('/src/ui/state/audio.ts');
    const { parseZip } = await import('/src/session/unzip.ts');
    const { decodeWav } = await import('/src/session/wav.ts');
    const bundle = await lf.buildExportBundle({ bpm: 120, bars: 1 }, {}, session);
    if (!bundle) throw new Error('No export bundle');
    window.__bundle = bundle.zipBytes;
    const stem = decodeWav(parseZip(bundle.zipBytes).find((entry) => entry.name.endsWith('-track1.wav')).data).channels[0];
    const want = Float32Array.from(pcm);
    return { stemErrors: want.reduce((n, x, k) => n + Number(stem[k] !== x), 0), stemSamples: Array.from(stem.slice(1024, 1027)) };
  }, editable);
  // The engine cleared (every lane EMPTY) and the lane's volume moved: only the import brings it back.
  await emit({ events: [transport(0), laneEvent(0, lane('Empty'))] });
  await page.evaluate(() => {
    window.__lf.looper.setVolume(0, 1);
    window.__lf.native.sent.length = 0;
  });
  results.push(await page.evaluate(async ([pcm, exported]) => {
    const lf = window.__lf;
    const { session } = await import('/src/ui/state/audio.ts');
    const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    const before = lf.native.loadedSessions.length;
    await lf.importSession(window.__bundle, session);
    const loads = lf.native.loadedSessions.length - before;
    const restored = splitSessionBytes(lf.native.loadedSessions.at(-1).slice().buffer).pcm[0];
    const want = Float32Array.from(pcm);
    const errors = exported.stemErrors + want.reduce((n, x, k) => n + Number(restored[k] !== x), 0);
    const volume = session.trackVolume(0);
    const volumeSent = lf.native.sent.some((c) => JSON.stringify(c) === JSON.stringify({ SetVolume: [0, 0.25] }));
    return { name: 'Download bundle preserves editable overdub headroom and quiet samples', loads, errors,
      stemSamples: exported.stemSamples, samples: Array.from(restored.slice(1024, 1027)), volume, volumeSent,
      pass: loads === 1 && errors === 0 && volume === 0.25 && volumeSent };
  }, [editable, exported]));

  // ── The dry fallback, and the import caps ─────────────────────────────────────────────────────────
  await engineHolds(editable, 'Playing');
  results.push(await page.evaluate(async () => {
    const lf = window.__lf;
    // The Tone module the app's offline render imports (Vite's pre-bundled copy).
    const transformed = await (await fetch('/src/session/offline-fx.ts')).text();
    const tonePath = transformed.match(/from\s+["']([^"']*\/tone[^"']*)["']/)?.[1];
    if (!tonePath) throw new Error('Could not resolve the application Tone module');
    const Tone = await import(tonePath);
    const { session } = await import('/src/ui/state/audio.ts');
    const { maxImportArchiveBytes } = await import('/src/session/import.ts');
    const { parseZip } = await import('/src/session/unzip.ts');
    const { makeZip } = await import('/src/session/zip.ts');
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
      const fallback = await lf.buildExportBundle({ bpm: 120, bars: 1 }, {}, session);
      const metadata = parseZip(fallback.zipBytes).find((entry) => entry.name.endsWith('-session.json'));
      fallbackKind = JSON.parse(new TextDecoder().decode(metadata.data)).master.kind;
    } finally {
      Tone.Reverb.prototype.generate = originalGenerate;
    }
    const contextPreserved = Tone.getContext() === originalContext;
    const cap = maxImportArchiveBytes(48000);
    let oversizedRejected = false;
    try { await lf.importSession(new Uint8Array(cap + 1), session); }
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
    try { await lf.importSession(hostile, session); }
    catch (error) { repeatedPayloadRejected = String(error).includes('128 entries; maximum is 9'); }
    const rejectionMs = performance.now() - began;
    return { name: 'A failed wet render exports a dry master; import refuses oversized and repeated-payload archives',
      fallbackKind, contextPreserved, oversizedRejected, cap, repeatedPayloadRejected, rejectionMs,
      pass: fallbackKind === 'dry-fallback' && contextPreserved && oversizedRejected && repeatedPayloadRejected };
  }));

  // ── F26: the master mixes a STOPPED lane and leaves out a muted one ───────────────────────────────
  const sine = Array.from({ length: MASTER }, (_, k) => 0.5 * Math.sin((2 * Math.PI * 220 * k) / RATE));
  await engineHolds(sine, 'Stopped');
  const masterPeak = (muted) => page.evaluate(async (muted) => {
    const lf = window.__lf;
    const { session } = await import('/src/ui/state/audio.ts');
    const { parseZip } = await import('/src/session/unzip.ts');
    const { decodeWav } = await import('/src/session/wav.ts');
    lf.looper.setMute(0, muted);
    const bundle = await lf.buildExportBundle({ bpm: 120, bars: 1 }, {}, session);
    const master = parseZip(bundle.zipBytes).find((entry) => entry.name.endsWith('-master.wav'));
    return Math.max(...decodeWav(master.data).channels.map((c) => c.reduce((m, x) => Math.max(m, Math.abs(x)), 0)));
  }, muted);
  await page.evaluate(() => window.__lf.looper.setVolume(0, 1));
  const heard = await masterPeak(false);
  const muted = await masterPeak(true);
  const stateOf = await page.evaluate(() => window.__lf.looper.stateOf(0));
  results.push({ name: 'The master mixes a STOPPED track and leaves out a muted one', stateOf, heard, muted,
    pass: stateOf === 'STOPPED' && heard > 0.1 && muted < 1e-4 });

  console.log(JSON.stringify(results, null, 2));
  assert.ok(results.every((result) => result.pass), JSON.stringify(results));
  // The injected reverb failure's fallback logs the one error this page may show.
  const unexpected = consoleErrors.filter((text) => !text.includes('[export] wet master render failed'));
  assert.deepEqual(unexpected, [], 'no other console errors');
});

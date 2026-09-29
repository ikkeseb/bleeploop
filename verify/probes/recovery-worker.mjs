/**
 * Recovery encoding through the real worker (`src/session/recovery-encode.ts`): transfer ownership of
 * the copied PCM, non-finite sample rejection, a worker startup failure followed by a successful retry,
 * and a malformed reply from the worker. Then the same failure behaviour through the production autosave
 * path (`src/session/autosave.ts`) on the web engine fake (`src/platform/host.web.ts`), whose snapshot
 * the probe scripts as the engine would answer it: a failed flush leaves the previous archive
 * restorable, a later commit while an encode is pending cannot reach the archive the worker already
 * holds, and a flush in flight with the player's CLEAR ALL behind it leaves recovery cleared rather than
 * resurrecting the older save. A restore is read back from the session the fake engine was asked to
 * load.
 *
 * Cannot see the native engine, its snapshot over Tauri IPC, native storage limits or WebView2.
 * Run: pnpm probe recovery-worker
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  // After the app booted (`__lf`): its modules load after the page (`src/main.tsx`), and their own
  // workers must not count as the encoder's.
  const { page } = await open({ init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
  await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
  const result = await page.evaluate(async () => {
    const { encodeRecovery } = await import('/src/session/recovery-encode.ts');
    const { parseZip } = await import('/src/session/unzip.ts');
    const { decodeWav } = await import('/src/session/wav.ts');
    const request = (sample) => ({
      snapshot: {
        sampleRate: 48000, masterLengthFrames: 4,
        tracks: [{ index: 0, pcm: new Float32Array([0, sample, -0.0000001, -1.75]),
          state: 'STOPPED', volume: 0.6, muted: true, reversed: true, fx: [] }],
      },
      meta: { bpm: 120, bars: 1 }, base: 'worker-check',
    });
    const NativeWorker = window.Worker;
    let made = 0;
    let terminated = 0;
    window.Worker = class extends NativeWorker {
      constructor(...args) { super(...args); made++; }
      terminate() { terminated++; super.terminate(); }
    };
    try {
      let encodeError = '';
      try { await encodeRecovery(request(NaN)); } catch (error) { encodeError = error.message; }
      const copied = request(1.75);
      const bytes = await encodeRecovery(copied);
      const entries = parseZip(bytes);
      const wav = entries.find((e) => e.name.endsWith('.wav'));
      const samples = Array.from(decodeWav(wav.data).channels[0]);
      const detached = copied.snapshot.tracks[0].pcm.byteLength === 0;
      const TrackedWorker = window.Worker;
      window.Worker = class { constructor() { throw new Error('Injected worker startup failure'); } };
      let startupError = '';
      try { await encodeRecovery(request(0)); } catch (error) { startupError = error.message; }
      window.Worker = TrackedWorker;
      const retry = await encodeRecovery(request(0.25));
      let malformedError = '';
      window.Worker = class extends TrackedWorker {
        postMessage() { queueMicrotask(() => this.onmessage({ data: null })); }
      };
      try { await encodeRecovery(request(0)); } catch (error) { malformedError = error.message; }
      return { encodeError, startupError, malformedError, samples, detached, retry: retry.byteLength > 0, made, terminated };
    } finally {
      window.Worker = NativeWorker;
    }
  });
  assert.match(result.encodeError, /non-finite WAV samples/);
  assert.equal(result.startupError, 'Injected worker startup failure');
  assert.deepEqual(result.samples, [0, 1.75, Math.fround(-0.0000001), -1.75]);
  assert.equal(result.detached, true);
  assert.equal(result.retry, true);
  assert.equal(result.malformedError, 'Recovery worker returned invalid data');
  assert.equal(result.made, 4);
  assert.equal(result.terminated, 4);
  console.log(JSON.stringify(result));

  const recovery = await page.evaluate(async () => {
    const lf = window.__lf;
    const { encodeSessionBytes, splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    await lf.autosave.ready();
    lf.autosave.start()(); // Boot started it: stop its timer, so this probe's writes stay explicit.
    const RATE = 48000;
    const frames = 2 * RATE; // one bar at 120 BPM
    let seq = 0;
    const emit = (frame) => lf.native.emit({ seq: ++seq, reset: false, events: [], ...frame });
    const lane = (state, length) => ({ state, length, armed: false, autoArmed: false, canUndo: false,
      canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
    /** A new engine's first frame (a reset): every lane EMPTY, which is not the player's clear. */
    const blank = () => emit({ reset: true, settings: [], anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
        ...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }))] });
    /** The engine commits a loop on lane 1 whose sample 17 is `value`; its snapshot answers that loop. */
    const load = (value) => {
      const pcm = new Float32Array(frames);
      pcm[17] = value;
      lf.native.snapshotBytes = encodeSessionBytes({ rate: RATE, masterLengthFrames: frames, bpm: 120,
        tracks: [{ index: 0, frames, reversed: false, state: 'Playing' }] }, [pcm]).buffer;
      emit({ events: [{ Transport: { frame: 0, master: frames, bpm: 120, locked: true } },
        { Lane: { frame: 0, lane: 0, info: lane('Playing', frames) } }],
      peaks: [{ lane: 0, start: 0, count: 1, min: [-Math.abs(value)], max: [Math.abs(value)] }] });
    };
    /** The engine's CLEAR ALL: `Cleared` before each lane's own event; its snapshot is empty. */
    const clearAll = () => {
      lf.native.snapshotBytes = null;
      emit({ events: [...[0, 1, 2, 3, 4].flatMap((i) => [{ Cleared: { frame: 0, lane: i } },
        { Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }]),
      { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }] });
    };
    /** Restore into a blank engine: whether it restored, and whether the one loaded lane holds `value`
     * at frame 17 and silence elsewhere. */
    const restores = async (value) => {
      blank();
      const loads = lf.native.loadedSessions.length;
      const restored = await lf.autosave.restoreLatest();
      if (lf.native.loadedSessions.length !== loads + 1) return { restored, exact: false };
      const { pcm } = splitSessionBytes(lf.native.loadedSessions.at(-1).slice().buffer);
      return { restored, exact: pcm.length === 1 && pcm[0].every((v, i) => v === (i === 17 ? Math.fround(value) : 0)) };
    };
    blank();
    const originalWorker = window.Worker;
    try {
      load(1.75);
      await lf.autosave.flush();
      load(0.5);
      window.Worker = class { constructor() { throw new Error('Injected worker startup failure'); } };
      let rejected = false;
      try { await lf.autosave.flush(); } catch { rejected = true; }
      window.Worker = originalWorker;
      const previous = await restores(1.75);

      // Pause at transfer, after the snapshot has been copied but before encoding returns.
      let transferred;
      let started;
      const observeTransfer = () => {
        started = new Promise((resolve) => { transferred = resolve; });
        window.Worker = class extends originalWorker {
          postMessage(...args) { super.postMessage(...args); transferred(); }
        };
      };
      load(0.375);
      observeTransfer();
      const saving = lf.autosave.flush();
      await started;
      load(-0.25); // A later commit must not reach the snapshot in the worker.
      await saving;
      window.Worker = originalWorker;
      const snapshot = await restores(0.375);

      load(0.625);
      observeTransfer();
      const olderSave = lf.autosave.flush();
      await started;
      clearAll();
      const clearFlush = lf.autosave.flush();
      await Promise.all([olderSave, clearFlush]);
      return { rejected, restoredPrevious: previous.restored, previousExact: previous.exact,
        restoredSnapshot: snapshot.restored, snapshotExact: snapshot.exact,
        clearedAfterSave: !(await lf.autosave.hasSaved()) };
    } finally {
      window.Worker = originalWorker;
    }
  });
  console.log(JSON.stringify(recovery));
  assert.deepEqual(recovery, { rejected: true, restoredPrevious: true, previousExact: true,
    restoredSnapshot: true, snapshotExact: true, clearedAfterSave: true });
});

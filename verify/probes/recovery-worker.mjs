/**
 * Recovery encoding through the real worker: transfer ownership of the copied PCM, non-finite sample
 * rejection, a worker startup failure followed by a successful retry, and a malformed reply from the
 * worker. Then the same failure behaviour through the production autosave path: a failed flush leaves
 * the previous archive restorable, a live edit made while an encode is pending cannot mutate the
 * snapshot the worker already copied, and two overlapping flushes leave recovery cleared rather than
 * resurrecting an older save. Cannot see native storage limits or WebView2.
 * Run: pnpm probe recovery-worker
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  // After the app booted (`__lf`): it loads its modules after the page (`src/main.tsx`), and their own
  // workers must not count as the encoder's.
  const { page } = await open();
  const result = await page.evaluate(async () => {
    const { encodeRecovery } = await import('/src/audio/export/recovery-encode.ts');
    const { parseZip } = await import('/src/audio/export/unzip.ts');
    const { decodeWav } = await import('/src/audio/export/wav.ts');
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

  await page.waitForFunction(() => !!window.__lf);
  const recovery = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.autosave.ready();
    lf.autosave.start()(); // Keep this probe's writes explicit.
    await lf.looper.init();
    const frames = lf.engine.ctx.sampleRate * 2;
    const load = async (value) => {
      lf.looper.clearAll();
      const pcm = new Float32Array(frames);
      pcm[17] = value;
      await lf.looper.loadSession({ bpm: 120, bars: 1, masterLengthFrames: frames, tracks: [{
        index: 0, pcm, volume: 0.6, muted: true, reversed: false, fx: lf.looper.fxState(0),
      }] });
    };
    const originalWorker = window.Worker;
    try {
      await load(1.75);
      await lf.autosave.flush();
      await load(0.5);
      window.Worker = class { constructor() { throw new Error('Injected worker startup failure'); } };
      let rejected = false;
      try { await lf.autosave.flush(); } catch { rejected = true; }
      window.Worker = originalWorker;
      lf.looper.clearAll();
      const restoredPrevious = await lf.autosave.restoreLatest();
      const oldPcm = lf.looper.exportSnapshot().tracks[0].pcm;
      const previousExact = oldPcm.every((v, i) => v === (i === 17 ? 1.75 : 0));

      // Pause at transfer, after the coherent snapshot has been copied but before encoding returns.
      let transferred;
      let started;
      const observeTransfer = () => {
        started = new Promise((resolve) => { transferred = resolve; });
        window.Worker = class extends originalWorker {
          postMessage(...args) { super.postMessage(...args); transferred(); }
        };
      };
      await load(0.375);
      observeTransfer();
      const saving = lf.autosave.flush();
      await started;
      await load(-0.25); // Later live edits must not mutate the snapshot in the worker.
      await saving;
      window.Worker = originalWorker;
      lf.looper.clearAll();
      const restoredSnapshot = await lf.autosave.restoreLatest();
      const snapshotPcm = lf.looper.exportSnapshot().tracks[0].pcm;
      const snapshotExact = snapshotPcm.every((v, i) => v === (i === 17 ? 0.375 : 0));

      observeTransfer();
      const olderSave = lf.autosave.flush();
      await started;
      lf.looper.clearAll();
      const clearFlush = lf.autosave.flush();
      await Promise.all([olderSave, clearFlush]);
      return { rejected, restoredPrevious, previousExact, restoredSnapshot, snapshotExact,
        clearedAfterSave: !(await lf.autosave.hasSaved()) };
    } finally {
      window.Worker = originalWorker;
    }
  });
  assert.deepEqual(recovery, { rejected: true, restoredPrevious: true, previousExact: true,
    restoredSnapshot: true, snapshotExact: true, clearedAfterSave: true });
  console.log(JSON.stringify(recovery));
});

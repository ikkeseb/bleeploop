/**
 * Real IndexedDB recovery writes and rollback (`src/session/autosave.ts`), with failures injected at
 * transaction/request boundaries: an aborted put after apparent success, a QuotaExceededError at put,
 * an aborted delete after apparent success (the player's CLEAR ALL), an aborted read transaction, and an
 * automatic retry once a later state change (a lane's volume) gives the failed save a fresh fingerprint.
 * The loops come from the web engine fake (`src/platform/host.web.ts`): the probe scripts each commit on
 * the feed and the snapshot the engine answers. The quota case injects the error at put(), not actual
 * disk exhaustion; no worker replacement, so every save encodes its production recovery archive.
 *
 * Cannot see the native engine, its snapshot over Tauri IPC, native storage limits or WebView2.
 * Run: pnpm probe recovery-transactions
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open({ init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
  await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.autosave.ready();
    lf.autosave.start()(); // Explicit writes until the final automatic retry scenario.
    const { session } = await import('/src/ui/state/audio.ts');
    const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
    const { parseZip } = await import('/src/session/unzip.ts');
    const { decodeWav } = await import('/src/session/wav.ts');
    const RATE = 48000;
    const frames = 2 * RATE; // one bar at 120 BPM
    let seq = 0;
    const emit = (frame) => lf.native.emit({ seq: ++seq, reset: false, events: [], ...frame });
    const lane = (state, length) => ({ state, length, armed: false, autoArmed: false, canUndo: false,
      canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
    emit({ reset: true, settings: [], anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
        ...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }))] });
    /** The engine commits a loop on lane 1 whose sample 17 is `sample`; its snapshot answers that loop. */
    const load = (sample) => {
      const pcm = new Float32Array(frames);
      pcm[17] = sample;
      lf.native.snapshotBytes = encodeSessionBytes({ rate: RATE, masterLengthFrames: frames, bpm: 120,
        tracks: [{ index: 0, frames, reversed: false, state: 'Playing' }] }, [pcm]).buffer;
      emit({ events: [{ Transport: { frame: 0, master: frames, bpm: 120, locked: true } },
        { Lane: { frame: 0, lane: 0, info: lane('Playing', frames) } }],
      peaks: [{ lane: 0, start: 0, count: 1, min: [-sample], max: [sample] }] });
    };
    /** The engine's CLEAR ALL: `Cleared` before each lane's own event; its snapshot is empty. */
    const clearAll = () => {
      lf.native.snapshotBytes = null;
      emit({ events: [...[0, 1, 2, 3, 4].flatMap((i) => [{ Cleared: { frame: 0, lane: i } },
        { Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }]),
      { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }] });
    };
    const unhandled = [];
    const onUnhandled = (event) => { unhandled.push(String(event.reason)); event.preventDefault(); };
    window.addEventListener('unhandledrejection', onUnhandled);
    const nativePut = IDBObjectStore.prototype.put;
    const nativeDelete = IDBObjectStore.prototype.delete;
    const nativeGet = IDBObjectStore.prototype.get;
    const target = (store) => store.name === 'recovery' && store.transaction.db.name === 'bleeploop';
    const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const read = () => new Promise((resolve, reject) => {
      const open = indexedDB.open('bleeploop', 1);
      open.onerror = () => reject(open.error);
      open.onsuccess = () => {
        const db = open.result;
        const tx = db.transaction('recovery', 'readonly');
        const request = nativeGet.call(tx.objectStore('recovery'), 'latest');
        tx.oncomplete = () => { db.close(); resolve(request.result); };
        tx.onabort = () => { db.close(); reject(tx.error); };
      };
    });
    const saved = async () => {
      const record = await read();
      if (!record) return null;
      const entries = parseZip(new Uint8Array(record.bytes));
      const stem = entries.find((entry) => entry.name.endsWith('-track1.wav'));
      const metadata = JSON.parse(new TextDecoder().decode(entries.find((entry) => entry.name.endsWith('-session.json')).data));
      return { sample: decodeWav(stem.data).channels[0][17], volume: metadata.tracks[0].volume };
    };
    const cases = [];
    try {
      for (const mode of ['abort-put-after-success', 'quota-put', 'abort-delete-after-success']) {
        load(1.75);
        await lf.autosave.flush();
        if (mode.startsWith('abort-delete')) clearAll();
        else load(0.375);
        let injected = 0;
        IDBObjectStore.prototype.put = function (...args) {
          if (!target(this)) return nativePut.apply(this, args);
          injected++;
          if (mode === 'quota-put') throw new DOMException('Injected storage quota failure', 'QuotaExceededError');
          const request = nativePut.apply(this, args);
          if (mode === 'abort-put-after-success') request.addEventListener('success', () => this.transaction.abort(), { once: true });
          return request;
        };
        // A clear deletes its rate's jam from both keys in one transaction: one injection per transaction.
        const aborted = new WeakSet();
        IDBObjectStore.prototype.delete = function (...args) {
          const request = nativeDelete.apply(this, args);
          if (target(this) && mode === 'abort-delete-after-success' && !aborted.has(this.transaction)) {
            aborted.add(this.transaction);
            injected++;
            request.addEventListener('success', () => this.transaction.abort(), { once: true });
          }
          return request;
        };
        let error = '';
        try { await lf.autosave.flush(); } catch (failure) { error = String(failure); }
        IDBObjectStore.prototype.put = nativePut;
        IDBObjectStore.prototype.delete = nativeDelete;
        const prior = await saved();
        await lf.autosave.flush();
        const next = await saved();
        const expected = mode.startsWith('abort-delete') ? null : 0.375;
        cases.push({ mode, injected, error, prior, next,
          pass: injected === 1 && error !== '' && prior?.sample === 1.75 && (next?.sample ?? null) === expected });
      }

      load(1.75);
      await lf.autosave.flush();
      IDBObjectStore.prototype.get = function (...args) {
        const request = nativeGet.apply(this, args);
        if (target(this)) this.transaction.abort();
        return request;
      };
      let readError = '';
      try { await lf.autosave.hasSaved(); } catch (error) { readError = String(error); }
      IDBObjectStore.prototype.get = nativeGet;
      await wait(100); // Observe a rejected transaction promise left behind after the request rejection.
      const readableAgain = await lf.autosave.hasSaved();
      cases.push({ mode: 'abort-read-request', readError, readableAgain, unhandled: [...unhandled],
        pass: readError !== '' && readableAgain && unhandled.length === 0 });

      let autoAborts = 0;
      IDBObjectStore.prototype.put = function (...args) {
        const request = nativePut.apply(this, args);
        if (target(this) && autoAborts === 0) {
          autoAborts++;
          request.addEventListener('success', () => this.transaction.abort(), { once: true });
        }
        return request;
      };
      load(0.625);
      const stop = lf.autosave.start(session); // The timer again, as boot starts it.
      const deadline = performance.now() + 15000;
      while (autoAborts === 0 && performance.now() < deadline) await wait(100);
      await wait(100);
      const prior = await saved();
      lf.looper.setVolume(0, 0.25); // A fresh fingerprint must retry after the failed save.
      let next;
      do { await wait(200); next = await saved(); }
      while (next?.sample !== 0.625 && performance.now() < deadline);
      stop();
      cases.push({ mode: 'automatic-retry-after-state-change', autoAborts, prior, next,
        pass: autoAborts === 1 && prior?.sample === 1.75 && next?.sample === 0.625 && next.volume === 0.25 });
    } finally {
      IDBObjectStore.prototype.put = nativePut;
      IDBObjectStore.prototype.delete = nativeDelete;
      IDBObjectStore.prototype.get = nativeGet;
      window.removeEventListener('unhandledrejection', onUnhandled);
    }
    return cases;
  });
  console.log(JSON.stringify(result, null, 2));
  assert.ok(result.every((entry) => entry.pass), JSON.stringify(result));
});

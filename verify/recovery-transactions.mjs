/** Real IndexedDB recovery writes and rollback, with failures at transaction/request boundaries.
 * node verify/recovery-transactions.mjs --url=http://localhost:1420
 * The quota case injects QuotaExceededError at put(), not actual disk exhaustion.
 * No worker replacement: every save encodes its production recovery archive.
 */
import { chromium } from 'playwright';

const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage();
  await page.goto(process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  await page.waitForFunction(() => !!window.__lf);
  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.autosave.ready();
    lf.autosave.start()(); // Explicit writes until the final automatic retry scenario.
    await lf.looper.init();
    const { parseZip } = await import('/src/audio/export/unzip.ts');
    const { decodeWav } = await import('/src/audio/export/wav.ts');
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
    const load = async (sample) => {
      lf.looper.clearAll();
      const pcm = new Float32Array(lf.engine.ctx.sampleRate * 2);
      pcm[17] = sample;
      await lf.looper.loadSession({ bpm: 120, bars: 1, masterLengthFrames: pcm.length,
        tracks: [{ index: 0, pcm, volume: 0.5, muted: true, reversed: false, fx: lf.looper.fxState(0) }] });
    };
    const cases = [];
    try {
      for (const mode of ['abort-put-after-success', 'quota-put', 'abort-delete-after-success']) {
        await load(1.75);
        await lf.autosave.flush();
        if (mode.startsWith('abort-delete')) lf.looper.clearAll();
        else await load(0.375);
        let injected = 0;
        IDBObjectStore.prototype.put = function (...args) {
          if (!target(this)) return nativePut.apply(this, args);
          injected++;
          if (mode === 'quota-put') throw new DOMException('Injected storage quota failure', 'QuotaExceededError');
          const request = nativePut.apply(this, args);
          if (mode === 'abort-put-after-success') request.addEventListener('success', () => this.transaction.abort(), { once: true });
          return request;
        };
        IDBObjectStore.prototype.delete = function (...args) {
          const request = nativeDelete.apply(this, args);
          if (target(this) && mode === 'abort-delete-after-success') {
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

      await load(1.75);
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
      await load(0.625);
      const stop = lf.autosave.start();
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
  if (result.some((entry) => !entry.pass)) process.exitCode = 1;
} finally {
  await browser.close();
}

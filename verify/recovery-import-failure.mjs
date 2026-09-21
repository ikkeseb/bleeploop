/**
 * Recovery import is transactional across Web Audio allocation/scheduling failures.
 * Run with Vite on :1420, or pass --url=http://localhost:1421.
 */
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const browser = await chromium.launch({ headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });

const scenarios = [
  { failure: 'buffer', finish: 'clear' },
  { failure: 'source-start', finish: 'retry' },
  { failure: 'read', finish: 'retry' },
];

try {
  const results = [];
  for (const scenario of scenarios) {
    const context = await browser.newContext();
    const page = await context.newPage();
    const errors = [];
    const reportedFailures = [];
    page.on('pageerror', (error) => errors.push(String(error)));
    page.on('console', (message) => {
      if (message.type() === 'error') reportedFailures.push(message.text());
    });
    await page.goto(url);
    await page.waitForFunction(() => !!window.__lf);

    const prepared = await page.evaluate(async () => {
      const lf = window.__lf;
      await lf.autosave.ready();
      lf.autosave.start()(); // Stop automatic writes while the fixture is assembled.
      await lf.autosave.clearSaved();
      lf.looper.clearAll();
      const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
      const frames = Math.round(lf.engine.ctx.sampleRate * 0.8);
      const tracks = [0, 1].map((index) => ({
        index,
        pcm: Float32Array.from({ length: frames }, (_, frame) => (index + 1) * 0.1 + frame / frames * 0.01),
        volume: index === 0 ? 0.5 : 0.75,
        muted: index === 1,
        reversed: false,
        fx: defaultFxStates(),
      }));
      await lf.looper.loadSession({ bpm: 300, bars: 1, masterLengthFrames: frames, tracks });
      await lf.autosave.flush();

      const readBytes = () => new Promise((resolve, reject) => {
        const open = indexedDB.open('bleeploop', 1);
        open.onerror = () => reject(open.error);
        open.onsuccess = () => {
          const db = open.result;
          const tx = db.transaction('recovery', 'readonly');
          const request = tx.objectStore('recovery').get('latest');
          tx.oncomplete = () => { db.close(); resolve(new Uint8Array(request.result.bytes)); };
          tx.onabort = () => { db.close(); reject(tx.error); };
        };
      });
      const bytes = await readBytes();
      const digest = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)))
        .map((value) => value.toString(16).padStart(2, '0')).join('');
      lf.looper.clearAll();
      return { frames, digest, byteLength: bytes.byteLength };
    });

    await page.addInitScript(({ failure, frames }) => {
      if (failure === 'read') {
        const real = IDBObjectStore.prototype.get;
        let calls = 0;
        IDBObjectStore.prototype.get = function (...args) {
          if (this.name === 'recovery' && this.transaction.db.name === 'bleeploop' && calls++ === 0) {
            throw new DOMException('injected recovery read failure', 'UnknownError');
          }
          return real.apply(this, args);
        };
        window.__recoveryImportFailureCalls = () => calls;
        return;
      }
      if (failure === 'buffer') {
        const real = AudioContext.prototype.createBuffer;
        let calls = 0;
        AudioContext.prototype.createBuffer = function (channels, length, sampleRate) {
          if (channels === 1 && length === frames && ++calls === 2) {
            throw new Error('injected second-lane buffer failure');
          }
          return real.call(this, channels, length, sampleRate);
        };
        window.__recoveryImportFailureCalls = () => calls;
        return;
      }
      const real = AudioBufferSourceNode.prototype.start;
      let calls = 0;
      AudioBufferSourceNode.prototype.start = function (...args) {
        if (this.loop && this.buffer?.length === frames && ++calls === 2) {
          throw new Error('injected second-lane source.start failure');
        }
        return real.apply(this, args);
      };
      window.__recoveryImportFailureCalls = () => calls;
    }, { ...scenario, frames: prepared.frames });

    await page.reload();
    await page.waitForFunction(() => !!window.__lf);
    const result = await page.evaluate(async ({ prepared, scenario }) => {
      const lf = window.__lf;
      const { engineState } = await import('/src/audio/looper/state.ts');
      await lf.autosave.ready();

      const recovery = async () => {
        const record = await new Promise((resolve, reject) => {
          const open = indexedDB.open('bleeploop', 1);
          open.onerror = () => reject(open.error);
          open.onsuccess = () => {
            const db = open.result;
            const tx = db.transaction('recovery', 'readonly');
            const request = tx.objectStore('recovery').get('latest');
            tx.oncomplete = () => { db.close(); resolve(request.result); };
            tx.onabort = () => { db.close(); reject(tx.error); };
          };
        });
        if (!record) return null;
        const bytes = new Uint8Array(record.bytes);
        const digest = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)))
          .map((value) => value.toString(16).padStart(2, '0')).join('');
        return { digest, byteLength: bytes.byteLength };
      };
      const state = () => ({
        masterFrames: lf.looper.masterFramesValue(),
        states: Array.from({ length: lf.looper.trackCount }, (_, i) => lf.looper.stateOf(i)),
        lengths: Array.from({ length: lf.looper.trackCount }, (_, i) => lf.looper.trackInfo(i).lengthFrames),
        liveSources: engineState.tracks.filter((track) => track.source !== null || track.retiringSources.size > 0).length,
      });

      const afterFailure = state();
      await new Promise((resolve) => setTimeout(resolve, 3200));
      const afterAutomaticWait = await recovery();
      await lf.autosave.flush(); // Same unconditional path used by the native close guard.
      const afterCloseFlush = await recovery();
      let finish;
      if (scenario.finish === 'clear') {
        lf.looper.clearAll(); // Explicit user action changes the protected blank fingerprint.
        await lf.autosave.flush();
        finish = { saved: await recovery() };
      } else {
        const restored = await lf.autosave.restoreLatest();
        const snapshot = lf.looper.exportSnapshot();
        finish = {
          restored,
          state: state(),
          tracks: snapshot.tracks.map((track) => ({
            index: track.index,
            first: track.pcm[0],
            last: track.pcm[track.pcm.length - 1],
            volume: track.volume,
            muted: track.muted,
          })),
        };
      }
      return {
        injectedCalls: window.__recoveryImportFailureCalls(),
        afterFailure,
        afterAutomaticWait,
        afterCloseFlush,
        finish,
        expected: prepared,
      };
    }, { prepared, scenario });

    const exact = (saved) => saved?.digest === prepared.digest && saved?.byteLength === prepared.byteLength;
    const blank = result.afterFailure.masterFrames === 0
      && result.afterFailure.states.every((state) => state === 'EMPTY')
      && result.afterFailure.lengths.every((length) => length === 0)
      && result.afterFailure.liveSources === 0;
    const finishPassed = scenario.finish === 'clear'
      ? result.finish.saved === null
      : result.finish.restored
        && result.finish.state.masterFrames === prepared.frames
        && result.finish.state.states.slice(0, 2).every((state) => state === 'PLAYING')
        && result.finish.tracks.length === 2
        && result.finish.tracks[0].first === Math.fround(0.1)
        && result.finish.tracks[1].first === Math.fround(0.2)
        && result.finish.tracks[0].volume === 0.5
        && result.finish.tracks[1].volume === 0.75
        && result.finish.tracks[1].muted;
    results.push({
      ...scenario,
      pass: result.injectedCalls >= (scenario.failure === 'read' ? 1 : 2)
        && blank && exact(result.afterAutomaticWait)
        && exact(result.afterCloseFlush) && finishPassed && errors.length === 0
        && reportedFailures.some((message) => message.includes('Jam recovery could not be restored')),
      errors,
      reportedFailures,
      ...result,
    });
    await context.close();
  }

  // A real user load can win while startup recovery is waiting for looper init. The failed restore
  // must not protect the live jam's fingerprint and resurrect the older archive on close.
  {
    const context = await browser.newContext();
    const page = await context.newPage();
    const errors = [];
    const reportedFailures = [];
    page.on('pageerror', (error) => errors.push(String(error)));
    page.on('console', (message) => {
      if (message.type() === 'error') reportedFailures.push(message.text());
    });
    await page.goto(url);
    await page.waitForFunction(() => !!window.__lf);
    const prepared = await page.evaluate(async () => {
      const lf = window.__lf;
      await lf.autosave.ready();
      lf.autosave.start()();
      await lf.autosave.clearSaved();
      lf.looper.clearAll();
      const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
      const frames = Math.round(lf.engine.ctx.sampleRate * 0.8);
      const pcm = new Float32Array(frames).fill(0.15);
      await lf.looper.loadSession({ bpm: 300, bars: 1, masterLengthFrames: frames, tracks: [{
        index: 0, pcm, volume: 0.8, muted: false, reversed: false, fx: defaultFxStates(),
      }] });
      await lf.autosave.flush();
      lf.looper.clearAll();
      return { frames };
    });

    await page.addInitScript(() => {
      let releaseRead;
      let releaseInit;
      const readGate = new Promise((resolve) => { releaseRead = resolve; });
      const initGate = new Promise((resolve) => { releaseInit = resolve; });
      window.__releaseRecoveryRead = releaseRead;
      window.__releaseLooperInit = releaseInit;
      window.__recoveryReadDelivered = false;
      window.__looperInitWaiting = false;

      const nativeGet = IDBObjectStore.prototype.get;
      let delayedRead = false;
      IDBObjectStore.prototype.get = function (...args) {
        const request = nativeGet.apply(this, args);
        if (delayedRead || this.name !== 'recovery' || this.transaction.db.name !== 'bleeploop') return request;
        delayedRead = true;
        return {
          get result() { return request.result; },
          get error() { return request.error; },
          set onsuccess(handler) {
            request.addEventListener('success', (event) => {
              void readGate.then(() => {
                window.__recoveryReadDelivered = true;
                handler.call(request, event);
              });
            }, { once: true });
          },
          set onerror(handler) {
            request.addEventListener('error', (event) => handler.call(request, event), { once: true });
          },
        };
      };

      const nativeAddModule = AudioWorklet.prototype.addModule;
      let delayedInit = false;
      AudioWorklet.prototype.addModule = function (...args) {
        const loading = nativeAddModule.apply(this, args);
        if (delayedInit) return loading;
        delayedInit = true;
        window.__looperInitWaiting = true;
        return loading.then(() => initGate);
      };
    });

    await page.reload();
    await page.waitForFunction(() => !!window.__lf);
    await page.evaluate(({ frames }) => {
      const lf = window.__lf;
      const loadSession = lf.looper.loadSession;
      window.__loadSessionOutcomes = [];
      lf.looper.loadSession = async (...args) => {
        try {
          const value = await loadSession(...args);
          window.__loadSessionOutcomes.push('resolved');
          return value;
        } catch (error) {
          window.__loadSessionOutcomes.push(String(error));
          throw error;
        }
      };
      window.__userLoadPromise = (async () => {
        const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
        const pcm = new Float32Array(frames).fill(0.625);
        await lf.looper.loadSession({ bpm: 300, bars: 1, masterLengthFrames: frames, tracks: [{
          index: 0, pcm, volume: 0.35, muted: true, reversed: false, fx: defaultFxStates(),
        }] });
      })();
    }, prepared);
    await page.waitForFunction(() => window.__looperInitWaiting === true);
    await page.evaluate(() => window.__releaseRecoveryRead());
    await page.waitForFunction(() => window.__recoveryReadDelivered === true);
    await page.waitForTimeout(100);
    await page.evaluate(() => window.__releaseLooperInit());

    const result = await page.evaluate(async () => {
      const lf = window.__lf;
      await window.__userLoadPromise;
      await lf.autosave.ready();
      const raceOutcomes = [...window.__loadSessionOutcomes];
      const beforeFlush = lf.looper.exportSnapshot();
      await lf.autosave.flush();
      lf.looper.clearAll();
      const restored = await lf.autosave.restoreLatest();
      const afterRestore = lf.looper.exportSnapshot();
      return {
        restored,
        raceOutcomes,
        before: beforeFlush.tracks.map((track) => ({
          first: track.pcm[0], volume: track.volume, muted: track.muted,
        })),
        after: afterRestore.tracks.map((track) => ({
          first: track.pcm[0], volume: track.volume, muted: track.muted,
        })),
      };
    });
    const expected = [{ first: Math.fround(0.625), volume: 0.35, muted: true }];
    results.push({
      failure: 'live-jam-wins-startup-race',
      pass: result.restored && JSON.stringify(result.before) === JSON.stringify(expected)
        && JSON.stringify(result.after) === JSON.stringify(expected)
        && result.raceOutcomes.includes('resolved')
        && result.raceOutcomes.some((outcome) => outcome.includes('import never overwrites a session'))
        && errors.length === 0,
      errors,
      reportedFailures,
      result,
    });
    await context.close();
  }

  console.log(JSON.stringify(results, null, 2));
  for (const result of results) assert.ok(result.pass, JSON.stringify(result));
} finally {
  await browser.close();
}

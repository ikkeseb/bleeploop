/**
 * The startup restore (`src/session/autosave.ts`) is transactional across a failed load, on the web
 * engine fake (`src/platform/host.web.ts`), whose feed and snapshot the probe scripts. A failed archive
 * read, or an engine that fails the restore's load (the fake's `loadSession` rejects once), must leave
 * the previous archive intact through the automatic save's wait and the close guard's flush, keep the
 * lanes blank, and report the failure; then a later retry restores it (the session the fake engine was
 * asked to load, and the mix the store took), or the player's clear recovers cleanly: a CLEAR ALL of the
 * blank lanes keeps the archive (only a clear that took a loop empties the recovery), a take cleared
 * with CLEAR ALL deletes it. A fourth scenario checks that a live user load winning a race against the
 * startup restore (the engine refuses the restore's load, its feed already showing the user's loops) is
 * never overwritten by the stale archive at close.
 *
 * Cannot see the native engine (its own refusal of a load into a non-empty looper is lf-engine's
 * `tests/session.rs`), native storage limits or WebView2.
 * Run: pnpm probe recovery-import-failure
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const FRAMES = 38400; // one bar at 300 BPM
const REFUSAL = 'the looper is not empty: clear every track first'; // lf_engine::SessionError::NotEmpty

const scenarios = [
  { failure: 'engine-load', finish: 'clear' },
  { failure: 'engine-load', finish: 'retry' },
  { failure: 'read', finish: 'retry' },
];

/** Before the app loads, on every load: engine mode on the fake, and the feed frames the probe sends. */
function engineScript({ rate, frames }) {
  window.__lfEngineFake = true;
  let seq = 0;
  const emit = (frame) => window.__lf.native.emit({ seq: ++seq, reset: false, events: [], ...frame });
  const lane = (state, length) => ({ state, length, armed: false, autoArmed: false, canUndo: false,
    canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
  window.__engine = {
    /** A new engine's first frame: every lane EMPTY (not the player's clear). */
    blank: () => emit({ reset: true, settings: [], anchor: { frame: 0, atMs: Date.now(), rate, grid: 0 },
      events: [{ Transport: { frame: 0, master: 0, bpm: 300, locked: false } },
        ...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }))] }),
    /** The engine holds `pcm` (one loop per lane from lane 1, playing): its feed, with each loop's
     * waveform, and its snapshot. */
    async commit(pcm) {
      const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
      window.__lf.native.snapshotBytes = encodeSessionBytes({ rate, masterLengthFrames: frames, bpm: 300,
        tracks: pcm.map((_, index) => ({ index, frames, reversed: false, state: 'Playing' })) }, pcm).buffer;
      emit({ events: [{ Transport: { frame: 0, master: frames, bpm: 300, locked: true } },
        ...pcm.map((_, i) => ({ Lane: { frame: 0, lane: i, info: lane('Playing', frames) } }))],
      peaks: pcm.map((p, i) => ({ lane: i, start: 0, count: 1, min: [-Math.abs(p[0])], max: [Math.abs(p[0])] })) });
    },
    /** The engine's CLEAR ALL: `Cleared` before each lane's own event; its snapshot is empty. */
    clearAll() {
      window.__lf.native.snapshotBytes = null;
      emit({ events: [...[0, 1, 2, 3, 4].flatMap((i) => [{ Cleared: { frame: 0, lane: i } },
        { Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }]),
      { Transport: { frame: 0, master: 0, bpm: 300, locked: false } }] });
    },
    /** The recovery's `latest` archive: its SHA-256 and size (none: null). */
    async recovery() {
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
    },
  };
}

/** Hold the first read of the recovery store until `window.__releaseRecoveryRead()`. */
function gateRecoveryRead() {
  let releaseRead;
  const readGate = new Promise((resolve) => { releaseRead = resolve; });
  window.__releaseRecoveryRead = releaseRead;
  window.__recoveryReadDelivered = false;
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
}

/** Open a fresh profile on the fake and save a jam of `tracks` there: lane i's PCM rises from `base` by
 * `slope` over the loop, with its mix. */
async function saveJam(browser, open, tracks) {
  const context = await browser.newContext();
  const app = await open({ context, init: (p) => p.addInitScript(engineScript, { rate: RATE, frames: FRAMES }) });
  await app.page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
  const prepared = await app.page.evaluate(async ({ tracks, frames }) => {
    const lf = window.__lf;
    await lf.autosave.ready();
    lf.autosave.start()(); // Stop automatic writes while the fixture is assembled.
    await lf.autosave.clearSaved();
    window.__engine.blank();
    tracks.forEach((t, i) => {
      lf.looper.setVolume(i, t.volume);
      lf.looper.setMute(i, t.muted);
    });
    await window.__engine.commit(tracks.map((t) => Float32Array.from({ length: frames }, (_, frame) => t.base + (frame / frames) * t.slope)));
    await lf.autosave.flush();
    return { frames, ...(await window.__engine.recovery()) };
  }, { tracks, frames: FRAMES });
  return { ...app, context, prepared };
}

await probe(async ({ open, browser }) => {
  const results = [];
  for (const scenario of scenarios) {
    const { page, pageErrors: errors, consoleErrors: reportedFailures, context, prepared } = await saveJam(browser, open, [
      { base: 0.1, slope: 0.01, volume: 0.5, muted: false },
      { base: 0.2, slope: 0.01, volume: 0.75, muted: true },
    ]);

    if (scenario.failure === 'read') {
      await page.addInitScript(() => {
        const real = IDBObjectStore.prototype.get;
        let calls = 0;
        IDBObjectStore.prototype.get = function (...args) {
          if (this.name === 'recovery' && this.transaction.db.name === 'bleeploop' && calls++ === 0) {
            throw new DOMException('injected recovery read failure', 'UnknownError');
          }
          return real.apply(this, args);
        };
        window.__recoveryImportFailureCalls = () => calls;
      });
    } else {
      // The restore reads the archive only once the engine fails its next load.
      await page.addInitScript(gateRecoveryRead);
    }
    await page.reload();
    await page.waitForFunction(() => !!window.__lf && window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
    if (scenario.failure === 'engine-load') {
      await page.evaluate(() => {
        const native = window.__lf.native;
        const real = native.loadSession;
        let calls = 0;
        native.loadSession = async (bytes) => {
          if (++calls === 1) throw new Error('injected engine load failure');
          return real.call(native, bytes);
        };
        window.__recoveryImportFailureCalls = () => calls;
        window.__releaseRecoveryRead();
      });
    }

    const result = await page.evaluate(async ({ scenario }) => {
      const lf = window.__lf;
      await lf.autosave.ready();
      const state = () => ({
        masterFrames: lf.looper.masterFramesValue(),
        states: Array.from({ length: lf.looper.trackCount }, (_, i) => lf.looper.stateOf(i)),
        lengths: Array.from({ length: lf.looper.trackCount }, (_, i) => lf.looper.trackInfo(i).lengthFrames),
      });

      const afterFailure = state();
      await new Promise((resolve) => setTimeout(resolve, 3200));
      const afterAutomaticWait = await window.__engine.recovery();
      await lf.autosave.flush(); // Same unconditional path used by the native close guard.
      const afterCloseFlush = await window.__engine.recovery();
      let finish;
      if (scenario.finish === 'clear') {
        window.__engine.clearAll(); // The player's CLEAR ALL of lanes that hold no loop.
        await lf.autosave.flush();
        const afterEmptyClear = await window.__engine.recovery();
        await window.__engine.commit([new Float32Array(38400).fill(0.5)]); // A take, then CLEAR ALL.
        window.__engine.clearAll();
        await lf.autosave.flush();
        finish = { afterEmptyClear, saved: await window.__engine.recovery() };
      } else {
        const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
        const restored = await lf.autosave.restoreLatest();
        const loaded = lf.native.loadedSessions.at(-1);
        const { header, pcm } = loaded ? splitSessionBytes(loaded.slice().buffer) : { header: null, pcm: [] };
        finish = {
          restored,
          loads: lf.native.loadedSessions.length,
          master: header?.masterLengthFrames ?? null,
          states: header?.tracks.map((t) => t.state) ?? [],
          tracks: pcm.map((track, i) => ({
            first: track[0],
            last: track[track.length - 1],
            volume: lf.looper.trackVolume(i),
            muted: lf.looper.trackMuted(i),
          })),
        };
      }
      return {
        injectedCalls: window.__recoveryImportFailureCalls(),
        afterFailure,
        afterAutomaticWait,
        afterCloseFlush,
        finish,
      };
    }, { scenario });

    const exact = (saved) => saved?.digest === prepared.digest && saved?.byteLength === prepared.byteLength;
    const blank = result.afterFailure.masterFrames === 0
      && result.afterFailure.states.every((state) => state === 'EMPTY')
      && result.afterFailure.lengths.every((length) => length === 0);
    const finishPassed = scenario.finish === 'clear'
      ? exact(result.finish.afterEmptyClear) && result.finish.saved === null
      : result.finish.restored
        && result.finish.loads === 1
        && result.finish.master === prepared.frames
        && JSON.stringify(result.finish.states) === JSON.stringify(['Playing', 'Playing'])
        && result.finish.tracks.length === 2
        && result.finish.tracks[0].first === Math.fround(0.1)
        && result.finish.tracks[1].first === Math.fround(0.2)
        && result.finish.tracks[0].volume === 0.5
        && result.finish.tracks[1].volume === 0.75
        && !result.finish.tracks[0].muted
        && result.finish.tracks[1].muted;
    results.push({
      ...scenario,
      pass: result.injectedCalls >= 1
        && blank && exact(result.afterAutomaticWait)
        && exact(result.afterCloseFlush) && finishPassed && errors.length === 0
        && reportedFailures.some((message) => message.includes('Jam recovery could not be restored')),
      errors,
      reportedFailures,
      expected: prepared,
      ...result,
    });
    await context.close();
  }

  // A real user load can win while the startup restore waits for its archive read. The engine refuses
  // the restore's load (its feed already shows the user's loops): the failed restore must not protect
  // the fingerprint, and the older archive must not come back at close.
  {
    const { page, pageErrors: errors, reportedFailures, context } = await (async () => {
      const app = await saveJam(browser, open, [{ base: 0.15, slope: 0, volume: 0.8, muted: false }]);
      return { ...app, reportedFailures: app.consoleErrors };
    })();
    await page.addInitScript(gateRecoveryRead);
    await page.reload();
    await page.waitForFunction(() => !!window.__lf && window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
    await page.evaluate(async ({ frames, refusal }) => {
      const lf = window.__lf;
      const { session } = await import('/src/ui/state/audio.ts');
      const native = lf.native;
      window.__loadSessionOutcomes = [];
      const loadSession = session.loadSession;
      session.loadSession = async (...args) => {
        try {
          const value = await loadSession(...args);
          window.__loadSessionOutcomes.push('resolved');
          return value;
        } catch (error) {
          window.__loadSessionOutcomes.push(String(error));
          throw error;
        }
      };
      // The user's load goes to the engine first; the restore's next load meets the engine holding it.
      const pcm = new Float32Array(frames).fill(0.625);
      const real = native.loadSession;
      let calls = 0;
      native.loadSession = async (bytes) => {
        if (++calls !== 2) return real.call(native, bytes);
        await window.__engine.commit([pcm]);
        throw new Error(refusal);
      };
      await session.loadSession({ bpm: 300, bars: 1, masterLengthFrames: frames, tracks: [{
        index: 0, pcm, volume: 0.35, muted: true, reversed: false, fx: lf.looper.fxState(0),
      }] });
    }, { frames: FRAMES, refusal: REFUSAL });
    await page.evaluate(() => window.__releaseRecoveryRead());
    await page.waitForFunction(() => window.__recoveryReadDelivered === true);

    const result = await page.evaluate(async () => {
      const lf = window.__lf;
      const { session } = await import('/src/ui/state/audio.ts');
      const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
      await lf.autosave.ready();
      const raceOutcomes = [...window.__loadSessionOutcomes];
      const beforeFlush = await session.exportSnapshot();
      await lf.autosave.flush();
      window.__engine.blank();
      const restored = await lf.autosave.restoreLatest();
      const loaded = splitSessionBytes(lf.native.loadedSessions.at(-1).slice().buffer);
      return {
        restored,
        raceOutcomes,
        before: beforeFlush.tracks.map((track) => ({
          first: track.pcm[0], volume: track.volume, muted: track.muted,
        })),
        after: loaded.pcm.map((track, i) => ({
          first: track[0], volume: lf.looper.trackVolume(i), muted: lf.looper.trackMuted(i),
        })),
      };
    });
    const expected = [{ first: Math.fround(0.625), volume: 0.35, muted: true }];
    results.push({
      failure: 'live-jam-wins-startup-race',
      pass: result.restored && JSON.stringify(result.before) === JSON.stringify(expected)
        && JSON.stringify(result.after) === JSON.stringify(expected)
        && result.raceOutcomes.includes('resolved')
        && result.raceOutcomes.some((outcome) => outcome.includes(REFUSAL))
        && errors.length === 0,
      errors,
      reportedFailures,
      result,
    });
    await context.close();
  }

  console.log(JSON.stringify(results, null, 2));
  for (const result of results) assert.ok(result.pass, JSON.stringify(result));
});

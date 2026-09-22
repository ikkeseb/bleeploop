/** Recovery playback-state round trip through the production autosave archive and import path.
 * Run against Vite: node verify/session-state-roundtrip.mjs --url=http://localhost:1420
 */
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });

try {
  const context = await browser.newContext();
  let page = await context.newPage();
  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf);

  const before = await page.evaluate(async () => {
    const lf = window.__lf;
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const { engineState } = await import('/src/audio/looper/state.ts');
    await lf.autosave.ready();
    lf.autosave.start()();
    await lf.autosave.clearSaved();
    lf.looper.clearAll();

    const frames = Math.round(lf.engine.ctx.sampleRate * 0.8); // one bar @ 300 BPM
    const tracks = [0, 1].map((index) => ({
      index,
      pcm: new Float32Array(frames).fill((index + 1) * 0.1),
      volume: 1,
      muted: false,
      reversed: false,
      state: index === 0 ? 'PLAYING' : 'STOPPED',
      fx: defaultFxStates(),
    }));
    await lf.looper.loadSession({ bpm: 300, bars: 1, masterLengthFrames: frames, tracks });
    await lf.autosave.flush();

    const savedAt = () => new Promise((resolve, reject) => {
      const open = indexedDB.open('bleeploop', 1);
      open.onerror = () => reject(open.error);
      open.onsuccess = () => {
        const db = open.result;
        const tx = db.transaction('recovery', 'readonly');
        const request = tx.objectStore('recovery').get('latest');
        tx.oncomplete = () => { db.close(); resolve(request.result?.savedAt ?? 0); };
        tx.onabort = () => { db.close(); reject(tx.error); };
      };
    });
    const firstSavedAt = await savedAt();

    // State is the only change. Normal autosave (not flush) must observe it through the fingerprint.
    lf.autosave.start();
    lf.looper.stop(0);
    const deadline = performance.now() + 7000;
    let stoppedSavedAt = firstSavedAt;
    while (stoppedSavedAt <= firstSavedAt && performance.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 100));
      stoppedSavedAt = await savedAt();
    }

    return {
      states: [lf.looper.stateOf(0), lf.looper.stateOf(1)],
      sources: engineState.tracks.slice(0, 2).map((track) => track.source !== null),
      stateOnlyAutosaveAdvanced: stoppedSavedAt > firstSavedAt,
    };
  });
  assert.deepEqual(before.states, ['STOPPED', 'STOPPED']);
  assert.deepEqual(before.sources, [false, false]);
  assert.equal(before.stateOnlyAutosaveAdvanced, true);

  await page.close();
  page = await context.newPage();
  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf);
  const after = await page.evaluate(async () => {
    const lf = window.__lf;
    const { engineState } = await import('/src/audio/looper/state.ts');
    await lf.autosave.ready();
    const restored = {
      states: [lf.looper.stateOf(0), lf.looper.stateOf(1)],
      sources: engineState.tracks.slice(0, 2).map((track) => ({
        source: track.source !== null,
        retiring: track.retiringSources.size,
      })),
    };

    const starts = [];
    const masterFrames = lf.looper.masterLengthFrames();
    let createGainCalls = 0;
    const nativeCreateGain = AudioContext.prototype.createGain;
    AudioContext.prototype.createGain = function (...args) {
      createGainCalls++;
      // Deterministically expire PLAY ALL's 20 ms lead if a cold imported lane still builds its
      // graph after the shared restart anchor was chosen. Correctly-prepared lanes never enter here.
      const until = performance.now() + 30;
      while (performance.now() < until) { /* injected cold-graph cost */ }
      return nativeCreateGain.apply(this, args);
    };
    const nativeStart = AudioBufferSourceNode.prototype.start;
    AudioBufferSourceNode.prototype.start = function (...args) {
      if (this.loop && this.buffer?.length === masterFrames) starts.push(args);
      return nativeStart.apply(this, args);
    };
    try {
      lf.looper.playAll();
    } finally {
      AudioBufferSourceNode.prototype.start = nativeStart;
      AudioContext.prototype.createGain = nativeCreateGain;
    }
    return {
      restored,
      played: {
        states: [lf.looper.stateOf(0), lf.looper.stateOf(1)],
        sources: engineState.tracks.slice(0, 2).map((track) => track.source !== null),
        starts,
        createGainCalls,
      },
    };
  });

  assert.deepEqual(after.restored.states, ['STOPPED', 'STOPPED']);
  assert.deepEqual(after.restored.sources, [
    { source: false, retiring: 0 },
    { source: false, retiring: 0 },
  ]);
  assert.deepEqual(after.played.states, ['PLAYING', 'PLAYING']);
  assert.deepEqual(after.played.sources, [true, true]);
  assert.equal(after.played.createGainCalls, 0, 'import must prepare stopped-lane graphs before PLAY ALL');
  assert.equal(after.played.starts.length, 2);
  assert.equal(after.played.starts[0][0], after.played.starts[1][0], 'PLAY ALL must share one restart anchor');

  console.log(JSON.stringify({ pass: true, before, after }, null, 2));
  await context.close();
} finally {
  await browser.close();
}

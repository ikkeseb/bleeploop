/**
 * A lane's playback state round-trips through local recovery, on the web engine fake
 * (`src/platform/host.web.ts`, the `engine-seam` pattern: the probe scripts the feed and the snapshot
 * the engine would answer through `__lf.native`), with the real autosave, recovery archive and import
 * executing (`src/session/autosave.ts`, `import.ts`):
 *
 * - a state-only change (a PLAYING lane the engine reports STOPPED, no PCM or mix edit) advances the
 *   recovery on the next normal autosave (no forced flush), where an unchanged jam saves nothing more;
 * - a reloaded page (a fresh engine) loads that jam back with both lanes STOPPED, the lane that played
 *   when it was first saved included.
 *
 * Cannot see the native engine (what a load or PLAY ALL does to the lanes: lf-engine `tests/session.rs`
 * and `tests/fade.rs`), Tauri IPC or the rig: the fake answers no command by itself.
 * Run: pnpm probe session-state-roundtrip
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const FRAMES = Math.round(RATE * 0.8); // one bar at 300 BPM

const lane = (state) => ({
  state,
  length: state === 'Empty' ? 0 : FRAMES,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: state === 'Playing' || state === 'Stopped',
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
});
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });

await probe(async ({ browser, open }) => {
  const context = await browser.newContext();
  const init = (p) => p.addInitScript(() => void (window.__lfEngineFake = true));
  const { page, consoleErrors } = await open({ context, init });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
  await page.evaluate(async () => {
    await window.__lf.autosave.ready();
    await window.__lf.autosave.clearSaved();
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  /** What the engine holds: the feed's lane states and the snapshot it answers (the PCM never changes). */
  const engineHolds = async (states) => {
    await page.evaluate(async ([states, frames]) => {
      const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
      window.__lf.native.snapshotBytes = encodeSessionBytes(
        { rate: 48000, masterLengthFrames: frames, bpm: 300,
          tracks: states.map((state, index) => ({ index, frames, reversed: false, state })) },
        states.map((_, index) => new Float32Array(frames).fill((index + 1) * 0.1)),
      ).buffer;
    }, [states, FRAMES]);
    await emit({ events: [{ Transport: { frame: 0, master: FRAMES, bpm: 300, locked: true } }, ...states.map((s, i) => laneEvent(i, lane(s)))] });
  };
  await emit({
    reset: true,
    settings: [],
    events: [{ Transport: { frame: 0, master: 0, bpm: 300, locked: false } }, ...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });

  // Two lanes, loaded as an import loads them: lane 1 PLAYING, lane 2 STOPPED.
  await page.evaluate(async (frames) => {
    const { session } = await import('/src/ui/state/audio.ts');
    const { defaultFxStates } = await import('/src/ui/state/fx-metadata.ts');
    await session.loadSession({ bpm: 300, bars: 1, masterLengthFrames: frames, tracks: [0, 1].map((index) => ({
      index, pcm: new Float32Array(frames).fill((index + 1) * 0.1), volume: 1, muted: false, reversed: false,
      state: index === 0 ? 'PLAYING' : 'STOPPED', fx: defaultFxStates() })) });
  }, FRAMES);
  await engineHolds(['Playing', 'Stopped']);
  const savedAt = () => page.evaluate(() => new Promise((resolve, reject) => {
    const request = indexedDB.open('bleeploop', 1);
    request.onerror = () => reject(request.error);
    request.onsuccess = () => {
      const db = request.result;
      const tx = db.transaction('recovery', 'readonly');
      const get = tx.objectStore('recovery').get('latest');
      tx.oncomplete = () => { db.close(); resolve(get.result?.savedAt ?? 0); };
      tx.onabort = () => { db.close(); reject(tx.error); };
    };
  }));
  // The normal autosave saves the jam once it holds still, and then holds still itself.
  let firstSavedAt = 0;
  for (const t0 = Date.now(); firstSavedAt === 0 && Date.now() - t0 < 7000; await page.waitForTimeout(100)) firstSavedAt = await savedAt();
  assert.ok(firstSavedAt > 0, 'autosave saved the jam');
  await page.waitForTimeout(3500);
  assert.equal(await savedAt(), firstSavedAt, 'with nothing changed, no further save');

  // State is the only change. Normal autosave (not flush) must observe it through the fingerprint.
  await engineHolds(['Stopped', 'Stopped']);
  const deadline = Date.now() + 7000;
  let stoppedSavedAt = firstSavedAt;
  while (stoppedSavedAt <= firstSavedAt && Date.now() < deadline) {
    await page.waitForTimeout(100);
    stoppedSavedAt = await savedAt();
  }
  const states = await page.evaluate(() => [window.__lf.looper.stateOf(0), window.__lf.looper.stateOf(1)]);
  console.log(JSON.stringify({ states, firstSavedAt, stoppedSavedAt }));
  assert.equal(stoppedSavedAt > firstSavedAt, true, 'a state-only change advances the next normal autosave');

  // A fresh engine: the reloaded page restores the jam into it.
  await page.reload();
  await page.waitForFunction(() => '__lf' in window && window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 15000 });
  const restored = await page.evaluate(async () => {
    const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    return splitSessionBytes(window.__lf.native.loadedSessions[0].slice().buffer).header;
  });
  console.log('restored', JSON.stringify(restored));
  assert.deepEqual(restored.tracks.map((t) => [t.index, t.state]), [[0, 'Stopped'], [1, 'Stopped']], 'the jam comes back with both lanes STOPPED');
  assert.deepEqual(consoleErrors, [], 'no console errors');
  await context.close();
});

/**
 * Recovery (`src/session/autosave.ts`) survives a real IndexedDB VersionError on the very first open,
 * in disposable browser storage: the production open call is substituted with one that opens version 1
 * against a database already seeded at version 2 (so the startup restore fails), then falls through to
 * the real open on the next call. On the web engine fake (`src/platform/host.web.ts`), whose feed and
 * snapshot the probe scripts: a committed loop saves, restores exactly (the session the fake engine was
 * asked to load), and the player's CLEAR ALL deletes it. Two attempts, each in a fresh browser context,
 * to catch order-dependent bugs a single run would hide.
 *
 * Cannot see the native engine, native storage limits or WebView2.
 * Run: pnpm probe recovery-failure
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open, browser }) => {
  for (let attempt = 0; attempt < 2; attempt++) {
    const context = await browser.newContext();
    try {
      const { page } = await open({
        context,
        init: (page) => page.addInitScript(() => {
          window.__lfEngineFake = true;
          const open = indexedDB.open.bind(indexedDB);
          // IndexedDB processes opens for one database in order. Version 1 then fails against version 2.
          const seed = open('lf-test-open-failure', 2);
          seed.onsuccess = () => seed.result.close();
          window.__recoveryOpenCalls = 0;
          indexedDB.open = (...args) => {
            window.__recoveryOpenCalls++;
            return window.__recoveryOpenCalls === 1 ? open('lf-test-open-failure', 1) : open(...args);
          };
        }),
      });
      await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
      await page.evaluate(() => window.__lf.autosave.ready());
      const result = await page.evaluate(async () => {
        const lf = window.__lf;
        const { encodeSessionBytes, splitSessionBytes } = await import('/src/platform/engine-wire.ts');
        const RATE = 48000;
        const frames = 2 * RATE; // one bar at 120 BPM
        let seq = 0;
        const emit = (frame) => lf.native.emit({ seq: ++seq, reset: false, events: [], ...frame });
        const lane = (state, length) => ({ state, length, armed: false, autoArmed: false, canUndo: false,
          canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
        const blank = () => emit({ reset: true, settings: [], anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
          events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
            ...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }))] });
        const commit = () => emit({ events: [{ Transport: { frame: 0, master: frames, bpm: 120, locked: true } },
          { Lane: { frame: 0, lane: 0, info: lane('Playing', frames) } }] });
        blank();
        const pcm = new Float32Array(frames);
        pcm[17] = 1.75;
        lf.native.snapshotBytes = encodeSessionBytes({ rate: RATE, masterLengthFrames: frames, bpm: 120,
          tracks: [{ index: 0, frames, reversed: false, state: 'Playing' }] }, [pcm]).buffer;
        commit();
        try {
          await lf.autosave.flush();
          const saved = await lf.autosave.hasSaved();
          blank(); // a new engine: blank lanes, not the player's clear
          const restored = await lf.autosave.restoreLatest();
          const loaded = lf.native.loadedSessions.at(-1);
          const exact = !!loaded && splitSessionBytes(loaded.slice().buffer).pcm[0].every((sample, i) => sample === pcm[i]);
          // The engine plays the restored loop; the player's CLEAR ALL empties it.
          commit();
          lf.native.snapshotBytes = null;
          emit({ events: [...[0, 1, 2, 3, 4].flatMap((i) => [{ Cleared: { frame: 0, lane: i } },
            { Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }]),
          { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }] });
          await lf.autosave.flush();
          return { saved, restored, exact, cleared: !(await lf.autosave.hasSaved()), opens: window.__recoveryOpenCalls };
        } catch (error) {
          return { error: String(error), opens: window.__recoveryOpenCalls };
        }
      });
      console.log(JSON.stringify({ attempt: attempt + 1, ...result }));
      assert.deepEqual(result, { saved: true, restored: true, exact: true, cleared: true, opens: 2 });
    } finally {
      await context.close();
    }
  }
});

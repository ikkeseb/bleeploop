/**
 * Recovery survives a real IndexedDB VersionError on the very first open, in disposable browser
 * storage: the production open call is substituted with one that opens version 1 against a database
 * already seeded at version 2, then falls through to the real open on the next call. Two attempts,
 * each in a fresh browser context, to catch order-dependent bugs a single run would hide. Cannot see
 * native storage limits or WebView2.
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
      await page.evaluate(() => window.__lf.autosave.ready());
      const result = await page.evaluate(async () => {
        const lf = window.__lf;
        await lf.looper.init();
        const frames = lf.engine.ctx.sampleRate * 2;
        const pcm = new Float32Array(frames);
        pcm[17] = 1.75;
        await lf.looper.loadSession({ bpm: 120, bars: 1, masterLengthFrames: frames, tracks: [{
          index: 0, pcm, volume: 0.5, muted: true, reversed: false, fx: lf.looper.fxState(0),
        }] });
        try {
          await lf.autosave.flush();
          const saved = await lf.autosave.hasSaved();
          lf.looper.clearAll();
          const restored = await lf.autosave.restoreLatest();
          const exact = lf.looper.exportSnapshot().tracks[0].pcm.every((sample, i) => sample === pcm[i]);
          lf.looper.clearAll();
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

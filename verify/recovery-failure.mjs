// Run with pnpm dev on port 1420: pnpm exec node verify/recovery-failure.mjs
// Real IndexedDB VersionError on the first open only, in disposable browser storage.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';

const browser = await chromium.launch({ headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  for (let attempt = 0; attempt < 2; attempt++) {
    const context = await browser.newContext();
    try {
      const page = await context.newPage();
      await page.addInitScript(() => {
        const open = indexedDB.open.bind(indexedDB);
        // IndexedDB processes opens for one database in order. Version 1 then fails against version 2.
        const seed = open('lf-test-open-failure', 2);
        seed.onsuccess = () => seed.result.close();
        window.__recoveryOpenCalls = 0;
        indexedDB.open = (...args) => {
          window.__recoveryOpenCalls++;
          return window.__recoveryOpenCalls === 1 ? open('lf-test-open-failure', 1) : open(...args);
        };
      });
      await page.goto('http://localhost:1420');
      await page.waitForFunction(() => !!window.__lf);
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
} finally {
  await browser.close();
}

/** Actual close-guard + IndexedDB deletion rollback, with only native close capabilities substituted.
 * node verify/recovery-close.mjs --url=http://localhost:1420
 * Browser proof of frontend close approval; does not exercise Rust/WebView2 window events.
 */
import { chromium } from 'playwright';

const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage();
  await page.route('**/src/app/close-guard.ts*', async (route) => {
    const response = await route.fetch();
    const source = await response.text();
    const capabilities = /import\s*\{[^}]*confirmNativeClose[^}]*\}\s*from\s*["'][^"']+["'];?/;
    if (!capabilities.test(source)) throw new Error('Cannot locate close-guard platform import');
    await route.fulfill({ response, body: source.replace(capabilities, `
      const platform = { kind: 'tauri' };
      const onNativeCloseRequested = (callback) => { window.__closeRequest = callback; };
      const confirmNativeClose = async () => { window.__closeApprovals = (window.__closeApprovals ?? 0) + 1; };
    `) });
  });
  await page.goto(process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  await page.waitForFunction(() => !!window.__lf && !!window.__closeRequest);
  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.autosave.ready();
    lf.autosave.start()();
    await lf.looper.init();
    const pcm = new Float32Array(lf.engine.ctx.sampleRate * 2);
    pcm[17] = 1.75;
    await lf.looper.loadSession({ bpm: 120, bars: 1, masterLengthFrames: pcm.length,
      tracks: [{ index: 0, pcm, volume: 0.5, muted: true, reversed: false, fx: lf.looper.fxState(0) }] });
    await lf.autosave.flush();
    lf.looper.clearAll();
    const original = IDBObjectStore.prototype.delete;
    let abortedDeletes = 0;
    IDBObjectStore.prototype.delete = function (...args) {
      const request = original.apply(this, args);
      if (this.name === 'recovery' && this.transaction.db.name === 'bleeploop') {
        request.addEventListener('success', () => { abortedDeletes++; this.transaction.abort(); }, { once: true });
      }
      return request;
    };
    const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    try {
      window.__closeRequest();
      const deadline = performance.now() + 5000;
      while (abortedDeletes === 0 && performance.now() < deadline) await wait(20);
      const failureNotice = 'Could not update recovery before closing';
      while (!(window.__closeApprovals > 0) && !document.body.textContent.includes(failureNotice)
        && performance.now() < deadline) await wait(20);
      const approvalsAfterFailure = window.__closeApprovals ?? 0;
      const previousJamPreserved = await lf.autosave.hasSaved();
      const visibleError = document.body.textContent.includes(failureNotice);
      IDBObjectStore.prototype.delete = original;
      window.__closeRequest();
      const retryDeadline = performance.now() + 5000;
      while ((window.__closeApprovals ?? 0) === approvalsAfterFailure && performance.now() < retryDeadline) await wait(20);
      const approvalsAfterRetry = window.__closeApprovals ?? 0;
      const staleJamDeleted = !(await lf.autosave.hasSaved());
      return { abortedDeletes, approvalsAfterFailure, previousJamPreserved, visibleError,
        approvalsAfterRetry, staleJamDeleted,
        pass: abortedDeletes === 1 && approvalsAfterFailure === 0 && previousJamPreserved
          && visibleError && approvalsAfterRetry === 1 && staleJamDeleted };
    } finally {
      IDBObjectStore.prototype.delete = original;
    }
  });
  console.log(JSON.stringify(result, null, 2));
  if (!result.pass) process.exitCode = 1;
} finally {
  await browser.close();
}

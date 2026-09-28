/**
 * The close guard (`src/app/close-guard.ts`) and the recovery's IndexedDB deletion rollback
 * (`src/session/autosave.ts`), with only the native close capabilities substituted, on the web engine
 * fake (`src/platform/host.web.ts`): the probe scripts the lanes on the feed and the snapshot the engine
 * answers. Two cases:
 *
 * - after the player's CLEAR ALL of a saved jam, a failed recovery deletion must not approve the close
 *   and shows the failure notice; a retry once the deletion works approves and clears the stale jam;
 * - closing DURING the very first take (the feed shows RECORDING, nothing committed, no master grid yet)
 *   must approve with no failure notice at all and save nothing.
 *
 * Browser proof of frontend close approval: cannot see Rust/WebView2 window events or the native engine.
 * Run: pnpm probe recovery-close
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

/** Page helpers: the feed frames the engine would send, and the snapshot it answers. */
function engineScript() {
  window.__lfEngineFake = true;
  const RATE = 48000;
  const frames = 2 * RATE; // one bar at 120 BPM
  let seq = 0;
  const emit = (frame) => window.__lf.native.emit({ seq: ++seq, reset: false, events: [], ...frame });
  const lane = (state, length) => ({ state, length, armed: false, autoArmed: false, canUndo: false,
    canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
  window.__engine = {
    /** The engine's first frame: every lane EMPTY. */
    blank: () => emit({ reset: true, settings: [], anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
        ...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }))] }),
    /** A loop committed on lane 1 (sample 17 is 1.75); the snapshot answers it. */
    async commit() {
      const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
      const pcm = new Float32Array(frames);
      pcm[17] = 1.75;
      window.__lf.native.snapshotBytes = encodeSessionBytes({ rate: RATE, masterLengthFrames: frames, bpm: 120,
        tracks: [{ index: 0, frames, reversed: false, state: 'Playing' }] }, [pcm]).buffer;
      emit({ events: [{ Transport: { frame: 0, master: frames, bpm: 120, locked: true } },
        { Lane: { frame: 0, lane: 0, info: lane('Playing', frames) } }] });
    },
    /** The engine's CLEAR ALL: `Cleared` before each lane's own event; its snapshot is empty. */
    clearAll() {
      window.__lf.native.snapshotBytes = null;
      emit({ events: [...[0, 1, 2, 3, 4].flatMap((i) => [{ Cleared: { frame: 0, lane: i } },
        { Lane: { frame: 0, lane: i, info: lane('Empty', 0) } }]),
      { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }] });
    },
    /** A first take records on lane 1: no loop yet. */
    firstTake: () => emit({ events: [{ Lane: { frame: 0, lane: 0, info: lane('Recording', 0) } }] }),
  };
}

await probe(async ({ open }) => {
  /** A page with the native close capabilities substituted. Each case gets its own page (an approved
   * close latches the guard's `closePending`, so a second close request on the same page is swallowed)
   * and its own console-error sink, so one case's injected failure cannot be read as another's. */
  const openGuardedPage = async () => {
    const app = await open({
      init: async (page) => {
        await page.addInitScript(engineScript);
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
      },
    });
    await app.page.waitForFunction(() => !!window.__closeRequest && window.__lf.native.opened.length >= 1);
    return app;
  };

  const { page } = await openGuardedPage();
  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.autosave.ready();
    lf.autosave.start()();
    window.__engine.blank();
    await window.__engine.commit();
    await lf.autosave.flush();
    window.__engine.clearAll();
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
  assert.ok(result.pass, JSON.stringify(result));

  // ── Closing during the FIRST take ───────────────────────────────────────────────────────
  // RECORDING makes the jam non-blank while nothing is committed and the master grid is still 0
  // frames. The recovery save must treat that as "nothing to save" rather than a grid failure, so the
  // close approves silently instead of warning about losing loops that never existed.
  const { page: firstTakePage, consoleErrors: firstTakeErrors } = await openGuardedPage();
  const firstTake = await firstTakePage.evaluate(async () => {
    const lf = window.__lf;
    await lf.autosave.ready();
    window.confirm = () => true; // the user's "yes, close" — the notice under test is the failure one
    const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const waitFor = async (predicate, label, ms = 10000) => {
      const deadline = performance.now() + ms;
      while (!predicate()) {
        if (performance.now() >= deadline) throw new Error(`Timed out: ${label}`);
        await wait(20);
      }
    };
    const approvalsBefore = window.__closeApprovals ?? 0;
    window.__engine.blank();
    window.__engine.firstTake();
    const stateAtClose = lf.looper.stateOf(0);
    const masterFramesAtClose = lf.looper.masterLengthFrames();
    let flushError = null;
    try {
      await lf.autosave.flush();
    } catch (error) {
      flushError = String(error);
    }
    window.__closeRequest();
    await waitFor(() => (window.__closeApprovals ?? 0) > approvalsBefore, 'close approval during the first take');
    const approvals = (window.__closeApprovals ?? 0) - approvalsBefore;
    const visibleError = document.body.textContent.includes('Could not update recovery before closing');
    const savedAfterClose = await lf.autosave.hasSaved();
    return {
      stateAtClose,
      masterFramesAtClose,
      flushError,
      approvals,
      visibleError,
      savedAfterClose,
      pass: stateAtClose === 'RECORDING' && masterFramesAtClose === 0 && flushError === null
        && approvals === 1 && !visibleError && !savedAfterClose,
    };
  });
  const autosaveErrors = firstTakeErrors.filter((text) => text.includes('[autosave]') || text.includes('[app] autosave'));
  console.log(JSON.stringify({ firstTake, autosaveErrors }, null, 2));
  assert.ok(firstTake.pass && autosaveErrors.length === 0, JSON.stringify({ firstTake, autosaveErrors }));
});

/**
 * The updater's UI half (`src/app/update.ts`): what a player sees when a newer release is out, and what
 * the command bar's UPDATE pill and its panel do, on a scripted updater (`window.__lfUpdateFake`,
 * `host.web.ts`) and the web engine fake. Five pages, each a fresh app:
 *
 * - no updater (the browser build): no pill in the command bar, no toast, and nothing about an update
 *   in Help, which is where the offer used to live;
 * - an offer with the looper empty: one toast names the version, the pill appears, its panel carries the
 *   version and the notes (bullets and bold dropped), and the press installs at once, asking nothing;
 * - an offer during a jam (a committed loop on the feed): the press asks first; "no" installs nothing,
 *   "yes" saves the jam for recovery and installs;
 * - the install's read-out: each stage the native side reports (`lf://update-progress`) reaches the
 *   panel — a download with a size fills the bar, one without leaves it indeterminate, and the signature
 *   check and the installer each say so;
 * - a failed install: the error toast, its `console.error` line, the read-out cleared, and the button
 *   back for another try.
 *
 * Cannot see the native half: GitHub, the real chunk timings, the signature check, the engine's
 * shutdown, the installer and the relaunch (`src-tauri/src/update.rs`; proven on a release,
 * `.github/workflows/build-exe.yml`).
 * Run: pnpm probe app-update [--shots=<dir>]
 */
import assert from 'node:assert/strict';
import { arg, probe } from '../harness/probe.ts';

const shots = arg('shots');
const OFFER = { version: '9.9.9', notes: '- **Faster takes.** A take lands sooner.\n- A second change' };
const TOAST = 'BleepLoop v9.9.9 is ready: press UPDATE in the command bar';

/** Page helpers: the scripted updater, the window.confirm answers, and the feed frames the engine sends. */
function pageScript(script) {
  if (script) window.__lfUpdateFake = { ...script, installs: 0 };
  window.__lfEngineFake = true;
  window.__confirms = [];
  window.__confirmAnswer = true;
  window.confirm = (message) => {
    window.__confirms.push(message);
    return window.__confirmAnswer;
  };
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
    /** A loop committed on lane 1; the snapshot answers it. */
    async commit() {
      const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
      const pcm = new Float32Array(frames);
      pcm[17] = 0.5;
      window.__lf.native.snapshotBytes = encodeSessionBytes({ rate: RATE, masterLengthFrames: frames, bpm: 120,
        tracks: [{ index: 0, frames, reversed: false, state: 'Playing' }] }, [pcm]).buffer;
      emit({ events: [{ Transport: { frame: 0, master: frames, bpm: 120, locked: true } },
        { Lane: { frame: 0, lane: 0, info: lane('Playing', frames) } }] });
    },
  };
}

await probe(async ({ open }) => {
  const openApp = async (script) => {
    const app = await open({ viewport: { width: 1280, height: 820 }, init: (page) => page.addInitScript(pageScript, script) });
    await app.page.waitForFunction(() => window.__lf.native.opened.length > 0, undefined, { timeout: 10_000 });
    await app.page.evaluate(() => window.__engine.blank());
    return app;
  };
  const pill = (page) => page.locator('.tool-pill');
  const panel = (page) => page.locator('#lf-update-popover .update');
  const installs = (page) => page.evaluate(() => window.__lfUpdateFake.installs);
  /** Play a native stage into the page (`lf://update-progress`'s payload shape). */
  const report = (page, stage) => page.evaluate((s) => window.__lfUpdateFake.report(s), stage);

  // 1. No updater.
  {
    const { page, consoleErrors } = await openApp(null);
    await page.waitForTimeout(500); // the launch check is async: give an offer the time to show
    assert.equal(await pill(page).count(), 0, 'an update pill without an updater');
    assert.equal(await page.locator('.toast', { hasText: 'is ready' }).count(), 0, 'an update toast without an updater');
    await page.locator('.tool--help').click();
    assert.equal(await page.getByRole('heading', { name: /Update ready/ }).count(), 0, 'Update ready in Help');
    assert.equal(await page.locator('#lf-help-popover').getByText(/update/i).count(), 0, 'Help still mentions updating');
    assert.deepEqual(consoleErrors, [], 'console errors');
    await page.close();
  }

  // 2. An offer, the looper empty.
  {
    const { page, consoleErrors } = await openApp({ update: OFFER });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    await pill(page).waitFor({ timeout: 5000 });
    assert.equal(await pill(page).innerText(), 'UPDATE', 'the pill reads'); // innerText: the CSS uppercase
    assert.equal(await pill(page).getAttribute('title'), 'BleepLoop v9.9.9 is ready to install', 'the pill title');
    assert.equal(await pill(page).getAttribute('aria-label'), 'Update ready, version 9.9.9', 'the pill name');
    // The old home must stay empty: Help is a reference, not where the app keeps its actions.
    await page.locator('.tool--help').click();
    assert.equal(await page.locator('#lf-help-popover').getByText(/update/i).count(), 0, 'the offer is back in Help');
    assert.equal(await page.locator('.tool--help').evaluate((el) => el.classList.contains('tool--badge')), false,
      'the Help cap still wears the old dot');
    await page.keyboard.press('Escape');

    await pill(page).click();
    await panel(page).waitFor({ timeout: 5000 });
    assert.match(await panel(page).locator('.update__title').innerText(), /^Update ready\s*v9\.9\.9$/, 'the panel title');
    assert.deepEqual(await panel(page).locator('.update__list li').allTextContents(),
      ['Faster takes. A take lands sooner.', 'A second change'], 'the notes');
    assert.equal(await panel(page).locator('.update__progress').count(), 0, 'a read-out before any press');
    if (shots) await page.screenshot({ path: `${shots}/app-update-panel.png` });
    await panel(page).getByRole('button', { name: 'Update and restart' }).click();
    await page.waitForFunction(() => window.__lfUpdateFake.installs === 1, undefined, { timeout: 5000 });
    assert.deepEqual(await page.evaluate(() => window.__confirms), [], 'an empty looper was asked');
    assert.deepEqual(consoleErrors, [], 'console errors');
    await page.close();
  }

  // 3. An offer during a jam.
  {
    const { page, consoleErrors } = await openApp({ update: OFFER });
    await page.evaluate(async () => {
      await window.__lf.autosave.ready();
      await window.__engine.commit();
    });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    await pill(page).click();
    const button = panel(page).getByRole('button', { name: 'Update and restart' });
    await page.evaluate(() => { window.__confirmAnswer = false; });
    await button.click();
    await page.waitForFunction(() => window.__confirms.length === 1, undefined, { timeout: 5000 });
    assert.match(await page.evaluate(() => window.__confirms[0]), /^Update to v9\.9\.9 now\?/, 'the jam ask');
    assert.equal(await installs(page), 0, 'installed after a "no"');
    await page.evaluate(() => { window.__confirmAnswer = true; });
    await button.click();
    await page.waitForFunction(() => window.__lfUpdateFake.installs === 1, undefined, { timeout: 10_000 });
    assert.equal(await page.evaluate(() => window.__confirms.length), 2, 'the second press was not asked');
    assert.equal(await page.evaluate(() => window.__lf.autosave.hasSaved()), true, 'the jam was not saved for recovery');
    assert.deepEqual(consoleErrors, [], 'console errors');
    await page.close();
  }

  // 4. The install's read-out. `hold` keeps install() unsettled, as the native one is once the
  //    installer runs, so each reported stage can be read off the panel.
  {
    const { page, consoleErrors } = await openApp({ update: OFFER, hold: true });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    await pill(page).click();
    const button = panel(page).getByRole('button', { name: /Update and restart|Updating/ });
    await button.click();
    await page.waitForFunction(() => window.__lfUpdateFake.installs === 1, undefined, { timeout: 5000 });
    assert.equal(await button.isEnabled(), false, 'the button stays pressable during an install');

    const stage = panel(page).locator('.update__stage');
    const fill = panel(page).locator('.update__fill');
    // A download with a declared size: the words carry both numbers and the bar is determinate.
    await report(page, { stage: 'downloading', downloaded: 4 * 1024 * 1024, total: 16 * 1024 * 1024 });
    await stage.waitFor({ timeout: 5000 });
    assert.equal(await stage.innerText(), 'Downloading 4.0 of 16.0 MB', 'the download read-out');
    assert.equal(await panel(page).locator('[role=progressbar]').getAttribute('aria-valuenow'), '0.25', 'the bar value');
    assert.equal(await fill.evaluate((el) => el.classList.contains('update__fill--unknown')), false,
      'a sized download reads as indeterminate');
    assert.equal(await fill.evaluate((el) => Math.round((el.getBoundingClientRect().width /
      el.parentElement.getBoundingClientRect().width) * 100)), 25, 'the fill width');
    if (shots) await page.screenshot({ path: `${shots}/app-update-downloading.png` });

    // No Content-Length: one number only, and the bar says "running" without claiming a figure.
    await report(page, { stage: 'downloading', downloaded: 9 * 1024 * 1024, total: null });
    await page.waitForFunction(() => document.querySelector('.update__stage')?.textContent === 'Downloading 9.0 MB',
      undefined, { timeout: 5000 });
    assert.equal(await panel(page).locator('[role=progressbar]').getAttribute('aria-valuenow'), null, 'a value without a size');
    assert.equal(await fill.evaluate((el) => el.classList.contains('update__fill--unknown')), true,
      'an unsized download reads as determinate');

    // The two waits past the download each name themselves.
    await report(page, { stage: 'verifying' });
    await page.waitForFunction(() => document.querySelector('.update__stage')?.textContent === 'Checking the signature',
      undefined, { timeout: 5000 });
    await report(page, { stage: 'installing' });
    await page.waitForFunction(
      () => document.querySelector('.update__stage')?.textContent === 'Installing — BleepLoop closes now',
      undefined, { timeout: 5000 });
    assert.deepEqual(consoleErrors, [], 'console errors');
    await page.close();
  }

  // 5. A failed install.
  {
    const { page, consoleErrors } = await openApp({ update: OFFER, fail: 'probe: the installer did not start' });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    await pill(page).click();
    const button = panel(page).getByRole('button', { name: 'Update and restart' });
    await button.click();
    await page.locator('.toast', { hasText: 'The update did not install' }).waitFor({ timeout: 5000 });
    assert.equal(await installs(page), 1, 'the install was not tried');
    assert.equal(await button.isEnabled(), true, 'the button stays disabled after a failure');
    // A stage left on screen under a pressable button would read as an install still running.
    assert.equal(await panel(page).locator('.update__progress').count(), 0, 'the read-out survived the failure');
    assert.equal(consoleErrors.length, 1, `console errors: ${JSON.stringify(consoleErrors)}`);
    assert.match(consoleErrors[0], /^\[app\] update failed/, 'the failure line');
    await page.close();
  }
});

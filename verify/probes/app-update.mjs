/**
 * The updater's UI half (`src/app/update.ts`): what a player sees when a newer release is out, and what
 * Help's UPDATE AND RESTART does, on a scripted updater (`window.__lfUpdateFake`, `host.web.ts`) and the
 * web engine fake. Four pages, each a fresh app:
 *
 * - no updater (the browser build): no dot on the Help cap, no "Update ready" in Help, no toast;
 * - an offer with the looper empty: one toast names the version, the Help cap wears its dot, Help opens
 *   on "Update ready" above the reference with the version and the notes (bullets and bold dropped),
 *   and the press installs at once, asking nothing;
 * - an offer during a jam (a committed loop on the feed): the press asks first; "no" installs nothing,
 *   "yes" saves the jam for recovery and installs;
 * - a failed install: the error toast, its `console.error` line, and the button back for another try.
 *
 * Cannot see the native half: GitHub, the signature check, the engine's shutdown, the installer and the
 * relaunch (`src-tauri/src/update.rs`; proven on a release, `.github/workflows/build-exe.yml`).
 * Run: pnpm probe app-update [--shots=<dir>]
 */
import assert from 'node:assert/strict';
import { arg, probe } from '../harness/probe.ts';

const shots = arg('shots');
const OFFER = { version: '9.9.9', notes: '- **Faster takes.** A take lands sooner.\n- A second change' };
const TOAST = 'BleepLoop v9.9.9 is ready: open Help (?) to update';

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
  const helpCap = (page) => page.locator('.tool--help');
  const updateSection = (page) => page.locator('#lf-help-popover .help__update');
  const installs = (page) => page.evaluate(() => window.__lfUpdateFake.installs);

  // 1. No updater.
  {
    const { page, consoleErrors } = await openApp(null);
    await page.waitForTimeout(500); // the launch check is async: give an offer the time to show
    assert.equal(await helpCap(page).evaluate((el) => el.classList.contains('tool--badge')), false, 'a dot without an updater');
    assert.equal(await page.locator('.toast', { hasText: 'is ready' }).count(), 0, 'an update toast without an updater');
    await helpCap(page).click();
    assert.equal(await page.getByRole('heading', { name: /Update ready/ }).count(), 0, 'Update ready without an updater');
    assert.deepEqual(consoleErrors, [], 'console errors');
    await page.close();
  }

  // 2. An offer, the looper empty.
  {
    const { page, consoleErrors } = await openApp({ update: OFFER });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    assert.equal(await helpCap(page).evaluate((el) => el.classList.contains('tool--badge')), true, 'the Help cap has no dot');
    assert.equal(await helpCap(page).getAttribute('title'), 'Help: an update is ready', 'the cap title');
    await helpCap(page).click();
    const section = updateSection(page);
    await section.waitFor({ timeout: 5000 });
    const headings = await page.locator('#lf-help-popover .help__h').allTextContents();
    assert.match(headings[0], /^Update ready\s*v9\.9\.9$/, `the first heading: ${headings[0]}`);
    assert.deepEqual(await section.locator('.help__list li').allTextContents(),
      ['Faster takes. A take lands sooner.', 'A second change'], 'the notes');
    if (shots) await page.screenshot({ path: `${shots}/app-update-help.png` });
    await section.getByRole('button', { name: 'Update and restart' }).click();
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
    await helpCap(page).click();
    const button = updateSection(page).getByRole('button', { name: 'Update and restart' });
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

  // 4. A failed install.
  {
    const { page, consoleErrors } = await openApp({ update: OFFER, fail: 'probe: the installer did not start' });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    await helpCap(page).click();
    const button = updateSection(page).getByRole('button', { name: 'Update and restart' });
    await button.click();
    await page.locator('.toast', { hasText: 'The update did not install' }).waitFor({ timeout: 5000 });
    assert.equal(await installs(page), 1, 'the install was not tried');
    assert.equal(await button.isEnabled(), true, 'the button stays disabled after a failure');
    assert.equal(consoleErrors.length, 1, `console errors: ${JSON.stringify(consoleErrors)}`);
    assert.match(consoleErrors[0], /^\[app\] update failed/, 'the failure line');
    await page.close();
  }
});

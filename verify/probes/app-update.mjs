/**
 * The updater's UI half (`src/app/update.ts`): what a player sees when a newer release is out, and what
 * the command bar's UPDATE pill and its panel do, on a scripted updater (`window.__lfUpdateFake`,
 * `host.web.ts`) and the web engine fake. Nine pages, each a fresh app:
 *
 * - no updater (the browser build): no pill in the command bar, no toast, and nothing about an update
 *   in Help, which is where the offer used to live;
 * - an offer with the looper empty: one toast (a note: no ✓, no "Done") names the version, the pill appears
 *   in a mode pill's height, corner and type size, its panel carries the
 *   version and the notes (bullets and bold dropped), and the press installs at once, asking nothing;
 * - an offer during a jam (a committed loop on the feed): the press asks first, in the app's own dialog
 *   (`src/ui/confirm/`; no native confirm opens); "no" installs nothing, "yes" saves the jam for
 *   recovery and installs;
 * - the install's read-out: the press turns the title to "Updating" and the button's word to UPDATING…
 *   without moving its box; each stage the native side reports (`lf://update-progress`) reaches the
 *   panel — a download with a size fills the bar, one without leaves it indeterminate, and the signature
 *   check and the installer each say so;
 * - a failed install: the error toast, its `console.error` line, the read-out cleared, and the button
 *   back for another try;
 * - the order: the download, then the jam's save from the engine's own snapshot, then the installer,
 *   which starts only once the save is in IndexedDB;
 * - a loop the engine committed during a held download, its lane still RECORDING on the feed: it is in
 *   the recovery the installer leaves behind (a save that read the feed's lanes would have skipped it);
 * - a failed download: no save, no install, the error toast and its line;
 * - a save that fails: no install and no native "close anyway?", the update's own toast, the jam playing
 *   on; a close asked while the update's save runs does not run a second quit sequence.
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

/** Page helpers: the scripted updater, a recorder of native confirms (the asks are the app's own, so it
 * must stay empty), and the feed frames the engine sends. */
function pageScript(script) {
  if (script) {
    window.__lfUpdateFake = { ...script, calls: [] };
    if (script.holdDownload) window.__lfUpdateFake.downloadHold = new Promise((resolve) => (window.__releaseDownload = resolve));
    // What the app had done when the installer was asked for: the engine snapshots taken and whether
    // a recovery was in IndexedDB.
    window.__lfUpdateFake.onInstall = async () => {
      window.__atInstall = { snapshots: window.__lf.native.snapshots.length, saved: await window.__lf.autosave.hasSaved() };
    };
  }
  window.__lfEngineFake = true;
  window.__confirms = [];
  window.confirm = (message) => {
    window.__confirms.push(message);
    return true;
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
    /** A loop committed on lane 1; the snapshot answers it. `feed: false`: the feed's frame showing it
     * has not come in yet, so lane 1 still reads RECORDING; `master`: a master of that many frames (not
     * whole bars: the save then fails). */
    async commit({ feed = true, master = frames } = {}) {
      const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
      const pcm = new Float32Array(master);
      pcm[17] = 0.5;
      window.__lf.native.snapshotBytes = encodeSessionBytes({ rate: RATE, masterLengthFrames: master, bpm: 120,
        tracks: [{ index: 0, frames: master, reversed: false, state: 'Playing' }] }, [pcm]).buffer;
      if (feed) {
        emit({ events: [{ Transport: { frame: 0, master, bpm: 120, locked: true } },
          { Lane: { frame: 0, lane: 0, info: lane('Playing', master) } }] });
      } else {
        emit({ events: [{ Lane: { frame: 0, lane: 0, info: lane('Recording', 0) } }] });
      }
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
  const installs = (page) => page.evaluate(() => window.__lfUpdateFake.calls.filter((c) => c === 'install').length);
  const calls = (page) => page.evaluate(() => window.__lfUpdateFake.calls);
  /** Play a native stage into the page (`lf://update-progress`'s payload shape). */
  const report = (page, stage) => page.evaluate((s) => window.__lfUpdateFake.report(s), stage);

  // 1. No updater.
  {
    const { page } = await openApp(null);
    await page.waitForTimeout(500); // the launch check is async: give an offer the time to show
    assert.equal(await pill(page).count(), 0, 'an update pill without an updater');
    assert.equal(await page.locator('.toast', { hasText: 'is ready' }).count(), 0, 'an update toast without an updater');
    await page.locator('.tool--help').click();
    assert.equal(await page.getByRole('heading', { name: /Update ready/ }).count(), 0, 'Update ready in Help');
    assert.equal(await page.locator('#lf-help-popover').getByText(/update/i).count(), 0, 'Help still mentions updating');
    await page.close();
  }

  // 2. An offer, the looper empty.
  {
    const { page } = await openApp({ update: OFFER });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    // An offer is a note, not a finished action: the neutral rule and an "i", and a screen reader hears
    // "Note", never the confirmation's ✓ and "Done".
    const kind = await page.locator('.toast', { hasText: TOAST }).evaluate((el) => {
      const accent = el.querySelector('.toast__accent');
      const dim = getComputedStyle(document.documentElement).getPropertyValue('--dim').trim();
      const probe = Object.assign(document.createElement('span'), { style: `color: ${dim}` });
      document.body.append(probe);
      const dimColor = getComputedStyle(probe).color;
      probe.remove();
      return { note: el.classList.contains('toast--note'), info: el.classList.contains('toast--info'),
        spoken: el.querySelector('.lf-visually-hidden')?.textContent, glyph: getComputedStyle(accent, '::after').content,
        dimRule: getComputedStyle(accent).color === dimColor };
    });
    assert.deepEqual(kind, { note: true, info: false, spoken: 'Note', glyph: '"i"', dimRule: true }, 'the offer toasts as a note');
    await pill(page).waitFor({ timeout: 5000 });
    assert.equal(await pill(page).innerText(), 'UPDATE', 'the pill reads'); // innerText: the CSS uppercase
    assert.equal(await pill(page).getAttribute('title'), 'BleepLoop v9.9.9 is ready to install', 'the pill title');
    assert.equal(await pill(page).getAttribute('aria-label'), 'Update ready, version 9.9.9', 'the pill name');
    // The pill is a mode pill in its geometry (CLICK's here): its height, corner and type, so it stands in
    // the bar as one of them; only its warm-white legend is its own.
    const shape = (el) => {
      const s = getComputedStyle(el);
      return { height: el.getBoundingClientRect().height, radius: s.borderTopLeftRadius, font: s.fontSize };
    };
    const geometry = { update: await pill(page).evaluate(shape),
      mode: await page.getByRole('button', { name: 'Metronome click', exact: true }).evaluate(shape) };
    console.log(JSON.stringify({ geometry }));
    assert.deepEqual(geometry.update, geometry.mode, 'the UPDATE pill is shaped like a mode pill');
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
    await page.waitForFunction(() => window.__lfUpdateFake.calls.includes('install'), undefined, { timeout: 5000 });
    assert.deepEqual(await page.evaluate(() => window.__confirms), [], 'a native confirm opened');
    assert.equal(await page.getByRole('alertdialog').count(), 0, 'an empty looper was asked');
    await page.close();
  }

  // 3. An offer during a jam.
  {
    const { page } = await openApp({ update: OFFER });
    await page.evaluate(async () => {
      await window.__lf.autosave.ready();
      await window.__engine.commit();
    });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    await pill(page).click();
    const button = panel(page).getByRole('button', { name: 'Update and restart' });
    // The ask is the app's own dialog: named, its text kept, focus on its no button.
    const ask = page.getByRole('alertdialog', { name: 'Update during a jam', exact: true });
    await button.click();
    await ask.waitFor({ timeout: 5000 });
    assert.match(await ask.locator('#lf-confirm-body').innerText(), /^Update to v9\.9\.9 now\?/, 'the jam ask');
    assert.equal(await page.evaluate(() => document.activeElement?.textContent.trim()), 'Cancel', 'focus on the no button');
    await ask.getByRole('button', { name: 'Cancel', exact: true }).click();
    await ask.waitFor({ state: 'detached' });
    assert.equal(await installs(page), 0, 'installed after a "no"');
    await button.click();
    await ask.getByRole('button', { name: 'Update', exact: true }).click();
    await page.waitForFunction(() => window.__lfUpdateFake.calls.includes('install'), undefined, { timeout: 10_000 });
    assert.deepEqual(await page.evaluate(() => window.__confirms), [], 'a native confirm opened');
    assert.equal(await page.evaluate(() => window.__lf.autosave.hasSaved()), true, 'the jam was not saved for recovery');
    await page.close();
  }

  // 4. The install's read-out. `hold` keeps install() unsettled, as the native one is once the
  //    installer runs, so each reported stage can be read off the panel.
  {
    const { page } = await openApp({ update: OFFER, hold: true });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    await pill(page).click();
    const button = panel(page).getByRole('button', { name: /Update and restart|Updating/ });
    const width = () => button.evaluate((el) => Math.round(el.getBoundingClientRect().width * 10) / 10);
    const before = await width();
    await button.click();
    await page.waitForFunction(() => window.__lfUpdateFake.calls.includes('install'), undefined, { timeout: 5000 });
    assert.equal(await button.isEnabled(), false, 'the button stays pressable during an install');
    // The press changes the words, never the button's box, and the title says what is happening.
    assert.equal(await button.innerText(), 'UPDATING…', 'the button while it installs');
    assert.equal(await width(), before, `the button resizes as it installs (${before} px before)`);
    assert.match(await panel(page).locator('.update__title').innerText(), /^Updating\s*v9\.9\.9$/, 'the title while it installs');

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
      () => document.querySelector('.update__stage')?.textContent === 'Installing: BleepLoop closes now',
      undefined, { timeout: 5000 });
    await page.close();
  }

  // 5. A failed install.
  {
    const { page, consumeConsoleErrors } = await openApp({ update: OFFER, failInstall: 'probe: the installer did not start' });
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    await pill(page).click();
    const button = panel(page).getByRole('button', { name: 'Update and restart' });
    await button.click();
    await page.locator('.toast', { hasText: 'Could not install the update.' }).waitFor({ timeout: 5000 });
    assert.equal(await installs(page), 1, 'the install was not tried');
    assert.equal(await button.isEnabled(), true, 'the button stays disabled after a failure');
    // A stage left on screen under a pressable button would read as an install still running.
    assert.equal(await panel(page).locator('.update__progress').count(), 0, 'the read-out survived the failure');
    assert.equal(consumeConsoleErrors(/^\[app\] update failed/).length, 1, 'the failure line, once');
    await page.close();
  }

  /** Press UPDATE AND RESTART on `page` (an empty looper asks nothing; a jam's ask is answered yes). */
  const press = async (page) => {
    await page.locator('.toast', { hasText: TOAST }).waitFor({ timeout: 5000 });
    await pill(page).click();
    await panel(page).getByRole('button', { name: 'Update and restart' }).click();
    const ask = page.getByRole('alertdialog', { name: 'Update during a jam', exact: true });
    if (await ask.isVisible()) await ask.getByRole('button', { name: 'Update', exact: true }).click();
  };

  // 6. The order, and 7. a loop the engine committed during the download that the feed does not show.
  {
    const { page } = await openApp({ update: OFFER, holdDownload: true });
    await page.evaluate(() => window.__lf.autosave.ready());
    await press(page);
    await page.waitForFunction(() => window.__lfUpdateFake.calls.length === 1, undefined, { timeout: 5000 });
    const during = await page.evaluate(() => window.__lf.native.snapshots.length);
    // The take commits in the engine while the package downloads; its feed frame has not come in.
    await page.evaluate(() => window.__engine.commit({ feed: false }));
    await page.evaluate(() => window.__releaseDownload());
    await page.waitForFunction(() => window.__atInstall !== undefined, undefined, { timeout: 10_000 });
    assert.deepEqual(await calls(page), ['download', 'install'], 'download, then install');
    const atInstall = await page.evaluate(() => window.__atInstall);
    assert.ok(atInstall.snapshots > during, `the save read the engine's snapshot after the download (${during} -> ${atInstall.snapshots})`);
    assert.equal(atInstall.saved, true, 'the loop the feed did not show yet is in IndexedDB before the installer starts');
    await page.close();
  }

  // 8. A failed download.
  {
    const { page, consumeConsoleErrors } = await openApp({ update: OFFER, failDownload: 'probe: GitHub is unreachable' });
    await page.evaluate(() => window.__lf.autosave.ready());
    await press(page);
    await page.locator('.toast', { hasText: 'Could not install the update.' }).waitFor({ timeout: 5000 });
    assert.deepEqual(await calls(page), ['download'], 'no install after a failed download');
    assert.equal(await page.evaluate(() => window.__lf.native.snapshots.length), 0, 'and no save: nothing quits');
    assert.equal(consumeConsoleErrors(/^\[app\] update failed/).length, 1);
    await page.close();
  }

  // 9. A save that fails holds the update back; a close meanwhile runs no second quit.
  {
    const { page, consumeConsoleErrors } = await openApp({ update: OFFER });
    await page.evaluate(async () => {
      await window.__lf.autosave.ready();
      // A loop whose master is not whole bars: the recovery save refuses it.
      await window.__engine.commit({ master: 2 * 48000 + 1 });
      let release;
      window.__lf.native.snapshotHold = new Promise((resolve) => (release = resolve));
      window.__releaseSnapshot = release;
    });
    await press(page);
    await page.waitForFunction(() => window.__lf.native.snapshots.length === 1, undefined, { timeout: 5000 });
    const closeMeanwhile = await page.evaluate(async () => {
      const { quitWithSave } = await import('/src/app/close-guard.ts');
      let closed = false;
      const ran = await quitWithSave('close', async () => void (closed = true));
      return { ran, closed };
    });
    assert.deepEqual(closeMeanwhile, { ran: false, closed: false }, "a close during the update's save runs no quit of its own");
    await page.evaluate(() => {
      window.__lf.native.snapshotHold = null;
      window.__releaseSnapshot();
    });
    await page.locator('.toast', { hasText: 'Could not save the loops, so the update waits.' }).waitFor({ timeout: 5000 });
    assert.deepEqual(await calls(page), ['download'], 'no install after a failed save');
    assert.deepEqual(await page.evaluate(() => window.__confirms), [], 'no native "close anyway?" for an update');
    assert.equal(consumeConsoleErrors(/^\[app\] autosave flush before the update failed/).length, 1);
    assert.equal(await panel(page).getByRole('button', { name: 'Update and restart' }).isEnabled(), true, 'the button is back');
    await page.close();
  }
});

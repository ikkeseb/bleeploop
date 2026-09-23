/**
 * Fresh-profile journey through actual controls and downloaded files: record from a real synth key
 * press, export and download the zip, close and reopen to restore from automatic recovery, retry a
 * malformed import, import the downloaded zip into a second, entirely empty profile, then clear and
 * reload. PCM hashes must match across both round trips. Polls completed recovery reads rather than
 * treating an asynchronous predicate as a saved result. Browser tier only: every state change goes
 * through real buttons, keys or file input; `__lf` is only used to observe audio and recovery state.
 * One filtered console error (`[transport] import failed`) is expected from the malformed-import case.
 * Run: pnpm probe first-session
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';
import { mkdir, readFile } from 'node:fs/promises';

await mkdir('logs/first-session', { recursive: true });

await probe(async ({ open, browser }) => {
  const errors = [];
  const attach = (page) => {
    page.on('console', message => {
      if (message.type() === 'error' && !message.text().startsWith('[transport] import failed')) errors.push(message.text());
    });
    page.on('pageerror', error => errors.push(String(error)));
    page.on('dialog', dialog => dialog.accept());
  };
  const openSession = async (context) => {
    const { page } = await open({ context, init: attach });
    // A new browser document needs a real gesture to release pending audio-context resume.
    await page.locator('.kb__key').first().click();
    await page.evaluate(() => Promise.race([
      window.__lf.autosave.ready(),
      new Promise((_, reject) => setTimeout(() => reject(new Error('Recovery startup timed out')), 10000)),
    ]));
    return page;
  };
  const snapshot = page => page.evaluate(async () => {
    const snap = window.__lf.looper.exportSnapshot();
    const track = snap.tracks[0];
    const hash = track ? [...new Uint8Array(await crypto.subtle.digest('SHA-256', track.pcm))]
      .map(v => v.toString(16).padStart(2, '0')).join('') : null;
    let peak = 0;
    if (track) for (const v of track.pcm) peak = Math.max(peak, Math.abs(v));
    return { hash, peak, tracks: snap.tracks.length, frames: snap.masterLengthFrames };
  });
  const clear = async page => {
    await page.getByRole('button', { name: 'Clear all tracks', exact: true }).click();
    await page.getByRole('button', { name: 'Clear all tracks, press again to confirm', exact: true }).click();
  };
  const waitSaved = async (page, expected) => {
    const deadline = Date.now() + 8000;
    while (await page.evaluate(() => window.__lf.autosave.hasSaved()) !== expected) {
      assert.ok(Date.now() < deadline, `Recovery did not become ${expected ? 'saved' : 'empty'}`);
      await page.waitForTimeout(100);
    }
  };

  const context = await browser.newContext({ viewport: { width: 1280, height: 820 }, acceptDownloads: true });
  let page = await openSession(context);
  assert.equal((await snapshot(page)).tracks, 0);
  assert.equal(await page.getByRole('button', { name: 'Export loops as a zip of WAV files' }).isDisabled(), true);
  assert.equal(await page.getByRole('button', { name: 'Import a session zip' }).isEnabled(), true);
  await page.getByRole('button', { name: 'Keyboard & layout help' }).click();
  assert.match(await page.locator('#lf-help-popover').innerText(), /Session/);
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: 'Fixed take length off', exact: true }).click();
  for (let i = 0; i < 3; i++) await page.getByRole('button', { name: 'Fewer bars', exact: true }).click();
  await page.getByRole('button', { name: 'Track 1 record', exact: true }).click();
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'RECORDING' && !window.__lf.looper.track(0)().armed);
  await page.keyboard.down('a');
  await page.waitForTimeout(800);
  await page.keyboard.up('a');
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'PLAYING');
  const recorded = await snapshot(page);
  assert.equal(recorded.tracks, 1);
  assert.ok(recorded.peak > 0.01, 'Real synth input must reach the recorded take');
  assert.equal(await page.getByRole('button', { name: 'Import a session zip' }).isDisabled(), true);
  await page.screenshot({ path: 'logs/first-session/recorded.png' });
  const downloadPromise = page.waitForEvent('download');
  await page.getByRole('button', { name: 'Export loops as a zip of WAV files' }).click();
  const download = await downloadPromise;
  assert.match(download.suggestedFilename(), /\.zip$/);
  const archivePath = 'logs/first-session/session.zip';
  await download.saveAs(archivePath);
  assert.equal(await download.failure(), null);
  const archive = await readFile(archivePath);
  assert.ok(archive.length > 1000);
  // Wait for normal autosave, without forcing a flush that could hide a missing UI save trigger.
  await waitSaved(page, true);
  await page.close();
  page = await openSession(context);
  const restored = await snapshot(page);
  assert.deepEqual(restored, recorded);
  await context.close();

  // Import the file into another entirely empty profile, not the recovery that just restored.
  const importing = await browser.newContext({ viewport: { width: 1280, height: 820 } });
  page = await openSession(importing);
  await page.locator('input[type=file]').setInputFiles({ name: 'broken.zip', mimeType: 'application/zip', buffer: Buffer.from('not a zip') });
  await page.getByText(/Import failed:/).waitFor();
  assert.equal((await snapshot(page)).tracks, 0);
  const pickerPromise = page.waitForEvent('filechooser');
  await page.getByRole('button', { name: 'Import a session zip' }).click();
  await (await pickerPromise).setFiles(archivePath);
  await page.waitForFunction(() => window.__lf.looper.masterLengthFrames() > 0);
  assert.deepEqual(await snapshot(page), recorded);
  await waitSaved(page, true);
  await clear(page);
  await waitSaved(page, false);
  await page.reload();
  await page.waitForFunction(() => !!window.__lf);
  await page.evaluate(() => window.__lf.autosave.ready());
  assert.equal((await snapshot(page)).tracks, 0);
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ pass: true, recorded, archiveBytes: archive.length,
    recoveryExact: true, importIntoFreshProfileExact: true, malformedImportRetried: true, clearSurvivedReload: true }));
  await importing.close();
}, { launch: {} });

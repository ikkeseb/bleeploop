/**
 * A fresh profile's first session through the real controls and downloaded files, in engine mode on the
 * web engine fake (`src/platform/host.web.ts`, the engine-seam pattern: an init script sets
 * `window.__lfEngineFake`, the probe scripts the feed through `__lf.native` and the snapshot the engine
 * would answer through `__lf.native.snapshotBytes`, and reads the sessions loaded into it):
 *
 * - an empty looper: Export is disabled, Import is enabled, Help names the Session tools;
 * - the first take through real controls: FIXED (aria-pressed off, then on) and its Fewer bars stepper
 *   send `SetFixedLength` / `SetFixedBars`; the lane's record core sends `SelectTrack` + `RecDub`; a PC
 *   key sends `NoteOn` / `NoteOff`; the take the feed then reports makes Import disabled;
 * - Export's download: a .zip whose stem is the snapshot's PCM, exactly;
 * - recovery: autosave saves the jam without an explicit flush; a closed and reopened page (a fresh
 *   engine) loads that session back into the engine, header and PCM;
 * - a malformed zip: "Import failed:" and nothing loads; the retry through the Import button's file
 *   picker loads the downloaded zip into an entirely empty second profile, header and PCM as recorded;
 * - CLEAR ALL's two presses (the second names the confirm) send `ClearAll`; the engine's clear empties
 *   the recovery, and a reload restores nothing.
 *
 * One filtered console error (`[transport] import failed`) is expected from the malformed import.
 * Cannot see the native engine: the take, its PCM and every lane state are scripted (a synth reaching
 * the take is lf-engine `tests/sound.rs`), nor the Rust side of the bytes or Tauri IPC.
 * Run: pnpm probe first-session
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdir, readFile } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';
import { parseZip } from '../../src/session/unzip.ts';
import { decodeWav } from '../../src/session/wav.ts';

await mkdir('logs/first-session', { recursive: true });

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const lane = (state, extra = {}) => ({
  state,
  length: state === 'Empty' || state === 'Recording' ? 0 : BAR,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: state === 'Playing' || state === 'Stopped',
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
  ...extra,
});
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });
const hash = (values) => createHash('sha256').update(Buffer.from(Float32Array.from(values).buffer)).digest('hex');

await probe(async ({ open, browser }) => {
  const errors = [];
  const attach = (page) => {
    page.on('console', (message) => {
      if (message.type() === 'error' && !message.text().startsWith('[transport] import failed')) errors.push(message.text());
    });
    page.on('pageerror', (error) => errors.push(String(error)));
    page.on('dialog', (dialog) => dialog.accept());
  };
  let seq = 0;
  const emit = (page, frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const emptyEngine = (page) =>
    emit(page, {
      reset: true,
      settings: [],
      events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, ...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      meter: { peak: 0, clip: false },
    });
  /** Open the app in `context` on the engine fake, once its device runs and recovery has started. */
  const openSession = async (context) => {
    const { page } = await open({ context, init: async (p) => { attach(p); await p.addInitScript(() => void (window.__lfEngineFake = true)); } });
    await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    await page.evaluate(() => Promise.race([
      window.__lf.autosave.ready(),
      new Promise((_, reject) => setTimeout(() => reject(new Error('Recovery startup timed out')), 10000)),
    ]));
    return page;
  };
  const sent = (page) => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = (page) => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  /** The session the fake engine was loaded with last: its header and each lane's PCM hash; null when none. */
  const loaded = (page) =>
    page.evaluate(async () => {
      const sessions = window.__lf.native.loadedSessions;
      if (sessions.length === 0) return null;
      const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
      const { header, pcm } = splitSessionBytes(sessions.at(-1).slice().buffer);
      return { header, pcm: pcm.map((p) => Array.from(p)) };
    }).then((s) => s && { header: s.header, hashes: s.pcm.map(hash) });
  const clear = async (page) => {
    await page.getByRole('button', { name: 'Clear all tracks', exact: true }).click();
    await page.getByRole('button', { name: 'Clear all tracks, press again to confirm', exact: true }).click();
  };
  const waitSaved = async (page, expected) => {
    const deadline = Date.now() + 15000;
    while (await page.evaluate(() => window.__lf.autosave.hasSaved()) !== expected) {
      assert.ok(Date.now() < deadline, `Recovery did not become ${expected ? 'saved' : 'empty'} (errors: ${JSON.stringify(errors)})`);
      await page.waitForTimeout(100);
    }
  };

  const context = await browser.newContext({ viewport: { width: 1280, height: 820 }, acceptDownloads: true });
  let page = await openSession(context);
  await emptyEngine(page);
  assert.equal(await page.getByRole('button', { name: 'Export loops as a zip of WAV files' }).isDisabled(), true, 'nothing to export');
  assert.equal(await page.getByRole('button', { name: 'Import a session zip' }).isEnabled(), true, 'an empty looper imports');
  await page.getByRole('button', { name: 'Keyboard & layout help' }).click();
  assert.match(await page.locator('#lf-help-popover').innerText(), /Session/);
  await page.keyboard.press('Escape');

  // ── The first take, through real controls ────────────────────────────────────────────────────────
  const fixed = page.getByRole('button', { name: 'Fixed take length', exact: true });
  assert.equal(await fixed.getAttribute('aria-pressed'), 'false');
  await clearSent(page);
  await fixed.click();
  assert.equal(await fixed.getAttribute('aria-pressed'), 'true');
  for (let i = 0; i < 3; i++) await page.getByRole('button', { name: 'Fewer bars', exact: true }).click();
  const fixedSent = await sent(page);
  console.log('FIXED sent', JSON.stringify(fixedSent));
  assert.deepEqual(fixedSent[0], { SetFixedLength: true }, 'FIXED sends SetFixedLength');
  assert.deepEqual(fixedSent.at(-1), { SetFixedBars: 1 }, 'Fewer bars steps down to one bar');
  assert.equal((await fixed.textContent()).trim(), 'FIXED 1');
  await clearSent(page);
  await page.getByRole('button', { name: 'Track 1 record', exact: true }).click();
  await page.waitForFunction(() => window.__lf.native.sent.length >= 2, undefined, { timeout: 5000 });
  assert.deepEqual(await sent(page), [{ SelectTrack: 0 }, { RecDub: 0 }], 'the record core selects, then REC/DUB');
  // The engine's count-in, then the take: the PC key plays into it.
  await emit(page, { events: [laneEvent(0, lane('Recording', { armed: true })), { Transport: { frame: 0, master: 0, bpm: 120, locked: true } }] });
  await emit(page, { events: [laneEvent(0, lane('Recording'))] });
  await clearSent(page);
  await page.evaluate(() => document.activeElement instanceof HTMLElement && document.activeElement.blur());
  await page.keyboard.down('a');
  await page.waitForTimeout(200);
  await page.keyboard.up('a');
  await page.waitForFunction(() => window.__lf.native.sent.some((c) => c.NoteOff !== undefined), undefined, { timeout: 5000 });
  const played = await sent(page);
  const on = played.find((c) => c.NoteOn);
  assert.ok(on, 'a PC key sends NoteOn');
  assert.deepEqual(played.find((c) => c.NoteOff !== undefined), { NoteOff: on.NoteOn[0] }, 'its release sends NoteOff');
  // The FIXED bar commits: the lane plays, and the engine's snapshot holds the take.
  const take = await page.evaluate(async (bar) => {
    const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
    const pcm = Float32Array.from({ length: bar }, (_, i) => (i < 0.2 * bar ? 0.4 * Math.sin((2 * Math.PI * 523.25 * i) / 48000) * Math.exp(-i / 9600) : 0));
    window.__lf.native.snapshotBytes = encodeSessionBytes(
      { rate: 48000, masterLengthFrames: bar, bpm: 120, tracks: [{ index: 0, frames: bar, reversed: false, state: 'Playing' }] },
      [pcm],
    ).buffer;
    return Array.from(pcm);
  }, BAR);
  const recorded = {
    header: { bpm: 120, bars: 1, masterLengthFrames: BAR, tracks: [{ index: 0, frames: BAR, reversed: false, state: 'Playing' }] },
    hashes: [hash(take)],
  };
  await emit(page, {
    events: [laneEvent(0, lane('Playing')), { Transport: { frame: 0, master: BAR, bpm: 120, locked: true } }],
    peaks: [{ lane: 0, start: 0, count: 4, min: [-0.4, -0.1, 0, 0], max: [0.4, 0.1, 0, 0] }],
  });
  assert.equal(await page.locator('.lp-lane').first().getAttribute('data-state'), 'play');
  assert.equal(await page.getByRole('button', { name: 'Import a session zip' }).isDisabled(), true, 'a loop present: no import');
  await page.screenshot({ path: 'logs/first-session/recorded.png' });

  // ── Export: the button's download ─────────────────────────────────────────────────────────────────
  const downloadPromise = page.waitForEvent('download');
  await page.getByRole('button', { name: 'Export loops as a zip of WAV files' }).click();
  const download = await downloadPromise;
  assert.match(download.suggestedFilename(), /\.zip$/);
  const archivePath = 'logs/first-session/session.zip';
  await download.saveAs(archivePath);
  assert.equal(await download.failure(), null);
  const archive = await readFile(archivePath);
  assert.ok(archive.length > 1000);
  const entries = parseZip(new Uint8Array(archive.buffer, archive.byteOffset, archive.length));
  const session = JSON.parse(new TextDecoder().decode(entries.find((e) => e.name.endsWith('-session.json')).data));
  assert.equal(session.tracks.length, 1, 'one track exported');
  const stem = decodeWav(entries.find((e) => e.name === session.tracks[0].file).data).channels[0];
  assert.equal(hash(stem), recorded.hashes[0], "the exported stem is the engine's take, exactly");
  // Wait for normal autosave, without forcing a flush that could hide a missing UI save trigger.
  await waitSaved(page, true);
  await page.close();

  // ── Recovery: a reopened page loads the jam back into a fresh engine ────────────────────────────
  page = await openSession(context);
  await page.waitForFunction(() => window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 15000 });
  const restored = await loaded(page);
  console.log('restored', JSON.stringify(restored.header));
  assert.deepEqual(restored, recorded, 'recovery loads the recorded take back, sample for sample');
  await context.close();

  // ── Import the file into another entirely empty profile, not the recovery that just restored ─────
  const importing = await browser.newContext({ viewport: { width: 1280, height: 820 } });
  page = await openSession(importing);
  await emptyEngine(page);
  await page.locator('input[type=file]').setInputFiles({ name: 'broken.zip', mimeType: 'application/zip', buffer: Buffer.from('not a zip') });
  await page.getByText(/Import failed:/).waitFor();
  assert.equal(await loaded(page), null, 'a malformed zip loads nothing');
  const pickerPromise = page.waitForEvent('filechooser');
  await page.getByRole('button', { name: 'Import a session zip' }).click();
  await (await pickerPromise).setFiles(archivePath);
  await page.waitForFunction(() => window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 10000 });
  assert.deepEqual(await loaded(page), recorded, 'the import loads the recorded take into the empty engine');
  // The engine plays what it loaded, and its snapshot answers it.
  await page.evaluate(async () => {
    const { encodeSessionBytes, splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    const { header, pcm } = splitSessionBytes(window.__lf.native.loadedSessions[0].slice().buffer);
    window.__lf.native.snapshotBytes = encodeSessionBytes({ rate: 48000, masterLengthFrames: header.masterLengthFrames, bpm: header.bpm, tracks: header.tracks }, pcm).buffer;
  });
  await emit(page, { events: [laneEvent(0, lane('Playing')), { Transport: { frame: 0, master: BAR, bpm: 120, locked: true } }] });
  await waitSaved(page, true);
  await clearSent(page);
  await clear(page);
  assert.deepEqual(await sent(page), ['ClearAll'], 'the confirming press sends CLEAR ALL');
  await page.evaluate(() => (window.__lf.native.snapshotBytes = null));
  await emit(page, {
    events: [
      ...[0, 1, 2, 3, 4].flatMap((i) => [{ Cleared: { frame: 0, lane: i } }, laneEvent(i, lane('Empty'))]),
      { Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
    ],
  });
  await waitSaved(page, false);
  await page.reload();
  await page.waitForFunction(() => !!window.__lf && window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
  await page.evaluate(() => window.__lf.autosave.ready());
  await page.waitForTimeout(500);
  assert.equal(await loaded(page), null, 'a cleared session is not restored after a reload');
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ pass: true, recorded: recorded.hashes, archiveBytes: archive.length,
    recoveryExact: true, importIntoFreshProfileExact: true, malformedImportRetried: true, clearSurvivedReload: true }));
  await importing.close();
});

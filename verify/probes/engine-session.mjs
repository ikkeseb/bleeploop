/**
 * Engine mode's session paths on the web engine fake (`src/platform/host.web.ts`): export, local
 * recovery and import go through `engineSession` (`src/ui/state/engine-store.ts`), which reads the
 * engine's snapshot bytes and loads sessions as bytes; Share output's saved pick is sent at boot and
 * forgotten on `ShareLost`. The probe scripts the lanes on the feed and the snapshot the engine would
 * answer, then drives the real UI:
 *
 * - export: the Export button's download holds both stems exactly as the snapshot's PCM (play order), the
 *   store's mix, the lane states, and a wet master rendered offline (no AudioContext is built);
 * - recovery: autosave saves the jam, and a reloaded page (a fresh engine) loads it back into the engine;
 * - import: the exported zip goes to the engine as one session (header, PCM, orientation), and the store
 *   sends the loaded lanes' mix;
 * - Share output: the saved pick reaches the engine at boot, and ShareLost clears it with a toast.
 *
 * Cannot see the native engine, the Rust side of the bytes or Tauri's raw IPC: the fake records them.
 * Run: pnpm probe engine-session
 */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { probe } from '../harness/probe.ts';
import { parseZip } from '../../src/audio/export/unzip.ts';
import { decodeWav } from '../../src/audio/export/wav.ts';

const RATE = 48000;
const MASTER = 2 * RATE; // one bar at 120 BPM

const lane = (state, extra = {}) => ({
  state,
  length: state === 'Empty' ? 0 : MASTER,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: state === 'Playing' || state === 'Stopped',
  reversed: false,
  stopAt: null,
  retakePass: 0,
  ...extra,
});
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });

await probe(async ({ browser, open }) => {
  const context = await browser.newContext({ viewport: { width: 1600, height: 900 }, acceptDownloads: true });
  const init = (p) =>
    p.addInitScript(() => {
      window.__lfEngineFake = true;
      window.__audioContexts = 0;
      const Native = window.AudioContext;
      window.AudioContext = class extends Native {
        constructor(...args) {
          super(...args);
          window.__audioContexts++;
        }
      };
      if (!sessionStorage.getItem('probe.seeded')) {
        sessionStorage.setItem('probe.seeded', '1');
        localStorage.setItem('lf.audioDevices', JSON.stringify({ shareDeviceId: 'probe-endpoint' }));
      }
    });
  const { page, consoleErrors } = await open({ context, init });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });

  // ── Share output: the saved pick reaches the engine once the device runs ─────────────────────────
  await page.waitForFunction(() => window.__lf.native.shares.length === 1, undefined, { timeout: 5000 });
  assert.deepEqual(await page.evaluate(() => window.__lf.native.shares), ['probe-endpoint']);

  // ── Two committed lanes on the feed, and the snapshot the engine would answer ────────────────────
  await emit({
    reset: true,
    events: [
      { Transport: { frame: 0, master: MASTER, bpm: 120, locked: true } },
      laneEvent(0, lane('Playing')),
      laneEvent(1, lane('Stopped', { reversed: true })),
      ...[2, 3, 4].map((i) => laneEvent(i, lane('Empty'))),
      { Selected: { frame: 0, lane: 0 } },
    ],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });
  const pcm = await page.evaluate(async (master) => {
    const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
    const a = Float32Array.from({ length: master }, (_, i) => 0.5 * Math.sin((2 * Math.PI * 440 * i) / 48000));
    const b = Float32Array.from({ length: master }, (_, i) => (i % 4800 < 100 ? 0.8 : 0) * (1 - i / master));
    window.__lf.native.snapshotBytes = encodeSessionBytes(
      {
        rate: 48000,
        masterLengthFrames: master,
        bpm: 120,
        tracks: [
          { index: 0, frames: master, reversed: false, state: 'Playing' },
          { index: 1, frames: master, reversed: true, state: 'Stopped' },
        ],
      },
      [a, b],
    ).buffer;
    return [Array.from(a), Array.from(b)];
  }, MASTER);
  await page.getByRole('slider', { name: 'Track 1 volume' }).fill('80');

  // ── Export: the button's download ─────────────────────────────────────────────────────────────────
  const downloading = page.waitForEvent('download');
  await page.getByRole('button', { name: 'Export loops as a zip of WAV files' }).click();
  const download = await downloading;
  assert.equal(await download.failure(), null);
  const zip = readFileSync(await download.path());
  const entries = parseZip(new Uint8Array(zip.buffer, zip.byteOffset, zip.length));
  const session = JSON.parse(new TextDecoder().decode(entries.find((e) => e.name.endsWith('-session.json')).data));
  console.log('export', download.suggestedFilename(), entries.map((e) => e.name).join(', '));
  console.log('session', JSON.stringify(session, (key, value) => (key === 'fx' ? undefined : value)));
  assert.equal(session.sampleRate, RATE);
  assert.equal(session.masterLengthFrames, MASTER);
  assert.equal(session.bpm, 120);
  assert.equal(session.bars, 1);
  assert.deepEqual(
    session.tracks.map((t) => [t.track, t.volume, t.muted, t.reversed, t.state]),
    [
      [1, 0.8, false, false, 'PLAYING'],
      [2, 1, false, true, 'STOPPED'],
    ],
  );
  for (const [k, t] of session.tracks.entries()) {
    const stem = decodeWav(entries.find((e) => e.name === t.file).data).channels[0];
    assert.deepEqual(Array.from(stem), pcm[k].map((x) => Math.fround(x)), `stem ${t.track} is the snapshot's PCM`);
  }
  assert.equal(session.master.kind, 'wet-v1', 'the wet master rendered offline');
  const master = decodeWav(entries.find((e) => e.name === session.master.file).data);
  assert.equal(master.channels.length, 2);
  assert.ok(master.channels[0].some((x) => Math.abs(x) > 0.01), 'the wet master carries lane 1');

  // ── Recovery: autosave keeps the jam; a fresh page loads it back into the engine ─────────────────
  for (let t0 = Date.now(); !(await page.evaluate(() => window.__lf.autosave.hasSaved())); ) {
    assert.ok(Date.now() - t0 < 15000, 'autosave saved the jam within 15 s');
    await page.waitForTimeout(250);
  }
  await page.reload();
  await page.waitForFunction(() => '__lf' in window && window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 15000 });
  const recovered = await page.evaluate(async () => {
    const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    const { header, pcm } = splitSessionBytes(window.__lf.native.loadedSessions[0].slice().buffer);
    return { header, pcm: pcm.map((block) => Array.from(block)), sent: window.__lf.native.sent.slice() };
  });
  console.log('recovered', JSON.stringify(recovered.header));
  assert.deepEqual(recovered.header, {
    bpm: 120,
    bars: 1,
    masterLengthFrames: MASTER,
    tracks: [
      { index: 0, frames: MASTER, reversed: false, state: 'Playing' },
      { index: 1, frames: MASTER, reversed: true, state: 'Stopped' },
    ],
  });
  assert.deepEqual(recovered.pcm, pcm.map((block) => block.map((x) => Math.fround(x))), 'the recovered PCM is the snapshot');
  assert.ok(recovered.sent.some((c) => JSON.stringify(c) === JSON.stringify({ SetVolume: [0, 0.8] })), 'the recovered mix is sent');

  // ── Import: the exported zip into the (still empty) engine ────────────────────────────────────────
  await page.locator('input[type="file"]').setInputFiles({ name: download.suggestedFilename(), mimeType: 'application/zip', buffer: zip });
  await page.waitForFunction(() => window.__lf.native.loadedSessions.length === 2, undefined, { timeout: 10000 });
  const imported = await page.evaluate(async () => {
    const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    const { header, pcm } = splitSessionBytes(window.__lf.native.loadedSessions[1].slice().buffer);
    return { header, pcm: pcm.map((block) => Array.from(block)) };
  });
  assert.deepEqual(imported.header, recovered.header, 'import sends the session the export holds');
  assert.deepEqual(imported.pcm, recovered.pcm);

  // ── ShareLost: a toast, and the pick is forgotten ────────────────────────────────────────────────
  assert.deepEqual(await page.evaluate(() => window.__lf.native.shares), ['probe-endpoint'], 'the reloaded page sent the saved pick');
  await emit({ device: [{ ShareLost: { reason: 'the Share output endpoint stopped' } }] });
  await page.getByText('Share output stopped').waitFor({ timeout: 3000 });
  assert.equal(await page.evaluate(() => JSON.parse(localStorage.getItem('lf.audioDevices')).shareDeviceId), '');

  assert.equal(await page.evaluate(() => window.__audioContexts), 0, 'no AudioContext was built (the master renders offline)');
  const expected = ['[engine] share output lost: the Share output endpoint stopped'];
  assert.deepEqual(consoleErrors, expected, 'no other console errors');
  await context.close();
});

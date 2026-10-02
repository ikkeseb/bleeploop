/**
 * The export's archive paths, with the real TypeScript executing (`export.ts`, `import.ts`, `unzip.ts`),
 * the engine side on the web engine fake (`src/platform/host.web.ts`, the `engine-seam` pattern: the
 * probe scripts the feed and the snapshot the engine would answer through `__lf.native`):
 *
 * - An editable export keeps float32 stems exactly (samples past ±1 and a 1e-7), and importing the zip
 *   hands the engine that PCM exactly and restores the lane's volume. The export asks the snapshot for
 *   the master, and its master is the snapshot's (`master.kind` 'wet-engine').
 * - A snapshot whose master render failed (the fake's `masterError`) still completes the export, with
 *   the dry mixdown (`master.kind` 'dry-fallback'), its console.error naming the engine's error. Import
 *   refuses an archive over its size cap and a small archive whose central directory repeats one payload
 *   128 times (entry cap 9).
 * - A lane's volume and the master's moved while the engine renders the master (the fake holds its
 *   snapshot answer) land in neither: session.json keeps the mix from when the export asked.
 * - A 44.1 kHz engine's eight-bar loop reads eight bars and 16 s in the command bar, and still does after
 *   its device stops (a `status: null` frame: the engine stays at its rate).
 *
 * The master here is the fake's stand-in (a dry sum under volume and mute), NOT the engine's sound: what
 * the master holds (FX, a STOPPED lane in it, a muted one out, alignment) is lf-engine's
 * `tests/render_master.rs` and engine_io's snapshot tests, and `pnpm native:export-master` in the app.
 * Cannot see the native snapshot and load (the fake answers them), download delivery in WebView2, or
 * anything audible.
 * Run: pnpm probe export-context
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

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
  fading: false,
  retakePass: 0,
  ...extra,
});
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });
const transport = (master) => ({ Transport: { frame: 0, master, bpm: 120, locked: master > 0 } });

await probe(async ({ open }) => {
  const results = [];

  // ── The engine fake: a device, and every lane EMPTY ───────────────────────────────────────────────
  const { page, consoleErrors } = await open({ init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  await emit({
    reset: true,
    settings: [],
    events: [transport(0), ...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });
  await page.evaluate(() => window.__lf.autosave.ready());

  /** The engine now holds `pcm` on lane 0 in `state`: the feed says so and the snapshot answers it. */
  const engineHolds = async (pcm, state) => {
    await page.evaluate(async ([pcm, state, master]) => {
      const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
      window.__lf.native.snapshotBytes = encodeSessionBytes(
        { rate: 48000, masterLengthFrames: master, bpm: 120, tracks: [{ index: 0, frames: master, reversed: false, state }] },
        [Float32Array.from(pcm)],
      ).buffer;
    }, [pcm, state, MASTER]);
    await emit({ events: [transport(MASTER), laneEvent(0, lane(state))] });
  };

  // ── The editable download and its import ──────────────────────────────────────────────────────────
  const editable = Array.from({ length: MASTER }, () => 0);
  editable.splice(1024, 3, 1.5, -1.5, 1e-7);
  // Loaded as an import loads it (the engine takes the bytes, the store the mix), then the engine plays it.
  await page.evaluate(async ([pcm, master]) => {
    const { session } = await import('/src/ui/state/audio.ts');
    const { defaultFxStates } = await import('/src/ui/state/fx-metadata.ts');
    await session.loadSession({ bpm: 120, bars: 1, masterLengthFrames: master,
      tracks: [{ index: 0, pcm: Float32Array.from(pcm), volume: 0.25, muted: false, reversed: false, fx: defaultFxStates() }] });
  }, [editable, MASTER]);
  await engineHolds(editable, 'Playing');
  const exported = await page.evaluate(async (pcm) => {
    const lf = window.__lf;
    const { session } = await import('/src/ui/state/audio.ts');
    const { parseZip } = await import('/src/session/unzip.ts');
    const { decodeWav } = await import('/src/session/wav.ts');
    const asked = lf.native.snapshots.length;
    const bundle = await lf.buildExportBundle(session);
    if (!bundle) throw new Error('No export bundle');
    window.__bundle = bundle.zipBytes;
    const entries = parseZip(bundle.zipBytes);
    const stem = decodeWav(entries.find((entry) => entry.name.endsWith('-track1.wav')).data).channels[0];
    const meta = JSON.parse(new TextDecoder().decode(entries.find((entry) => entry.name.endsWith('-session.json')).data));
    const master = decodeWav(entries.find((entry) => entry.name === meta.master.file).data).channels;
    const want = Float32Array.from(pcm);
    return { stemErrors: want.reduce((n, x, k) => n + Number(stem[k] !== x), 0), stemSamples: Array.from(stem.slice(1024, 1027)),
      askedMaster: lf.native.snapshots.slice(asked), masterKind: meta.master.kind, masterShape: master.map((c) => c.length) };
  }, editable);
  results.push({ name: 'The export asks the snapshot for the master and ships it as wet-engine',
    askedMaster: exported.askedMaster, masterKind: exported.masterKind, masterShape: exported.masterShape,
    pass: JSON.stringify(exported.askedMaster) === '[true]' && exported.masterKind === 'wet-engine'
      && JSON.stringify(exported.masterShape) === JSON.stringify([MASTER, MASTER]) });
  // The engine cleared (every lane EMPTY) and the lane's volume moved: only the import brings it back.
  await emit({ events: [transport(0), laneEvent(0, lane('Empty'))] });
  await page.evaluate(() => {
    window.__lf.looper.setVolume(0, 1);
    window.__lf.native.sent.length = 0;
  });
  results.push(await page.evaluate(async ([pcm, exported]) => {
    const lf = window.__lf;
    const { session } = await import('/src/ui/state/audio.ts');
    const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    const before = lf.native.loadedSessions.length;
    await lf.importSession(window.__bundle, session);
    const loads = lf.native.loadedSessions.length - before;
    const restored = splitSessionBytes(lf.native.loadedSessions.at(-1).slice().buffer).pcm[0];
    const want = Float32Array.from(pcm);
    const errors = exported.stemErrors + want.reduce((n, x, k) => n + Number(restored[k] !== x), 0);
    const volume = session.trackVolume(0);
    const volumeSent = lf.native.sent.some((c) => JSON.stringify(c) === JSON.stringify({ SetVolume: [0, 0.25] }));
    return { name: 'Download bundle preserves editable overdub headroom and quiet samples', loads, errors,
      stemSamples: exported.stemSamples, samples: Array.from(restored.slice(1024, 1027)), volume, volumeSent,
      pass: loads === 1 && errors === 0 && volume === 0.25 && volumeSent };
  }, [editable, exported]));

  // ── The dry fallback, and the import caps ─────────────────────────────────────────────────────────
  await engineHolds(editable, 'Playing');
  results.push(await page.evaluate(async () => {
    const lf = window.__lf;
    const { session } = await import('/src/ui/state/audio.ts');
    const { maxImportArchiveBytes } = await import('/src/session/import.ts');
    const { parseZip } = await import('/src/session/unzip.ts');
    const { makeZip } = await import('/src/session/zip.ts');
    const { decodeWav } = await import('/src/session/wav.ts');
    lf.native.masterError = 'export render: an injected failure';
    let fallbackKind;
    let fallbackShape;
    try {
      const fallback = await lf.buildExportBundle(session);
      const entries = parseZip(fallback.zipBytes);
      const meta = JSON.parse(new TextDecoder().decode(entries.find((entry) => entry.name.endsWith('-session.json')).data));
      fallbackKind = meta.master.kind;
      fallbackShape = decodeWav(entries.find((entry) => entry.name === meta.master.file).data).channels.map((c) => c.length);
    } finally {
      lf.native.masterError = null;
    }
    const cap = maxImportArchiveBytes(48000);
    let oversizedRejected = false;
    try { await lf.importSession(new Uint8Array(cap + 1), session); }
    catch (error) { oversizedRejected = String(error).includes(`maximum is ${cap}`); }
    // A small valid ZIP can repeat one large local payload through many central-directory records.
    const one = makeZip([{ name: 'same.wav', data: new Uint8Array(1024 * 1024) }]);
    const view = new DataView(one.buffer);
    const eocd = one.length - 22;
    const start = view.getUint32(eocd + 16, true), size = view.getUint32(eocd + 12, true);
    const count = 128;
    const hostile = new Uint8Array(start + count * size + 22);
    hostile.set(one.subarray(0, start));
    for (let i = 0; i < count; i++) hostile.set(one.subarray(start, eocd), start + i * size);
    hostile.set(one.subarray(eocd), hostile.length - 22);
    const directory = new DataView(hostile.buffer), end = hostile.length - 22;
    directory.setUint16(end + 8, count, true);
    directory.setUint16(end + 10, count, true);
    directory.setUint32(end + 12, count * size, true);
    let repeatedPayloadRejected = false;
    const began = performance.now();
    try { await lf.importSession(hostile, session); }
    catch (error) { repeatedPayloadRejected = String(error).includes('128 entries; maximum is 9'); }
    const rejectionMs = performance.now() - began;
    return { name: 'A snapshot without its master exports a dry master; import refuses oversized and repeated-payload archives',
      fallbackKind, fallbackShape, oversizedRejected, cap, repeatedPayloadRejected, rejectionMs,
      pass: fallbackKind === 'dry-fallback' && JSON.stringify(fallbackShape) === JSON.stringify([96000, 96000])
        && oversizedRejected && repeatedPayloadRejected };
  }));

  // ── A fader moved while the master renders ────────────────────────────────────────────────────────
  await engineHolds(editable, 'Playing');
  results.push(await page.evaluate(async () => {
    const lf = window.__lf;
    const { session } = await import('/src/ui/state/audio.ts');
    const { parseZip } = await import('/src/session/unzip.ts');
    const volume = session.trackVolume(0);
    const level = session.masterLevel();
    let release = () => {};
    lf.native.snapshotHold = new Promise((resolve) => (release = resolve));
    let meta;
    try {
      const asked = lf.native.snapshots.length;
      const pending = lf.buildExportBundle(session);
      // The engine has the request and renders: the player moves lane 1's fader and the master's.
      while (lf.native.snapshots.length === asked) await new Promise((resolve) => setTimeout(resolve, 5));
      lf.looper.setVolume(0, volume / 2);
      lf.master.setVolume(level / 2);
      release();
      const entries = parseZip((await pending).zipBytes);
      meta = JSON.parse(new TextDecoder().decode(entries.find((entry) => entry.name.endsWith('-session.json')).data));
    } finally {
      lf.native.snapshotHold = null;
      release();
    }
    const kept = { volume: meta.tracks[0].volume, level: meta.master.level };
    const moved = { volume: session.trackVolume(0), level: session.masterLevel() };
    return { name: 'A fader moved while the master renders stays out of session.json', before: { volume, level }, kept, moved,
      pass: kept.volume === volume && kept.level === level && moved.volume !== volume && moved.level !== level };
  }));

  // ── The grid as the snapshot holds it ─────────────────────────────────────────────────────────────
  // The feed shows two bars, but the engine halved the loop before the export's snapshot: the Export
  // button's archive declares the snapshot's grid, and imports as it.
  await engineHolds(editable, 'Playing');
  await emit({ events: [transport(2 * MASTER), laneEvent(0, lane('Playing', { length: 2 * MASTER }))] });
  results.push(await page.evaluate(async () => {
    const { session } = await import('/src/ui/state/audio.ts');
    const { parseZip } = await import('/src/session/unzip.ts');
    const { validateSession } = await import('/src/session/session-schema.ts');
    const blobs = [];
    const createObjectURL = URL.createObjectURL;
    URL.createObjectURL = (blob) => (blobs.push(blob), createObjectURL(blob));
    try {
      document.querySelector('.tool--export').click();
      for (const t0 = Date.now(); blobs.length === 0 && Date.now() - t0 < 10000; ) await new Promise((resolve) => setTimeout(resolve, 20));
    } finally {
      URL.createObjectURL = createObjectURL;
    }
    if (blobs.length === 0) throw new Error('the Export button downloaded nothing');
    const zip = new Uint8Array(await blobs[0].arrayBuffer());
    const meta = JSON.parse(new TextDecoder().decode(parseZip(zip).find((entry) => entry.name.endsWith('-session.json')).data));
    let validation = 'ok';
    try { validateSession(meta); } catch (error) { validation = String(error); }
    const grid = { bpm: meta.bpm, bars: meta.bars, masterLengthFrames: meta.masterLengthFrames, feedMaster: session.masterFramesValue() };
    return { name: "The Export button's archive declares the snapshot's grid", grid, validation, zip: Array.from(zip),
      pass: grid.bars === 1 && grid.bpm === 120 && grid.masterLengthFrames === 96000 && validation === 'ok' };
  }));
  const gridZip = results.at(-1).zip;
  delete results.at(-1).zip;
  // The engine empties: the archive imports with its own grid.
  await emit({ events: [transport(0), laneEvent(0, lane('Empty'))] });
  results.push(await page.evaluate(async (zip) => {
    const lf = window.__lf;
    const { session } = await import('/src/ui/state/audio.ts');
    const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    let imported = 'ok';
    try { await lf.importSession(Uint8Array.from(zip), session); } catch (error) { imported = String(error); }
    const header = imported === 'ok' ? splitSessionBytes(lf.native.loadedSessions.at(-1).slice().buffer).header : null;
    return { name: 'That archive round-trips with the snapshot grid', imported, header: header && { bpm: header.bpm, bars: header.bars, master: header.masterLengthFrames },
      pass: imported === 'ok' && header?.bars === 1 && header?.masterLengthFrames === 96000 };
  }, gridZip));

  console.log(JSON.stringify(results, null, 2));
  assert.ok(results.every((result) => result.pass), JSON.stringify(results));
  // The injected render failure's fallback logs the one error this page may show, naming the engine's.
  const fallbackLogs = consoleErrors.filter((text) => text.includes('[export] the engine rendered no wet master'));
  assert.equal(fallbackLogs.length, 1, 'the fallback logs once');
  assert.ok(fallbackLogs[0].includes('an injected failure'), `the log carries the engine's error: ${fallbackLogs[0]}`);
  const unexpected = consoleErrors.filter((text) => !text.includes('[export] the engine rendered no wet master'));
  assert.deepEqual(unexpected, [], 'no other console errors');

  // ── A 44.1 kHz engine whose device stopped keeps its rate ─────────────────────────────────────────
  // An eight-bar loop at 120 BPM and 44.1 kHz (705600 frames, 16 s): the loop readout reads it so while
  // the device runs, and still after the device stops and the engine stays (a `status: null` frame).
  const slow = await open({
    init: (p) => p.addInitScript(() => {
      window.__lfEngineFake = true;
      globalThis.__lfEngineFakeRate = 44100;
    }),
  });
  await slow.page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
  const EIGHT = 705600;
  const slowEmit = (frame) => slow.page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  await slowEmit({
    reset: true,
    settings: [],
    events: [transport(EIGHT), laneEvent(0, lane('Playing', { length: EIGHT })), ...[1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))),
      { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: 44100, grid: 0 },
    meter: { peak: 0, clip: false },
  });
  const readout = () => slow.page.locator('.transport__loop-v').textContent();
  const running = await readout();
  await slowEmit({ status: null });
  const stopped = await readout();
  console.log(JSON.stringify({ scene: '44.1 kHz loop readout', running, stopped }));
  assert.match(running, /^8 BARS · 16\.0 s$/, 'a running 44.1 kHz device reads its eight-bar loop');
  assert.match(stopped, /^8 BARS · 16\.0 s$/, 'the stopped device leaves the engine at 44.1 kHz: the loop still reads eight bars');
  assert.deepEqual(slow.consoleErrors, [], 'no console errors at 44.1 kHz');
});

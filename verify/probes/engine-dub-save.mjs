/**
 * Engine mode's recovery saves while the player keeps layering, on the web engine fake
 * (`src/platform/host.web.ts`). The probe scripts a first take that commits and goes straight into an
 * overdub that runs for ten seconds (its waveform redrawn ten times a second, as the feed draws a layer
 * summing), with a later take recording on another lane meanwhile, then the layer's commit. The fake's
 * snapshot answers what the engine answers: during the dub, the loop before the layer
 * (`lf_engine::session`; `engine_io/tests.rs` proves it on the real engine).
 *
 * - one save lands within a few seconds of the take's commit, while the dub runs, and holds the loop
 *   before the layer (the dubbed lane's stem is the committed take, exactly);
 * - it stays one: the layer's waveform and the other lane's take move nothing the recovery holds;
 * - the layer's commit is the next save, with the layer in it.
 *
 * Counts every write to the recovery's `latest` slot (`src/audio/autosave.ts`). Cannot see the native
 * engine or the save's cost on the rig.
 * Run: pnpm probe engine-dub-save
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const MASTER = 2 * RATE; // one bar at 120 BPM

const lane = (state, extra = {}) => ({
  state,
  length: state === 'Empty' || state === 'Recording' ? 0 : MASTER,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: state === 'Playing',
  reversed: false,
  stopAt: null,
  retakePass: 0,
  ...extra,
});
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });
/** A waveform update of `bins` bins for lane `i` (the feed's peaks), its values moving with `t`. */
const peaks = (i, bins, t) => ({
  lane: i,
  start: 0,
  count: bins,
  min: Array.from({ length: bins }, (_, k) => -0.1 - 0.01 * ((k + t) % 7)),
  max: Array.from({ length: bins }, (_, k) => 0.1 + 0.01 * ((k + t) % 5)),
});

await probe(async ({ browser, open }) => {
  const context = await browser.newContext({ viewport: { width: 1600, height: 900 } });
  const { page, consoleErrors } = await open({
    context,
    init: (p) =>
      p.addInitScript(() => {
        window.__lfEngineFake = true;
        // Every write to the recovery's latest slot, and when.
        window.__latestPuts = [];
        const put = IDBObjectStore.prototype.put;
        IDBObjectStore.prototype.put = function (value, key) {
          if (this.name === 'recovery' && value?.key === 'latest') window.__latestPuts.push(performance.now());
          return put.call(this, value, key);
        };
      }),
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
  await page.evaluate(() => window.__lf.autosave.ready());
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const now = () => page.evaluate(() => performance.now());
  const puts = () => page.evaluate(() => window.__latestPuts.slice());
  /** The snapshot the engine answers: lane 0 holding `seed`'s loop in `state`; its signature. */
  const snapshot = (seed, state) =>
    page.evaluate(
      async ([seed, state, master]) => {
        const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
        const pcm = Float32Array.from({ length: master }, (_, i) => 0.2 * Math.sin((2 * Math.PI * 330 * i) / 48000) + seed * 0.05 * Math.sin((2 * Math.PI * 550 * i) / 48000));
        window.__lf.native.snapshotBytes = encodeSessionBytes(
          { rate: 48000, masterLengthFrames: master, bpm: 120, tracks: [{ index: 0, frames: master, reversed: false, state }] },
          [pcm],
        ).buffer;
        return Array.from(pcm.slice(0, 64));
      },
      [seed, state, MASTER],
    );
  /** The stems of the recovery's latest slot: their first samples. */
  const latestStems = () =>
    page.evaluate(async () => {
      const db = await new Promise((resolve, reject) => {
        const r = indexedDB.open('bleeploop');
        r.onsuccess = () => resolve(r.result);
        r.onerror = () => reject(r.error);
      });
      const record = await new Promise((resolve, reject) => {
        const r = db.transaction('recovery', 'readonly').objectStore('recovery').get('latest');
        r.onsuccess = () => resolve(r.result);
        r.onerror = () => reject(r.error);
      });
      db.close();
      const { parseZip } = await import('/src/audio/export/unzip.ts');
      const { decodeWav } = await import('/src/audio/export/wav.ts');
      const entries = parseZip(new Uint8Array(record.bytes));
      const session = JSON.parse(new TextDecoder().decode(entries.find((e) => e.name.endsWith('-session.json')).data));
      return session.tracks.map((t) => ({ track: t.track, state: t.state, head: Array.from(decodeWav(entries.find((e) => e.name === t.file).data).channels[0].slice(0, 64)) }));
    });

  // The engine's first frame: every lane EMPTY.
  await emit({
    reset: true,
    settings: [],
    events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, ...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });

  // A first take records for a second, commits, and goes straight into an overdub.
  await emit({ events: [laneEvent(0, lane('Recording'))] });
  for (let t = 0; t < 10; t++) {
    await emit({ peaks: [peaks(0, 5 * (t + 1), t)] });
    await page.waitForTimeout(100);
  }
  const take = await snapshot(0, 'Playing');
  await emit({ events: [{ Transport: { frame: 0, master: MASTER, bpm: 120, locked: true } }, laneEvent(0, lane('Playing'))], peaks: [peaks(0, 94, 0)] });
  const committedAt = await now();
  await page.waitForTimeout(300);
  await snapshot(0, 'Overdubbing'); // the engine hands the loop before the layer while it sums
  await emit({ events: [laneEvent(0, lane('Overdubbing', { canUndo: true }))] });

  // Ten seconds of layering: the layer's waveform moves ten times a second; lane 2 records a take from 4 s.
  for (let t = 0; t < 100; t++) {
    const frame = { peaks: [peaks(0, 94, t + 1)] };
    if (t === 40) frame.events = [laneEvent(1, lane('Recording'))];
    if (t > 40) frame.peaks.push(peaks(1, t - 40, t));
    await emit(frame);
    await page.waitForTimeout(100);
  }
  const duringDub = (await puts()).filter((at) => at > committedAt);
  console.log(`saves during the dub: ${duringDub.map((at) => `+${Math.round(at - committedAt)} ms`).join(', ') || 'none'}`);
  assert.equal(duringDub.length, 1, 'one save while the dub runs, not none and not one per waveform update');
  assert.ok(duringDub[0] - committedAt < 5000, 'within a few seconds of the take committing');
  const dubbed = await latestStems();
  console.log('saved', JSON.stringify(dubbed.map((t) => ({ track: t.track, state: t.state, head: t.head.slice(0, 3) }))));
  assert.deepEqual(dubbed.map((t) => t.track), [1], 'the dubbed lane alone (the other lane only records)');
  assert.deepEqual(dubbed[0].head, take.map((x) => Math.fround(x)), 'the loop before the layer, exactly');

  // The layer commits: the next save holds it.
  const layered = await snapshot(1, 'Playing');
  await emit({ events: [laneEvent(0, lane('Playing', { canUndo: true }))], peaks: [peaks(0, 94, 999)] });
  const layerAt = await now();
  for (let t0 = Date.now(); (await puts()).filter((at) => at > layerAt).length === 0; await page.waitForTimeout(250)) {
    assert.ok(Date.now() - t0 < 8000, 'the layer is saved within 8 s of its commit');
  }
  await page.waitForTimeout(500);
  assert.deepEqual((await latestStems())[0].head, layered.map((x) => Math.fround(x)), 'the committed layer is in the save');
  assert.equal((await puts()).filter((at) => at > committedAt).length, 2, 'two commits, two saves');
  assert.deepEqual(consoleErrors, [], 'no console errors');
  await context.close();
});

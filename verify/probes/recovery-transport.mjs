/**
 * A recovery save whose snapshot pinned another transport state than the one inspected
 * (`src/session/autosave.ts` `persistedFingerprint`), on the web engine fake (`src/platform/host.web.ts`).
 * The feed shows lane 1 PLAYING the whole time, while the snapshot the engine answers for the first save
 * holds it STOPPED (the player stopped it while the save ran and started it again): a plain STOP or PLAY
 * moves no lane revision and no waveform, so only the state tells the jam apart.
 *
 * - the first save holds the lane STOPPED, as its snapshot does;
 * - the jam does not count as saved: a second save follows, holding the lane PLAYING.
 *
 * Counts every write to the recovery's `latest` slot. Cannot see the native engine or its snapshot over
 * Tauri IPC.
 * Run: pnpm probe recovery-transport
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const MASTER = 2 * RATE; // one bar at 120 BPM

const lane = (state) => ({
  state,
  length: state === 'Empty' ? 0 : MASTER,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
});
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });

await probe(async ({ open }) => {
  const { page } = await open({
    init: (p) =>
      p.addInitScript(() => {
        window.__lfEngineFake = true;
        window.__latestPuts = 0;
        const put = IDBObjectStore.prototype.put;
        IDBObjectStore.prototype.put = function (value, key) {
          if (this.name === 'recovery' && value?.key === 'latest') window.__latestPuts++;
          return put.call(this, value, key);
        };
      }),
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
  await page.evaluate(() => window.__lf.autosave.ready());
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const puts = () => page.evaluate(() => window.__latestPuts);
  /** The snapshot the engine answers: lane 1 holding one loop in `state`. */
  const snapshot = (state) =>
    page.evaluate(
      async ([state, master]) => {
        const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
        const pcm = Float32Array.from({ length: master }, (_, i) => 0.2 * Math.sin((2 * Math.PI * 330 * i) / 48000));
        window.__lf.native.snapshotBytes = encodeSessionBytes(
          { rate: 48000, masterLengthFrames: master, bpm: 120, tracks: [{ index: 0, frames: master, reversed: false, state }] },
          [pcm],
        ).buffer;
      },
      [state, MASTER],
    );
  /** The state of lane 1 in the recovery's latest slot. */
  const savedState = () =>
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
      const { parseZip } = await import('/src/session/unzip.ts');
      const entries = parseZip(new Uint8Array(record.bytes));
      return JSON.parse(new TextDecoder().decode(entries.find((e) => e.name.endsWith('-session.json')).data)).tracks[0].state;
    });
  const waitPuts = async (n, ms) => {
    for (const t0 = Date.now(); (await puts()) < n; await page.waitForTimeout(200)) if (Date.now() - t0 > ms) return false;
    return true;
  };

  await emit({
    reset: true,
    settings: [],
    events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, ...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: [0, 0], clip: [false, false] },
  });
  // The feed: lane 1 commits and plays. The engine's snapshot for the save: the lane STOPPED.
  await snapshot('Stopped');
  await emit({ events: [{ Transport: { frame: 0, master: MASTER, bpm: 120, locked: true } }, laneEvent(0, lane('Playing'))] });
  assert.ok(await waitPuts(1, 8000), 'the committed loop is saved');
  const first = await savedState();
  // Played again: the engine's snapshot holds it PLAYING now, as the feed shows all along.
  await snapshot('Playing');
  const second = await waitPuts(2, 6000);
  const last = await savedState();
  console.log(JSON.stringify({ puts: await puts(), first, second, last }));
  assert.equal(first, 'STOPPED', 'the first save holds the state its snapshot pinned');
  assert.ok(second, 'a jam whose save pinned another transport state does not count as saved: a second save follows');
  assert.equal(last, 'PLAYING', 'and holds the lane playing');
});

/**
 * Engine mode's recovery keeps a jam the engine lost without the player clearing it, on the web engine
 * fake (`src/platform/host.web.ts`; `window.__lfEngineFakeRate` sets the rate its device runs at). The
 * probe scripts the lanes on the feed and the snapshot the engine answers, and reads the recovery's
 * IndexedDB slots (`src/audio/autosave.ts`: `latest`, and `kept` for a jam at another rate):
 *
 * - reset: a reset frame that blanks every lane (a new engine: another rate, a fault) keeps the saved
 *   jam, through the close guard's flush and a CLEAR ALL on the empty lanes;
 * - clear: the player's CLEAR ALL of a saved jam deletes it;
 * - launch: a launch whose device runs at another rate keeps the jam and says why; a new jam there
 *   moves it aside instead of overwriting it, and a launch at its rate restores it;
 * - confirm: a device pick the engine refuses (the rate differs while it holds loops) asks the player;
 *   declined, the pick goes back and nothing switches; confirmed, the jam is saved first, the switch is
 *   forced, and the new engine's empty lanes keep the recovery.
 *
 * Cannot see the native engine or Tauri: the refusal, the rates and every lane are scripted (the Rust
 * side: `src-tauri/src/engine_io/tests.rs`). `--case=reset|clear|launch|confirm` runs one.
 * Run: pnpm probe engine-recovery-keep
 */
import assert from 'node:assert/strict';
import { arg, probe } from '../harness/probe.ts';

const only = arg('case');
const bar = (rate) => 2 * rate; // one 4/4 bar at 120 BPM

const lane = (state, frames) => ({
  state,
  length: state === 'Empty' ? 0 : frames,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: state === 'Playing' || state === 'Stopped',
  reversed: false,
  stopAt: null,
  retakePass: 0,
});
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });
const emptyLanes = (from = 0) => [0, 1, 2, 3, 4].slice(from).map((i) => laneEvent(i, lane('Empty', 0)));

/** Before the app loads: engine mode on the fake, at the rate the session says, and page helpers. */
const init = (p) =>
  p.addInitScript(() => {
    window.__lfEngineFake = true;
    // Pages of one context share it, so a relaunch meets the rate the page before set.
    window.__lfEngineFakeRate = Number(localStorage.getItem('probe.rate') ?? 48000);
    /** A Float32Array's identity: its length and an FNV-1a hash of its bits. */
    const sig = (pcm) => {
      const bits = new Uint32Array(pcm.buffer, pcm.byteOffset, pcm.length);
      let h = 2166136261;
      for (const x of bits) h = Math.imul(h ^ x, 16777619) >>> 0;
      return `${pcm.length}:${h.toString(16)}`;
    };
    window.__probe = {
      sig,
      /** Two lanes at `rate` (lane 1 stopped) as the engine would hold them: their snapshot answer, and
       * each lane's signature. */
      async jam(rate, seed) {
        const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
        const frames = 2 * rate;
        const pcm = [0, 1].map((k) =>
          Float32Array.from({ length: frames }, (_, i) => 0.3 * Math.sin((2 * Math.PI * (220 + 110 * seed + 55 * k) * i) / rate)),
        );
        window.__lf.native.snapshotBytes = encodeSessionBytes(
          {
            rate,
            masterLengthFrames: frames,
            bpm: 120,
            tracks: [
              { index: 0, frames, reversed: false, state: 'Playing' },
              { index: 1, frames, reversed: false, state: 'Stopped' },
            ],
          },
          pcm,
        ).buffer;
        return pcm.map(sig);
      },
      /** A recovery slot as saved: its rate, time and each stem's signature (null when empty). */
      async slot(key) {
        const db = await new Promise((resolve, reject) => {
          const r = indexedDB.open('bleeploop');
          r.onsuccess = () => resolve(r.result);
          r.onerror = () => reject(r.error);
        });
        const record = await new Promise((resolve, reject) => {
          const r = db.transaction('recovery', 'readonly').objectStore('recovery').get(key);
          r.onsuccess = () => resolve(r.result);
          r.onerror = () => reject(r.error);
        });
        db.close();
        if (!record) return null;
        const { parseZip } = await import('/src/audio/export/unzip.ts');
        const { decodeWav } = await import('/src/audio/export/wav.ts');
        const entries = parseZip(new Uint8Array(record.bytes));
        const session = JSON.parse(new TextDecoder().decode(entries.find((e) => e.name.endsWith('-session.json')).data));
        const stems = session.tracks.map((t) => sig(decodeWav(entries.find((e) => e.name === t.file).data).channels[0]));
        return { rate: record.rate ?? null, sampleRate: session.sampleRate, savedAt: record.savedAt, stems };
      },
    };
  });

/** Open the app in a fresh profile; resolves once its device opened (at the session's rate). */
async function boot(browser, open, context = null) {
  const ctx = context ?? (await browser.newContext({ viewport: { width: 1600, height: 900 } }));
  const app = await open({ context: ctx, init });
  await app.page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
  return { ...app, context: ctx };
}

let seq = 0;
const emit = (page, frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], device: [], peaks: [], anchor: null, meter: null, ...frame });

/** The engine's first frame: every lane EMPTY at `rate` (a new engine, or the one a launch meets). */
const emptyEngine = (page, rate) =>
  emit(page, {
    reset: true,
    settings: [],
    events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, ...emptyLanes(), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate, grid: 0 },
    meter: { peak: 0, clip: false },
  });

/** Two committed lanes at `rate` on the feed, the snapshot to match; their signatures. */
async function commitJam(page, rate, seed) {
  const sigs = await page.evaluate(([r, s]) => window.__probe.jam(r, s), [rate, seed]);
  await emit(page, {
    events: [
      { Transport: { frame: 0, master: bar(rate), bpm: 120, locked: true } },
      laneEvent(0, lane('Playing', bar(rate))),
      laneEvent(1, lane('Stopped', bar(rate))),
    ],
  });
  return sigs;
}

const slot = (page, key) => page.evaluate((k) => window.__probe.slot(k), key);

/** Wait until the `latest` slot holds `stems` (a save), at most 15 s. */
async function saved(page, stems, what) {
  for (let t0 = Date.now(); ; await page.waitForTimeout(250)) {
    const latest = await slot(page, 'latest').catch(() => null);
    if (latest && JSON.stringify(latest.stems) === JSON.stringify(stems)) return latest;
    assert.ok(Date.now() - t0 < 15000, `${what}: autosave saved the jam within 15 s (latest: ${JSON.stringify(latest)})`);
  }
}

/** Let autosave see the change, wait out its quiet window, then flush as the close guard does. */
async function settle(page) {
  await page.waitForTimeout(3200);
  await page.evaluate(() => window.__lf.autosave.flush());
}

const cases = {
  async reset({ browser, open }) {
    const { page, consoleErrors, context } = await boot(browser, open);
    await emptyEngine(page, 48000);
    const jam = await commitJam(page, 48000, 1);
    const before = await saved(page, jam, 'the jam');

    // A new engine (another rate, a fault): the reset frame blanks every lane.
    await emptyEngine(page, 48000);
    await settle(page);
    const afterReset = await slot(page, 'latest');
    console.log('after the reset', JSON.stringify(afterReset));
    assert.deepEqual(afterReset, before, 'a reset frame that blanks every lane keeps the recovery');

    // CLEAR ALL on the lanes the reset emptied: the engine reports Cleared on each, but none held a loop.
    await emit(page, { events: [0, 1, 2, 3, 4].flatMap((i) => [{ Cleared: { frame: 0, lane: i } }, laneEvent(i, lane('Empty', 0))]) });
    await settle(page);
    assert.deepEqual(await slot(page, 'latest'), before, 'a CLEAR ALL of lanes that held no loop keeps it too');
    assert.deepEqual(consoleErrors, []);
    await context.close();
  },

  async clear({ browser, open }) {
    const { page, consoleErrors, context } = await boot(browser, open);
    await emptyEngine(page, 48000);
    const jam = await commitJam(page, 48000, 2);
    await saved(page, jam, 'the jam');
    // The engine's CLEAR ALL: Cleared before each lane's own event, then the empty transport.
    await emit(page, {
      events: [
        ...[0, 1, 2, 3, 4].flatMap((i) => [{ Cleared: { frame: 0, lane: i } }, laneEvent(i, lane('Empty', 0))]),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
      ],
    });
    await settle(page);
    const latest = await slot(page, 'latest');
    console.log('after CLEAR ALL', JSON.stringify(latest));
    assert.equal(latest, null, "the player's CLEAR ALL deletes the recovery");
    assert.deepEqual(consoleErrors, []);
    await context.close();
  },

  async launch({ browser, open }) {
    const first = await boot(browser, open);
    const { context } = first;
    await emptyEngine(first.page, 48000);
    const jamA = await commitJam(first.page, 48000, 3);
    await saved(first.page, jamA, 'the 48 kHz jam');
    await first.page.evaluate(() => localStorage.setItem('probe.rate', '44100'));
    await first.page.close();

    // A launch on a device at 44.1 kHz: the 48 kHz jam cannot load there. It stays, and a toast says why.
    const second = await boot(browser, open, context);
    await second.page.evaluate(() => window.__lf.autosave.ready());
    await second.page.getByText('Loops kept in recovery').waitFor({ timeout: 5000 });
    assert.equal(await second.page.evaluate(() => window.__lf.native.loadedSessions.length), 0, 'nothing loads at the wrong rate');
    assert.deepEqual((await slot(second.page, 'latest')).stems, jamA, 'the 48 kHz jam is still the latest');

    // A new jam at 44.1 kHz saves without overwriting it: the 48 kHz one moves aside.
    await emptyEngine(second.page, 44100);
    const jamB = await commitJam(second.page, 44100, 4);
    const latestB = await saved(second.page, jamB, 'the 44.1 kHz jam');
    const kept = await slot(second.page, 'kept');
    console.log('latest', JSON.stringify(latestB), 'kept', JSON.stringify(kept));
    assert.equal(latestB.rate, 44100);
    assert.deepEqual([kept?.rate, kept?.sampleRate, kept?.stems], [48000, 48000, jamA], 'the 48 kHz jam is kept aside');
    await second.page.evaluate(() => localStorage.setItem('probe.rate', '48000'));
    await second.page.close();

    // Back on a 48 kHz device: the kept jam comes back, and the slots swap.
    const third = await boot(browser, open, context);
    await third.page.waitForFunction(() => window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 15000 });
    const loaded = await third.page.evaluate(async () => {
      const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
      const { header, pcm } = splitSessionBytes(window.__lf.native.loadedSessions[0].slice().buffer);
      return { header, sigs: pcm.map((p) => window.__probe.sig(p)) };
    });
    console.log('restored', JSON.stringify(loaded.header));
    assert.equal(loaded.header.masterLengthFrames, bar(48000));
    assert.deepEqual(loaded.sigs, jamA, 'the 48 kHz jam is restored exactly');
    await saved(third.page, jamA, 'the slots swap');
    assert.deepEqual((await slot(third.page, 'kept'))?.stems, jamB, 'the 44.1 kHz jam waits in its place');
    const errors = [...first.consoleErrors, ...second.consoleErrors, ...third.consoleErrors];
    assert.deepEqual(errors, [], 'no console errors');
    await context.close();
  },

  async confirm({ browser, open }) {
    const { page, consoleErrors, context } = await boot(browser, open);
    await emptyEngine(page, 48000);
    const jam = await commitJam(page, 48000, 5);
    const before = await saved(page, jam, 'the jam');
    await page.evaluate(() => {
      window.__lf.native.refusal = { device: 'Probe Interface', from: 48000, to: 44100 };
      window.__lf.ui.openSettings();
    });
    const buffer = page.getByRole('combobox', { name: 'Buffer size in frames' });
    const was = await buffer.inputValue();
    const pick = was === '256' ? '512' : '256';
    const opens = () => page.evaluate(() => window.__lf.native.forced.slice());
    const opened = (await opens()).length;

    // Declined: nothing switches, and the pick goes back to the device that runs.
    let asked = '';
    page.once('dialog', (d) => {
      asked = d.message();
      void d.dismiss();
    });
    await buffer.selectOption(pick);
    await page.waitForFunction((n) => window.__lf.native.forced.length === n + 1, opened, { timeout: 5000 });
    await page.waitForFunction((v) => document.querySelector('select[aria-label="Buffer size in frames"]').value === v, was, { timeout: 5000 });
    console.log('asked:', JSON.stringify(asked));
    assert.match(asked, /Probe Interface runs at 44\.1 kHz\. Your loops were recorded at 48 kHz/);
    assert.match(asked, /stay in recovery/);
    assert.deepEqual(await opens(), [...Array(opened).fill(false), false], 'declined: no forced open');
    assert.equal(await page.evaluate(() => JSON.parse(localStorage.getItem('lf.audioDevices')).bufferFrames), Number(was), 'the saved pick went back');

    // Confirmed: the jam goes to the recovery first, then the switch is forced.
    await page.evaluate(() => {
      window.__lfEngineFakeRate = 44100;
    });
    const acceptedAt = Date.now();
    page.once('dialog', (d) => void d.accept());
    await buffer.selectOption(pick);
    await page.waitForFunction(() => window.__lf.native.forced.at(-1) === true, undefined, { timeout: 10000 });
    const flushed = await slot(page, 'latest');
    assert.ok(flushed.savedAt >= acceptedAt, 'the recovery was saved after the confirm, before the switch');
    assert.deepEqual(flushed.stems, before.stems);
    assert.deepEqual((await opens()).slice(opened), [false, false, true], 'declined once, then refused and forced');

    // The new engine at 44.1 kHz starts empty: the recovery keeps the 48 kHz jam.
    await page.evaluate(() => (window.__lf.native.refusal = null));
    await emptyEngine(page, 44100);
    await settle(page);
    assert.deepEqual((await slot(page, 'latest'))?.stems, before.stems, 'the switch kept the jam in recovery');
    assert.deepEqual(consoleErrors, []);
    await context.close();
  },
};

await probe(async (p) => {
  const names = only ? only.split(',') : Object.keys(cases);
  for (const name of names) {
    assert.ok(cases[name], `no case ${name}`);
    console.log(`── ${name}`);
    await cases[name](p);
  }
});

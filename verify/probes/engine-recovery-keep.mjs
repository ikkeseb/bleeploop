/**
 * Engine mode's recovery keeps a jam the engine lost without the player clearing it, on the web engine
 * fake (`src/platform/host.web.ts`; `window.__lfEngineFakeRate` sets the rate its device runs at). The
 * probe scripts the lanes on the feed and the snapshot the engine answers, and reads the recovery's
 * IndexedDB keys (`src/audio/autosave.ts`: `latest`, and `kept-<rate>` for each other rate's jam):
 *
 * - reset: a reset frame that blanks every lane (a new engine: another rate, a fault) keeps the saved
 *   jam, through the close guard's flush and a CLEAR ALL on the empty lanes;
 * - clear: the player's CLEAR ALL of a saved jam deletes it;
 * - partial: a CLEAR of one lane, or a CLEAR ALL followed by a new jam, lets no empty snapshot delete
 *   the jam (a new engine's, read before its reset frame arrives); the CLEAR of the last loop does;
 * - launch: a launch whose device runs at another rate keeps the jam and says why; a new jam there
 *   moves it aside instead of overwriting it, and a launch at its rate restores it;
 * - rates: three rates keep three jams, and a launch at each restores its own;
 * - sweep: a player's clear at a rate deletes the jam kept at that rate too, so it never comes back;
 * - confirm: a device pick the engine refuses (the rate differs while it holds loops) asks the player;
 *   declined, the pick goes back and nothing switches; confirmed, the jam is saved first, the switch is
 *   forced, and the new engine's empty lanes keep the recovery;
 * - first-take: confirmed during a first take whose commit (the device's stop) has not reached the feed
 *   yet, the take is saved from the engine's snapshot before the forced open;
 * - failed: a switch whose recovery save fails reopens the device that runs, and so does a second one.
 *
 * Cannot see the native engine or Tauri: the refusal, the rates and every lane are scripted (the Rust
 * side: `src-tauri/src/engine_io/tests.rs`). `--case=<name>[,<name>]` runs some.
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
  fading: false,
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
      /** `lanes` of a jam at `rate` (lane 0 playing, lane 1 stopped) as the engine would hold them: the
       * bytes of their snapshot, and each lane's signature. */
      async snapshot(rate, seed, lanes = [0, 1]) {
        const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
        const frames = 2 * rate;
        const pcm = lanes.map((k) =>
          Float32Array.from({ length: frames }, (_, i) => 0.3 * Math.sin((2 * Math.PI * (220 + 110 * seed + 55 * k) * i) / rate)),
        );
        const tracks = lanes.map((k) => ({ index: k, frames, reversed: false, state: k === 0 ? 'Playing' : 'Stopped' }));
        return { bytes: encodeSessionBytes({ rate, masterLengthFrames: frames, bpm: 120, tracks }, pcm).buffer, sigs: pcm.map(sig) };
      },
      /** The engine's snapshot answers `lanes` of the jam; their signatures. */
      async jam(rate, seed, lanes = [0, 1]) {
        const { bytes, sigs } = await window.__probe.snapshot(rate, seed, lanes);
        window.__lf.native.snapshotBytes = bytes;
        return sigs;
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

/** The device a new engine runs on, at `rate` (a fallback after a loss). */
const fallback = (rate) => ({
  backend: 'Wasapi',
  sampleRate: rate,
  block: 480,
  inputName: 'Backup input',
  outputName: 'Backup output',
  alignFrames: 0,
  inputFrames: 0,
  inputOpen: true,
});

/** The engine's first frame: every lane EMPTY at `rate` (a new engine, or the one a launch meets); with
 * `status`, on that device. */
const emptyEngine = (page, rate, status) =>
  emit(page, {
    reset: true,
    settings: [],
    events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, ...emptyLanes(), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate, grid: 0 },
    meter: { peak: 0, clip: false },
    ...(status ? { status } : {}),
  });

/** The engine's CLEAR of `lanes`: Cleared before each lane's own event, then (all of them) the empty
 * transport. */
const clearLanes = (page, lanes = [0, 1, 2, 3, 4]) =>
  emit(page, {
    events: [
      ...lanes.flatMap((i) => [{ Cleared: { frame: 0, lane: i } }, laneEvent(i, lane('Empty', 0))]),
      ...(lanes.length === 5 ? [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } }] : []),
    ],
  });

/** Close `app`'s page and launch again in its profile on a device at `rate`. */
async function relaunch(browser, open, app, rate) {
  await app.page.evaluate((r) => localStorage.setItem('probe.rate', String(r)), rate);
  await app.page.close();
  const next = await boot(browser, open, app.context);
  await next.page.evaluate(() => window.__lf.autosave.ready());
  return next;
}

/** The session the fake engine was asked to load last: its signatures (none loaded: null). */
const loaded = (page) =>
  page.evaluate(async () => {
    const sessions = window.__lf.native.loadedSessions;
    if (sessions.length === 0) return null;
    const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    const { header, pcm } = splitSessionBytes(sessions.at(-1).slice().buffer);
    return { master: header.masterLengthFrames, sigs: pcm.map((p) => window.__probe.sig(p)) };
  });

/** The stems each recovery key holds (keys it lacks: null). */
async function keys(page, names) {
  const out = {};
  for (const name of names) out[name] = (await slot(page, name))?.stems ?? null;
  return out;
}

/** A committed loop's waveform, as the feed draws it (its values follow `seed`). */
const wave = (i, seed) => ({ lane: i, start: 0, count: 4, min: [-0.1, -0.2, -0.1 * seed, -0.3], max: [0.1, 0.2, 0.1 * seed, 0.3] });

/** Two committed lanes at `rate` on the feed, with their waveforms, the snapshot to match; their
 * signatures. */
async function commitJam(page, rate, seed) {
  const sigs = await page.evaluate(([r, s]) => window.__probe.jam(r, s), [rate, seed]);
  await emit(page, {
    events: [
      { Transport: { frame: 0, master: bar(rate), bpm: 120, locked: true } },
      laneEvent(0, lane('Playing', bar(rate))),
      laneEvent(1, lane('Stopped', bar(rate))),
    ],
    peaks: [wave(0, seed), wave(1, seed)],
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

  async partial({ browser, open }) {
    const { page, consoleErrors, context } = await boot(browser, open);
    await emptyEngine(page, 48000);
    const jam = await commitJam(page, 48000, 6);
    await saved(page, jam, 'the jam');

    // CLEAR on lane 1 alone: the engine's snapshot holds lane 2 now, and the recovery saves that.
    await page.evaluate(() => window.__probe.jam(48000, 6, [1]));
    await clearLanes(page, [0]);
    const kept = await saved(page, [jam[1]], 'the jam without lane 1');
    // A new engine (another rate, a fault) whose reset frame has not arrived: its snapshot is empty while
    // the lanes here still show lane 2's loop. The close guard's flush reads it.
    await page.evaluate(() => (window.__lf.native.snapshotBytes = null));
    await page.evaluate(() => window.__lf.autosave.flush());
    const afterFlush = await slot(page, 'latest');
    console.log('partial CLEAR, then an empty snapshot:', JSON.stringify(afterFlush?.stems ?? null));
    assert.deepEqual(afterFlush, kept, "a partial CLEAR does not let a new engine's empty snapshot delete the jam");
    await emptyEngine(page, 48000);
    await settle(page);
    assert.deepEqual(await slot(page, 'latest'), kept, 'nor its reset frame');

    // CLEAR ALL, and a new jam commits before the recovery deleted the old one: the clear is spent on
    // nothing, and a new engine's empty snapshot keeps the new jam.
    const jamB = await commitJam(page, 48000, 7);
    await saved(page, jamB, 'the next jam');
    await page.evaluate(() => (window.__lf.native.snapshotBytes = null));
    await clearLanes(page);
    await page.waitForTimeout(300);
    const jamC = await commitJam(page, 48000, 8);
    const savedC = await saved(page, jamC, 'the jam recorded after CLEAR ALL');
    await page.evaluate(() => (window.__lf.native.snapshotBytes = null));
    await page.evaluate(() => window.__lf.autosave.flush());
    const afterRecommit = await slot(page, 'latest');
    console.log('CLEAR ALL, a new jam, then an empty snapshot:', JSON.stringify(afterRecommit?.stems ?? null));
    assert.deepEqual(afterRecommit, savedC, 'a commit after CLEAR ALL cancels its permission to delete');

    // The CLEAR of the last loop empties the looper: that deletes the recovery.
    await emptyEngine(page, 48000);
    const jamD = await commitJam(page, 48000, 9);
    await saved(page, jamD, 'a fourth jam');
    await page.evaluate(() => window.__probe.jam(48000, 9, [1]));
    await clearLanes(page, [0]);
    await saved(page, [jamD[1]], 'the fourth jam without lane 1');
    await page.evaluate(() => (window.__lf.native.snapshotBytes = null));
    await clearLanes(page, [1]);
    await settle(page);
    assert.equal(await slot(page, 'latest'), null, 'the CLEAR of the last loop deletes the recovery');
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
    const kept = await slot(second.page, 'kept-48000');
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
    assert.deepEqual((await slot(third.page, 'kept-44100'))?.stems, jamB, 'the 44.1 kHz jam waits in its place');
    assert.equal(await slot(third.page, 'kept-48000'), null, 'the restored jam is the latest, not kept too');
    const errors = [...first.consoleErrors, ...second.consoleErrors, ...third.consoleErrors];
    assert.deepEqual(errors, [], 'no console errors');
    await context.close();
  },

  async rates({ browser, open }) {
    const first = await boot(browser, open);
    const errors = [first.consoleErrors];
    await emptyEngine(first.page, 48000);
    const jamA = await commitJam(first.page, 48000, 10);
    await saved(first.page, jamA, 'the 48 kHz jam');

    const second = await relaunch(browser, open, first, 44100);
    errors.push(second.consoleErrors);
    await emptyEngine(second.page, 44100);
    const jamB = await commitJam(second.page, 44100, 11);
    await saved(second.page, jamB, 'the 44.1 kHz jam');

    // A third rate: its jam must not push the first one out.
    const third = await relaunch(browser, open, second, 96000);
    errors.push(third.consoleErrors);
    await third.page.getByText('Loops kept in recovery').waitFor({ timeout: 5000 });
    await emptyEngine(third.page, 96000);
    const jamC = await commitJam(third.page, 96000, 12);
    await saved(third.page, jamC, 'the 96 kHz jam');

    // Back at 48 kHz, then at 44.1 kHz: each launch restores its own rate's jam.
    const fourth = await relaunch(browser, open, third, 48000);
    errors.push(fourth.consoleErrors);
    await fourth.page.waitForFunction(() => window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 15000 }).catch(() => null);
    const at48 = await loaded(fourth.page);
    console.log('restored at 48 kHz', JSON.stringify(at48));
    assert.deepEqual(at48?.sigs, jamA, 'the 48 kHz jam survived two jams at other rates');
    await saved(fourth.page, jamA, 'the 48 kHz jam is the latest again');
    const fifth = await relaunch(browser, open, fourth, 44100);
    errors.push(fifth.consoleErrors);
    await fifth.page.waitForFunction(() => window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 15000 }).catch(() => null);
    assert.deepEqual((await loaded(fifth.page))?.sigs, jamB, 'and the 44.1 kHz one');
    await saved(fifth.page, jamB, 'the 44.1 kHz jam is the latest');
    const held = await keys(fifth.page, ['kept-48000', 'kept-44100', 'kept-96000']);
    console.log('kept', JSON.stringify(held));
    assert.deepEqual(held, { 'kept-48000': jamA, 'kept-44100': null, 'kept-96000': jamC }, 'one jam per rate, the latest not kept twice');
    assert.deepEqual(errors.flat(), [], 'no console errors');
    await fifth.context.close();
  },

  async sweep({ browser, open }) {
    const first = await boot(browser, open);
    const errors = [first.consoleErrors];
    await emptyEngine(first.page, 48000);
    const jamA = await commitJam(first.page, 48000, 13);
    await saved(first.page, jamA, 'the 48 kHz jam');
    const second = await relaunch(browser, open, first, 44100);
    errors.push(second.consoleErrors);
    await emptyEngine(second.page, 44100);
    const jamB = await commitJam(second.page, 44100, 14);
    await saved(second.page, jamB, 'the 44.1 kHz jam');

    // The engine is rebuilt at 48 kHz (a fallback device): the player records there and clears it all
    // before a save lands. That clear is the 48 kHz jam's: the one kept at 48 kHz goes with it.
    await emptyEngine(second.page, 48000, fallback(48000));
    await commitJam(second.page, 48000, 15);
    await second.page.waitForTimeout(300);
    await second.page.evaluate(() => (window.__lf.native.snapshotBytes = null));
    await clearLanes(second.page);
    await settle(second.page);
    const held = await keys(second.page, ['latest', 'kept-48000']);
    console.log('after the clear at 48 kHz', JSON.stringify(held));
    assert.deepEqual(held, { latest: jamB, 'kept-48000': null }, 'the clear at 48 kHz deleted the jam kept there, not the 44.1 kHz one');

    const third = await relaunch(browser, open, second, 48000);
    errors.push(third.consoleErrors);
    const back = await loaded(third.page);
    console.log('restored at 48 kHz', JSON.stringify(back));
    assert.equal(back, null, 'the cleared 48 kHz jam does not come back');
    await third.page.getByText('Loops kept in recovery').waitFor({ timeout: 5000 });
    assert.deepEqual(errors.flat(), [], 'no console errors');
    await third.context.close();
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

  async 'first-take'({ browser, open }) {
    const { page, consoleErrors, context } = await boot(browser, open);
    await emptyEngine(page, 48000);
    await page.evaluate(() => window.__lf.autosave.ready());
    // A first take records on lane 1: nothing is committed, the snapshot is empty.
    await emit(page, { events: [laneEvent(0, lane('Recording', 0))] });
    await page.waitForTimeout(600);
    assert.equal(await slot(page, 'latest'), null, 'a take in flight saves nothing');

    // The device's stop punches the take out and commits it, but its feed frame is withheld: the lane
    // here still reads RECORDING. What the recovery holds is read as the forced open is asked for.
    const take = await page.evaluate(async () => {
      const native = window.__lf.native;
      const { bytes, sigs } = await window.__probe.snapshot(48000, 16, [0]);
      const close = native.close;
      native.close = async () => {
        await close.call(native);
        native.snapshotBytes = bytes;
      };
      const openDevice = native.open;
      native.open = (request, force = false) => {
        if (force) window.__atForcedOpen = window.__probe.slot('latest');
        return openDevice.call(native, request, force);
      };
      native.refusal = { device: 'Probe Interface', from: 48000, to: 44100 };
      window.__lf.ui.openSettings();
      return sigs;
    });
    const buffer = page.getByRole('combobox', { name: 'Buffer size in frames' });
    const pick = (await buffer.inputValue()) === '256' ? '512' : '256';
    await page.evaluate(() => (window.__lfEngineFakeRate = 44100));
    page.once('dialog', (d) => void d.accept());
    await buffer.selectOption(pick);
    await page.waitForFunction(() => window.__lf.native.forced.at(-1) === true, undefined, { timeout: 10000 });
    const atForced = await page.evaluate(() => window.__atForcedOpen);
    console.log('the recovery as the switch was forced:', JSON.stringify(atForced));
    assert.deepEqual([atForced?.rate, atForced?.stems], [48000, take], 'the committed take was saved before the forced open');

    // The new engine at 44.1 kHz starts empty; the recovery keeps the take.
    await page.evaluate(() => (window.__lf.native.refusal = null));
    await emptyEngine(page, 44100);
    await settle(page);
    assert.deepEqual((await slot(page, 'latest'))?.stems, take, 'the switch kept the take in recovery');
    assert.deepEqual(consoleErrors, []);
    await context.close();
  },

  async failed({ browser, open }) {
    const { page, consoleErrors, context } = await boot(browser, open);
    await emptyEngine(page, 48000);
    const jam = await commitJam(page, 48000, 17);
    const before = await saved(page, jam, 'the jam');
    await page.evaluate(() => window.__lf.ui.openSettings());
    const buffer = page.getByRole('combobox', { name: 'Buffer size in frames' });
    const running = Number(await buffer.inputValue());
    // The picks name a device at 44.1 kHz for any other buffer: the engine refuses it while it holds the
    // loops (never the device that runs), and the recovery cannot save.
    await page.evaluate((keep) => {
      const native = window.__lf.native;
      const openDevice = native.open;
      native.open = (request, force = false) => {
        const rate = request.buffer === keep ? 48000 : 44100;
        window.__lfEngineFakeRate = rate;
        native.refusal = rate === 48000 ? null : { device: 'Probe Interface', from: 48000, to: rate };
        return openDevice.call(native, request, force);
      };
      const put = IDBObjectStore.prototype.put;
      IDBObjectStore.prototype.put = function (...args) {
        if (this.name === 'recovery') throw new DOMException('Injected storage quota failure', 'QuotaExceededError');
        return put.apply(this, args);
      };
    }, running);
    const others = ['256', '512', '1024'].filter((v) => v !== String(running));
    /** What the fake runs, and what the store shows. */
    const device = () =>
      page.evaluate(async () => {
        const { engineDevice } = await import('/src/ui/state/engine-store.ts');
        const shown = engineDevice();
        const runs = await window.__lf.native.status();
        return { runs: runs && [runs.sampleRate, runs.block], shown: shown && [shown.sampleRate, shown.block] };
      });

    for (const pick of others) {
      page.once('dialog', (d) => void d.accept());
      const opens = await page.evaluate(() => window.__lf.native.opened.length);
      await buffer.selectOption(pick);
      // Refused, then the reopen after the failed save.
      await page.waitForFunction((n) => window.__lf.native.opened.length >= n + 2, opens, { timeout: 10000 }).catch(() => null);
      await page.waitForTimeout(300);
      const last = await page.evaluate(() => ({ request: window.__lf.native.opened.at(-1), forced: window.__lf.native.forced.at(-1) }));
      const now = await device();
      console.log(`switch to ${pick}: reopened ${JSON.stringify(last)}, device ${JSON.stringify(now)}`);
      assert.deepEqual([last.request?.buffer, last.forced], [running, false], `switch to ${pick}: the device that ran reopens`);
      assert.deepEqual(now, { runs: [48000, running], shown: [48000, running] }, `switch to ${pick}: it runs, and the store shows it`);
      assert.equal(await page.evaluate(() => JSON.parse(localStorage.getItem('lf.audioDevices')).bufferFrames), running, 'its picks are back');
    }
    assert.deepEqual(await slot(page, 'latest'), before, 'the jam stays in recovery');
    const expected = consoleErrors.filter((e) => !e.includes('the recovery save before a rate change failed'));
    assert.deepEqual(expected, [], 'no other console errors');
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

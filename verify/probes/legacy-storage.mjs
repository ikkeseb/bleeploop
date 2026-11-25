/**
 * An install from before the guitar-first rule boots on its own stored keys and keeps them: the page opens
 * over a real older install's localStorage (`OpenOptions.storage`, not the harness's synthetic returning
 * key), in engine mode on the web engine fake (the engine-seam pattern: `window.__lfEngineFake` set before
 * the app loads, a fresh engine's reset frame emitted through `__lf.native`), each case in a browser context
 * of its own:
 *
 * - full older install: `OLDER_INSTALL`, every `lf.` key the app writes with a non-default value (surveyed
 *   from `src/`). After the boot settles, and again after a reload, every seeded key holds its exact text,
 *   only the new OTT switch is added (off), chain mode's `lf.rig` is never read (as in each case out of
 *   chain mode: a `getItem` spy), and the screen is the returning one: no amp-sim ask,
 *   slot B not folded, slot A Off and slot B on Pad as stored, slot A's input on the stored In 2 (and the
 *   device opened with the stored picks), the keyboard at the top with the stored weights, no start line
 *   on the empty looper;
 * - minimal older install: `lf.masterVolume` alone. Still returning: `lf.guitarFirst` never written, Lead and
 *   Bass, both inputs Auto, the keyboard at the bottom, no start line. The boot adds exactly `BOOT_ADDS`,
 *   what it stores on any install that kept none of it: the engine's applied meter and input sends;
 * - fresh install (empty storage), for contrast: `lf.guitarFirst` is written and the guitar-first first
 *   screen shows, slot A's input on In 1 (the device opened with it, slot B Auto), so the probe tells the
 *   two apart.
 *
 * - full older install in chain mode (DEV, `window.__lfChainFake` beside the engine fake; contract §6), the
 *   native rig record `absent` (the rig fake, `window.__lfRigFake`), with RING MOD off at its defaults and
 *   bound to no pedal (the first `bindings` report, scripted: the fake's subscribe sends none, names only a
 *   REC/DUB pedal): after the boot settles (`rigRecallDone`), no v0.8.9 key is changed or removed and no
 *   `lf.rig` written; the page opens the native record and imports the upgrade once (read from the fake's
 *   `calls`: slot A the remembered amp-sim, unscanned in the browser build, at its saved level, with the old
 *   sends waiting in its `pending`: ECHO on, REVERB bypassed, no RING MOD; slot B the stored Pad as a SYNTH
 *   unit at unity, its instrument levels from `lf.synthGain.*`, nothing pending, as no OTT was ever stored);
 *   the legacy sends go to the engine off and nothing else of them, and the app never writes an
 *   `lf.inputSend.*` key (a `localStorage` spy counts every write a module under `src/` makes). A relaunch
 *   over the record the import left (the fake scripted `present`) opens it and imports nothing;
 * - a legacy page with the rig host available (the fake's `available: true`, no chain fake) never calls
 *   `rig.open` (the record is chain mode's alone).
 *
 * Not booted: the plugin host (the browser build has none), so the device prune, the ASIO probe and legacy
 * rig recall, which run only with it, leave `lf.audioDevices` and `lf.rigRecall` alone here; `audio-settings-
 * startup` and `rig-recall` cover those (and `rig-recall` the chain-mode handover's edges). `lf.rigRecallInFlight` is not in the fixture: it is stored from a
 * recall's first load until the loads have settled, so an install holds it at boot only after a launch
 * that died or was stopped while restoring, which the next boot answers by forgetting the record (the
 * `rig-recall` probe's crash phase). Cannot see WebView2's storage or the native engine.
 * Run: pnpm probe legacy-storage
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

/** A unit record without its minted id. */
const withoutId = (u) => Object.fromEntries(Object.entries(u).filter(([k]) => k !== 'id'));

const RATE = 48000;
const AMP_SIM = { format: 'vst3', path: 'C:\\Program Files\\Common Files\\VST3\\Amp Sim.vst3', id: 'com.example.ampsim', name: 'Amp Sim' };
const DELAY = { format: 'clap', path: 'C:\\Program Files\\Common Files\\CLAP\\Delay.clap', id: 'com.example.delay', name: 'Delay' };
const pluginGain = (slot, desc) => `lf.pluginGain.${slot}.${JSON.stringify([desc.format, desc.path, desc.id])}`;

/** What an install from before the guitar-first rule stores, each value as the app writes it and none its
 * default: a guitarist's rig, the amp-sim in slot A over Off, a pad in slot B, the keyboard moved to the
 * top and resized. */
const OLDER_INSTALL = {
  // layout-store.ts `persist`
  'lf.layout': JSON.stringify({ stageSizes: { keyboard: 0.8, looper: 1.6 }, keyboardPlacement: 'top', lastVisiblePlacement: 'top' }),
  // audio-settings.ts `writeAudioDeviceSettings`, its fields in the order it writes them
  'lf.audioDevices': JSON.stringify({
    inputDeviceId: 'in:Focusrite USB', inputChannel: '1', slotInputChannels: ['1', '0'], outputDeviceId: 'out:Focusrite USB',
    bufferFrames: 128, sampleRate: 48000, asioEnabled: true, asioDriver: 'Focusrite USB ASIO', shareDeviceId: '',
  }),
  // instrument.ts: `sourceKey`, `inputGainKey`, `synthGainKey`, `pluginGainKey`
  'lf.slotSource.0': 'off',
  'lf.slotSource.1': 'pad',
  'lf.slotInputGain.0': '1.2',
  'lf.slotInputGain.1': '0.8',
  'lf.synthGain.lead': '0.5',
  'lf.synthGain.bass': '0.7',
  'lf.synthGain.pad': '0.6',
  'lf.synthGain.piano': '0.9',
  'lf.synthGain.organ': '0.4',
  'lf.synthGain.drum': '1.1',
  [pluginGain(0, AMP_SIM)]: '0.75',
  [pluginGain(1, DELAY)]: '0.65',
  // rig-recall.ts `writeRig`
  'lf.rigRecall': JSON.stringify([AMP_SIM, null]),
  // wave-looks.ts: the wave's look (the I key)
  'lf.waveLook': 'dense',
  // engine-store.ts: the master and click levels, FADE's bars, the meter (3/4), the input sends
  'lf.masterVolume': '0.8',
  'lf.clickVolume': '0.45',
  'lf.fadeBars': '4',
  'lf.meter': '1',
  'lf.inputSend.echo': '1',
  'lf.inputSend.echoTime': '2',
  'lf.inputSend.echoFeedback': '0.55',
  'lf.inputSend.echoLevel': '0.35',
  'lf.inputSend.reverb': '0',
  'lf.inputSend.reverbLevel': '0.6',
  'lf.inputSend.ring': '0',
  'lf.inputSend.ringFreq': '220',
  'lf.inputSend.ringLevel': '0.3',
  // stage-store.ts: the stage view's second look
  'lf.stageView': '1',
  // the web build's learned MIDI, kept until native MIDI's store has it (midi-actions.ts)
  'lf.midiLearn': JSON.stringify([{ port: 'input-1', portName: 'FS-6 Pedal', channel: 0, kind: 'cc', number: 64, action: 'recDub', pressHigh: true, momentary: true }]),
};

const MINIMAL_INSTALL = { 'lf.masterVolume': '0.8' };

/** The full older install with RING MOD off at its defaults (`engine-store.ts` `INPUT_SEND_PARAMS`): bound
 * to no pedal, it is the send §6 does not migrate. */
const CHAIN_INSTALL = { ...OLDER_INSTALL, 'lf.inputSend.ringFreq': '440', 'lf.inputSend.ringLevel': '0.5' };
/** Native MIDI's first `bindings` report for it: one REC/DUB pedal, no IN FX pedal. */
const BINDINGS = { bindings: { revision: 1, bindings: [{
  binding: { portId: 'winmm:0:FS-6 Pedal', portName: 'FS-6 Pedal', channel: 0, kind: 'cc', number: 64, action: 'recDub', target: null, pressHigh: true, momentary: true, hold: false },
  origin: 'native', ordinal: false, blocked: null, displayName: 'FS-6 Pedal', state: 'live',
}] } };


/** What every boot over an engine that remembers nothing stores when the install kept none of it, the
 * branch's rule aside (`src/ui/state/engine-store.ts`): the meter the reset frame's `Transport` shows
 * (`applyEvent`, no kept meter to ask for), and each input send's state as the engine's `Toggled` answers
 * the boot's own `SetInputSend` (`adoptSettings` sends what the engine lacks; `applyToggle` keeps it). */
const BOOT_ADDS = { 'lf.inputSend.echo': '0', 'lf.inputSend.reverb': '0', 'lf.inputSend.ring': '0', 'lf.inputSend.ott': '0', 'lf.meter': '0' };

await probe(async ({ browser, open }) => {
  let seq = 0;
  const emit = (page, frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const emptyLane = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };
  /** A fresh engine's first frame: no settings remembered, every lane EMPTY, 4/4 at 120. */
  const freshEngine = (page) => emit(page, {
    reset: true,
    settings: [],
    events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, ...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: emptyLane } })), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: [0, 0], clip: [false, false] },
  });
  /** The page's engine runs, a fresh engine's reset frame has landed and recovery has started. */
  const boot = async (page) => {
    await page.waitForFunction(() => '__lf' in window && window.__lf.native.opened.length === 1, undefined, { timeout: 30_000 });
    seq = 0;
    await freshEngine(page);
    await page.evaluate(() => window.__lf.autosave.ready());
    await page.waitForTimeout(500);
  };
  /** `boot` in chain mode: native MIDI's first `bindings` report once the page subscribed, then the fresh
   * engine's reset frame (its `chains` the fake's own, empty), and the rig recall settled. */
  const chainBoot = async (page) => {
    await page.waitForFunction(() => '__lf' in window && window.__lf.native.opened.length === 1 && window.__lf.native.midiCalls.some(([c]) => c === 'subscribe'), undefined, { timeout: 30_000 });
    await page.evaluate((b) => window.__lf.native.midiEmit(b), BINDINGS);
    seq = 0;
    await freshEngine(page);
    await page.evaluate(async () => {
      await window.__lf.autosave.ready();
      await (await import('/src/ui/state/rig-recall.ts')).rigRecallDone();
    });
    await page.waitForTimeout(500);
  };
  const storage = (page) => page.evaluate(() => Object.fromEntries(Object.keys(localStorage).sort().map((k) => [k, localStorage.getItem(k)])));
  /** What the screen shows of the guitar-first rule. */
  const look = (page) => page.evaluate(() => {
    const inA = document.querySelector('[aria-label="Input for slot A"]');
    const zone = (sel) => document.querySelector(sel)?.getBoundingClientRect();
    const kb = zone('.zone--keyboard'), loop = zone('.zone--looper');
    return {
      placement: window.__lf.layoutStore.keyboardPlacement(),
      stageSizes: window.__lf.layoutStore.stageSizes(),
      keyboardAbove: kb && loop && kb.height > 0 ? kb.bottom <= loop.top + 1 : null,
      keyboardBelow: kb && loop && kb.height > 0 ? kb.top >= loop.bottom - 1 : null,
      slots: [...document.querySelectorAll('.slot')].map((el) => ({
        picker: el.querySelector('.slot__source')?.selectedOptions[0]?.textContent.trim(),
        ask: !!el.querySelector('.slot__source--ask'),
        folded: el.classList.contains('slot--folded'),
      })),
      start: [...document.querySelectorAll('.lp-lane')].map((l) => l.querySelector('.lp-lane__start')?.textContent.trim() ?? ''),
      // slot A's input pick (null while its source takes no input), and each slot's channel the device opened with
      inputA: inA ? { value: inA.value, text: inA.selectedOptions[0]?.textContent.trim() } : null,
      asked: window.__lf.native.opened.at(-1)?.inputChannels ?? null,
    };
  });
  /** Opens the app in a fresh context over `seeded` (null: an empty storage, a fresh install). */
  const openOver = async (seeded) => {
    const context = await browser.newContext();
    const app = await open({
      context,
      viewport: { width: 1280, height: 820 },
      ...(seeded ? { storage: seeded } : { firstLaunch: true }),
      init: (p) => p.addInitScript(() => {
        window.__lfEngineFake = true;
        // Out of chain mode nothing reads the rig record (the release is as before it).
        window.__rigReads = 0;
        const real = Storage.prototype.getItem;
        Storage.prototype.getItem = function (key) {
          if (this === window.localStorage && key === 'lf.rig') window.__rigReads++;
          return real.call(this, key);
        };
      }),
    });
    await boot(app.page);
    return { context, page: app.page };
  };
  /** Nothing read `lf.rig` on this page (the chain mode's record; `openOver`'s spy). */
  const assertNoRigRead = async (page, when) =>
    assert.equal(await page.evaluate(() => window.__rigReads), 0, `${when}: lf.rig was read out of chain mode`);
  /** Every seeded key holds its exact text, and the keys beyond the seed are exactly `added`. */
  const assertKept = (stored, seeded, added, when) => {
    for (const [k, v] of Object.entries(seeded)) assert.equal(stored[k], v, `${when}: ${k} was rewritten`);
    const extra = Object.fromEntries(Object.entries(stored).filter(([k]) => !(k in seeded)));
    assert.deepEqual(extra, added, `${when}: keys the boot added`);
  };
  const assertReturning = (l, [a, b], when) => {
    assert.deepEqual(l.slots.map((s) => s.picker), [a, b], `${when}: the slot pickers`);
    assert.ok(l.slots.every((s) => !s.ask && !s.folded), `${when}: a slot asks or folds: ${JSON.stringify(l.slots)}`);
    assert.ok(l.start.every((t) => t === ''), `${when}: the start line shows: ${JSON.stringify(l.start)}`);
  };

  // a) A full older install: every key kept, the returning screen as it stored it, across a reload too.
  {
    const { context, page } = await openOver(OLDER_INSTALL);
    for (const when of ['full older install, booted', 'full older install, reloaded']) {
      if (when.endsWith('reloaded')) {
        await page.reload();
        await boot(page);
      }
      const stored = await storage(page);
      assert.equal(stored['lf.guitarFirst'], undefined, `${when}: lf.guitarFirst was written`);
      assertKept(stored, OLDER_INSTALL, { 'lf.inputSend.ott': '0' }, when);
      await assertNoRigRead(page, when);
      const l = await look(page);
      assertReturning(l, ['Off', 'Pad'], when);
      assert.equal(l.placement, 'top', `${when}: the keyboard placement`);
      assert.equal(l.keyboardAbove, true, `${when}: the keyboard is not drawn above the looper: ${JSON.stringify(l)}`);
      assert.deepEqual(l.stageSizes, { keyboard: 0.8, looper: 1.6 }, `${when}: the stage weights`);
      assert.deepEqual(l.inputA, { value: '1', text: 'In 2' }, `${when}: slot A's input pick`);
      assert.deepEqual(l.asked, [1, 0], `${when}: the channels the device opened with`);
      console.log(`${when}: ${Object.keys(stored).length} keys kept, ${JSON.stringify(l.slots)}, keyboard ${l.placement}`);
    }
    await context.close();
  }

  // b) A minimal older install: one key is enough to keep today's defaults.
  {
    const { context, page } = await openOver(MINIMAL_INSTALL);
    const when = 'minimal older install';
    const stored = await storage(page);
    assert.equal(stored['lf.guitarFirst'], undefined, `${when}: lf.guitarFirst was written`);
    assertKept(stored, MINIMAL_INSTALL, BOOT_ADDS, when);
    await assertNoRigRead(page, when);
    const l = await look(page);
    assertReturning(l, ['Lead', 'Bass'], when);
    assert.equal(l.placement, 'bottom', `${when}: the keyboard placement`);
    assert.equal(l.keyboardBelow, true, `${when}: the keyboard is not drawn below the looper: ${JSON.stringify(l)}`);
    assert.deepEqual(l.asked, [null, null], `${when}: the channels the device opened with (both Auto)`);
    console.log(`${when}: ${JSON.stringify(stored)}, ${JSON.stringify(l.slots)}, keyboard ${l.placement}`);
    await context.close();
  }

  // c) A fresh install, for contrast: the marker is written and the guitar-first first screen shows.
  {
    const { context, page } = await openOver(null);
    const when = 'fresh install';
    const stored = await storage(page);
    assert.equal(stored['lf.guitarFirst'], '1', `${when}: the guitar-first marker`);
    await assertNoRigRead(page, when);
    const l = await look(page);
    assert.deepEqual(l.slots, [
      { picker: 'Pick your amp-sim', ask: true, folded: false },
      { picker: 'Add a second sound', ask: true, folded: true },
    ], `${when}: the slots`);
    assert.equal(l.placement, 'hidden', `${when}: the keyboard placement`);
    assert.deepEqual(l.start, ['SPACE or your pedal records here', '', '', '', ''], `${when}: the start line`);
    assert.deepEqual(l.inputA, { value: '0', text: 'In 1' }, `${when}: slot A's input pick`);
    assert.deepEqual(l.asked, [0, null], `${when}: the channels the device opened with (slot A on In 1, slot B Auto)`);
    console.log(`${when}: ${JSON.stringify(stored)}, ${JSON.stringify(l.slots)}, keyboard ${l.placement}`);
    await context.close();
  }

  // d) The full older install in DEV chain mode (contract §6), the native rig record `absent` (the rig fake,
  // `__lfRigFake`, scripted per launch through `rigScript` in sessionStorage, as that launch's process holds
  // the record): every v0.8.9 key kept, no `lf.rig`, one import of the upgrade, the legacy sends silent and
  // their keys never written; a relaunch over the record the import left opens it and imports nothing.
  {
    const context = await browser.newContext();
    const app = await open({
      context,
      viewport: { width: 1280, height: 820 },
      storage: CHAIN_INSTALL,
      init: (p) => p.addInitScript(() => {
        Object.assign(window, { __lfEngineFake: true, __lfChainFake: true, __lsWrites: [] });
        window.__lfRigFake = JSON.parse(sessionStorage.getItem('rigScript') ?? '{}');
        // Every write or removal a module of the app makes (its stack names `/src/`), not the harness's seed.
        for (const method of ['setItem', 'removeItem']) {
          const real = Storage.prototype[method];
          Storage.prototype[method] = function (key, ...rest) {
            if (this === window.localStorage && /\/src\//.test(new Error().stack ?? '')) window.__lsWrites.push(`${method} ${key}`);
            return real.call(this, key, ...rest);
          };
        }
      }),
    });
    const { page } = app;
    const pad = await page.evaluate(async () => (await import('/src/ui/state/instruments.ts')).SYNTHS.findIndex((s) => s.id === 'pad'));
    for (const when of ['chain mode, booted', 'chain mode, relaunched']) {
      if (when.endsWith('relaunched')) await page.reload();
      await chainBoot(page);
      const stored = await storage(page);
      // Ordinary boot writes may add keys (the meter, a layout); §6's rule is the old keys, kept as they are.
      for (const [k, v] of Object.entries(CHAIN_INSTALL)) assert.equal(stored[k], v, `${when}: ${k} was changed or removed`);
      assert.equal(stored['lf.rig'], undefined, `${when}: lf.rig was written (the record is native's)`);
      assert.deepEqual(Object.keys(stored).filter((k) => k.startsWith('lf.inputSend.') && !(k in CHAIN_INSTALL)), [], `${when}: an lf.inputSend key was added`);
      const writes = await page.evaluate(() => window.__lsWrites);
      assert.deepEqual(writes.filter((w) => /^setItem lf\.rig$/.test(w) || / lf\.(inputSend\.|rigRecall)/.test(w)), [], `${when}: the app wrote lf.rig, a legacy send or a recall key`);
      const rig = await page.evaluate(() => {
        const fake = window.__lf.native.rig;
        return { calls: fake.calls, status: fake.status, volumes: fake.volumes };
      });
      console.log(`${when}: rig calls ${JSON.stringify(rig.calls)}; writes ${JSON.stringify(writes)}`);
      const legacySends = await page.evaluate(() => window.__lf.native.sent.filter((c) => 'SetInputSend' in c || 'SetInputSendParam' in c));
      assert.deepEqual(legacySends.filter((c) => !('SetInputSend' in c && c.SetInputSend[1] === false)), [], `${when}: a legacy send went on or took a value`);
      assert.deepEqual([...new Set(legacySends.map((c) => c.SetInputSend[0]))].sort(), ['echo', 'ott', 'reverb', 'ring'], `${when}: the four legacy sends are sent off`);
      if (when.endsWith('relaunched')) {
        assert.deepEqual(rig.calls, [['open']], `${when}: the relaunch opens the record and imports nothing (no second upgrade)`);
        continue;
      }
      assert.deepEqual(rig.calls.map(([name]) => name), ['open', 'import'], `${when}: open, then one import`);
      // The whole payload, ids aside, equals the upgrade: no unit more or less in any list, no field changed.
      const payload = JSON.parse(rig.calls[1][1]);
      const units = payload.slots.flatMap((s) => [...(s.units ?? []), ...(s.pending ?? [])]);
      const unId = (s) => Object.fromEntries(Object.entries(s).map(([k, v]) => [k, k === 'units' || k === 'pending' ? v.map(withoutId) : v]));
      assert.deepEqual({ ...payload, slots: payload.slots.map(unId) }, { version: 1, slots: [
        // slot A: the remembered amp-sim at its saved plugin level; pending ECHO on, REVERB bypassed, RING MOD
        // (off, at its defaults, unbound) not migrated
        { volume: 0.75, units: [{ kind: 'plugin', plugin: AMP_SIM, bypass: false }], pending: [
          { kind: 'echo', params: { echoTime: 2, echoFeedback: 0.55, echoLevel: 0.35 }, bypass: false },
          { kind: 'reverb', params: { reverbLevel: 0.6 }, bypass: true },
        ] },
        // slot B: the stored Pad at unity, its levels from `lf.synthGain.*`, nothing pending (no OTT stored)
        { volume: 1, units: [{ kind: 'synth', bypass: false, params: {
          instrument: pad, gainLead: 0.5, gainBass: 0.7, gainPad: 0.6, gainPiano: 0.9, gainOrgan: 0.4, gainDrum: 1.1 } }] },
      ] }, `${when}: the import is the upgrade`);
      const ids = units.map((u) => u.id);
      assert.ok(ids.every((id) => Number.isInteger(id) && id > 0), `${when}: a unit without its id: ${JSON.stringify(ids)}`);
      assert.equal(new Set(ids).size, ids.length, `${when}: each unit has its own id`);
      // The next launch's process holds the record the import left.
      await page.evaluate((r) => sessionStorage.setItem('rigScript', JSON.stringify({ status: r.status, volumes: r.volumes })), rig);
    }
    await context.close();
  }

  // e) A legacy page (no chain mode) with the rig host available (the fake's `available`, apart from the chain
  // fake): boot's chain-mode gate alone keeps it from opening the native rig record.
  {
    const context = await browser.newContext();
    const app = await open({
      context,
      viewport: { width: 1280, height: 820 },
      storage: OLDER_INSTALL,
      init: (p) => p.addInitScript(() => Object.assign(window, { __lfEngineFake: true, __lfRigFake: { available: true } })),
    });
    await boot(app.page);
    const { available, calls } = await app.page.evaluate(() => ({ available: window.__lf.native.rig.available, calls: window.__lf.native.rig.calls }));
    assert.equal(available, true, 'the legacy page\'s rig host is available (else the case proves nothing)');
    assert.deepEqual(calls, [], 'a legacy page called the native rig record');
    console.log(`legacy page with the rig fake: rig calls ${JSON.stringify(calls)}`);
    await context.close();
  }
});

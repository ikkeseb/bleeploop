/**
 * An install from before the guitar-first rule boots on its own stored keys and keeps them: the page opens
 * over a real older install's localStorage (`OpenOptions.storage`, not the harness's synthetic returning
 * key), in engine mode on the web engine fake (the engine-seam pattern: `window.__lfEngineFake` set before
 * the app loads, a fresh engine's reset frame emitted through `__lf.native`), each case in a browser context
 * of its own:
 *
 * - full older install: `OLDER_INSTALL`, every `lf.` key the app writes with a non-default value (surveyed
 *   from `src/`). After the boot settles, and again after a reload, every seeded key holds its exact text,
 *   no `lf.guitarFirst` exists, nothing else was added, and the screen is the returning one: no amp-sim ask,
 *   slot B not folded, slot A Off and slot B on Pad as stored, the keyboard at the top with the stored
 *   weights, no start line on the empty looper;
 * - minimal older install: `lf.masterVolume` alone. Still returning: `lf.guitarFirst` never written, Lead and
 *   Bass, the keyboard at the bottom, no start line. The boot adds exactly `BOOT_ADDS`, what it stores on
 *   any install that kept none of it: the engine's applied meter and input sends;
 * - fresh install (empty storage), for contrast: `lf.guitarFirst` is written and the guitar-first first
 *   screen shows, so the probe tells the two apart.
 *
 * Not booted: the plugin host (the browser build has none), so the device prune, the ASIO probe and rig
 * recall, which run only with it, leave `lf.audioDevices` and `lf.rigRecall` alone here; `audio-settings-
 * startup` and `rig-recall` cover those. `lf.rigRecallInFlight` is not in the fixture: it is stored from a
 * recall's first load until the loads have settled, so an install holds it at boot only after a launch
 * that died or was stopped while restoring, which the next boot answers by forgetting the record (the
 * `rig-recall` probe's crash phase). Cannot see WebView2's storage or the native engine.
 * Run: pnpm probe legacy-storage
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

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

/** What every boot over an engine that remembers nothing stores when the install kept none of it, the
 * branch's rule aside (`src/ui/state/engine-store.ts`): the meter the reset frame's `Transport` shows
 * (`applyEvent`, no kept meter to ask for), and each input send's state as the engine's `Toggled` answers
 * the boot's own `SetInputSend` (`adoptSettings` sends what the engine lacks; `applyToggle` keeps it). */
const BOOT_ADDS = { 'lf.inputSend.echo': '0', 'lf.inputSend.reverb': '0', 'lf.inputSend.ring': '0', 'lf.meter': '0' };

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
  const storage = (page) => page.evaluate(() => Object.fromEntries(Object.keys(localStorage).sort().map((k) => [k, localStorage.getItem(k)])));
  /** What the screen shows of the guitar-first rule. */
  const look = (page) => page.evaluate(() => {
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
    };
  });
  /** Opens the app in a fresh context over `seeded` (null: an empty storage, a fresh install). */
  const openOver = async (seeded) => {
    const context = await browser.newContext();
    const app = await open({
      context,
      viewport: { width: 1280, height: 820 },
      ...(seeded ? { storage: seeded } : { firstLaunch: true }),
      init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
    });
    await boot(app.page);
    return { context, page: app.page };
  };
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
      assertKept(stored, OLDER_INSTALL, {}, when);
      const l = await look(page);
      assertReturning(l, ['Off', 'Pad'], when);
      assert.equal(l.placement, 'top', `${when}: the keyboard placement`);
      assert.equal(l.keyboardAbove, true, `${when}: the keyboard is not drawn above the looper: ${JSON.stringify(l)}`);
      assert.deepEqual(l.stageSizes, { keyboard: 0.8, looper: 1.6 }, `${when}: the stage weights`);
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
    const l = await look(page);
    assertReturning(l, ['Lead', 'Bass'], when);
    assert.equal(l.placement, 'bottom', `${when}: the keyboard placement`);
    assert.equal(l.keyboardBelow, true, `${when}: the keyboard is not drawn below the looper: ${JSON.stringify(l)}`);
    console.log(`${when}: ${JSON.stringify(stored)}, ${JSON.stringify(l.slots)}, keyboard ${l.placement}`);
    await context.close();
  }

  // c) A fresh install, for contrast: the marker is written and the guitar-first first screen shows.
  {
    const { context, page } = await openOver(null);
    const when = 'fresh install';
    const stored = await storage(page);
    assert.equal(stored['lf.guitarFirst'], '1', `${when}: the guitar-first marker`);
    const l = await look(page);
    assert.deepEqual(l.slots, [
      { picker: 'Pick your amp-sim', ask: true, folded: false },
      { picker: 'Add a second sound', ask: true, folded: true },
    ], `${when}: the slots`);
    assert.equal(l.placement, 'hidden', `${when}: the keyboard placement`);
    assert.deepEqual(l.start, ['SPACE or your pedal records here', '', '', '', ''], `${when}: the start line`);
    console.log(`${when}: ${JSON.stringify(stored)}, ${JSON.stringify(l.slots)}, keyboard ${l.placement}`);
    await context.close();
  }
});

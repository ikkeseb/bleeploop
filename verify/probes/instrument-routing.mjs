// Browser probe for the instrument routing seam: a failed unload aborts a plugin swap or clear and
// keeps the plugin, a swap stays silent (no synth built), picking a source moves the MIDI/keys slot
// (effects and failed unloads excepted), and host-side sustain defers plugin note-offs.
// Drives the production instrument module and MIDI parser with an instrumented host and a virtual Web
// MIDI port; it makes no native unload, plugin-audio or latency claim.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });

try {
  const page = await browser.newPage();
  await page.route('**/src/platform/host.web.ts', async (route) => {
    const response = await route.fetch();
    const body = (await response.text()).replace('available: false', 'available: true');
    await route.fulfill({ response, body });
  });
  await page.addInitScript(() => {
    const inputs = new Map([['a', { id: 'a', name: 'Probe a', state: 'connected', onmidimessage: null }]]);
    const access = { inputs, onstatechange: null };
    window.__probeMidi = access;
    Object.defineProperty(navigator, 'requestMIDIAccess', { configurable: true, value: async () => access });
  });
  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf && !!window.__probeMidi.inputs.get('a').onmidimessage);

  const result = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/audio/instrument.ts');
    const slots = await import('/src/audio/instrument-slots.ts');
    const { toasts } = await import('/src/notify.ts');
    const { SYNTHS } = await import('/src/audio/synths/index.ts');
    const lf = window.__lf;
    await lf.engine.start();
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const idle = async () => { while (slots.slotPendingCounts().some((n) => n > 0)) await pause(10); };
    const send = (bytes) => window.__probeMidi.inputs.get('a').onmidimessage({ data: Uint8Array.from(bytes) });
    const desc = (id, isEffect) => ({ id, name: `Probe ${id}`, path: `C:\\probe\\${id}.vst3`, format: 'vst3', isEffect });
    const [instA, instB, fx] = [desc('a', false), desc('b', false), desc('fx', true)];

    // Count SynthEngine builds/disposals through the production factories.
    const engines = { created: 0, disposed: 0 };
    for (const factory of SYNTHS) {
      const create = factory.create;
      factory.create = () => {
        const synth = create();
        const dispose = synth.dispose.bind(synth);
        engines.created++;
        synth.dispose = () => { engines.disposed++; dispose(); };
        return synth;
      };
    }
    const live = () => engines.created - engines.disposed;

    const calls = [];
    let unloadFailures = 0;
    let routedDuringUnload;
    platform.pluginHost.available = true;
    platform.pluginHost.scanPlugins = async () => [instA, instB, fx];
    platform.pluginHost.loadPlugin = async (slot, path, id) => { calls.push(['load', slot, id]); };
    platform.pluginHost.unloadPlugin = async (slot) => {
      calls.push(['unload', slot]);
      routedDuringUnload = lf.inputRouter.activeId;
      if (unloadFailures > 0) { unloadFailures--; throw new Error('injected unload failure'); }
    };
    platform.pluginHost.noteOn = async (slot, note) => { calls.push(['on', slot, note]); };
    platform.pluginHost.noteOff = async (slot, note) => { calls.push(['off', slot, note]); };
    slots.setNativeHostReady(true);
    const out = {};

    // D13: a synth pick in the inactive slot makes it the MIDI slot, and a MIDI note sounds there.
    instrument.selectSynth(0, 'organ'); instrument.setActiveSlot(0); await idle();
    instrument.selectSynth(1, 'bass'); await idle();
    const analyser = lf.engine.ctx.createAnalyser(); analyser.fftSize = 4096;
    lf.engine.instrumentBus.connect(analyser);
    const data = new Float32Array(analyser.fftSize);
    const rms = () => {
      analyser.getFloatTimeDomainData(data);
      return Math.sqrt(data.reduce((sum, sample) => sum + sample * sample, 0) / data.length);
    };
    send([0x90, 45, 110]); await pause(150);
    out.synthPick = { active: instrument.activeSlot(), engine: lf.inputRouter.activeId, rms: rms() };
    send([0x80, 45, 0]);
    lf.engine.instrumentBus.disconnect(analyser);

    // D13: an effect load keeps routing; an instrument load moves it.
    instrument.setActiveSlot(0);
    await instrument.selectPlugin(1, fx);
    out.afterEffect = { active: instrument.activeSlot(), slot1: instrument.slotPlugins()[1]?.id };
    await instrument.selectPlugin(1, instA);
    out.afterInstrument = { active: instrument.activeSlot(), slot1: instrument.slotPlugins()[1]?.id };

    // A6: a rejected unload aborts the swap (old plugin kept, no load, toast); a retry completes.
    // An active-slot swap routes nowhere while the unload runs and builds no synth engine.
    const createdBeforeSwaps = engines.created;
    calls.length = 0; unloadFailures = 1;
    await instrument.selectPlugin(1, instB);
    out.failedSwap = {
      slot1: instrument.slotPlugins()[1]?.id,
      calls: calls.map((c) => c.join(':')),
      toast: toasts().find((t) => t.message === 'Plugin unload failed')?.detail,
      routed: routedDuringUnload,
    };
    calls.length = 0;
    await instrument.selectPlugin(1, instB);
    out.retrySwap = {
      slot1: instrument.slotPlugins()[1]?.id,
      calls: calls.map((c) => c.join(':')),
      routed: routedDuringUnload,
      enginesBuilt: engines.created - createdBeforeSwaps,
    };

    // D12-S: host-side sustain defers the plugin sink's note-off until pedal-up.
    const notes = () => calls.filter((c) => c[0] === 'on' || c[0] === 'off').map((c) => c.join(':'));
    instrument.setActiveSlot(1); await pause(20);
    calls.length = 0;
    send([0xb0, 64, 127]); send([0x90, 60, 100]); send([0x80, 60, 0]); await pause(20);
    out.pedalHeld = notes();
    send([0xb0, 64, 0]); await pause(20);
    out.pedalUp = notes();
    calls.length = 0;
    send([0xb0, 64, 127]); send([0x90, 62, 100]); send([0x80, 62, 0]); send([0x90, 62, 90]); await pause(20);
    out.restrike = notes();
    send([0x80, 62, 0]); send([0xb0, 64, 0]); await pause(20);
    out.restrikeRelease = notes();

    // A pedal-held plugin note is released exactly once when MIDI moves to the other slot.
    calls.length = 0;
    send([0xb0, 64, 127]); send([0x90, 64, 100]); send([0x80, 64, 0]); await pause(20);
    out.switchHeld = notes();
    instrument.setActiveSlot(0);
    send([0xb0, 64, 0]); await pause(20);
    out.switchReleased = notes();

    // A failed unload on CLEAR keeps the plugin (routed to it, no idle synth engine left) ...
    instrument.setActiveSlot(1);
    const liveBeforeClear = live();
    calls.length = 0; unloadFailures = 1;
    await instrument.clearPlugin(1);
    out.failedClear = {
      slot1: instrument.slotPlugins()[1]?.id,
      calls: calls.map((c) => c.join(':')),
      routed: routedDuringUnload,
      engine: lf.inputRouter.activeId,
      liveEngines: live() - liveBeforeClear,
    };
    // ... and a synth pick whose unload fails leaves the MIDI slot where it was.
    instrument.setActiveSlot(0);
    unloadFailures = 1;
    instrument.selectSynth(1, 'lead'); await idle();
    out.failedPick = { slot1: instrument.slotPlugins()[1]?.id, active: instrument.activeSlot() };
    return out;
  });
  console.log(JSON.stringify(result));
  assert.equal(result.synthPick.active, 1, 'a synth pick makes its slot the MIDI slot');
  assert.equal(result.synthPick.engine, 'bass', 'MIDI routes to the picked slot\'s engine');
  assert.ok(result.synthPick.rms > 0.01, 'a MIDI note sounds on the picked slot\'s engine');
  assert.deepEqual(result.afterEffect, { active: 0, slot1: 'fx' }, 'an effect load leaves routing alone');
  assert.deepEqual(result.afterInstrument, { active: 1, slot1: 'a' }, 'an instrument load moves routing');
  assert.deepEqual(result.failedSwap, {
    slot1: 'a', calls: ['unload:1'], routed: null,
    toast: 'The plugin stays in the slot but is silent. Choose none or another plugin to retry.',
  }, 'a failed unload keeps the old plugin, makes no load call and raises the toast naming the way out');
  assert.deepEqual(result.retrySwap, { slot1: 'b', calls: ['unload:1', 'load:1:b'], routed: null, enginesBuilt: 0 },
    'the retry swap completes; neither swap routes to or builds a synth engine');
  assert.deepEqual(result.pedalHeld, ['on:1:60'], 'pedal down defers the plugin note-off');
  assert.deepEqual(result.pedalUp, ['on:1:60', 'off:1:60'], 'pedal up releases exactly once');
  assert.deepEqual(result.restrike, ['on:1:62', 'off:1:62', 'on:1:62'], 'a sustained re-strike sends noteOff before noteOn');
  assert.deepEqual(result.restrikeRelease, ['on:1:62', 'off:1:62', 'on:1:62', 'off:1:62'], 'the re-struck note releases on pedal up');
  // Fails on the code before this check: the switch flush released held notes only, not pedal-sustained ones.
  assert.deepEqual(result.switchHeld, ['on:1:64'], 'pedal down defers the plugin note-off before the switch');
  assert.deepEqual(result.switchReleased, ['on:1:64', 'off:1:64'], 'switching slots releases the sustained note exactly once');
  // Fails on the code before this check: a failed clear left the slot empty on its synth.
  assert.deepEqual(result.failedClear, { slot1: 'b', calls: ['unload:1'], routed: 'bass', engine: null, liveEngines: 0 },
    'a failed clear keeps the plugin, routes back to it and disposes the synth it built meanwhile');
  // Fails on the code before this check: selectSynth activated its slot even when the unload failed.
  assert.deepEqual(result.failedPick, { slot1: 'b', active: 0 }, 'a synth pick whose unload fails does not move the MIDI slot');
  console.log('PASS: failed-unload swap/clear keep the plugin, silent swap, pick-moves-MIDI-slot (effects and failed unloads excepted), host-side plugin sustain');
} finally {
  await browser.close();
}

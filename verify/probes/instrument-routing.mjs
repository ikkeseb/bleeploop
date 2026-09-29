/**
 * The instrument routing seam (`src/ui/state/instrument.ts` over `input-router.ts` and the real MIDI
 * parser), on the web engine fake (`src/platform/host.web.ts`, the engine-seam pattern) with the plugin
 * host on (host.web.ts served with `available: true`), its load and unload instrumented, and a virtual
 * Web MIDI port. What the router plays reaches the engine as commands, read from `__lf.native.sent`:
 *
 * - picking a source moves the MIDI/keys slot: a synth pick in the inactive slot sends its
 *   `SelectInstrument` and a MIDI note then goes there; an effect load leaves routing alone, an
 *   instrument load moves it;
 * - a failed unload aborts a swap (the old plugin kept, no load, the toast naming the way out); the retry
 *   completes; while a swap's unload runs the router has no sink, and neither swap selects a built-in;
 * - host-side sustain on a plugin target: pedal down defers the `NoteOff`, pedal up sends it once, a
 *   sustained re-strike sends `NoteOff` before `NoteOn`; a pedal-held note is released exactly once, before
 *   the new target, when MIDI moves to the other slot;
 * - a failed CLEAR keeps the plugin: the slot's built-in is selected while the unload runs, the plugin
 *   target again once it failed; a synth pick whose unload fails does not move the MIDI slot.
 *
 * Cannot see the native plugin host (load, unload), the engine (what a target plays: lf-engine
 * `tests/slots.rs`) or Tauri IPC.
 * Run: pnpm probe instrument-routing
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open({
    init: async (p) => {
      await p.route('**/src/platform/host.web.ts', async (route) => {
        const response = await route.fetch();
        const body = (await response.text()).replace('available: false', 'available: true');
        await route.fulfill({ response, body });
      });
      await p.addInitScript(() => {
        window.__lfEngineFake = true;
        const inputs = new Map([['a', { id: 'a', name: 'Probe a', state: 'connected', onmidimessage: null }]]);
        const access = { inputs, onstatechange: null };
        window.__probeMidi = access;
        Object.defineProperty(navigator, 'requestMIDIAccess', { configurable: true, value: async () => access });
      });
    },
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1 && !!window.__probeMidi.inputs.get('a').onmidimessage);

  const result = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const slots = await import('/src/ui/state/instrument-slots.ts');
    const { toasts } = await import('/src/notify.ts');
    const lf = window.__lf;
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const deadline = Date.now() + 10000;
    while (!instrument.nativeHostReady() || instrument.scanning()) {
      if (Date.now() > deadline) throw new Error('native host boot did not finish');
      await pause(20);
    }
    const lane = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };
    lf.native.emit({
      seq: 1,
      reset: true,
      settings: [],
      events: [...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane } })),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
      meter: { peak: 0, clip: false },
    });
    const idle = async () => { while (slots.slotPendingCounts().some((n) => n > 0)) await pause(10); };
    const send = (bytes) => window.__probeMidi.inputs.get('a').onmidimessage({ data: Uint8Array.from(bytes) });
    const desc = (id, isEffect) => ({ id, name: `Probe ${id}`, path: `C:\\probe\\${id}.vst3`, format: 'vst3', isEffect });
    const [instA, instB, fx] = [desc('a', false), desc('b', false), desc('fx', true)];

    /** What the engine received since the last call, each command as JSON. */
    let seen = 0;
    const sent = async () => {
      await pause(10); // the outbox flushes on a microtask, the fake records on its async send
      const all = lf.native.sent.slice(seen);
      seen = lf.native.sent.length;
      return all.map((c) => JSON.stringify(c));
    };
    const notes = (commands) => commands.filter((c) => c.startsWith('{"NoteO'));
    const targets = (commands) => commands.filter((c) => c.startsWith('{"SelectInstrument'));

    const calls = [];
    let unloadFailures = 0;
    let routedDuringUnload;
    platform.pluginHost.scanPlugins = async () => [instA, instB, fx];
    platform.pluginHost.loadPlugin = async (slot, path, id) => { calls.push(['load', slot, id]); return { slot, descriptor: [instA, instB, fx].find((d) => d.id === id) }; };
    platform.pluginHost.unloadPlugin = async (slot) => {
      calls.push(['unload', slot]);
      // The router's sink while the unload runs (a TS-private field, read for the probe), and what the
      // engine was told meanwhile.
      routedDuringUnload = { sink: lf.inputRouter.sink === null ? null : 'engine', targets: targets(await sent()) };
      if (unloadFailures > 0) { unloadFailures--; throw new Error('injected unload failure'); }
    };
    const out = {};

    // D13: a synth pick in the inactive slot makes it the MIDI slot, and a MIDI note goes there.
    instrument.selectSynth(0, 'organ'); instrument.setActiveSlot(0); await idle();
    await sent();
    instrument.selectSynth(1, 'bass'); await idle();
    send([0x90, 45, 110]);
    send([0x80, 45, 0]);
    const picked = await sent();
    out.synthPick = { active: instrument.activeSlot(), targets: targets(picked), notes: notes(picked) };

    // D13: an effect load keeps routing; an instrument load moves it.
    instrument.setActiveSlot(0);
    await instrument.selectPlugin(1, fx);
    out.afterEffect = { active: instrument.activeSlot(), slot1: instrument.slotPlugins()[1]?.id };
    await instrument.selectPlugin(1, instA);
    out.afterInstrument = { active: instrument.activeSlot(), slot1: instrument.slotPlugins()[1]?.id };
    await sent();

    // A6: a rejected unload aborts the swap (old plugin kept, no load, toast); a retry completes.
    // An active-slot swap routes nowhere while the unload runs and never selects a built-in.
    calls.length = 0; unloadFailures = 1;
    await instrument.selectPlugin(1, instB);
    out.failedSwap = {
      slot1: instrument.slotPlugins()[1]?.id,
      calls: calls.map((c) => c.join(':')),
      toast: toasts().find((t) => t.message === 'Plugin unload failed')?.detail,
      routed: routedDuringUnload,
      targets: targets(await sent()),
    };
    calls.length = 0;
    await instrument.selectPlugin(1, instB);
    out.retrySwap = {
      slot1: instrument.slotPlugins()[1]?.id,
      calls: calls.map((c) => c.join(':')),
      routed: routedDuringUnload,
      targets: targets(await sent()),
    };

    // D12-S: host-side sustain defers the plugin target's note-off until pedal-up.
    instrument.setActiveSlot(1); await sent();
    send([0xb0, 64, 127]); send([0x90, 60, 100]); send([0x80, 60, 0]);
    out.pedalHeld = notes(await sent());
    send([0xb0, 64, 0]);
    out.pedalUp = notes(await sent());
    send([0xb0, 64, 127]); send([0x90, 62, 100]); send([0x80, 62, 0]); send([0x90, 62, 90]);
    out.restrike = notes(await sent());
    send([0x80, 62, 0]); send([0xb0, 64, 0]);
    out.restrikeRelease = notes(await sent());

    // A pedal-held plugin note is released exactly once, before the new target, when MIDI moves to the
    // other slot.
    send([0xb0, 64, 127]); send([0x90, 64, 100]); send([0x80, 64, 0]);
    out.switchHeld = notes(await sent());
    instrument.setActiveSlot(0);
    send([0xb0, 64, 0]);
    out.switchReleased = (await sent()).filter((c) => c.startsWith('{"NoteO') || c.startsWith('{"SelectInstrument'));

    // A failed unload on CLEAR keeps the plugin: the slot's built-in while the unload runs, the plugin
    // target again once it failed ...
    instrument.setActiveSlot(1); await sent();
    calls.length = 0; unloadFailures = 1;
    await instrument.clearPlugin(1);
    out.failedClear = {
      slot1: instrument.slotPlugins()[1]?.id,
      calls: calls.map((c) => c.join(':')),
      routed: routedDuringUnload,
      after: targets(await sent()),
    };
    // ... and a synth pick whose unload fails leaves the MIDI slot where it was.
    instrument.setActiveSlot(0);
    unloadFailures = 1;
    instrument.selectSynth(1, 'lead'); await idle();
    out.failedPick = { slot1: instrument.slotPlugins()[1]?.id, active: instrument.activeSlot() };
    return out;
  });
  console.log(JSON.stringify(result));
  const on = (n) => JSON.stringify({ NoteOn: [n, 110 / 127] });
  assert.equal(result.synthPick.active, 1, 'a synth pick makes its slot the MIDI slot');
  assert.deepEqual(result.synthPick.targets, [JSON.stringify({ SelectInstrument: { Builtin: 'bass' } })], 'MIDI routes to the picked slot\'s instrument');
  assert.deepEqual(result.synthPick.notes, [on(45), JSON.stringify({ NoteOff: 45 })], 'a MIDI note goes to the picked slot\'s instrument');
  assert.deepEqual(result.afterEffect, { active: 0, slot1: 'fx' }, 'an effect load leaves routing alone');
  assert.deepEqual(result.afterInstrument, { active: 1, slot1: 'a' }, 'an instrument load moves routing');
  assert.deepEqual(result.failedSwap, {
    slot1: 'a', calls: ['unload:1'], routed: { sink: null, targets: [] }, targets: [],
    toast: 'The plugin stays in the slot but is silent. Choose none or another plugin to retry.',
  }, 'a failed unload keeps the old plugin, makes no load call and raises the toast naming the way out');
  assert.deepEqual(result.retrySwap, { slot1: 'b', calls: ['unload:1', 'load:1:b'], routed: { sink: null, targets: [] }, targets: [] },
    'the retry swap completes; neither swap routes anywhere meanwhile or selects a built-in');
  const n = (list) => list.map((c) => (typeof c === 'number' ? JSON.stringify({ NoteOff: c }) : JSON.stringify({ NoteOn: c })));
  assert.deepEqual(result.pedalHeld, n([[60, 100 / 127]]), 'pedal down defers the plugin note-off');
  assert.deepEqual(result.pedalUp, n([60]), 'pedal up releases exactly once');
  assert.deepEqual(result.restrike, n([[62, 100 / 127], 62, [62, 90 / 127]]), 'a sustained re-strike sends NoteOff before NoteOn');
  assert.deepEqual(result.restrikeRelease, n([62]), 'the re-struck note releases on pedal up');
  assert.deepEqual(result.switchHeld, n([[64, 100 / 127]]), 'pedal down defers the plugin note-off before the switch');
  assert.deepEqual(result.switchReleased, [JSON.stringify({ NoteOff: 64 }), JSON.stringify({ SelectInstrument: { Builtin: 'organ' } })],
    'switching slots releases the sustained note exactly once, before the new target');
  assert.deepEqual(result.failedClear, {
    slot1: 'b', calls: ['unload:1'],
    routed: { sink: 'engine', targets: [JSON.stringify({ SelectInstrument: { Builtin: 'bass' } })] },
    after: [JSON.stringify({ SelectInstrument: { Slot: 1 } })],
  }, 'a failed clear keeps the plugin: its built-in while the unload runs, the plugin target again after');
  assert.deepEqual(result.failedPick, { slot1: 'b', active: 0 }, 'a synth pick whose unload fails does not move the MIDI slot');
});

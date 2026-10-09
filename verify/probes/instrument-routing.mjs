/**
 * The instrument routing seam (`src/ui/state/instrument.ts`): which note target the UI hands native
 * MIDI's router, on the web engine fake (`src/platform/host.web.ts`, the engine-seam pattern) with the
 * plugin host on (host.web.ts served with `available: true`) and its load and unload instrumented. The
 * targets are `input.selectTarget(slot, target)` events, read from `__lf.native.inputSent`:
 *
 * - picking a source moves the MIDI/keys slot: a synth pick in the inactive slot routes to its
 *   instrument; an effect load leaves routing alone, an instrument load moves it;
 * - a failed unload aborts a swap (the old plugin kept, no load, the toast naming the way out); while a
 *   swap's unload runs the notes go nowhere (`Off`) and neither swap selects a built-in; the failed one
 *   routes back to the plugin, the retry completes and routes to the new one;
 * - a failed CLEAR keeps the plugin: the slot's built-in is selected while the unload runs, the plugin
 *   target again once it failed; a synth pick whose unload fails does not move the MIDI slot;
 * - a slot switch routes to the other slot's source.
 *
 * What moved to Rust with the router (`src-tauri/src/engine_io/midi/router.rs` tests): the release of
 * held and sustained notes before a new target, the same slot and target changing nothing, host-side
 * sustain on a plugin target and a sustained re-strike. Cannot see the native plugin host (load, unload),
 * the engine (what a target plays: lf-engine `tests/slots.rs`) or Tauri IPC.
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
      await p.addInitScript(() => void (window.__lfEngineFake = true));
    },
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1);

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
    const desc = (id, isEffect) => ({ id, name: `Probe ${id}`, path: `C:\\probe\\${id}.vst3`, format: 'vst3', isEffect });
    const [instA, instB, fx] = [desc('a', false), desc('b', false), desc('fx', true)];

    /** The note targets the UI sent since the last call, each as 'slot:target'. */
    let seen = 0;
    const targets = async () => {
      await pause(10); // the outbox flushes on a microtask
      const all = lf.native.inputSent.slice(seen);
      seen = lf.native.inputSent.length;
      return all.filter((e) => e.selectTarget).map((e) => `${e.selectTarget.slot}:${JSON.stringify(e.selectTarget.target)}`);
    };

    const calls = [];
    let unloadFailures = 0;
    let routedDuringUnload;
    platform.pluginHost.scanPlugins = async () => [instA, instB, fx];
    platform.pluginHost.loadPlugin = async (slot, path, id) => { calls.push(['load', slot, id]); return { slot, descriptor: [instA, instB, fx].find((d) => d.id === id) }; };
    platform.pluginHost.unloadPlugin = async (slot) => {
      calls.push(['unload', slot]);
      // Where the notes were routed while the unload runs.
      routedDuringUnload = await targets();
      if (unloadFailures > 0) { unloadFailures--; throw new Error('injected unload failure'); }
    };
    const out = {};

    // D13: a synth pick in the inactive slot makes it the MIDI slot, routed to its instrument.
    instrument.selectSynth(0, 'organ'); instrument.setActiveSlot(0); await idle();
    await targets();
    instrument.selectSynth(1, 'bass'); await idle();
    out.synthPick = { active: instrument.activeSlot(), targets: await targets() };

    // D13: an effect load keeps routing; an instrument load moves it.
    instrument.setActiveSlot(0); await targets();
    await instrument.selectPlugin(1, fx);
    out.afterEffect = { active: instrument.activeSlot(), slot1: instrument.slotPlugins()[1]?.id, targets: await targets() };
    await instrument.selectPlugin(1, instA);
    out.afterInstrument = { active: instrument.activeSlot(), slot1: instrument.slotPlugins()[1]?.id, targets: await targets() };

    // A6: a rejected unload aborts the swap (old plugin kept, no load, toast); a retry completes.
    // An active-slot swap routes nowhere while the unload runs and never selects a built-in.
    calls.length = 0; unloadFailures = 1;
    await instrument.selectPlugin(1, instB);
    out.failedSwap = {
      slot1: instrument.slotPlugins()[1]?.id,
      calls: calls.map((c) => c.join(':')),
      toast: toasts().find((t) => t.message === 'Plugin unload failed')?.detail,
      routed: routedDuringUnload,
      after: await targets(),
    };
    calls.length = 0;
    await instrument.selectPlugin(1, instB);
    out.retrySwap = { slot1: instrument.slotPlugins()[1]?.id, calls: calls.map((c) => c.join(':')), routed: routedDuringUnload, after: await targets() };

    // A slot switch routes to the other slot's source.
    instrument.setActiveSlot(0);
    out.slotSwitch = await targets();

    // A failed unload on CLEAR keeps the plugin: the slot's built-in while the unload runs, the plugin
    // target again once it failed ...
    instrument.setActiveSlot(1); await targets();
    calls.length = 0; unloadFailures = 1;
    await instrument.clearPlugin(1);
    out.failedClear = { slot1: instrument.slotPlugins()[1]?.id, calls: calls.map((c) => c.join(':')), routed: routedDuringUnload, after: await targets() };
    // ... and a synth pick whose unload fails leaves the MIDI slot where it was.
    instrument.setActiveSlot(0);
    unloadFailures = 1;
    instrument.selectSynth(1, 'lead'); await idle();
    out.failedPick = { slot1: instrument.slotPlugins()[1]?.id, active: instrument.activeSlot() };
    out.engineNotes = lf.native.sent.filter((c) => c.SelectInstrument || c.NoteOn || c.NoteOff !== undefined).length;
    return out;
  });
  console.log(JSON.stringify(result));
  const builtin = (slot, id) => `${slot}:${JSON.stringify({ Builtin: id })}`;
  const plugin = (slot) => `${slot}:${JSON.stringify({ Slot: slot })}`;
  const off = (slot) => `${slot}:"Off"`;
  assert.equal(result.synthPick.active, 1, 'a synth pick makes its slot the MIDI slot');
  assert.deepEqual(result.synthPick.targets, [builtin(1, 'bass')], "the notes route to the picked slot's instrument");
  assert.deepEqual({ ...result.afterEffect, targets: [...new Set(result.afterEffect.targets)] }, { active: 0, slot1: 'fx', targets: [] }, 'an effect load leaves routing alone');
  assert.deepEqual({ active: result.afterInstrument.active, slot1: result.afterInstrument.slot1 }, { active: 1, slot1: 'a' }, 'an instrument load moves routing');
  assert.equal(result.afterInstrument.targets.at(-1), plugin(1), 'to the plugin');
  assert.deepEqual(result.failedSwap, {
    slot1: 'a', calls: ['unload:1'], routed: [off(1)], after: [plugin(1)],
    toast: 'The plugin stays in the slot but is silent. Choose none or another plugin to retry.',
  }, 'a failed unload keeps the old plugin, makes no load call, raises the toast naming the way out, routes nowhere while it runs and back to the plugin after');
  assert.deepEqual(result.retrySwap, { slot1: 'b', calls: ['unload:1', 'load:1:b'], routed: [off(1)], after: [plugin(1)] },
    'the retry swap completes; neither swap selects a built-in');
  assert.deepEqual(result.slotSwitch, [builtin(0, 'organ')], "a slot switch routes to the other slot's source");
  assert.deepEqual(result.failedClear, { slot1: 'b', calls: ['unload:1'], routed: [builtin(1, 'bass')], after: [plugin(1)] },
    'a failed clear keeps the plugin: its built-in while the unload runs, the plugin target again after');
  assert.deepEqual(result.failedPick, { slot1: 'b', active: 0 }, 'a synth pick whose unload fails does not move the MIDI slot');
  assert.equal(result.engineNotes, 0, 'no note or target goes past the router (engine_send)');
});

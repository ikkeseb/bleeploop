/**
 * Hot plugin swap in the UI, on the web engine fake (`src/platform/host.web.ts`, the engine-seam
 * pattern) with the native slot chrome on (host.web.ts served with `available: true`) and the plugin
 * host substituted in the page: every load answers a new instance, `swapPlugin` is held until the probe
 * answers it, `listLoaded` answers what the probe scripts, and the probe fires `plugin:editor-closed` and
 * `plugin:retired` itself. Engine commands are read from `__lf.native.sent`.
 *
 * Proves, per group:
 * 1. a pick of another file while a plugin plays calls `swapPlugin` with the saved level and never
 *    unloads or loads; while it is pending the slot keeps the outgoing plugin and sends no live-off and
 *    no gain; the same file (another id, its path in other case and slashes) goes cold;
 * 2. the answer is adopted (plugin and instance), and its activated kind decides the MIDI claim: an
 *    instrument takes the active slot, an effect does not;
 * 3. a level change while a swap is pending sends nothing, and nothing after a successful swap;
 * 4. `plugin:retired` releases the outgoing file's editor affinity (seen through the other slot's
 *    `editorAffinityBlocker`), after the answer and before it (released once, at the answer), a
 *    duplicate is ignored, and `released: false` leaves the file uncertain;
 * 5. a failed swap reads the host's listing: the outgoing listed plays on, with one toast, the provoked
 *    `console.error` and the held level sent once; the slot listed empty is cleared;
 * 6. an event of the replaced instance is dropped (its `editor-closed` leaves the new bar's editor
 *    open), and a `stale instance` refusal of a param set raises no toast and no log;
 * 7. GO LIVE pressed during a pending swap (the cap, then the named action's press) sends its toggle at
 *    once, before the swap answers, and the cap is not locked by it;
 * 8. an effect → effect hot pick sends no `SetSlotLive` after it (live stays as the engine has it) and
 *    opens the new plugin's editor;
 * 9. a hot swap that activates another kind sends no live-off and shows GO LIVE on until the feed's
 *    `live` says off, then off with no event;
 * 10. two picks queued behind a pending swap: only the newest runs;
 * 11. a swap's incoming file is held from its start: the other slot's load of it meanwhile is unordered;
 * 12. a file retiring in one slot keeps its affinity after the same file's retire in the other slot ends;
 * 13. a device rebuild's resync sends no gain for a slot with a swap pending (the other slot's it sends);
 * 14. a failed swap that leaves a live slot empty sends its live-off first;
 * 15. that live-off refused: GO LIVE stays (live, as the feed has it), and its press is a live-off;
 * 16. a `busy:` refusal of the param listing keeps the listed controls, with no toast;
 * 17. a failed swap whose listing shows another instance adopts it; a listing that fails keeps the
 *     outgoing shown, toasts once and drops the held level;
 * 18. a failed swap's incoming file stays held while its listing is awaited: the other slot's unload of
 *     that file meanwhile orphans the entry, so the adopted instance's EDITOR is refused;
 * 19. GO LIVE pressed (the cap's press, then the named action's) while a failed swap's live-off is in
 *     flight waits behind the recovery: the slot it emptied ends not live, and the cap is locked;
 * 20. a `busy:` refusal of an editor open or close keeps the editor as it was: no chip, no toast;
 * 21. a non-busy editor close error and a non-busy listing error still toast;
 * 22. a level change made while a failed swap's recovery runs (its answer in, its listing awaited) is
 *     held, and sent once when the outgoing plays on;
 * 23. GO LIVE pressed while a failed swap's recovery adopts a plugin waits behind it and is answered for
 *     the adopted plugin: an effect's press (the named action's) is the engine's toggle, an instrument's
 *     (the cap's) a live-off;
 * 24. a failed swap's incoming file is let go when its recovery ends: the other slot's later unload of
 *     that file leaves no entry (the uncertain rule refuses it, no orphan of a stale hold);
 * 25. a stranded GO LIVE is carried by a synth pick while the slot stays live (its press a live-off) and
 *     cleared by a plugin landing: an instrument shows no GO LIVE though the feed still says live.
 *
 * Cannot see the native swap, its commit, the engine's live-off at a kind change or any timing: the
 * fake answers what the probe scripts.
 * Run: pnpm probe plugin-hot-swap
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page, consumeConsoleErrors } = await open({
    viewport: { width: 1280, height: 820 },
    init: async (p) => {
      await p.route('**/src/platform/host.web.ts', async (route) => {
        const response = await route.fetch();
        const body = (await response.text()).replace('available: false', 'available: true');
        await route.fulfill({ response, body });
      });
      await p.addInitScript(() => void (window.__lfEngineFake = true));
    },
  });

  const keys = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const slots = await import('/src/ui/state/instrument-slots.ts');
    const live = await import('/src/ui/instrument/live.ts');
    const nativeIo = await import('/src/ui/state/native-io.ts');
    const notify = await import('/src/notify.ts');
    const { pluginDescriptorKey } = await import('/src/ui/state/plugin-descriptor.ts');
    const pause = (ms) => new Promise((r) => setTimeout(r, ms));
    const until = async (read, what) => {
      for (let i = 0; i < 300 && !read(); i++) await pause(10);
      if (!read()) throw new Error(`timed out waiting for ${what}`);
    };
    const deadline = Date.now() + 10000;
    while (!instrument.nativeHostReady() || instrument.scanning()) {
      if (Date.now() > deadline) throw new Error('native host boot did not finish');
      await pause(20);
    }
    const desc = (id, isEffect, path = `C:\\probe\\${id}.vst3`) => ({ id, name: `Probe ${id.toUpperCase()}`, path, format: 'vst3', isEffect });
    const d = {};
    for (const id of 'abcdefghijklmnopqrstvwxyz') d[id] = desc(id, false);
    for (const id of ['fx1', 'fx2', 'fx3']) d[id] = desc(id, true);
    d.u = desc('u', undefined); // unclassified: takes input, activates as whatever the host says
    d.bCase = desc('b-copy', false, 'c:/PROBE/B.vst3'); // the same file as `b`, as native's `same_file` reads it
    const all = Object.values(d);
    const find = (path, id) => all.find((x) => x.path === path && x.id === id);

    const H = { d, calls: [], instance: 0, swapHold: null, listed: [], listFails: false, listGate: null, listCalls: 0, paramsError: null, editorError: null, opened: [], closedCbs: new Set(), paramsCbs: new Set(), retiredCb: null, staleSets: 0 };
    const answer = (slot, path, id, effect) => {
      const descriptor = find(path, id);
      return { slot, descriptor, instance: ++H.instance, effect: effect ?? descriptor.isEffect === true };
    };
    const host = platform.pluginHost;
    host.scanPlugins = async () => all;
    host.loadPlugin = async (slot, path, id) => {
      H.calls.push(['load', slot, path]);
      return answer(slot, path, id);
    };
    host.unloadPlugin = async (slot) => void H.calls.push(['unload', slot]);
    host.swapPlugin = (slot, path, id, gain) => {
      H.calls.push(['swap', slot, path, gain]);
      return new Promise((resolve, reject) => {
        H.swapHold = { resolve: (effect) => resolve(answer(slot, path, id, effect)), reject };
      });
    };
    host.listLoaded = async () => {
      H.listCalls++;
      await H.listGate;
      if (H.listFails) throw new Error('injected listing failure');
      return H.listed;
    };
    host.openEditor = async (slot, instance) => {
      if (H.editorError) throw new Error(H.editorError);
      H.opened.push([slot, instance]);
    };
    host.closeEditor = async () => {
      if (H.editorError) throw new Error(H.editorError);
    };
    host.listParams = async () => {
      if (H.paramsError) throw new Error(H.paramsError);
      return [{ id: 1, name: 'Alpha', minValue: 0, maxValue: 1, defaultValue: 0.5, value: 0.5 }];
    };
    host.onParamsChanged = (cb) => {
      H.paramsCbs.add(cb);
      return () => H.paramsCbs.delete(cb);
    };
    host.setParameter = async () => {
      H.staleSets++;
      throw new Error('stale instance: slot 1 plays another instance');
    };
    host.onEditorClosed = (cb) => {
      H.closedCbs.add(cb);
      return () => H.closedCbs.delete(cb);
    };
    host.onRetired = (cb) => {
      H.retiredCb = cb;
      return () => {};
    };
    instrument.watchRetiredPlugins(); // boot's watcher subscribed to the web stub's no-op

    Object.assign(H, {
      instrument, slots, live, nativeIo, until, pause,
      release(effect) {
        const hold = H.swapHold;
        H.swapHold = null;
        hold.resolve(effect);
      },
      fail(message) {
        const hold = H.swapHold;
        H.swapHold = null;
        hold.reject(new Error(message));
      },
      idle: () => until(() => slots.slotPendingCounts().every((n) => n === 0), 'the slots to settle'),
      held: () => until(() => H.swapHold !== null, 'a swap call'),
      mark: () => window.__lf.native.sent.length,
      // The slot commands the UI sent since `from`: live flags, GO LIVE toggles and gains.
      slotSent: (from, slot) =>
        window.__lf.native.sent.slice(from).flatMap((c) =>
          c.SetSlotLive?.[0] === slot ? [['live', c.SetSlotLive[1]]]
            : c.Action?.Toggle?.Live === slot ? [['toggle']]
              : c.SetSlotGain?.[0] === slot ? [['gain', Math.round(c.SetSlotGain[1] * 100) / 100]] : []),
      shown: (slot) => ({ id: instrument.slotPlugins()[slot]?.id ?? null, instance: instrument.slotInstances()[slot] }),
      toasts: () => notify.toasts().map((t) => `${t.message} x${t.count}`),
      clearToasts: () => notify.toasts().forEach((t) => notify.dismissToast(t.id)),
      blocker: (slot, path) => instrument.editorAffinityBlocker(slot, path),
      retire: (slot, instance, released) => H.retiredCb({ slot, instance, released }),
      editorClosed: (slot, instance) => H.closedCbs.forEach((cb) => cb({ slot, instance })),
      paramsChanged: (slot, instance) => H.paramsCbs.forEach((cb) => cb({ slot, instance })),
      capPressed: (slot) => document.querySelector(`button[aria-label="Live input for slot ${'AB'[slot]}"]`)?.getAttribute('aria-pressed') ?? null,
    });
    window.__hot = H;
    await instrument.scanForPlugins();
    await instrument.selectPlugin(0, d.a);
    return Object.fromEntries(Object.entries(d).map(([k, v]) => [k, pluginDescriptorKey(v)]));
  });
  await page.waitForSelector('.slot__source');
  const picker = (slot) => page.locator('.slot__source').nth(slot);
  const cap = page.locator('button[aria-label="Live input for slot B"]');

  // 1. A hot pick: one swap at the saved level, the outgoing plugin kept meanwhile; the same file goes cold.
  await page.evaluate((key) => localStorage.setItem(`lf.pluginGain.0.${key}`, '0.7'), keys.b);
  const from1 = await page.evaluate(() => {
    window.__hot.calls.length = 0;
    return window.__hot.mark();
  });
  await picker(0).selectOption(keys.b, { noWaitAfter: true });
  const pending1 = await page.evaluate(async (from) => {
    const H = window.__hot;
    await H.held();
    await H.pause(30); // the outbox drains on a microtask
    return {
      calls: [...H.calls],
      shown: H.shown(0),
      editorBar: document.querySelectorAll('button[aria-label="Editor for slot A"]').length,
      sent: H.slotSent(from, 0),
    };
  }, from1);
  console.log('1. pending', JSON.stringify(pending1));
  assert.deepEqual(pending1, {
    calls: [['swap', 0, 'C:\\probe\\b.vst3', 0.7]],
    shown: { id: 'a', instance: 1 },
    editorBar: 1,
    sent: [],
  }, 'a hot pick swaps at the saved level and keeps the outgoing plugin, with no live-off and no gain');
  const cold1 = await page.evaluate(async () => {
    const H = window.__hot;
    H.release(false);
    await H.idle();
    const adopted = H.shown(0);
    H.calls.length = 0;
    const cold = H.instrument.selectPlugin(0, H.d.bCase);
    await H.until(() => H.calls.length >= 2 || H.swapHold, 'the same-file pick');
    if (H.swapHold) H.release(false); // a wrongly hot pick, answered so the assertion shows it
    await cold;
    return { adopted, calls: [...H.calls], after: H.shown(0) };
  });
  console.log('1. same file', JSON.stringify(cold1));
  assert.deepEqual(cold1, {
    adopted: { id: 'b', instance: 2 },
    calls: [['unload', 0], ['load', 0, 'c:/PROBE/B.vst3']],
    after: { id: 'b-copy', instance: 3 },
  }, 'the same file under another id goes cold');

  // 2. The answer is adopted, and the activated kind decides the MIDI claim.
  const claim2 = await page.evaluate(async () => {
    const H = window.__hot;
    const { instrument } = H;
    instrument.setActiveSlot(0);
    await instrument.selectPlugin(1, H.d.fx1); // cold, into the empty slot
    const toInstrument = instrument.selectPlugin(1, H.d.c);
    await H.held();
    H.release(false);
    await toInstrument;
    const instrumentSwap = { shown: H.shown(1), active: instrument.activeSlot() };
    instrument.setActiveSlot(0);
    const toEffect = instrument.selectPlugin(1, H.d.fx2);
    await H.held();
    H.release(true);
    await toEffect;
    return { instrumentSwap, effectSwap: { shown: H.shown(1), active: instrument.activeSlot() } };
  });
  console.log('2. claim', JSON.stringify(claim2));
  assert.deepEqual(claim2, {
    instrumentSwap: { shown: { id: 'c', instance: 5 }, active: 1 },
    effectSwap: { shown: { id: 'fx2', instance: 6 }, active: 0 },
  }, 'an instrument answer claims MIDI, an effect answer does not');

  // 3. A level change while a swap is pending is held; a successful swap sends none.
  const held3 = await page.evaluate(async () => {
    const H = window.__hot;
    const from = H.mark();
    const swap = H.instrument.selectPlugin(0, H.d.d);
    await H.held();
    H.instrument.setSlotLevel(0, 0.4);
    await H.pause(30);
    const pending = H.slotSent(from, 0);
    H.release(false);
    await swap;
    await H.pause(30);
    return { pending, after: H.slotSent(from, 0), shown: H.shown(0) };
  });
  console.log('3. held level', JSON.stringify(held3));
  assert.deepEqual(held3, { pending: [], after: [], shown: { id: 'd', instance: 7 } },
    'no gain is sent while the swap is pending, nor after it succeeded');

  // 4. `plugin:retired` releases the outgoing file's affinity, after the answer or before it.
  const retired4 = await page.evaluate(async () => {
    const H = window.__hot;
    const { instrument, d } = H;
    const owner = () => H.blocker(1, d.d.path)?.owner ?? null;
    const out = {};
    let swap = instrument.selectPlugin(0, d.e); // d (7) → e
    await H.held();
    out.pending = owner();
    H.release(false);
    await swap;
    out.answered = owner();
    H.retire(0, 7, true);
    out.retiredAfter = owner();
    swap = instrument.selectPlugin(0, d.d); // e (8) → d again: slot A owns d's file afresh
    await H.held();
    H.release(false);
    await swap;
    H.retire(0, 7, true); // the same event again
    out.duplicate = owner();
    swap = instrument.selectPlugin(0, d.f); // d (9) → f, retired before the answer
    await H.held();
    H.retire(0, 9, true);
    out.retiredBefore = owner();
    H.release(false);
    await swap;
    out.answeredAfterRetired = owner();
    swap = instrument.selectPlugin(0, d.g); // f (10) → g, its retire unproven
    await H.held();
    H.release(false);
    await swap;
    H.retire(0, 10, false);
    out.unreleased = H.blocker(1, d.f.path);
    return out;
  });
  console.log('4. retired', JSON.stringify(retired4));
  assert.deepEqual(retired4, {
    pending: 'slot A',
    answered: 'slot A',
    retiredAfter: null,
    duplicate: 'slot A',
    retiredBefore: "slot A's previous load",
    answeredAfterRetired: null,
    unreleased: { owner: null },
  }, 'the outgoing affinity is released once, at its retire or at the answer after it; released: false stays uncertain');

  // 5. A failed swap reads the slot's truth from the host's listing.
  await page.evaluate(() => window.__hot.clearToasts());
  const playsOn5 = await page.evaluate(async () => {
    const H = window.__hot;
    const from = H.mark();
    const swap = H.instrument.selectPlugin(0, H.d.h); // g (11) → h, refused
    await H.held();
    H.instrument.setSlotLevel(0, 0.6);
    H.listed = [{ slot: 0, descriptor: H.d.g, instance: 11, effect: false }];
    H.fail('injected swap failure');
    await swap;
    await H.pause(30);
    const out = { shown: H.shown(0), sent: H.slotSent(from, 0), toasts: H.toasts() };
    H.clearToasts();
    return out;
  });
  console.log('5. plays on', JSON.stringify(playsOn5));
  assert.deepEqual(playsOn5, {
    shown: { id: 'g', instance: 11 },
    sent: [['gain', 0.6]],
    toasts: ['Could not load Probe H. Probe G plays on. x1'],
  }, 'the outgoing listed plays on, with one toast and the held level sent once');
  assert.equal(consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure').length, 1, 'the failed swap is logged');
  const emptied5 = await page.evaluate(async () => {
    const H = window.__hot;
    const from = H.mark();
    const swap = H.instrument.selectPlugin(0, H.d.i); // g (11) → i, the slot ends empty
    await H.held();
    H.listed = [{ slot: 1, descriptor: H.d.fx2, instance: 6, effect: true }];
    H.fail('injected swap failure, slot emptied');
    await swap;
    await H.pause(30);
    const out = { shown: H.shown(0), sent: H.slotSent(from, 0), toasts: H.toasts() };
    H.clearToasts();
    return out;
  });
  console.log('5. emptied', JSON.stringify(emptied5));
  assert.deepEqual(emptied5, {
    shown: { id: null, instance: null },
    sent: [['live', false], ['gain', 1]],
    toasts: ['Could not load Probe I, and the slot is empty now. Pick a source again. x1'],
  }, 'a slot listed empty is cleared, its live input stopped and its gain back at the input level');
  assert.equal(consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, slot emptied').length, 1, 'the failed swap is logged');

  // 6. The replaced instance's events are dropped; a stale refusal is silent.
  await page.evaluate(() => window.__hot.instrument.selectPlugin(0, window.__hot.d.j)); // cold (12)
  await picker(0).selectOption(keys.k, { noWaitAfter: true });
  await page.evaluate(async () => {
    const H = window.__hot;
    await H.held();
    H.release(false); // k (13)
    await H.idle();
  });
  await page.waitForFunction(() => document.querySelector('button[aria-label="Editor for slot A"]')?.getAttribute('aria-pressed') === 'true');
  const closed6 = await page.evaluate(async () => {
    const H = window.__hot;
    const pressed = () => document.querySelector('button[aria-label="Editor for slot A"]').getAttribute('aria-pressed');
    H.editorClosed(0, 12);
    await H.pause(30);
    const oldInstance = pressed();
    H.editorClosed(0, 13);
    await H.pause(30);
    return { opened: H.opened.at(-1), oldInstance, newInstance: pressed() };
  });
  console.log('6. editor-closed', JSON.stringify(closed6));
  assert.deepEqual(closed6, { opened: [0, 13], oldInstance: 'true', newInstance: 'false' },
    "the replaced instance's editor-closed leaves the new bar's editor open; its own closes it");
  await page.getByRole('button', { name: /Plugin parameters for slot A/ }).click();
  await page.locator('#slot-0-params input[aria-label="Alpha"]').fill('0.8');
  const stale6 = await page.evaluate(async () => {
    const H = window.__hot;
    await H.until(() => H.staleSets > 0, 'the param set');
    await H.pause(50);
    return { sets: H.staleSets, toasts: H.toasts() };
  });
  console.log('6. stale set', JSON.stringify(stale6));
  assert.deepEqual(stale6, { sets: 1, toasts: [] }, 'a stale instance refusal raises no toast (and no log: the harness fails on one)');

  // 7. GO LIVE during a pending hot swap is pressed at once (slot B holds the effect fx2).
  const from7 = await page.evaluate(() => window.__hot.mark());
  await picker(1).selectOption(keys.fx3, { noWaitAfter: true });
  await page.evaluate(() => window.__hot.held());
  assert.equal(await cap.isDisabled(), false, 'the GO LIVE cap is not locked by a hot swap');
  const pressed = (on) => page.waitForFunction((v) => document.querySelector('button[aria-label="Live input for slot B"]')?.getAttribute('aria-pressed') === v, String(on));
  await cap.click();
  await pressed(true);
  assert.equal(await page.evaluate(() => window.__hot.live.pressGoLive(1)), true, 'the named action presses');
  await pressed(false);
  await cap.click();
  await pressed(true);
  const live7 = await page.evaluate((from) => {
    const H = window.__hot;
    return { sent: H.slotSent(from, 1), shown: H.shown(1), swapHeld: H.swapHold !== null };
  }, from7);
  console.log('7. presses', JSON.stringify(live7));
  assert.deepEqual(live7, {
    sent: [['toggle'], ['toggle'], ['toggle']],
    shown: { id: 'fx2', instance: 6 },
    swapHeld: true,
  }, 'each press sends its toggle before the swap answers');

  // 8. Effect → effect: no live flag sent after it, the editor opens.
  const effect8 = await page.evaluate(async (from) => {
    const H = window.__hot;
    H.release(true); // fx3 (14)
    await H.idle();
    await H.until(() => H.opened.some(([s, i]) => s === 1 && i === 14), 'the editor to open');
    await H.pause(30);
    return { sent: H.slotSent(from, 1), shown: H.shown(1) };
  }, from7);
  console.log('8. effect to effect', JSON.stringify(effect8));
  assert.deepEqual(effect8, { sent: [['toggle'], ['toggle'], ['toggle']], shown: { id: 'fx3', instance: 14 } },
    'an effect hot pick sends no SetSlotLive and opens its editor');
  assert.equal(await cap.getAttribute('aria-pressed'), 'true', 'the slot stays live as the feed has it');

  // 9. A swap to another kind: the UI sends no live-off; GO LIVE follows the feed's `live` alone.
  const kind9 = await page.evaluate(async () => {
    const H = window.__hot;
    const from = H.mark();
    const swap = H.instrument.selectPlugin(1, H.d.u); // fx3 (14) → u, activated as an instrument
    await H.held();
    H.release(false);
    await swap;
    await H.pause(30);
    return { sent: H.slotSent(from, 1), shown: H.shown(1) };
  });
  console.log('9. kind change', JSON.stringify(kind9));
  assert.deepEqual(kind9, { sent: [], shown: { id: 'u', instance: 15 } }, 'a kind change sends nothing for the live flag');
  assert.equal(await cap.getAttribute('aria-pressed'), 'true', 'GO LIVE stays on until the feed says off');
  await page.evaluate(() => window.__lf.native.emit({ seq: 9000, reset: false, events: [], live: [false, false] }));
  await pressed(false);
  assert.match(await cap.innerText(), /GO LIVE/, 'GO LIVE shows off once the feed says so, with no event');

  // 10. Two picks queued behind a pending swap: only the newest runs.
  const queued10 = await page.evaluate(async () => {
    const H = window.__hot;
    const { instrument, d } = H;
    H.calls.length = 0;
    const first = instrument.selectPlugin(1, d.l);
    await H.held();
    const overtaken = instrument.selectPlugin(1, d.m);
    const newest = instrument.selectPlugin(1, d.n);
    H.release(false);
    await first;
    let settled = false;
    const rest = Promise.all([overtaken, newest]).then(() => (settled = true));
    while (!settled) {
      await H.until(() => settled || H.swapHold, 'the queued picks');
      if (H.swapHold) H.release(false);
    }
    await rest;
    return { swaps: H.calls.map(([kind, , path]) => `${kind} ${path}`), shown: H.shown(1) };
  });
  console.log('10. queued', JSON.stringify(queued10));
  assert.deepEqual(queued10, {
    swaps: ['swap C:\\probe\\l.vst3', 'swap C:\\probe\\n.vst3'],
    shown: { id: 'n', instance: 17 },
  }, 'a pick a newer one overtook in the queue does nothing');

  // 11. A swap's incoming file is held from its start: the other slot's load of it meanwhile is unordered.
  const UNORDERED = 'an unknown slot (both loaded it at once)';
  const incoming11 = await page.evaluate(async () => {
    const H = window.__hot;
    const { instrument, d } = H;
    await instrument.clearPlugin(1);
    const swap = instrument.selectPlugin(0, d.o); // k (13) → o
    await H.held();
    await instrument.selectPlugin(1, d.o); // cold into the empty slot B while A's swap is pending
    const overlap = H.blocker(0, d.o.path)?.owner ?? null;
    H.release(false);
    await swap;
    return { overlap, shown: [H.shown(0).id, H.shown(1).id] };
  });
  console.log('11. incoming', JSON.stringify(incoming11));
  assert.deepEqual(incoming11, { overlap: UNORDERED, shown: ['o', 'o'] },
    "a load overlapping the other slot's swap to the same file cannot be ordered");

  // 12. Both slots hold o; each swaps away. B's retire ends first: A's o still retires, so the entry stays.
  const retire12 = await page.evaluate(async () => {
    const H = window.__hot;
    const { instrument, d } = H;
    const owner = () => H.blocker(0, d.o.path)?.owner ?? null;
    const [a, b] = [H.shown(0).instance, H.shown(1).instance];
    for (const [slot, to] of [[0, d.p], [1, d.q]]) {
      const swap = instrument.selectPlugin(slot, to);
      await H.held();
      H.release(false);
      await swap;
    }
    H.retire(1, b, true);
    const oneRetiring = owner();
    H.retire(0, a, true);
    return { oneRetiring, noneLeft: owner() };
  });
  console.log('12. retiring', JSON.stringify(retire12));
  assert.deepEqual(retire12, { oneRetiring: UNORDERED, noneLeft: null },
    'a file retiring in one slot keeps its affinity until its last holder lets go');

  // 13. A rebuild's resync while a swap is pending sends the other slot's gain, not the swapping slot's.
  const resync13 = await page.evaluate(async () => {
    const H = window.__hot;
    const swap = H.instrument.selectPlugin(0, H.d.r); // p → r
    await H.held();
    const from = H.mark();
    H.instrument.engineResync(false);
    await H.pause(30);
    const sent = { swapping: H.slotSent(from, 0), other: H.slotSent(from, 1).length };
    H.release(false);
    await swap;
    return sent;
  });
  console.log('13. resync', JSON.stringify(resync13));
  assert.deepEqual(resync13, { swapping: [], other: 1 }, "a resync leaves a pending swap's gain to native's replay");

  // 14. A failed swap that leaves a live slot empty stops its input first.
  const goLiveOn = (slot) => page.evaluate(async (s) => {
    const H = window.__hot;
    await H.live.toggleLive(s);
    await H.until(() => H.capPressed(s) === 'true', 'the slot to go live');
  }, slot);
  await page.evaluate(async () => {
    const H = window.__hot;
    const swap = H.instrument.selectPlugin(1, H.d.fx1); // q → fx1
    await H.held();
    H.release(true);
    await swap;
  });
  await goLiveOn(1);
  const failSwapEmpty = (to) => page.evaluate(async (key) => {
    const H = window.__hot;
    const from = H.mark();
    const swap = H.instrument.selectPlugin(1, H.d[key]);
    await H.held();
    H.listed = [];
    H.fail('injected swap failure, slot emptied');
    await swap;
    await H.pause(50);
    const out = { shown: H.shown(1), sent: H.slotSent(from, 1), cap: H.capPressed(1) };
    H.clearToasts();
    return out;
  }, to);
  const emptied14 = await failSwapEmpty('fx2');
  console.log('14. emptied live', JSON.stringify(emptied14));
  assert.deepEqual(emptied14, { shown: { id: null, instance: null }, sent: [['live', false], ['gain', 1]], cap: null },
    'the emptied slot is taken off live before it reads empty');
  consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, slot emptied');

  // 15. The same with the live-off lost on its way: GO LIVE stays as the stop, the feed's live its state.
  await page.evaluate(() => window.__hot.instrument.selectPlugin(1, window.__hot.d.fx1)); // cold
  await goLiveOn(1);
  await page.evaluate(async () => {
    const H = window.__hot;
    const swap = H.instrument.selectPlugin(1, H.d.fx2);
    await H.held();
    H.listed = [];
    window.__lf.native.failSends = 2; // the live-off's batch and its one resend
    H.fail('injected swap failure, slot emptied, live-off lost');
    await swap;
    await H.idle();
  });
  const stranded15 = await page.evaluate(() => ({
    shown: window.__hot.shown(1),
    pressed: window.__hot.capPressed(1),
    disabled: document.querySelector('button[aria-label="Live input for slot B"]')?.disabled ?? null,
  }));
  console.log('15. stranded', JSON.stringify(stranded15));
  assert.deepEqual(stranded15, { shown: { id: null, instance: null }, pressed: 'true', disabled: false },
    'a refused live-off keeps GO LIVE reachable, live as the feed has it');
  const from15 = await page.evaluate(() => {
    window.__hot.clearToasts();
    return window.__hot.mark();
  });
  await cap.click();
  await cap.waitFor({ state: 'detached' });
  assert.deepEqual(await page.evaluate((from) => window.__hot.slotSent(from, 1), from15), [['live', false]], 'its press is a live-off');
  consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, slot emptied, live-off lost');
  consumeConsoleErrors(/did not reach|lost the batch/);

  // 16. A `busy:` refusal of the param listing keeps the listed controls, with no toast.
  const busy16 = await page.evaluate(async () => {
    const H = window.__hot;
    H.clearToasts();
    const sliders = () => document.querySelectorAll('#slot-0-params input[aria-label="Alpha"]').length;
    await H.until(() => sliders() === 1, 'slot A to list its controls');
    H.paramsError = 'busy: the plugin in slot 1 is unloading';
    H.paramsChanged(0, H.shown(0).instance);
    await H.pause(50);
    H.paramsError = null;
    return { sliders: sliders(), empty: document.querySelector('#slot-0-params .pc__empty')?.textContent ?? null, toasts: H.toasts() };
  });
  console.log('16. busy listing', JSON.stringify(busy16));
  assert.deepEqual(busy16, { sliders: 1, empty: null, toasts: [] }, 'a busy listing keeps the controls and raises no toast');
  assert.equal(consumeConsoleErrors('[PluginControls] listParams failed Error: busy:').length, 1, 'the busy refusal is logged');

  // 17. The two recovery branches left: another instance listed is adopted; a failed listing keeps the outgoing.
  const recovery17 = await page.evaluate(async () => {
    const H = window.__hot;
    const { instrument, d } = H;
    const from = H.mark();
    let swap = instrument.selectPlugin(0, d.s); // r → s, which the host lists after all
    await H.held();
    instrument.setSlotLevel(0, 0.3);
    H.listed = [{ slot: 0, descriptor: d.s, instance: 99, effect: false }];
    H.fail('injected swap failure, another listed');
    await swap;
    await H.pause(30);
    const adopted = { shown: H.shown(0), sent: H.slotSent(from, 0), toasts: H.toasts() };
    H.clearToasts();
    const from2 = H.mark();
    swap = instrument.selectPlugin(0, d.t); // s (99) → t, the listing fails too
    await H.held();
    instrument.setSlotLevel(0, 0.2);
    H.listFails = true;
    H.fail('injected swap failure, listing fails');
    await swap;
    H.listFails = false;
    await H.pause(30);
    const unlisted = { shown: H.shown(0), sent: H.slotSent(from2, 0), toasts: H.toasts() };
    H.clearToasts();
    return { adopted, unlisted };
  });
  console.log('17. recovery', JSON.stringify(recovery17));
  assert.deepEqual(recovery17, {
    adopted: { shown: { id: 's', instance: 99 }, sent: [], toasts: ['Could not load Probe S. x1'] },
    unlisted: { shown: { id: 's', instance: 99 }, sent: [], toasts: ['Could not load Probe T. x1'] },
  }, 'another listed instance is adopted; a failed listing keeps the outgoing; neither sends the held level');
  assert.equal(consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, another listed').length, 1);
  assert.equal(consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, listing fails').length, 1);
  assert.equal(consumeConsoleErrors('listing the slots failed Error: injected listing failure').length, 1);

  // 18. A failed swap's incoming file stays held while its listing is awaited: B's unload of it orphans the entry.
  const held18 = await page.evaluate(async () => {
    const H = window.__hot;
    const { instrument, d } = H;
    await instrument.selectPlugin(1, d.y); // cold into the empty slot B: B owns y's editor
    let openList;
    H.listGate = new Promise((r) => (openList = r));
    H.listed = [{ slot: 0, descriptor: d.y, instance: 200, effect: false }];
    const calls = H.listCalls;
    const swap = instrument.selectPlugin(0, d.y); // s (99) → y, refused, though native kept y
    await H.held();
    H.fail('injected swap failure, incoming kept');
    await H.until(() => H.listCalls > calls, 'the recovery listing');
    await instrument.clearPlugin(1); // B lets y go while A's recovery waits on the listing
    H.listGate = null;
    openList();
    await swap;
    const out = { shown: H.shown(0), owner: H.blocker(0, d.y.path)?.owner ?? null };
    H.clearToasts();
    return out;
  });
  console.log('18. incoming held', JSON.stringify(held18));
  assert.deepEqual(held18, { shown: { id: 'y', instance: 200 }, owner: "slot B's previous load" },
    "the incoming file keeps its holder through the recovery, so the adopted instance's EDITOR stays refused");
  assert.equal(consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, incoming kept').length, 1);

  // 19. GO LIVE pressed while a failed swap's live-off is in flight waits behind the recovery.
  for (const via of ['cap', 'action']) {
    await page.evaluate(() => window.__hot.instrument.selectPlugin(1, window.__hot.d.fx1)); // cold into the empty slot B
    await goLiveOn(1);
    const pressed19 = await page.evaluate(async (by) => {
      const H = window.__hot;
      const fake = window.__lf.native;
      const from = H.mark();
      const swap = H.instrument.selectPlugin(1, H.d.fx2);
      await H.held();
      H.listed = [];
      let answer;
      fake.sendHold = new Promise((r) => (answer = r));
      H.fail('injected swap failure, slot emptied, pressed meanwhile');
      await H.until(() => H.slotSent(from, 1).some(([k, v]) => k === 'live' && v === false), 'the live-off');
      const capDisabled = document.querySelector('button[aria-label="Live input for slot B"]')?.disabled ?? null;
      if (by === 'cap') void H.live.toggleLive(1); // the cap's press, as its click runs it
      else H.live.pressGoLive(1);
      await H.pause(30);
      fake.sendHold = null;
      answer();
      await swap;
      await H.idle();
      await H.pause(50);
      const out = { capDisabled, shown: H.shown(1), live: H.nativeIo.inputArmed()[1], stop: H.live.liveShown(1) };
      H.clearToasts();
      return out;
    }, via);
    console.log(`19. pressed in recovery (${via})`, JSON.stringify(pressed19));
    assert.deepEqual(pressed19, { capDisabled: true, shown: { id: null, instance: null }, live: false, stop: false },
      `a ${via} press during the recovery leaves the emptied slot not live, and the cap is locked meanwhile`);
    assert.equal(consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, slot emptied, pressed meanwhile').length, 1);
  }

  // 20. A `busy:` refusal of an editor open or close keeps the editor as it was.
  await page.evaluate(async () => {
    const H = window.__hot;
    const swap = H.instrument.selectPlugin(0, H.d.v); // y (200) → v: slot A owns v's editor
    await H.held();
    H.release(false);
    await swap;
    H.clearToasts();
  });
  const editorA = page.locator('button[aria-label="Editor for slot A"]');
  const pressEditor = (error) => page.evaluate(async (message) => {
    const H = window.__hot;
    H.editorError = message;
    document.querySelector('button[aria-label="Editor for slot A"]').click();
    await H.pause(50);
    H.editorError = null;
    const button = document.querySelector('button[aria-label="Editor for slot A"]');
    return { pressed: button.getAttribute('aria-pressed'), chip: button.parentElement.querySelector('.pc__err')?.textContent ?? null, toasts: H.toasts() };
  }, error);
  const BUSY = 'busy: the plugin in slot 1 is swapping';
  const busyOpen = await pressEditor(BUSY);
  await editorA.click();
  await page.waitForFunction(() => document.querySelector('button[aria-label="Editor for slot A"]')?.getAttribute('aria-pressed') === 'true');
  const busyClose = await pressEditor(BUSY);
  console.log('20. busy editor', JSON.stringify({ busyOpen, busyClose }));
  assert.deepEqual({ busyOpen, busyClose }, {
    busyOpen: { pressed: 'false', chip: null, toasts: [] },
    busyClose: { pressed: 'true', chip: null, toasts: [] },
  }, 'a busy editor refusal keeps the editor as it was, with no chip and no toast');
  assert.equal(consumeConsoleErrors(`[PluginControls] editor toggle failed Error: ${BUSY}`).length, 2, 'each busy refusal is logged');

  // 21. The negative controls: a non-busy close error and a non-busy listing error still toast.
  const closeFailed = await pressEditor('injected editor close failure');
  const listFailed21 = await page.evaluate(async () => {
    const H = window.__hot;
    H.clearToasts();
    H.paramsError = 'injected param listing failure';
    H.paramsChanged(0, H.shown(0).instance);
    await H.pause(50);
    H.paramsError = null;
    return H.toasts();
  });
  console.log('21. non-busy errors', JSON.stringify({ closeFailed, listFailed21 }));
  assert.deepEqual({ closeFailed, listFailed21 }, {
    closeFailed: { pressed: 'true', chip: 'close failed', toasts: ['Could not close the plugin editor. Press EDITOR again. x1'] },
    listFailed21: ["Could not load the plugin's controls. Choose Off, then load the plugin again. x1"],
  }, 'a non-busy editor or listing error toasts');
  assert.equal(consumeConsoleErrors('[PluginControls] editor toggle failed Error: injected editor close failure').length, 1);
  assert.equal(consumeConsoleErrors('[PluginControls] listParams failed Error: injected param listing failure').length, 1);
  // 22. A level change during a failed swap's recovery is held until the recovery ends, then sent once.
  const recovery22 = await page.evaluate(async () => {
    const H = window.__hot;
    const { instrument, d } = H;
    H.clearToasts();
    const outgoing = H.shown(0); // v
    let openList;
    H.listGate = new Promise((r) => (openList = r));
    H.listed = [{ slot: 0, descriptor: d.v, instance: outgoing.instance, effect: false }];
    const calls = H.listCalls;
    const from = H.mark();
    const swap = instrument.selectPlugin(0, d.w); // v → w, refused; v plays on
    await H.held();
    H.fail('injected swap failure, level in recovery');
    await H.until(() => H.listCalls > calls, 'the recovery listing');
    const answerPending = H.instrument.hotSwapPending(0);
    instrument.setSlotLevel(0, 0.45);
    await H.pause(30);
    const during = H.slotSent(from, 0);
    H.listGate = null;
    openList();
    await swap;
    await H.pause(30);
    const out = { answerPending, during, after: H.slotSent(from, 0), shown: H.shown(0).id, toasts: H.toasts() };
    H.clearToasts();
    return out;
  });
  console.log('22. level in recovery', JSON.stringify(recovery22));
  assert.deepEqual(recovery22, {
    answerPending: false,
    during: [],
    after: [['gain', 0.45]],
    shown: 'v',
    toasts: ['Could not load Probe W. Probe V plays on. x1'],
  }, "a level change during the recovery is held, and sent once the outgoing plays on");
  assert.equal(consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, level in recovery').length, 1);

  // 23. GO LIVE pressed while a failed swap's recovery adopts a plugin: answered for the adopted plugin.
  await page.evaluate(() => window.__hot.instrument.selectPlugin(1, window.__hot.d.fx1)); // cold into the empty slot B
  for (const [via, to, adopted] of [['action', 'fx2', 300], ['cap', 'x', 301]]) {
    await goLiveOn(1);
    const pressed23 = await page.evaluate(async ([by, key, instance]) => {
      const H = window.__hot;
      const desc = H.d[key];
      let openList;
      H.listGate = new Promise((r) => (openList = r));
      H.listed = [{ slot: 1, descriptor: desc, instance, effect: desc.isEffect === true }]; // the commit's answer lost
      const calls = H.listCalls;
      const from = H.mark();
      const swap = H.instrument.selectPlugin(1, desc);
      await H.held();
      H.fail(`injected swap failure, adopted, pressed by ${by}`);
      await H.until(() => H.listCalls > calls, 'the recovery listing');
      const capDisabled = document.querySelector('button[aria-label="Live input for slot B"]')?.disabled ?? null;
      if (by === 'cap') void H.live.toggleLive(1); // the cap's press, as its click runs it
      else H.live.pressGoLive(1);
      await H.pause(30);
      const during = H.slotSent(from, 1);
      H.listGate = null;
      openList();
      await swap;
      await H.idle();
      await H.pause(50);
      const out = { capDisabled, during, after: H.slotSent(from, 1), shown: H.shown(1), live: H.nativeIo.inputArmed()[1], stop: H.live.liveShown(1) };
      H.clearToasts();
      return out;
    }, [via, to, adopted]);
    console.log(`23. pressed in an adopting recovery (${via})`, JSON.stringify(pressed23));
    assert.deepEqual(pressed23, via === 'action'
      ? { capDisabled: true, during: [], after: [['toggle']], shown: { id: 'fx2', instance: 300 }, live: false, stop: true }
      : { capDisabled: true, during: [], after: [['live', false]], shown: { id: 'x', instance: 301 }, live: false, stop: false },
    `a ${via} press during an adopting recovery waits for it and is the adopted plugin's press`);
    assert.equal(consumeConsoleErrors(`[instrument] plugin swap failed Error: injected swap failure, adopted, pressed by ${via}`).length, 1);
  }

  // 24. A failed swap's incoming file is let go when its recovery ends: B's later unload of it leaves no entry.
  const released24 = await page.evaluate(async () => {
    const H = window.__hot;
    const { instrument, d } = H;
    await instrument.clearPlugin(1);
    await instrument.selectPlugin(1, d.z); // cold into the empty slot B: B owns z's editor
    const before = H.blocker(1, d.z.path);
    H.listed = [{ slot: 0, descriptor: d.v, instance: H.shown(0).instance, effect: false }];
    const swap = instrument.selectPlugin(0, d.z); // v → z, refused; v plays on
    await H.held();
    H.fail('injected swap failure, incoming let go');
    await swap;
    const recovered = H.blocker(1, d.z.path);
    await instrument.clearPlugin(1); // B lets z go: no holder of z is left
    const unloaded = H.blocker(1, d.z.path);
    await instrument.selectPlugin(1, d.z); // a later load of z elsewhere
    const out = { before, recovered, unloaded, reloaded: H.blocker(1, d.z.path), shown: [H.shown(0).id, H.shown(1).id] };
    H.clearToasts();
    return out;
  });
  console.log('24. incoming let go', JSON.stringify(released24));
  assert.deepEqual(released24, {
    before: null,
    recovered: null,
    unloaded: { owner: null },
    reloaded: { owner: null },
    shown: ['v', 'z'],
  }, "the incoming file's holder ends with the recovery: no stale hold orphans the entry at B's unload");
  assert.equal(consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, incoming let go').length, 1);

  // 25. A stranded GO LIVE: carried by a synth pick while the slot stays live, cleared by a plugin landing.
  const strand = () => page.evaluate(async () => {
    const H = window.__hot;
    await H.instrument.clearPlugin(1);
    await H.instrument.selectPlugin(1, H.d.fx1); // cold into the empty slot B
    await H.live.toggleLive(1);
    await H.until(() => H.capPressed(1) === 'true', 'the slot to go live');
    const swap = H.instrument.selectPlugin(1, H.d.fx3);
    await H.held();
    H.listed = [];
    window.__lf.native.failSends = 2; // the live-off's batch and its one resend
    H.fail('injected swap failure, stranded again');
    await swap;
    await H.idle();
    return H.instrument.liveStranded(1) && H.nativeIo.inputArmed()[1];
  });
  assert.equal(await strand(), true, 'the failed swap strands slot B live');
  const synth25 = await page.evaluate(async () => {
    const H = window.__hot;
    window.__lf.native.failSends = 2; // the synth pick's live-off is lost too
    H.instrument.selectSynth(1, H.instrument.slotIds()[1]);
    await H.idle();
    await H.pause(50);
    H.clearToasts();
    return { plugin: H.shown(1).id, stranded: H.instrument.liveStranded(1), live: H.nativeIo.inputArmed()[1], pressed: H.capPressed(1) };
  });
  console.log('25. synth pick, still live', JSON.stringify(synth25));
  assert.deepEqual(synth25, { plugin: null, stranded: true, live: true, pressed: 'true' },
    'a synth pick whose live-off is lost keeps GO LIVE as the stop');
  const from25 = await page.evaluate(() => window.__hot.mark());
  await cap.click();
  await cap.waitFor({ state: 'detached' });
  assert.deepEqual(await page.evaluate((from) => window.__hot.slotSent(from, 1), from25), [['live', false]], "the synth slot's stop is a live-off");
  assert.equal(await strand(), true, 'the failed swap strands slot B live again');
  const plugin25 = await page.evaluate(async () => {
    const H = window.__hot;
    window.__lf.native.failSends = 2; // the pick's live-off is lost: the feed still says live
    await H.instrument.selectPlugin(1, H.d.c); // an instrument, cold into the stranded slot
    await H.idle();
    await H.pause(50);
    H.clearToasts();
    return { plugin: H.shown(1).id, stranded: H.instrument.liveStranded(1), live: H.nativeIo.inputArmed()[1], pressed: H.capPressed(1) };
  });
  console.log('25. plugin pick', JSON.stringify(plugin25));
  assert.deepEqual(plugin25, { plugin: 'c', stranded: false, live: true, pressed: null },
    "a plugin landing clears the stranded flag: the instrument shows no GO LIVE though the feed says live");
  assert.equal(consumeConsoleErrors('[instrument] plugin swap failed Error: injected swap failure, stranded again').length, 2);
  consumeConsoleErrors(/did not reach|lost the batch/);
}, { launch: {} });

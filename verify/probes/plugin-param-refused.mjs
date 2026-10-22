/**
 * A plugin param slider returns to the plugin's real value when the host refuses the change
 * (`PluginParams` in `src/ui/instrument/PluginControls.tsx`), on the web engine fake with the native slot
 * chrome on (host.web.ts served with `available: true`, the plugin-slot-pending pattern) and the plugin
 * host methods replaced in the page: `listParams` answers from a mutable "plugin" state, `setParameter`
 * applies an accepted value to it and rejects the refused ones, each rejection released by the probe.
 *
 * Proves: (1) a refused slider edit snaps the slider back to the host's value; (2) a refusal that lands
 * after a newer accepted edit of the same param leaves the newer value, and likewise after an
 * editor-originated change of that param; (3) a refusal that lands after the slot's plugin changed
 * changes nothing (no live-value read, no throw); (5) a wholesale refresh whose listing predates a
 * refused drag neither overwrites the drag nor stops its snap-back; (4) each refused set logs one `[PluginControls]
 * setParameter refused (slot N, param ID)` console.error line, and no uncaught page error or unhandled
 * rejection remains (`probe` fails on any); (6) accepted sets that the host completes newest first (as
 * two native calls in flight at once can) still leave the plugin on each slider's last value: one set
 * is in flight per loaded plugin, and the values dragged meanwhile go after it, only the newest per param;
 * (3) also: a call still in flight across a swap does not hold up the new plugin's drags; (7) while a
 * dragged value waits for a param, a plugin-side change to it neither moves the slider nor stops the
 * value, so plugin and slider end equal; with nothing waiting, each plugin-side change (a stale echo,
 * then the newest) moves the slider in order; (8) the waiting drag owns its param against a refresh
 * too: a preset load whose listing is still out when the drag is sent leaves the slider on the drag,
 * and the plugin ends on it; (9) so does a refresh that changes no value (a param title changed).
 *
 * Cannot see the native `plugin_set_param` (what makes it refuse: a full event ring, an unlisted param
 * id), WebView2, or real timing.
 * Run: pnpm probe plugin-param-refused
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

  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const deadline = Date.now() + 10000;
    while (!instrument.nativeHostReady() || instrument.scanning()) {
      if (Date.now() > deadline) throw new Error('native host boot did not finish');
      await new Promise((r) => setTimeout(r, 20));
    }
    const desc = (id) => ({ id, name: `Probe ${id}`, path: `C:\\probe\\${id}.vst3`, format: 'vst3', isEffect: false });
    const descriptors = [desc('a'), desc('b')];
    const host = platform.pluginHost;

    // The "plugin": its real values, what listParams reports. A set the probe refuses never lands here.
    const live = { 1: 0.25, 2: 0.5 };
    const state = { live, setCalls: [], listCalls: 0, pendingRefusals: [], refuse: new Set(), paramCb: null, heldSets: [] };
    const meta = (id, name) => ({ id, name, minValue: 0, maxValue: 1, defaultValue: 0.5, value: live[id] });
    host.scanPlugins = async () => descriptors;
    host.loadPlugin = async (slot, path, id) => ({ slot, descriptor: descriptors.find((d) => d.path === path && d.id === id) });
    host.unloadPlugin = async () => {};
    host.openEditor = async () => {};
    host.closeEditor = async () => {};
    // With `holdList` set, a listing reports the values of its call and answers once released.
    host.listParams = async () => {
      state.listCalls++;
      const listed = [meta(1, 'Alpha'), meta(2, 'Beta')];
      if (state.holdList) await new Promise((r) => { state.releaseList = r; });
      return listed;
    };
    host.onParamChanged = (cb) => {
      state.paramCb = cb;
      return () => { state.paramCb = null; };
    };
    host.onParamsChanged = (cb) => {
      state.paramsCb = cb;
      return () => { state.paramsCb = null; };
    };
    // A param in `refuse` rejects, each rejection held until the probe releases it; others land, or with
    // `holdAccepted` set wait in `heldSets` and land when the probe releases them.
    host.setParameter = (slot, id, value) => {
      state.setCalls.push({ slot, id, value });
      if (!state.refuse.has(id) && state.holdAccepted) {
        return new Promise((resolve) => {
          state.heldSets.push(() => {
            live[id] = value;
            resolve();
          });
        });
      }
      if (!state.refuse.has(id)) {
        live[id] = value;
        return Promise.resolve();
      }
      return new Promise((_, reject) => {
        state.pendingRefusals.push(() => reject(new Error(`injected refusal of param ${id}`)));
      });
    };
    await instrument.scanForPlugins();
    await instrument.selectPlugin(0, descriptors[0]);
    window.__paramProbe = { state, instrument, descriptors };
  });

  await page.getByRole('button', { name: /Plugin parameters for slot A/ }).click();
  const alpha = page.locator('input[aria-label="Alpha"]');
  const beta = page.locator('input[aria-label="Beta"]');
  await alpha.waitFor();
  const shown = async (input) => Number(await input.inputValue());
  const probeState = (read) => page.evaluate(read);
  const refusals = [];
  /** Every refusal line so far, taken from the page's console errors as they come. */
  const refusalLines = () => {
    refusals.push(...consumeConsoleErrors('setParameter refused'));
    return refusals;
  };
  const releaseRefusal = () => page.evaluate(() => window.__paramProbe.state.pendingRefusals.shift()());
  const settle = () => page.evaluate(() => new Promise((r) => setTimeout(r, 50)));
  assert.equal(await shown(alpha), 0.25);
  assert.equal(await shown(beta), 0.5);

  // (0) an accepted edit stays where it was put, and nothing is logged.
  await beta.fill('0.7');
  await settle();
  assert.equal(await shown(beta), 0.7);
  assert.deepEqual(refusalLines(), []);

  // (1) a refused edit snaps back to the plugin's value (Alpha: 0.25, not the 0.9 dragged to).
  await probeState(() => window.__paramProbe.state.refuse.add(1));
  await alpha.fill('0.9');
  assert.equal(await shown(alpha), 0.9, 'optimistic until the host answers');
  await releaseRefusal();
  await page.waitForFunction(() => document.querySelector('input[aria-label="Alpha"]').value === '0.25');
  assert.equal(await page.locator('.param', { has: alpha }).locator('.param__val').textContent(), '0.25');
  assert.equal(refusalLines().length, 1, `one log line per refused set: ${JSON.stringify(refusalLines())}`);
  assert.match(refusalLines()[0], /slot 1, param 1/);

  // (2a) refused edit, then a newer ACCEPTED edit of the same param, then the refusal lands: 0.6 stays.
  await alpha.fill('0.8');
  await probeState(() => window.__paramProbe.state.refuse.delete(1));
  await alpha.fill('0.6');
  await settle();
  const listBefore = await probeState(() => window.__paramProbe.state.listCalls);
  await releaseRefusal();
  await settle();
  assert.equal(await shown(alpha), 0.6, 'a refusal never overrides a newer edit');
  assert.equal(await probeState(() => window.__paramProbe.state.live[1]), 0.6);
  assert.equal(await probeState(() => window.__paramProbe.state.listCalls), listBefore, 'a superseded refusal reads nothing');
  assert.equal(refusalLines().length, 2);

  // (2b) refused edit, then the plugin's own editor moves the param, then the refusal lands.
  await probeState(() => window.__paramProbe.state.refuse.add(1));
  await alpha.fill('0.1');
  await probeState(() => {
    const { state } = window.__paramProbe;
    state.live[1] = 0.4;
    state.paramCb({ slot: 0, id: 1, value: 0.4 });
  });
  await page.waitForFunction(() => document.querySelector('input[aria-label="Alpha"]').value === '0.4');
  await releaseRefusal();
  await settle();
  assert.equal(await shown(alpha), 0.4, 'a refusal never overrides an editor-originated change');
  assert.equal(refusalLines().length, 3);

  // (3) refused edit, then the slot's plugin changes (the drawer remounts), then the refusal lands:
  // it logs, reads nothing and the new drawer's sliders are untouched.
  await alpha.fill('0.95');
  await page.evaluate(async () => {
    const { instrument, descriptors } = window.__paramProbe;
    await instrument.selectPlugin(0, descriptors[1]);
  });
  await page.waitForFunction(() => window.__paramProbe.instrument.slotPlugins()[0]?.id === 'b');
  await page.locator('input[aria-label="Alpha"]').waitFor();
  await settle();
  const listAfterSwap = await probeState(() => window.__paramProbe.state.listCalls);
  const swapped = await shown(page.locator('input[aria-label="Alpha"]'));
  // The old plugin's call still in flight does not hold up the new plugin's first drag.
  await page.locator('input[aria-label="Beta"]').fill('0.55');
  await settle();
  assert.equal(await probeState(() => window.__paramProbe.state.live[2]), 0.55, 'a fresh queue per loaded plugin');
  await releaseRefusal();
  await settle();
  assert.equal(await probeState(() => window.__paramProbe.state.listCalls), listAfterSwap, 'no live-value read after a swap');
  assert.equal(await shown(page.locator('input[aria-label="Alpha"]')), swapped);
  assert.equal(swapped, 0.4, 'the new drawer shows the plugin\'s own value');
  assert.equal(refusalLines().length, 4);

  // (5) a wholesale refresh is in flight (listing 0.4) while the editor moves the param to 0.3 and a
  // drag to 0.9 is refused; the stale listing lands, then the refusal: the slider shows the plugin's 0.3.
  await probeState(() => {
    const { state } = window.__paramProbe;
    state.holdList = true;
    state.paramsCb(0);
  });
  await page.waitForFunction(() => typeof window.__paramProbe.state.releaseList === 'function');
  await probeState(() => {
    const { state } = window.__paramProbe;
    state.holdList = false;
    state.live[1] = 0.3;
    state.paramCb({ slot: 0, id: 1, value: 0.3 });
  });
  await page.waitForFunction(() => document.querySelector('input[aria-label="Alpha"]').value === '0.3');
  await page.locator('input[aria-label="Alpha"]').fill('0.9');
  await probeState(() => window.__paramProbe.state.releaseList());
  await settle();
  assert.equal(await shown(page.locator('input[aria-label="Alpha"]')), 0.9, 'a listing older than the drag leaves it');
  await releaseRefusal();
  await page.waitForFunction(() => document.querySelector('input[aria-label="Alpha"]').value === '0.3');
  assert.equal(refusalLines().length, 5);

  // (6) five drags across two params while the host holds every accepted set, each held set released
  // newest first: the plugin ends on each slider's last value, and only the newest waiting value per
  // param was sent (Alpha 0.2 in flight; then Beta 0.65 and Alpha 0.35).
  await probeState(() => {
    const { state } = window.__paramProbe;
    state.refuse.delete(1);
    state.holdAccepted = true;
    state.setCalls.length = 0;
  });
  const alphaNow = page.locator('input[aria-label="Alpha"]');
  const betaNow = page.locator('input[aria-label="Beta"]');
  for (const [input, value] of [[alphaNow, '0.2'], [betaNow, '0.6'], [alphaNow, '0.3'], [betaNow, '0.65'], [alphaNow, '0.35']]) {
    await input.fill(value);
  }
  for (;;) {
    await settle();
    const released = await probeState(() => {
      const held = window.__paramProbe.state.heldSets;
      if (held.length === 0) return false;
      while (held.length > 0) held.pop()();
      return true;
    });
    if (!released) break;
  }
  const ended = await probeState(() => {
    const { state } = window.__paramProbe;
    state.holdAccepted = false;
    return { live: { ...state.live }, sent: state.setCalls.map((c) => `${c.id}=${c.value}`) };
  });
  assert.deepEqual(ended.live, { 1: 0.35, 2: 0.65 }, `the plugin ends on the sliders' values (sent: ${ended.sent})`);
  assert.equal(await shown(alphaNow), 0.35);
  assert.equal(await shown(betaNow), 0.65);
  assert.deepEqual(ended.sent, ['1=0.2', '2=0.65', '1=0.35'], 'one set in flight, then the newest value per param');

  // (7a) while a dragged value waits for a param, the drag owns it: Alpha 0.2 in flight, Beta 0.7 waits,
  // then the plugin's own editor moves Beta to 0.9. The slider stays on 0.7 and 0.7 is sent, so the
  // plugin and the slider end equal.
  const releaseHeld = async () => {
    for (;;) {
      await settle();
      const released = await probeState(() => {
        const held = window.__paramProbe.state.heldSets;
        if (held.length === 0) return false;
        while (held.length > 0) held.shift()();
        return true;
      });
      if (!released) break;
    }
  };
  await probeState(() => {
    const { state } = window.__paramProbe;
    state.holdAccepted = true;
    state.setCalls.length = 0;
  });
  await alphaNow.fill('0.2');
  await betaNow.fill('0.7');
  await probeState(() => {
    const { state } = window.__paramProbe;
    state.live[2] = 0.9;
    state.paramCb({ slot: 0, id: 2, value: 0.9 });
  });
  await settle();
  assert.equal(await shown(betaNow), 0.7, 'a plugin-side change does not move a slider whose drag waits');
  await releaseHeld();
  const edited = await probeState(() => {
    const { state } = window.__paramProbe;
    return { live: { ...state.live }, sent: state.setCalls.map((c) => `${c.id}=${c.value}`) };
  });
  assert.equal(edited.live[2], 0.7, `the waiting drag lands over the editor's change (sent: ${edited.sent})`);
  assert.equal(await shown(betaNow), 0.7);
  assert.deepEqual(edited.sent, ['1=0.2', '2=0.7']);

  // (7b) nothing waits, so each plugin-side change moves the slider, in order: Beta 0.2 then 0.85 both
  // land, then the plugin echoes both (a VST2 calls audioMasterAutomate from inside its setter, the value
  // back through f32; the native set only queues, so the echoes come after the replies). The stale echo
  // moves the slider for a moment; the newest lands last, so the plugin and the slider end on 0.85.
  await probeState(() => window.__paramProbe.state.setCalls.length = 0);
  await betaNow.fill('0.2');
  await betaNow.fill('0.85');
  await releaseHeld();
  await probeState(() => {
    const { paramCb } = window.__paramProbe.state;
    paramCb({ slot: 0, id: 2, value: Math.fround(0.2) });
    paramCb({ slot: 0, id: 2, value: Math.fround(0.85) });
  });
  await settle();
  const echoed = await probeState(() => {
    const { state } = window.__paramProbe;
    return { live: { ...state.live }, sent: state.setCalls.map((c) => `${c.id}=${c.value}`) };
  });
  assert.equal(echoed.live[2], 0.85, `the plugin ends on the last drag (sent: ${echoed.sent})`);
  assert.ok(Math.abs((await shown(betaNow)) - 0.85) < 1e-6, 'the newest echo lands last on the slider');
  assert.deepEqual(echoed.sent, ['2=0.2', '2=0.85']);

  // (8) the waiting drag owns its param against a refresh too: Alpha 0.45 in flight, Beta 0.8 waits, a
  // preset moves Beta to 0.96 and its listing (which reads 0.96) is held; Alpha settles during the
  // listing, so Beta 0.8 is sent, then the listing lands. The slider stays on 0.8 and the plugin ends on it.
  await probeState(() => window.__paramProbe.state.setCalls.length = 0);
  await alphaNow.fill('0.45');
  await betaNow.fill('0.8');
  await probeState(() => {
    const { state } = window.__paramProbe;
    state.releaseList = null;
    state.holdList = true;
    state.live[2] = 0.96;
    state.paramsCb(0);
  });
  await page.waitForFunction(() => typeof window.__paramProbe.state.releaseList === 'function');
  await probeState(() => window.__paramProbe.state.holdList = false);
  await releaseHeld();
  await probeState(() => window.__paramProbe.state.releaseList());
  await settle();
  const during = await probeState(() => {
    const { state } = window.__paramProbe;
    return { live: { ...state.live }, sent: state.setCalls.map((c) => `${c.id}=${c.value}`) };
  });
  assert.equal(during.live[2], 0.8, `a drag waiting at a preset load is still sent (sent: ${during.sent})`);
  assert.equal(await shown(betaNow), 0.8, 'the preset\'s listing leaves a waiting drag\'s slider');
  assert.deepEqual(during.sent, ['1=0.45', '2=0.8']);

  // (9) a refresh that changes no value (a param title changed) while a value waits: Beta 0.3 in flight,
  // Beta 0.35 waits, the plugin reports a wholesale change and its listing (Beta 0.8) lands at once. The
  // slider stays on 0.35, and 0.35 is still sent.
  await probeState(() => window.__paramProbe.state.setCalls.length = 0);
  await betaNow.fill('0.3');
  await betaNow.fill('0.35');
  await probeState(() => window.__paramProbe.state.paramsCb(0));
  await settle();
  assert.equal(await shown(betaNow), 0.35, 'a refresh leaves a waiting drag\'s slider');
  await releaseHeld();
  const retitled = await probeState(() => {
    const { state } = window.__paramProbe;
    state.holdAccepted = false;
    return { live: { ...state.live }, sent: state.setCalls.map((c) => `${c.id}=${c.value}`) };
  });
  assert.equal(retitled.live[2], 0.35, `a refresh does not drop a waiting drag (sent: ${retitled.sent})`);
  assert.equal(await shown(betaNow), 0.35);
  assert.deepEqual(retitled.sent, ['2=0.3', '2=0.35']);

  // (4) the page saw only the refusal lines as errors (the harness fails on any other) and none unhandled.
  console.log(`refusals logged: ${JSON.stringify(refusalLines().map((t) => t.split("\n")[0]))}`);
}, { launch: {} });

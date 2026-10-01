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
 * rejection remains (`probe` fails on any).
 *
 * Cannot see the native `plugin_set_param` (what makes it refuse: a full event ring, an unlisted param
 * id), WebView2, or real timing.
 * Run: pnpm probe plugin-param-refused
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
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
    const state = { live, setCalls: [], listCalls: 0, pendingRefusals: [], refuse: new Set(), paramCb: null };
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
    // A param in `refuse` rejects, each rejection held until the probe releases it; others land.
    host.setParameter = (slot, id, value) => {
      state.setCalls.push({ slot, id, value });
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

  await page.getByRole('button', { name: /Plugin parameters for slot 1/ }).click();
  const alpha = page.locator('input[aria-label="Alpha"]');
  const beta = page.locator('input[aria-label="Beta"]');
  await alpha.waitFor();
  const shown = async (input) => Number(await input.inputValue());
  const probeState = (read) => page.evaluate(read);
  const refusalLines = () => consoleErrors.filter((t) => t.includes('setParameter refused'));
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

  // (4) the page saw only the refusal lines as errors: none unhandled (probe also fails on pageerror).
  assert.equal(consoleErrors.length, 5, `no other console error: ${JSON.stringify(consoleErrors)}`);
  console.log(`refusals logged: ${JSON.stringify(refusalLines().map((t) => t.split("\n")[0]))}`);
}, { launch: {} });

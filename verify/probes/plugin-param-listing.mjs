/**
 * A plugin's param sliders show the newest state when listings and editor changes race
 * (`PluginParams` in `src/ui/instrument/PluginControls.tsx`), on the web engine fake with the native slot
 * chrome on (host.web.ts served with `available: true`, the plugin-param-refused pattern) and the plugin
 * host methods replaced in the page: `listParams` reports the "plugin" state of its call and answers only
 * when the probe releases it, so the probe picks the order in which listings land.
 *
 * Proves: (1) a knob turned in the plugin's editor while the drawer's first listing is in flight shows
 * its newer value once that listing lands; (2) when two wholesale refreshes overlap (two quick preset
 * loads) and the older listing lands last, the newer listing's params and values stay; the stale
 * listing changes nothing; (3) no console error and no uncaught page error (`probe` fails on any).
 *
 * Cannot see the native `plugin_list_params` / param events (their real order and timing), WebView2,
 * or a listing that fails.
 * Run: pnpm probe plugin-param-listing
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
    const desc = { id: 'a', name: 'Probe a', path: 'C:\\probe\\a.vst3', format: 'vst3', isEffect: false };
    const host = platform.pluginHost;

    // The "plugin": its param set and live values. A listing snapshots them at its call and waits in
    // `pending` until the probe releases it.
    const state = {
      params: [{ id: 1, name: 'Alpha' }, { id: 2, name: 'Beta' }],
      live: { 1: 0.25, 2: 0.5 },
      pending: [],
      paramCbs: new Set(),
      paramsCbs: new Set(),
    };
    host.scanPlugins = async () => [desc];
    host.loadPlugin = async (slot) => ({ slot, descriptor: desc });
    host.unloadPlugin = async () => {};
    host.openEditor = async () => {};
    host.closeEditor = async () => {};
    host.setParameter = async () => {};
    host.listParams = () => {
      const listed = state.params.map(({ id, name }) =>
        ({ id, name, minValue: 0, maxValue: 1, defaultValue: 0.5, value: state.live[id] }));
      return new Promise((resolve) => state.pending.push(() => resolve(listed)));
    };
    host.onParamChanged = (cb) => (state.paramCbs.add(cb), () => state.paramCbs.delete(cb));
    host.onParamsChanged = (cb) => (state.paramsCbs.add(cb), () => state.paramsCbs.delete(cb));
    // The plugin's editor moves a knob / loads a preset: its state changes, then it reports.
    state.editorTurn = (id, value) => {
      state.live[id] = value;
      for (const cb of state.paramCbs) cb({ slot: 0, id, value });
    };
    state.presetLoad = (params, live) => {
      state.params = params;
      state.live = live;
      for (const cb of state.paramsCbs) cb(0);
    };
    await instrument.scanForPlugins();
    await instrument.selectPlugin(0, desc);
    window.__listProbe = state;
  });

  const shown = async (name) => Number(await page.locator(`input[aria-label="${name}"]`).inputValue());
  const run = (fn, ...args) => page.evaluate(fn, ...args);
  const pendingListings = () => run(() => window.__listProbe.pending.length);
  const release = (i) => run((i) => window.__listProbe.pending.splice(i, 1)[0](), i);
  const settle = () => run(() => new Promise((r) => setTimeout(r, 50)));
  // Each case's verdict is collected, so one red case does not hide the other's.
  const failures = [];
  const check = (label, actual, want) => {
    const ok = JSON.stringify(actual) === JSON.stringify(want);
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${label}: got ${JSON.stringify(actual)}, want ${JSON.stringify(want)}`);
    if (!ok) failures.push(label);
  };

  // (1) The drawer's first listing (Alpha 0.25) is in flight when the editor turns Alpha to 0.6.
  await page.getByRole('button', { name: /Plugin parameters for slot 1/ }).click();
  await page.waitForFunction(() => window.__listProbe?.pending.length === 1);
  await run(() => window.__listProbe.editorTurn(1, 0.6));
  await settle();
  await release(0);
  await page.locator('input[aria-label="Alpha"]').waitFor();
  await settle();
  check('(1) an editor turn newer than the first listing wins [Alpha, Beta]',
    [await shown('Alpha'), await shown('Beta')], [0.6, 0.5]);

  // (2) Two quick preset loads: listing A (Alpha 0.7, Beta 0.2), then listing B (a new param set:
  // Alpha 0.1, Beta 0.9, Gamma 0.4). B lands first, then the stale A.
  await run(() => window.__listProbe.presetLoad(
    [{ id: 1, name: 'Alpha' }, { id: 2, name: 'Beta' }], { 1: 0.7, 2: 0.2 }));
  await run(() => window.__listProbe.presetLoad(
    [{ id: 1, name: 'Alpha' }, { id: 2, name: 'Beta' }, { id: 3, name: 'Gamma' }], { 1: 0.1, 2: 0.9, 3: 0.4 }));
  await page.waitForFunction(() => window.__listProbe.pending.length === 2);
  await release(1);
  await page.locator('input[aria-label="Gamma"]').waitFor();
  await settle();
  assert.deepEqual([await shown('Alpha'), await shown('Beta'), await shown('Gamma')], [0.1, 0.9, 0.4]);
  await release(0);
  await settle();
  const after = {
    alpha: await shown('Alpha'),
    beta: await shown('Beta'),
    gamma: await page.locator('input[aria-label="Gamma"]').count(),
  };
  check('(2) a stale listing landing last changes nothing', after, { alpha: 0.1, beta: 0.9, gamma: 1 });
  assert.equal(await pendingListings(), 0);
  assert.deepEqual(failures, [], 'every case holds');

  // (3) nothing logged as an error (probe also fails on pageerror / unhandled rejection).
  assert.deepEqual(consoleErrors, [], `no console error: ${JSON.stringify(consoleErrors)}`);
}, { launch: {} });

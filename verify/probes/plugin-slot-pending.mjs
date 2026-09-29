/**
 * Plugin slot pending UI, on the web engine fake (`src/platform/host.web.ts`, the engine-seam pattern)
 * with the native slot chrome on (host.web.ts served with `available: true`) and deferred host replies:
 * a deferred swap/unload/load keeps the slot's busy label honest and its source controls locked while
 * the other slot stays usable; two installed copies of one plugin (same id, other path) are two choices
 * and picking the second swaps to its path; a rejected operation still decrements the pending count and
 * restores interaction; programmatic queue callers count at enqueue time (no enabled frame between
 * operations) and slot independence holds; an error thrown inside the queued operation itself still
 * releases its pending count; a load the host rejects leaves the slot empty and a retry after it lands
 * its own plugin; and an effect's automatic GO LIVE runs only once its source load is back
 * (`SetSlotLive` to the engine), then its editor opens and the picker unlocks.
 *
 * Cannot see the native host's teardown or any timing; the engine answers nothing (GO LIVE there is one
 * command, so it no longer holds the slot pending the way the web path's arm did).
 * Run: pnpm probe plugin-slot-pending [--screenshot=<path>]
 */
import assert from 'node:assert/strict';
import { probe, arg } from '../harness/probe.ts';

const screenshot = arg('screenshot');

await probe(async ({ open }) => {
  const { page } = await open({
    viewport: { width: 1280, height: 820 },
    // Render the real native-only slot chrome in the browser tier, in engine mode. The host methods
    // themselves are replaced below before any operation is driven.
    init: async (p) => {
      await p.route('**/src/platform/host.web.ts', async (route) => {
        const response = await route.fetch();
        const body = (await response.text()).replace('available: false', 'available: true');
        await route.fulfill({ response, body });
      });
      await p.addInitScript(() => void (window.__lfEngineFake = true));
    },
  });

  const setup = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const slots = await import('/src/ui/state/instrument-slots.ts');
    const descriptorIdentity = await import('/src/ui/state/plugin-descriptor.ts');
    const deadline = Date.now() + 10000;
    while (!instrument.nativeHostReady() || instrument.scanning()) {
      if (Date.now() > deadline) throw new Error('native host boot did not finish');
      await new Promise((r) => setTimeout(r, 20));
    }
    const descriptors = ['a', 'b', 'c', 'fx'].map((id) => ({
      id,
      name: `Probe ${id.toUpperCase()}`,
      path: `C:\\probe\\${id}.vst3`,
      format: 'vst3',
      isEffect: id === 'fx',
    }));
    descriptors.push({
      ...descriptors[0],
      path: 'C:\\probe\\vendor\\a.vst3',
    });

    platform.pluginHost.scanPlugins = async () => descriptors;
    const loadCalls = [];
    platform.pluginHost.loadPlugin = async (slot, path, id) => {
      loadCalls.push({ slot, path, id });
      return { slot, descriptor: descriptors.find((d) => d.path === path && d.id === id) };
    };
    platform.pluginHost.unloadPlugin = async () => {};
    platform.pluginHost.openEditor = async () => {};
    platform.pluginHost.closeEditor = async () => {};
    platform.pluginHost.listParams = async () => [];
    await instrument.scanForPlugins();
    await instrument.selectPlugin(0, descriptors[0]);

    window.__slotPendingProbe = {
      platform,
      instrument,
      slots,
      descriptors,
      descriptorKey: descriptorIdentity.pluginDescriptorKey,
      loadCalls,
      releases: [],
    };
    return {
      slot0: instrument.slotPlugins()[0]?.id,
      pending: [...slots.slotPendingCounts()],
      keys: descriptors.map(descriptorIdentity.pluginDescriptorKey),
    };
  });
  const { keys } = setup;
  assert.deepEqual({ slot0: setup.slot0, pending: setup.pending }, { slot0: 'a', pending: [0, 0] });
  await page.waitForSelector('.slot__source');

  // The host identity is (format, path, id), not id alone. Both installed copies must render as
  // distinct choices, and choosing the second copy must perform a real swap to its path.
  const duplicateOptions = await page.locator('.slot__source').first().locator('option').evaluateAll(
    (options) => options
      .filter((option) => option.textContent?.includes('Probe A'))
      .map((option) => ({ value: option.value, text: option.textContent, title: option.title })),
  );
  assert.deepEqual(duplicateOptions.map((option) => option.value), [keys[0], keys[4]]);
  assert.deepEqual(duplicateOptions.map((option) => option.text), [
    'Probe A (vst3) · probe',
    'Probe A (vst3) · vendor',
  ]);
  await page.locator('.slot__source').first().selectOption(keys[4]);
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 0);
  assert.equal(await page.locator('.slot__source').first().inputValue(), keys[4]);
  assert.equal(
    await page.evaluate(() => window.__slotPendingProbe.loadCalls.at(-1)?.path),
    'C:\\probe\\vendor\\a.vst3',
  );
  await page.locator('.slot__source').first().selectOption(keys[0]);
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 0);

  const deferNext = async (kind) => page.evaluate((operation) => {
    const probe = window.__slotPendingProbe;
    let release;
    const promise = new Promise((resolve, reject) => { release = { resolve, reject }; });
    probe.releases.push(release);
    probe.platform.pluginHost[operation] = async () => promise;
  }, kind);
  const release = async (method, value) => page.evaluate(({ methodName, result }) => {
    window.__slotPendingProbe.releases.shift()[methodName](result);
  }, { methodName: method, result: value });
  const slotUi = async (slot) => page.locator('.slot').nth(slot).evaluate((el) => ({
    busy: el.getAttribute('aria-busy'),
    name: el.querySelector('.slot__source')?.selectedOptions[0]?.textContent?.trim(),
    pickerDisabled: el.querySelector('.slot__source')?.disabled,
    enabledSourceButtons: [...el.querySelectorAll('.slot__acts button')].filter((button) => !button.disabled).length,
  }));

  // Deferred swap: the outgoing descriptor disappears before native unload resolves, but the slot
  // stays honestly labelled and none of its source controls can enqueue another user action.
  await deferNext('unloadPlugin');
  await page.locator('.slot__source').first().selectOption(keys[1], { noWaitAfter: true });
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 1);
  assert.deepEqual(await slotUi(0), {
    busy: 'true', name: 'Updating…',
    pickerDisabled: true, enabledSourceButtons: 0,
  });
  assert.equal((await slotUi(1)).pickerDisabled, false, 'the other slot must remain usable');
  assert.ok(await page.locator('.slot__source').first().evaluate((el) => el.getBoundingClientRect().width >= 120),
    'pending label must remain readable at the default window size');
  if (screenshot) await page.screenshot({ path: screenshot, fullPage: true });
  await release('resolve');
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 0);
  assert.equal((await slotUi(0)).pickerDisabled, false);
  assert.equal(await page.locator('.slot__source').first().inputValue(), keys[1]);

  // Deferred unload to synth uses the same pending surface and unlocks after completion.
  await deferNext('unloadPlugin');
  await page.locator('.slot__source').first().selectOption('lead', { noWaitAfter: true });
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 1);
  assert.equal((await slotUi(0)).pickerDisabled, true);
  await release('resolve');
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 0);
  assert.equal((await slotUi(0)).pickerDisabled, false);

  // A rejected operation must decrement in finally and restore interaction.
  await deferNext('loadPlugin');
  await page.locator('.slot__source').first().selectOption(keys[0], { noWaitAfter: true });
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 1);
  await release('reject', 'injected load failure');
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 0);
  assert.equal((await slotUi(0)).pickerDisabled, false);

  // Programmatic callers retain queue semantics. Counting at enqueue time prevents an enabled frame
  // between operations, and slot 1 remains independent.
  const queued = await page.evaluate(async () => {
    const probe = window.__slotPendingProbe;
    let releaseFirst;
    probe.platform.pluginHost.loadPlugin = (_slot, _path, id) => id === 'b'
      ? new Promise((resolve) => { releaseFirst = resolve; })
      : Promise.reject(new Error('injected queued failure'));
    const first = probe.instrument.selectPlugin(0, probe.descriptors[1]);
    const second = probe.instrument.selectPlugin(0, probe.descriptors[2]);
    await new Promise((resolve) => setTimeout(resolve, 0));
    const before = [...probe.slots.slotPendingCounts()];
    releaseFirst();
    await first;
    const between = [...probe.slots.slotPendingCounts()];
    await second;
    return { before, between, after: [...probe.slots.slotPendingCounts()] };
  });
  assert.deepEqual(queued, { before: [2, 0], between: [1, 0], after: [0, 0] });

  // Errors that escape the operation itself must also release its pending count and leave the
  // next queued operation runnable (plugin-load errors above are caught by the instrument).
  const thrown = await page.evaluate(async () => {
    const { slots } = window.__slotPendingProbe;
    const failed = slots.serializeSlot(1, () => { throw new Error('queue failure'); });
    const caught = failed.catch(error => error.message);
    const next = slots.serializeSlot(1, async () => [...slots.slotPendingCounts()]);
    return { error: await caught, next: await next, after: [...slots.slotPendingCounts()] };
  });
  assert.deepEqual(thrown, { error: 'queue failure', next: [0, 1], after: [0, 0] });

  // A load the host rejects leaves the slot empty; a retry into the same slot after it lands its own
  // plugin (slot 1, so slot 0's state above and below is untouched).
  const loadStates = await page.evaluate(async () => {
    const { platform, instrument } = window.__slotPendingProbe;
    const desc = (id) => ({ id, name: `Probe ${id}`, path: `C:\\probe\\${id}.vst3`, format: 'vst3', isEffect: false });
    const waitFor = async (read) => {
      for (let i = 0; i < 100 && !read(); i++) await new Promise((resolve) => setTimeout(resolve, 0));
      if (!read()) throw new Error('instrumented load was not called');
    };
    const load = platform.pluginHost.loadPlugin;
    let rejectLoad;
    platform.pluginHost.loadPlugin = () => new Promise((_, reject) => { rejectLoad = reject; });
    const failedPick = instrument.selectPlugin(1, desc('failed-a'));
    await waitFor(() => rejectLoad);
    rejectLoad(new Error('injected native load timeout'));
    await failedPick;
    const afterFailedLoad = instrument.slotPlugins()[1]?.id ?? null;
    let resolveRetry;
    platform.pluginHost.loadPlugin = (slot, _path, id) =>
      new Promise((resolve) => { resolveRetry = () => resolve({ slot, descriptor: desc(id) }); });
    const retry = instrument.selectPlugin(1, desc('retry-b'));
    await waitFor(() => resolveRetry);
    resolveRetry();
    await retry;
    const afterRetry = instrument.slotPlugins()[1]?.id ?? null;
    platform.pluginHost.loadPlugin = load;
    await instrument.clearPlugin(1);
    return { afterFailedLoad, afterRetry };
  });
  assert.deepEqual(loadStates, { afterFailedLoad: null, afterRetry: 'retry-b' },
    'a failed load leaves the slot empty, and a retry after it lands its own plugin');

  // An effect's automatic GO LIVE is deliberately queued from PluginBar.onMount behind its source
  // load: it goes live only once the load is back, then the editor opens and the picker unlocks.
  await page.evaluate(() => {
    const probe = window.__slotPendingProbe;
    probe.platform.pluginHost.loadPlugin = (slot, path, id) => new Promise((resolve) => {
      probe.loadRelease = () => resolve({ slot, descriptor: probe.descriptors.find((d) => d.path === path && d.id === id) });
    });
    probe.platform.pluginHost.openEditor = async () => { probe.autoEditorOpened = true; };
    window.__lf.native.sent.length = 0;
  });
  await page.locator('.slot__source').first().selectOption(keys[3], { noWaitAfter: true });
  await page.waitForFunction(() => !!window.__slotPendingProbe.loadRelease);
  assert.equal((await slotUi(0)).pickerDisabled, true);
  const liveSent = () => page.evaluate(() => window.__lf.native.sent.filter((c) => c.SetSlotLive).map((c) => c.SetSlotLive));
  assert.deepEqual(await liveSent(), [], 'no GO LIVE while the source load runs');
  await page.evaluate(() => window.__slotPendingProbe.loadRelease());
  await page.waitForFunction(() => window.__slotPendingProbe.autoEditorOpened === true);
  assert.deepEqual(await liveSent(), [[0, true]], 'the effect goes live once its load is back');
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 0);
  assert.equal((await slotUi(0)).pickerDisabled, false);
}, { launch: {} });

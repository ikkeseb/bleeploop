// Browser regression for the plugin slot's pending UI. It drives the production slot queue and
// rendered controls with deferred host replies; it makes no native teardown or timing claim.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const screenshot = process.argv.find((arg) => arg.startsWith('--screenshot='))?.slice(13);
const browser = await chromium.launch();

try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 820 } });
  // Render the real native-only slot chrome in the browser tier. The host methods themselves are
  // replaced below before any operation is driven.
  await page.route('**/src/platform/host.web.ts', async (route) => {
    const response = await route.fetch();
    const body = (await response.text()).replace('available: false', 'available: true');
    await route.fulfill({ response, body });
  });
  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf);

  const setup = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/audio/instrument.ts');
    const slots = await import('/src/audio/instrument-slots.ts');
    const descriptors = ['a', 'b', 'c', 'fx'].map((id) => ({
      id,
      name: `Probe ${id.toUpperCase()}`,
      path: `C:\\probe\\${id}.vst3`,
      format: 'vst3',
      isEffect: id === 'fx',
    }));

    platform.pluginHost.available = true;
    platform.pluginHost.scanPlugins = async () => descriptors;
    platform.pluginHost.loadPlugin = async (slot, _path, id) => ({ slot, descriptor: descriptors.find((d) => d.id === id) });
    platform.pluginHost.unloadPlugin = async () => {};
    platform.pluginHost.openEditor = async () => {};
    platform.pluginHost.closeEditor = async () => {};
    platform.pluginHost.listParams = async () => [];
    slots.setNativeHostReady(true);
    await instrument.scanForPlugins();
    await instrument.selectPlugin(0, descriptors[0]);

    window.__slotPendingProbe = { platform, instrument, slots, descriptors, releases: [] };
    return { slot0: instrument.slotPlugins()[0]?.id, pending: [...slots.slotPendingCounts()] };
  });
  assert.deepEqual(setup, { slot0: 'a', pending: [0, 0] });
  await page.waitForSelector('.slot__select');

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
    source: el.querySelector('.slot__k')?.textContent?.trim(),
    name: el.querySelector('.slot__name')?.textContent?.trim(),
    pickerDisabled: el.querySelector('.slot__select')?.disabled,
    enabledSourceButtons: [...el.querySelectorAll('.seg button')].filter((button) => !button.disabled).length,
  }));

  // Deferred swap: the outgoing descriptor disappears before native unload resolves, but the slot
  // stays honestly labelled and none of its source controls can enqueue another user action.
  await deferNext('unloadPlugin');
  await page.locator('.slot__select').first().selectOption('b', { noWaitAfter: true });
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 1);
  assert.deepEqual(await slotUi(0), {
    busy: 'true', source: 'Source', name: 'Updating…',
    pickerDisabled: true, enabledSourceButtons: 0,
  });
  assert.equal((await slotUi(1)).pickerDisabled, false, 'the other slot must remain usable');
  assert.equal(await page.locator('.slot__name').first().evaluate(el => el.scrollWidth <= el.clientWidth),
    true, 'pending label must remain readable at the default window size');
  if (screenshot) await page.screenshot({ path: screenshot, fullPage: true });
  await release('resolve');
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 0);
  assert.equal((await slotUi(0)).pickerDisabled, false);
  assert.equal(await page.locator('.slot__select').first().inputValue(), 'b');

  // Deferred unload to synth uses the same pending surface and unlocks after completion.
  await deferNext('unloadPlugin');
  await page.locator('.slot__select').first().selectOption('', { noWaitAfter: true });
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 1);
  assert.equal((await slotUi(0)).pickerDisabled, true);
  await release('resolve');
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 0);
  assert.equal((await slotUi(0)).pickerDisabled, false);

  // A rejected operation must decrement in finally and restore interaction.
  await deferNext('loadPlugin');
  await page.locator('.slot__select').first().selectOption('a', { noWaitAfter: true });
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

  // An effect's automatic GO LIVE is deliberately queued from PluginBar.onMount behind its source
  // load. It must keep the same pending surface alive without disabling the internal auto-start.
  await page.evaluate(() => {
    const probe = window.__slotPendingProbe;
    probe.platform.pluginHost.loadPlugin = () => new Promise((resolve) => {
      probe.loadRelease = resolve;
    });
    probe.platform.pluginHost.armInput = () => new Promise((resolve) => {
      probe.armRelease = resolve;
    });
    probe.platform.pluginHost.armMonitor = async () => {};
    probe.platform.pluginHost.setMonitorGain = async () => {};
    probe.platform.pluginHost.openEditor = async () => { probe.autoEditorOpened = true; };
  });
  await page.locator('.slot__select').first().selectOption('fx', { noWaitAfter: true });
  await page.waitForFunction(() => !!window.__slotPendingProbe.loadRelease);
  assert.equal((await slotUi(0)).pickerDisabled, true);
  await page.evaluate(() => window.__slotPendingProbe.loadRelease());
  await page.waitForFunction(() => !!window.__slotPendingProbe.armRelease);
  assert.equal(
    await page.evaluate(() => window.__slotPendingProbe.slots.slotPendingCounts()[0]),
    1,
    'auto GO LIVE must remain pending after its source load leaves the queue',
  );
  assert.equal((await slotUi(0)).pickerDisabled, true);
  await page.evaluate(() => window.__slotPendingProbe.armRelease());
  await page.waitForFunction(() => window.__slotPendingProbe.slots.slotPendingCounts()[0] === 0);
  assert.equal((await slotUi(0)).pickerDisabled, false);

  await page.waitForFunction(() => window.__slotPendingProbe.autoEditorOpened === true);
  console.log('PASS: pending swap/unload, failure recovery, queue ownership and automatic live/editor startup');
} finally {
  await browser.close();
}

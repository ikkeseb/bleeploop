/**
 * The real startup and settings orchestration (`bootEngine` in `src/app/boot.ts`,
 * `src/ui/state/audio-devices.ts`) on the web engine fake (`src/platform/host.web.ts`, the engine-seam
 * pattern) with an instrumented plugin host installed before the app boots (host.web.ts served with
 * `available: true`):
 *
 * - the saved buffer size and driver choice reach the engine's device open before plugins become
 *   selectable, and a manual rescan made while the host starts does not bypass startup;
 * - the last of two buffer picks is the one shown and saved;
 * - under ASIO the input and output device selects are disabled and name the ASIO device, and a slot's
 *   input pick offers the ASIO device's inputs; pruning an unplugged Windows endpoint leaves the slots'
 *   input picks alone;
 * - a startup whose plugin host fails says so, leaves native selection closed and a following manual
 *   scan does nothing.
 *
 * Cannot see the native host, the engine or a driver. logs/layout/audio-settings.png shows the panel.
 * Run: pnpm probe audio-settings-startup
 */
import { probe } from '../harness/probe.ts';
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';

/** Runs before the app: this launch's host, which logs its calls, holds `init` until `releaseInit()`
 * and fails it when `window.__startupFail` is set. */
function installHost() {
  const calls = [];
  let releaseInit = () => {};
  window.__startup = { calls, releaseInit: () => releaseInit() };
  window.__startupHost = {
    init: () => {
      calls.push('init');
      if (window.__startupFail) return Promise.reject(new Error('Injected failed startup'));
      return new Promise((resolve) => (releaseInit = resolve));
    },
    listLoaded: async () => [],
    listInputDevices: async () => [],
    listOutputDevices: async () => [],
    asioStatus: async () => ({ status: 'ready', detail: '' }),
    asioProbe: async () => ({ status: 'ready', detail: '' }),
    asioDeviceInfo: async () => ({ name: 'Probe ASIO', inputChannels: 4, outputChannels: 2, bufferMin: null, bufferMax: null }),
    scanPlugins: async () => { calls.push('scan'); return []; },
  };
}

/** Serve host.web.ts with the plugin host on, this launch's host methods over it and the engine
 * fake's device open logged in the same call list. */
const serveHost = (p) => p.route('**/src/platform/host.web.ts', async (route) => {
  const response = await route.fetch();
  const body = (await response.text()).replace('available: false', 'available: true') + `
Object.assign(webPluginHost, window.__startupHost ?? {});
const __probeOpen = webEngineFake.open;
webEngineFake.open = (request, force) => {
  window.__startup?.calls.push('open:' + request.backend + ':' + request.buffer);
  return __probeOpen(request, force);
};
`;
  await route.fulfill({ response, body });
});

await probe(async ({ open }) => {
  const { page } = await open({
    viewport: { width: 960, height: 600 },
    init: async (p) => {
      await serveHost(p);
      await p.addInitScript(() => {
        window.__lfEngineFake = true;
        localStorage.setItem('lf.audioDevices', JSON.stringify({ bufferFrames: 128, asioEnabled: false }));
      });
      await p.addInitScript(installHost);
    },
  });
  await page.waitForFunction(() => window.__startup.calls.includes('init'), undefined, { timeout: 10000 });
  const result = await page.evaluate(async () => {
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const instrument = await import('/src/ui/state/instrument.ts');
    const settings = await import('/src/ui/state/audio-devices.ts');
    const { readAudioDeviceSettings, writeAudioDeviceSettings } = await import('/src/ui/state/audio-settings.ts');
    const { calls } = window.__startup;
    await window.__lf.scanForPlugins(); // A manual rescan while the host starts must not bypass startup.
    const earlyScan = calls.includes('scan');
    window.__startup.releaseInit();
    const deadline = Date.now() + 10000;
    while (!instrument.nativeHostReady() || instrument.scanning() || !calls.includes('scan')) {
      if (Date.now() > deadline) throw new Error('native host boot did not finish');
      await pause(20);
    }
    const startup = [...calls];

    await Promise.all([settings.setBufferSize(64), settings.setBufferSize(256)]);
    const final = { shown: settings.bufferFrames(), saved: readAudioDeviceSettings().bufferFrames };

    await settings.setAsioEnabled(true);
    writeAudioDeviceSettings({ inputDeviceId: 'unplugged-windows-endpoint', inputChannel: '1' });
    settings.saveSlotInputChannels(['1', '']);
    await settings.refreshAndPruneDevices();
    const s = readAudioDeviceSettings();
    const afterWindowsPrune = { inputChannel: s.inputChannel, slotInputChannels: s.slotInputChannels };

    instrument.selectOff(0);
    while (instrument.slotPendingCounts()[0] > 0) await pause(10);
    window.__lf.ui.openSettings();
    await pause(100);
    const input = document.querySelector('[aria-label="Audio input device"]');
    const output = document.querySelector('[aria-label="Output device"]');
    const slotInput = document.querySelector('[aria-label="Input for slot 1"]');
    const asioUi = {
      inputDisabled: input.disabled,
      outputDisabled: output.disabled,
      name: input.selectedOptions[0].textContent,
      slotInputs: slotInput ? slotInput.options.length - 1 : null,
    };
    window.__lf.ui.closeSettings();
    return { startup, earlyScan, final, afterWindowsPrune, asioUi };
  });
  console.log(JSON.stringify(result));
  assert.equal(result.earlyScan, false, 'a manual rescan must not bypass startup');
  assert.deepEqual(result.startup, ['open:Wasapi:128', 'init', 'scan'],
    'the saved buffer and driver reach the device open before plugins become selectable');
  assert.deepEqual(result.final, { shown: 256, saved: 256 }, 'the last buffer pick is shown and saved');
  assert.deepEqual(result.asioUi, { inputDisabled: true, outputDisabled: true, name: 'Probe ASIO', slotInputs: 4 });
  assert.deepEqual(result.afterWindowsPrune, { inputChannel: '1', slotInputChannels: ['1', ''] },
    'pruning an unrelated Windows endpoint must not reset the ASIO input picks');
  await page.evaluate(() => {
    for (const toast of window.__lf.notify.toasts()) window.__lf.notify.dismissToast(toast.id);
    window.__lf.ui.openSettings();
  });
  await mkdir('logs/layout', { recursive: true });
  await page.screenshot({ path: 'logs/layout/audio-settings.png' });

  // A launch whose plugin host fails to start.
  const failed = await open({
    viewport: { width: 960, height: 600 },
    init: async (p) => {
      await serveHost(p);
      await p.addInitScript(() => {
        window.__lfEngineFake = true;
        window.__startupFail = true;
      });
      await p.addInitScript(installHost);
    },
  });
  await failed.page.waitForFunction(() => window.__lf.notify.toasts().some((t) => t.message === 'The audio engine failed to start'), undefined, { timeout: 10000 });
  const blocked = await failed.page.evaluate(async () => {
    const instrument = await import('/src/ui/state/instrument.ts');
    await window.__lf.scanForPlugins();
    return { ready: instrument.nativeHostReady(), init: window.__startup.calls.includes('init'), scanned: window.__startup.calls.includes('scan') };
  });
  console.log(JSON.stringify({ blocked }));
  assert.deepEqual(blocked, { ready: false, init: true, scanned: false }, 'a failed startup blocks the native host and a following manual scan');
}, { launch: {} });

/**
 * Real startup/settings orchestration with an instrumented platform host: a saved buffer size and
 * ASIO preference reach the host before plugins become selectable; a manual rescan does not bypass
 * startup; a rejected buffer/driver write leaves the shown and saved settings unchanged; concurrent
 * buffer writes serialize to the host one at a time; pruning an unplugged Windows endpoint leaves an
 * unrelated ASIO channel alone; a failed startup blocks the native host and a following manual scan.
 * No hardware claims. Run: pnpm probe audio-settings-startup
 */
import { probe } from '../harness/probe.ts';
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';

await probe(async ({ open }) => {
  const { page } = await open({
    viewport: { width: 960, height: 600 },
    init: (p) => p.addInitScript(() => localStorage.setItem('lf.audioDevices', JSON.stringify({ bufferFrames: 128, asioEnabled: false }))),
  });
  const result = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const { pluginBridge } = await import('/src/audio/plugin-bridge.ts');
    const { bootPluginHost } = await import('/src/app/boot.ts');
    const settings = await import('/src/audio/audio-devices.ts');
    const instrument = await import('/src/audio/instrument.ts');
    const { readAudioDeviceSettings, writeAudioDeviceSettings } = await import('/src/audio/audio-settings.ts');
    const calls = [];
    const host = platform.pluginHost;
    host.available = true;
    pluginBridge.init = async () => {};
    host.init = async () => { calls.push('init'); };
    host.listLoaded = async () => [];
    host.listInputDevices = async () => [];
    host.listOutputDevices = async () => [];
    host.setBufferSize = async (frames) => { calls.push(`buffer:${frames}`); };
    host.setAsioEnabled = async (enabled) => { calls.push(`asio:${enabled}`); };
    // Availability follows the startup status report (`applyAsioReport`), not `host.asioAvailable`.
    host.asioStatus = async () => ({ status: 'ready', detail: '' });
    host.asioProbe = async () => ({ status: 'ready', detail: '' });
    host.asioDeviceInfo = async () => ({ name: 'Probe ASIO', inputChannels: 4, outputChannels: 2 });
    host.scanPlugins = async () => { calls.push('scan'); return []; };
    const dispose = bootPluginHost();
    await instrument.scanForPlugins(); // A manual rescan must not bypass startup.
    const earlyScan = calls.includes('scan');
    await new Promise((resolve) => setTimeout(resolve, 150));
    dispose();
    const startup = [...calls];
    host.setBufferSize = async () => { throw new Error('Injected rejected buffer setting'); };
    await settings.setBufferSize(512);
    const rejectedBuffer = { shown: settings.bufferFrames(), saved: readAudioDeviceSettings().bufferFrames };
    host.setAsioEnabled = async () => { throw new Error('Injected rejected driver setting'); };
    await settings.setAsioEnabled(true);
    const rejectedDriver = { shown: settings.asioEnabled(), saved: readAudioDeviceSettings().asioEnabled };
    const writes = [];
    let active = 0;
    let maxActive = 0;
    host.setBufferSize = async (frames) => {
      maxActive = Math.max(maxActive, ++active);
      await new Promise((resolve) => setTimeout(resolve, frames === 64 ? 40 : 1));
      writes.push(frames); --active;
    };
    await Promise.all([settings.setBufferSize(64), settings.setBufferSize(256)]);
    host.setAsioEnabled = async () => {};
    await settings.setAsioEnabled(true);
    writeAudioDeviceSettings({ inputDeviceId: 'unplugged-windows-endpoint', inputChannel: '1' });
    await settings.refreshAndPruneDevices();
    const asioChannelAfterWindowsPrune = readAudioDeviceSettings().inputChannel;
    window.__lf.ui.openSettings();
    await new Promise((resolve) => setTimeout(resolve, 50));
    const input = document.querySelector('[aria-label="Audio input device"]');
    const output = document.querySelector('[aria-label="Monitor output device"]');
    const channel = document.querySelector('[aria-label="Input channel"]');
    const asioUi = { inputDisabled: input.disabled, outputDisabled: output.disabled,
      name: input.selectedOptions[0].textContent, channels: channel.options.length - 1 };
    host.setAsioEnabled = async () => { throw new Error('Injected UI driver rejection'); };
    const checkbox = document.querySelector('[aria-label="Use ASIO low-latency audio"]');
    checkbox.click();
    await new Promise((resolve) => setTimeout(resolve, 40));
    const rejectedCheckbox = checkbox.checked;
    window.__lf.ui.closeSettings();
    const final = { shown: settings.bufferFrames(), saved: readAudioDeviceSettings().bufferFrames };
    host.setBufferSize = async () => { throw new Error('Injected failed startup'); };
    const scanCount = calls.filter((call) => call === 'scan').length;
    const disposeFailed = bootPluginHost();
    await new Promise((resolve) => setTimeout(resolve, 50));
    await instrument.scanForPlugins();
    disposeFailed();
    const failedStartupBlocked = !instrument.nativeHostReady() && calls.filter((call) => call === 'scan').length === scanCount;
    return { startup, earlyScan, rejectedBuffer, rejectedDriver, writes, maxActive, asioUi, rejectedCheckbox, failedStartupBlocked, asioChannelAfterWindowsPrune,
      final };
  });
  console.log(JSON.stringify(result));
  assert.equal(result.earlyScan, false);
  assert.ok(result.startup.indexOf('buffer:128') >= 0 && result.startup.indexOf('buffer:128') < result.startup.indexOf('scan'), 'saved buffer must reach host before plugins become selectable');
  assert.ok(result.startup.indexOf('asio:false') < result.startup.indexOf('scan'), 'saved driver must reach host before plugins become selectable');
  assert.deepEqual(result.rejectedBuffer, { shown: 128, saved: 128 });
  assert.deepEqual(result.rejectedDriver, { shown: false, saved: false });
  assert.equal(result.maxActive, 1);
  assert.deepEqual(result.writes, [64, 256]);
  assert.deepEqual(result.final, { shown: 256, saved: 256 });
  assert.deepEqual(result.asioUi, { inputDisabled: true, outputDisabled: true, name: 'Probe ASIO', channels: 4 });
  assert.equal(result.rejectedCheckbox, true);
  assert.equal(result.failedStartupBlocked, true);
  assert.equal(result.asioChannelAfterWindowsPrune, '1', 'pruning an unrelated Windows endpoint must not reset the ASIO channel');
  await page.evaluate(() => {
    for (const toast of window.__lf.notify.toasts()) window.__lf.notify.dismissToast(toast.id);
    window.__lf.ui.openSettings();
  });
  await mkdir('logs/layout', { recursive: true });
  await page.screenshot({ path: 'logs/layout/audio-settings.png' });
}, { launch: {} });
